use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nomifun_ai_agent::runtime_handle::{AgentRuntimeHandle, AgentRuntimeControl, MockAgentRuntime};
use nomifun_ai_agent::protocol::events::{
    AgentStreamEvent, ErrorEventData, FinishEventData, TextEventData, ThinkingEventData,
};
use nomifun_ai_agent::types::{AgentRuntimeBuildOptions, SendMessageData};
use nomifun_ai_agent::{
    AgentRuntimeRegistry, AgentSendError, NomiSessionResetOutcome,
};

use crate::response_middleware::{CronCommandResult, CronCreateParams, CronUpdateParams, ICronService};
use nomifun_api_types::AgentErrorCode;
use nomifun_api_types::{
    CloneConversationRequest, ConfirmRequest, CreateConversationRequest, ListConversationsQuery,
    ListMessagesQuery, SearchMessagesQuery, SendMessageRequest, UpdateConversationRequest,
    WebSocketMessage,
};
use nomifun_common::{
    AdaptationPolicy, AgentExecutionEventKind, AgentExecutionStatus, AgentKillReason,
    AgentStepMode, AgentToolPolicy, AgentType, AppError, Confirmation, ConversationSource,
    ConversationId, ConversationStatus,
    DecisionPolicy, DelegationPolicy, ExecutionAttemptStatus, ExecutionStepKind,
    ExecutionStepStatus, MessageId, PaginatedResult, ParticipantAssignmentSource, PlanGate,
    StepFailurePolicy, TimestampMs, now_ms,
};
use nomifun_db::models::{
    AcpSessionRow, AgentMetadataRow, ConversationArtifactRow,
    ConversationDeliveryReceiptRow, ConversationRow, MessageRow, UpdateAgentHandshakeParams,
    UpsertAgentMetadataParams,
};
use nomifun_db::{
    AgentExecutionLeaseToken, AgentExecutionTurnAuthority, AttemptConversationEffectParams,
    ConversationDeliveryReceiptClaim, ConversationFilters, ConversationRowUpdate,
    CreateAcpSessionParams,
    CreateAgentExecutionAttemptParams, CreateAgentExecutionParams, DbError,
    IAcpSessionRepository, IAgentExecutionRepository, IAgentMetadataRepository,
    IConversationRepository, MessageRowUpdate, MessageSearchRow, NewAgentExecutionEvent,
    NewAgentExecutionParticipant, NewAgentExecutionStep, PersistedSessionState,
    ReconcileAgentExecutionPlanParams, SaveRuntimeStateParams,
    SettleAgentExecutionAttemptParams, SortOrder, SqliteAgentExecutionRepository,
    SqliteConversationRepository, TurnLifecycleTransition, TurnReceiptCompletion,
};
use nomifun_realtime::{EventBroadcaster, UserEventSink};
use serde_json::json;
use tokio::sync::{Notify, broadcast};

use crate::service::{
    BackgroundTurnReconciliationDisposition, ConversationService,
    PublicTurnDeliveryState, QuiescentOrphanReconciliation,
};
use crate::RepositoryExecutionConversationBoundary;
use crate::skill_resolver::{FixedSkillResolver, ResolvedAgentSkill, SkillResolver};
use nomifun_knowledge::{
    KnowledgeBinding, KnowledgeCompleter, KnowledgeEventEmitter, KnowledgeService,
};

#[path = "service_test/acp_error_recovery_test.rs"]
mod acp_error_recovery_test;

const SQLITE_TEST_OWNER: &str = "0190f5fe-7c00-7a00-8000-000000000001";
const TEST_USER_1: &str = "0190f5fe-7c00-7a00-8000-000000000011";
const TEST_USER_2: &str = "0190f5fe-7c00-7a00-8000-000000000012";
const PROVIDER_ID_1: &str = "0190f5fe-7c00-7a00-8000-000000000001";
const TEST_ACP_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000101";
const TEST_NOMI_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000114";
const MESSAGE_ID_1: &str = "0190f5fe-7c00-7a00-8000-000000000001";
const PROVIDER_ID_2: &str = "0190f5fe-7c00-7a00-8000-000000000002";
const PROVIDER_ID_3: &str = "0190f5fe-7c00-7a00-8000-000000000003";

struct AlwaysRetainedExecutionBoundary;

#[async_trait::async_trait]
impl crate::ExecutionConversationBoundary for AlwaysRetainedExecutionBoundary {
    async fn projection(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
    ) -> Result<crate::ConversationExecutionProjection, AppError> {
        Ok(crate::ConversationExecutionProjection::default())
    }

    async fn is_active_attempt(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
    ) -> Result<bool, AppError> {
        Ok(false)
    }

    async fn is_retained_attempt(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
    ) -> Result<bool, AppError> {
        Ok(true)
    }
}

struct ActiveRetainedExecutionBoundary;

#[async_trait::async_trait]
impl crate::ExecutionConversationBoundary for ActiveRetainedExecutionBoundary {
    async fn projection(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
    ) -> Result<crate::ConversationExecutionProjection, AppError> {
        Ok(crate::ConversationExecutionProjection::default())
    }

    async fn is_active_attempt(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
    ) -> Result<bool, AppError> {
        Ok(true)
    }

    async fn is_retained_attempt(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
    ) -> Result<bool, AppError> {
        Ok(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlledExecutionClaimBehavior {
    Normal,
    BlockCommittedReturnOnce,
    ExplicitError,
}

struct ControlledExecutionBoundary {
    inner: Arc<dyn crate::ExecutionConversationBoundary>,
    behavior: ControlledExecutionClaimBehavior,
    committed_before_return: Notify,
    block_committed_return_once: AtomicBool,
    abandon_calls: AtomicUsize,
}

impl ControlledExecutionBoundary {
    fn new(
        inner: Arc<dyn crate::ExecutionConversationBoundary>,
        behavior: ControlledExecutionClaimBehavior,
    ) -> Self {
        Self {
            inner,
            behavior,
            committed_before_return: Notify::new(),
            block_committed_return_once: AtomicBool::new(matches!(
                behavior,
                ControlledExecutionClaimBehavior::BlockCommittedReturnOnce
            )),
            abandon_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl crate::ExecutionConversationBoundary for ControlledExecutionBoundary {
    async fn projection(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<crate::ConversationExecutionProjection, AppError> {
        self.inner.projection(owner_id, conversation_id).await
    }

    async fn is_active_attempt(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<bool, AppError> {
        self.inner
            .is_active_attempt(owner_id, conversation_id)
            .await
    }

    async fn is_retained_attempt(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<bool, AppError> {
        self.inner
            .is_retained_attempt(owner_id, conversation_id)
            .await
    }

    async fn claim_attempt_turn_receipt(
        &self,
        owner_id: &str,
        conversation_id: &str,
        operation_id: &str,
        candidate_message_id: &str,
        kind: &str,
        request_payload: &str,
        authority: &AgentExecutionTurnAuthority,
        expected_admission_epoch: i64,
        now: i64,
    ) -> Result<ConversationDeliveryReceiptClaim, AppError> {
        if self.behavior == ControlledExecutionClaimBehavior::ExplicitError {
            return Err(AppError::Conflict(
                "injected Agent Execution claim transaction rollback".to_owned(),
            ));
        }
        let claim = self
            .inner
            .claim_attempt_turn_receipt(
                owner_id,
                conversation_id,
                operation_id,
                candidate_message_id,
                kind,
                request_payload,
                authority,
                expected_admission_epoch,
                now,
            )
            .await?;
        if self
            .block_committed_return_once
            .swap(false, Ordering::SeqCst)
        {
            self.committed_before_return.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(claim)
    }

    async fn abandon_exact_attempt_turn_admission(
        &self,
        owner_id: &str,
        conversation_id: &str,
        operation_id: &str,
        candidate_message_id: &str,
        request_payload: &str,
        authority: &AgentExecutionTurnAuthority,
        expected_admitted_epoch: i64,
        reason: &str,
        completed_at: i64,
    ) -> Result<TurnLifecycleTransition, AppError> {
        self.abandon_calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .abandon_exact_attempt_turn_admission(
                owner_id,
                conversation_id,
                operation_id,
                candidate_message_id,
                request_payload,
                authority,
                expected_admitted_epoch,
                reason,
                completed_at,
            )
            .await
    }

    async fn validate_attempt_turn_effect(
        &self,
        owner_id: &str,
        conversation_id: &str,
        operation_id: &str,
        kind: &str,
        request_payload: &str,
        authority: &AgentExecutionTurnAuthority,
        now: i64,
    ) -> Result<ConversationDeliveryReceiptRow, AppError> {
        self.inner
            .validate_attempt_turn_effect(
                owner_id,
                conversation_id,
                operation_id,
                kind,
                request_payload,
                authority,
                now,
            )
            .await
    }
}

#[derive(Default)]
struct BlockingNoExecutionBoundary {
    entered_retention_check: Notify,
    release_retention_check: Notify,
}

#[async_trait::async_trait]
impl crate::ExecutionConversationBoundary for BlockingNoExecutionBoundary {
    async fn projection(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
    ) -> Result<crate::ConversationExecutionProjection, AppError> {
        Ok(crate::ConversationExecutionProjection::default())
    }

    async fn is_active_attempt(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
    ) -> Result<bool, AppError> {
        Ok(false)
    }

    async fn is_retained_attempt(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
    ) -> Result<bool, AppError> {
        self.entered_retention_check.notify_one();
        self.release_retention_check.notified().await;
        Ok(false)
    }
}

async fn init_database_memory() -> Result<nomifun_db::Database, nomifun_db::DbError> {
    nomifun_db::init_database_memory_with_owner(
        nomifun_common::UserId::parse(SQLITE_TEST_OWNER.to_owned())
            .expect("canonical fixture owner"),
    )
    .await
}

#[derive(Clone, Debug)]
struct SkillLinkCall {
    workspace: PathBuf,
    rel_dirs: Vec<String>,
    skill_names: Vec<String>,
}

struct RecordingSkillResolver {
    names: Vec<String>,
    links: Arc<Mutex<Vec<SkillLinkCall>>>,
}

impl RecordingSkillResolver {
    fn new(names: Vec<String>) -> Self {
        Self {
            names,
            links: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl SkillResolver for RecordingSkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        self.names.clone()
    }

    async fn resolve_skills(&self, names: &[String]) -> Vec<ResolvedAgentSkill> {
        names
            .iter()
            .map(|name| ResolvedAgentSkill {
                name: name.clone(),
                source_path: std::env::temp_dir().join(format!("skill-source-{name}")),
            })
            .collect()
    }

    async fn link_workspace_skills(&self, workspace: &Path, rel_dirs: &[&str], skills: &[ResolvedAgentSkill]) -> usize {
        self.links.lock().unwrap().push(SkillLinkCall {
            workspace: workspace.to_path_buf(),
            rel_dirs: rel_dirs.iter().map(|s| (*s).to_owned()).collect(),
            skill_names: skills.iter().map(|skill| skill.name.clone()).collect(),
        });

        let mut linked = 0;
        for rel_dir in rel_dirs {
            let target_dir = workspace.join(rel_dir);
            if std::fs::create_dir_all(&target_dir).is_err() {
                continue;
            }
            for skill in skills {
                if std::fs::create_dir_all(target_dir.join(&skill.name)).is_ok() {
                    linked += 1;
                }
            }
        }
        linked
    }
}

// ── Mock EventBroadcaster ──────────────────────────────────────────

struct MockBroadcaster {
    events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    deliveries: Mutex<Vec<(String, WebSocketMessage<serde_json::Value>)>>,
}

impl MockBroadcaster {
    fn new() -> Self {
        Self {
            events: Mutex::new(vec![]),
            deliveries: Mutex::new(vec![]),
        }
    }

    fn take_events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
        std::mem::take(&mut self.events.lock().unwrap())
    }

    fn take_deliveries(&self) -> Vec<(String, WebSocketMessage<serde_json::Value>)> {
        std::mem::take(&mut self.deliveries.lock().unwrap())
    }
}

impl EventBroadcaster for MockBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

impl UserEventSink for MockBroadcaster {
    fn send_to_user(&self, user_id: &str, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event.clone());
        self.deliveries
            .lock()
            .unwrap()
            .push((user_id.to_owned(), event));
    }
}

// ── Mock Repository ────────────────────────────────────────────────

struct MockRepo {
    rows: Mutex<Vec<ConversationRow>>,
    messages: Mutex<Vec<MessageRow>>,
    artifacts: Mutex<Vec<ConversationArtifactRow>>,
    delivery_receipts: Mutex<HashMap<String, ConversationDeliveryReceiptRow>>,
    turn_admissions: Mutex<HashMap<String, (i64, Option<String>)>>,
    fail_set_mcp_server_ids: AtomicBool,
    fail_next_messages_keyset: AtomicBool,
    block_turn_finalization: AtomicBool,
    turn_finalization_attempted: Notify,
}

impl MockRepo {
    fn new() -> Self {
        Self {
            rows: Mutex::new(vec![]),
            messages: Mutex::new(vec![]),
            artifacts: Mutex::new(vec![]),
            delivery_receipts: Mutex::new(HashMap::new()),
            turn_admissions: Mutex::new(HashMap::new()),
            fail_set_mcp_server_ids: AtomicBool::new(false),
            fail_next_messages_keyset: AtomicBool::new(false),
            block_turn_finalization: AtomicBool::new(false),
            turn_finalization_attempted: Notify::new(),
        }
    }

    fn fail_next_mcp_selection_write(&self) {
        self.fail_set_mcp_server_ids.store(true, Ordering::SeqCst);
    }

    fn fail_next_messages_keyset_read(&self) {
        self.fail_next_messages_keyset
            .store(true, Ordering::SeqCst);
    }

    fn block_turn_finalization(&self, blocked: bool) {
        self.block_turn_finalization.store(blocked, Ordering::SeqCst);
    }

    async fn wait_for_turn_finalization_attempt(&self) {
        self.turn_finalization_attempted.notified().await;
    }
}

#[async_trait::async_trait]
impl IConversationRepository for MockRepo {
    async fn get(&self, id: &str) -> Result<Option<ConversationRow>, nomifun_db::DbError> {
        let rows = self.rows.lock().unwrap();
        Ok(rows.iter().find(|r| r.conversation_id == id).cloned())
    }

    async fn has_accepted_delivery_receipt_operation_prefix(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id_prefix: &str,
    ) -> Result<bool, nomifun_db::DbError> {
        Ok(self
            .delivery_receipts
            .lock()
            .unwrap()
            .values()
            .any(|receipt| {
                receipt.user_id == user_id
                    && receipt.conversation_id == conversation_id
                    && receipt.operation_id.starts_with(operation_id_prefix)
                    && receipt.status == "accepted"
            }))
    }

    async fn get_turn_admission_state(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<nomifun_db::ConversationTurnAdmissionState, nomifun_db::DbError> {
        let rows = self.rows.lock().unwrap();
        rows.iter()
            .find(|row| row.conversation_id == conversation_id && row.user_id == user_id)
            .ok_or_else(|| nomifun_db::DbError::NotFound("conversation".to_owned()))?;
        let admission = self
            .turn_admissions
            .lock()
            .unwrap()
            .get(conversation_id)
            .cloned()
            .unwrap_or((0, None));
        Ok(nomifun_db::ConversationTurnAdmissionState {
            epoch: admission.0,
            active_operation_id: admission.1,
        })
    }

    async fn validate_active_turn_operation(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
    ) -> Result<bool, nomifun_db::DbError> {
        let owned = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .any(|row| row.conversation_id == conversation_id && row.user_id == user_id);
        if !owned {
            return Err(DbError::NotFound("conversation".to_owned()));
        }
        Ok(self
            .turn_admissions
            .lock()
            .unwrap()
            .get(conversation_id)
            .and_then(|(_, active)| active.as_deref())
            == Some(operation_id))
    }

    async fn get_delivery_receipt(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
    ) -> Result<Option<ConversationDeliveryReceiptRow>, DbError> {
        Ok(self
            .delivery_receipts
            .lock()
            .unwrap()
            .get(operation_id)
            .filter(|receipt| {
                receipt.user_id == user_id
                    && receipt.conversation_id == conversation_id
            })
            .cloned())
    }

    async fn claim_turn_delivery_receipt_and_admit_with_candidate(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        candidate_message_id: &str,
        request_payload: &str,
        expected_admission_epoch: i64,
        now: i64,
    ) -> Result<ConversationDeliveryReceiptClaim, DbError> {
        MessageId::parse(candidate_message_id).map_err(|error| {
            DbError::Conflict(format!("invalid candidate message id: {error}"))
        })?;
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|row| {
                row.conversation_id == conversation_id && row.user_id == user_id
            })
            .ok_or_else(|| DbError::NotFound("conversation".to_owned()))?;
        let mut admissions = self.turn_admissions.lock().unwrap();
        let admission = admissions
            .entry(conversation_id.to_owned())
            .or_insert((0, None));
        let mut receipts = self.delivery_receipts.lock().unwrap();
        if let Some(receipt) = receipts.get(operation_id) {
            if receipt.user_id != user_id
                || receipt.conversation_id != conversation_id
                || receipt.kind != "turn"
                || receipt.request_payload != request_payload
            {
                return Err(DbError::Conflict(
                    "conversation delivery operation identity was reused"
                        .to_owned(),
                ));
            }
            return Ok(ConversationDeliveryReceiptClaim {
                receipt: receipt.clone(),
                claimed_new: false,
            });
        }
        if admission.0 != expected_admission_epoch
            || admission.1.is_some()
            || !matches!(row.status.as_deref(), Some("pending" | "finished"))
            || receipts.values().any(|receipt| {
                receipt.user_id == user_id
                    && receipt.conversation_id == conversation_id
                    && receipt.kind == "turn"
                    && receipt.status == "accepted"
            })
        {
            return Err(DbError::Conflict(
                "Conversation lifecycle rejected durable turn admission"
                    .to_owned(),
            ));
        }
        let receipt = ConversationDeliveryReceiptRow {
            id: receipts.len() as i64 + 1,
            operation_id: operation_id.to_owned(),
            message_id: candidate_message_id.to_owned(),
            conversation_id: conversation_id.to_owned(),
            user_id: user_id.to_owned(),
            kind: "turn".to_owned(),
            request_payload: request_payload.to_owned(),
            status: "accepted".to_owned(),
            result_ok: None,
            result_text: None,
            result_error: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
            projected_conversation_id: Some(conversation_id.to_owned()),
            projected_message_id: None,
        };
        row.status = Some("running".to_owned());
        row.updated_at = row.updated_at.max(now);
        admission.0 += 1;
        admission.1 = Some(operation_id.to_owned());
        receipts.insert(operation_id.to_owned(), receipt.clone());
        Ok(ConversationDeliveryReceiptClaim {
            receipt,
            claimed_new: true,
        })
    }

    async fn create(&self, row: &ConversationRow) -> Result<String, nomifun_db::DbError> {
        let mut rows = self.rows.lock().unwrap();
        if rows
            .iter()
            .any(|existing| existing.conversation_id == row.conversation_id)
        {
            return Err(nomifun_db::DbError::Conflict(format!(
                "Conversation {}",
                row.conversation_id
            )));
        }
        let mut stored = row.clone();
        if stored.id <= 0 {
            stored.id = rows.iter().map(|existing| existing.id).max().unwrap_or(0) + 1;
        }
        let conversation_id = stored.conversation_id.clone();
        rows.push(stored);
        Ok(conversation_id)
    }

    async fn update(&self, id: &str, updates: &ConversationRowUpdate) -> Result<(), nomifun_db::DbError> {
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|r| r.conversation_id == id)
            .ok_or_else(|| nomifun_db::DbError::NotFound(format!("Conversation {id}")))?;

        if let Some(name) = &updates.name {
            row.name = name.clone();
        }
        if let Some(pinned) = updates.pinned {
            row.pinned = pinned;
        }
        if let Some(pinned_at) = &updates.pinned_at {
            row.pinned_at = *pinned_at;
        }
        if let Some(model) = &updates.model {
            row.model = model.clone();
        }
        if let Some(extra) = &updates.extra {
            row.extra = extra.clone();
        }
        if let Some(status) = &updates.status {
            row.status = Some(status.clone());
        }
        if let Some(cron_job_id) = &updates.cron_job_id {
            row.cron_job_id = cron_job_id.clone();
        }
        if let Some(updated_at) = updates.updated_at {
            row.updated_at = updated_at;
        }
        Ok(())
    }

    async fn finalize_turn(
        &self,
        user_id: &str,
        conversation_id: &str,
        receipt_completion: Option<&TurnReceiptCompletion>,
        completed_at: TimestampMs,
    ) -> Result<TurnLifecycleTransition, DbError> {
        self.turn_finalization_attempted.notify_one();
        if self.block_turn_finalization.load(Ordering::SeqCst) {
            return Err(DbError::Init(
                "injected durable turn finalization failure".to_owned(),
            ));
        }
        if receipt_completion.is_some() {
            return Err(DbError::Init(
                "MockRepo does not implement delivery receipts".to_owned(),
            ));
        }
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|row| row.conversation_id == conversation_id && row.user_id == user_id)
            .ok_or_else(|| DbError::NotFound("conversation".to_owned()))?;
        match row.status.as_deref() {
            Some("running") => {
                row.status = Some("finished".to_owned());
                row.updated_at = completed_at;
                Ok(TurnLifecycleTransition::Committed)
            }
            Some("finished") => Ok(TurnLifecycleTransition::AlreadyApplied),
            _ => Ok(TurnLifecycleTransition::Stale),
        }
    }

    async fn finalize_exact_turn_operation(
        &self,
        user_id: &str,
        conversation_id: &str,
        completion: &TurnReceiptCompletion,
        completed_at: TimestampMs,
    ) -> Result<TurnLifecycleTransition, DbError> {
        self.turn_finalization_attempted.notify_one();
        if self.block_turn_finalization.load(Ordering::SeqCst) {
            return Err(DbError::Init(
                "injected durable turn finalization failure".to_owned(),
            ));
        }
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|row| {
                row.conversation_id == conversation_id && row.user_id == user_id
            })
            .ok_or_else(|| DbError::NotFound("conversation".to_owned()))?;
        let mut admissions = self.turn_admissions.lock().unwrap();
        let admission = admissions
            .entry(conversation_id.to_owned())
            .or_insert((0, None));
        let mut receipts = self.delivery_receipts.lock().unwrap();
        let receipt = receipts
            .get_mut(&completion.operation_id)
            .ok_or_else(|| DbError::Conflict("missing exact turn receipt".to_owned()))?;
        if receipt.user_id != user_id
            || receipt.conversation_id != conversation_id
            || receipt.kind != completion.kind
            || receipt.request_payload != completion.request_payload
            || !matches!(receipt.status.as_str(), "accepted" | "completed")
        {
            return Err(DbError::Conflict(
                "exact turn receipt identity mismatch".to_owned(),
            ));
        }
        let receipt_was_completed = receipt.status == "completed";
        if !receipt_was_completed {
            receipt.status = "completed".to_owned();
            receipt.result_ok = Some(completion.result_ok);
            receipt.result_text = completion.result_text.clone();
            receipt.result_error = completion.result_error.clone();
            receipt.updated_at = receipt.updated_at.max(completed_at);
            receipt.completed_at = Some(completed_at.max(receipt.created_at));
        }

        if admission.1.as_deref() == Some(completion.operation_id.as_str())
            && matches!(row.status.as_deref(), Some("running" | "finished"))
        {
            row.status = Some("finished".to_owned());
            row.updated_at = row.updated_at.max(completed_at);
            admission.0 += 1;
            admission.1 = None;
            return Ok(TurnLifecycleTransition::Committed);
        }
        if receipt_was_completed
            && admission.1.is_none()
            && row.status.as_deref() == Some("finished")
        {
            return Ok(TurnLifecycleTransition::AlreadyApplied);
        }
        Ok(TurnLifecycleTransition::Stale)
    }

    async fn finalize_exact_cancelled_turn_generation(
        &self,
        user_id: &str,
        conversation_id: &str,
        expected_admission_epoch: i64,
        expected_active_operation_id: Option<&str>,
        _reason: &str,
        completed_at: TimestampMs,
    ) -> Result<TurnLifecycleTransition, DbError> {
        if let Some(operation_id) = expected_active_operation_id {
            let mut rows = self.rows.lock().unwrap();
            let row = rows
                .iter_mut()
                .find(|row| {
                    row.conversation_id == conversation_id
                        && row.user_id == user_id
                })
                .ok_or_else(|| DbError::NotFound("conversation".to_owned()))?;
            let mut admissions = self.turn_admissions.lock().unwrap();
            let admission = admissions
                .entry(conversation_id.to_owned())
                .or_insert((0, None));
            if admission.0 != expected_admission_epoch
                || admission.1.as_deref() != Some(operation_id)
            {
                return Ok(TurnLifecycleTransition::Stale);
            }
            let mut receipts = self.delivery_receipts.lock().unwrap();
            let receipt = receipts.get_mut(operation_id).ok_or_else(|| {
                DbError::Conflict("missing cancelled-turn receipt".to_owned())
            })?;
            if receipt.user_id != user_id
                || receipt.conversation_id != conversation_id
                || receipt.kind != "turn"
                || !matches!(receipt.status.as_str(), "accepted" | "completed")
            {
                return Err(DbError::Conflict(
                    "cancelled-turn receipt identity mismatch".to_owned(),
                ));
            }
            if receipt.status == "accepted" {
                receipt.status = "completed".to_owned();
                receipt.result_ok = Some(false);
                receipt.result_text = None;
                receipt.result_error = Some(_reason.to_owned());
                receipt.updated_at = receipt.updated_at.max(completed_at);
                receipt.completed_at =
                    Some(completed_at.max(receipt.created_at));
            }
            row.status = Some("finished".to_owned());
            row.updated_at = row.updated_at.max(completed_at);
            admission.0 += 1;
            admission.1 = None;
            return Ok(TurnLifecycleTransition::Committed);
        }
        if expected_admission_epoch != 0 {
            return Ok(TurnLifecycleTransition::Stale);
        }
        self.finalize_turn(user_id, conversation_id, None, completed_at)
            .await
    }

    async fn reset_terminal_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
        updated_at: TimestampMs,
    ) -> Result<TurnLifecycleTransition, DbError> {
        // The service tests use one in-memory aggregate lock order to model the
        // production repository's all-or-nothing reset transaction.
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|row| row.conversation_id == conversation_id && row.user_id == user_id)
            .ok_or_else(|| DbError::NotFound("conversation".to_owned()))?;
        match row.status.as_deref() {
            Some("pending" | "finished") => {}
            _ => return Ok(TurnLifecycleTransition::Stale),
        }
        let mut extra: serde_json::Value = serde_json::from_str(&row.extra)
            .map_err(|error| DbError::Conflict(format!("invalid Conversation extra: {error}")))?;
        let object = extra
            .as_object_mut()
            .ok_or_else(|| DbError::Conflict("Conversation extra must be an object".to_owned()))?;
        for key in [
            "sessionKey",
            "session_key",
            "runtimeValidation",
            "runtime_validation",
            "acp_session_id",
            "acpSessionId",
            "acp_session_conversation_id",
            "acpSessionConversationId",
            "acp_session_updated_at",
            "acpSessionUpdatedAt",
            "_edit_resubmit_fence",
        ] {
            object.remove(key);
        }
        let reset_extra = serde_json::to_string(&extra)
            .map_err(|error| DbError::Conflict(format!("could not encode Conversation extra: {error}")))?;
        let mut messages = self.messages.lock().unwrap();
        let mut artifacts = self.artifacts.lock().unwrap();
        messages.retain(|message| message.conversation_id != conversation_id);
        artifacts.retain(|artifact| artifact.conversation_id != conversation_id);
        row.status = Some("pending".to_owned());
        row.extra = reset_extra;
        row.updated_at = row.updated_at.max(updated_at);
        Ok(TurnLifecycleTransition::Committed)
    }

    async fn clear_terminal_conversation_messages(
        &self,
        user_id: &str,
        conversation_id: &str,
        updated_at: TimestampMs,
    ) -> Result<TurnLifecycleTransition, DbError> {
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|row| row.conversation_id == conversation_id && row.user_id == user_id)
            .ok_or_else(|| DbError::NotFound("conversation".to_owned()))?;
        if !matches!(row.status.as_deref(), Some("pending" | "finished")) {
            return Ok(TurnLifecycleTransition::Stale);
        }
        self.messages
            .lock()
            .unwrap()
            .retain(|message| message.conversation_id != conversation_id);
        self.artifacts
            .lock()
            .unwrap()
            .retain(|artifact| artifact.conversation_id != conversation_id);
        row.updated_at = row.updated_at.max(updated_at);
        Ok(TurnLifecycleTransition::Committed)
    }

    async fn delete(&self, id: &str) -> Result<(), nomifun_db::DbError> {
        let mut rows = self.rows.lock().unwrap();
        let len_before = rows.len();
        rows.retain(|r| r.conversation_id != id);
        if rows.len() == len_before {
            return Err(nomifun_db::DbError::NotFound(format!("Conversation {id}")));
        }
        Ok(())
    }

    async fn set_mcp_server_ids(
        &self,
        _conversation_id: &str,
        _mcp_server_ids: &[String],
    ) -> Result<(), nomifun_db::DbError> {
        if self
            .fail_set_mcp_server_ids
            .swap(false, Ordering::SeqCst)
        {
            return Err(nomifun_db::DbError::Init(
                "injected MCP selection write failure".to_owned(),
            ));
        }
        Ok(())
    }

    async fn list_paginated(
        &self,
        user_id: &str,
        filters: &ConversationFilters,
    ) -> Result<PaginatedResult<ConversationRow>, nomifun_db::DbError> {
        let rows = self.rows.lock().unwrap();
        let matched: Vec<_> = rows
            .iter()
            .filter(|r| r.user_id == user_id)
            .filter(|r| {
                filters
                    .source
                    .as_ref()
                    .is_none_or(|s| r.source.as_deref() == Some(s.as_str()))
            })
            .filter(|r| filters.pinned.as_ref().is_none_or(|&p| r.pinned == p))
            .cloned()
            .collect();
        let total = matched.len() as u64;
        let limit = filters.effective_limit() as usize;
        let items: Vec<_> = matched.into_iter().take(limit).collect();
        let has_more = (total as usize) > limit;
        Ok(PaginatedResult { items, total, has_more })
    }

    async fn find_by_source_and_chat(
        &self,
        _user_id: &str,
        _source: &str,
        _chat_id: &str,
        _agent_type: &str,
    ) -> Result<Option<ConversationRow>, nomifun_db::DbError> {
        Ok(None)
    }

    async fn list_by_cron_job(
        &self,
        _user_id: &str,
        _cron_job_id: &str,
    ) -> Result<Vec<ConversationRow>, nomifun_db::DbError> {
        Ok(vec![])
    }

    async fn list_associated(
        &self,
        _user_id: &str,
        _conversation_id: &str,
    ) -> Result<Vec<ConversationRow>, nomifun_db::DbError> {
        Ok(vec![])
    }

    async fn get_messages(
        &self,
        conv_id: &str,
        page: u32,
        page_size: u32,
        order: SortOrder,
    ) -> Result<PaginatedResult<MessageRow>, nomifun_db::DbError> {
        let messages = self.messages.lock().unwrap();
        let mut matched: Vec<_> = messages
            .iter()
            .filter(|message| message.conversation_id == conv_id)
            .cloned()
            .collect();
        matched.sort_by_key(|message| message.created_at);
        if matches!(order, SortOrder::Desc) {
            matched.reverse();
        }

        let start = page.saturating_sub(1) as usize * page_size as usize;
        let end = (start + page_size as usize).min(matched.len());
        let items = if start < matched.len() {
            matched[start..end].to_vec()
        } else {
            Vec::new()
        };
        Ok(PaginatedResult {
            items,
            total: matched.len() as u64,
            has_more: end < matched.len(),
        })
    }

    async fn get_messages_keyset(
        &self,
        conv_id: &str,
        before: Option<(i64, String)>,
        limit: u32,
    ) -> Result<PaginatedResult<MessageRow>, nomifun_db::DbError> {
        if self
            .fail_next_messages_keyset
            .swap(false, Ordering::SeqCst)
        {
            return Err(nomifun_db::DbError::Init(
                "injected transcript lookup failure".to_owned(),
            ));
        }
        let messages = self.messages.lock().unwrap();
        let mut matched: Vec<_> = messages
            .iter()
            .filter(|message| message.conversation_id == conv_id)
            .filter(|message| {
                before.as_ref().is_none_or(|(created_at, id)| {
                    (message.created_at, message.message_id.as_str())
                        < (*created_at, id.as_str())
                })
            })
            .cloned()
            .collect();
        matched.sort_by(|left, right| {
            (right.created_at, right.message_id.as_str())
                .cmp(&(left.created_at, left.message_id.as_str()))
        });
        let has_more = matched.len() > limit as usize;
        matched.truncate(limit as usize);
        Ok(PaginatedResult {
            items: matched,
            total: 0,
            has_more,
        })
    }

    async fn get_message(&self, conv_id: &str, message_id: &str) -> Result<Option<MessageRow>, nomifun_db::DbError> {
        let messages = self.messages.lock().unwrap();
        Ok(messages
            .iter()
            .find(|message| {
                message.conversation_id == conv_id && message.message_id == message_id
            })
            .cloned())
    }

    async fn insert_message(&self, message: &MessageRow) -> Result<(), nomifun_db::DbError> {
        let mut messages = self.messages.lock().unwrap();
        if messages
            .iter()
            .any(|existing| existing.message_id == message.message_id)
        {
            return Err(nomifun_db::DbError::Conflict(format!(
                "Message {}",
                message.message_id
            )));
        }
        let mut stored = message.clone();
        if stored.id <= 0 {
            stored.id = messages.iter().map(|existing| existing.id).max().unwrap_or(0) + 1;
        }
        messages.push(stored);
        Ok(())
    }

    async fn update_message(&self, id: &str, updates: &MessageRowUpdate) -> Result<(), nomifun_db::DbError> {
        let mut messages = self.messages.lock().unwrap();
        let message = messages
            .iter_mut()
            .find(|message| message.message_id == id)
            .ok_or_else(|| nomifun_db::DbError::NotFound(format!("Message {id}")))?;

        if let Some(content) = &updates.content {
            message.content = content.clone();
        }
        if let Some(status) = &updates.status {
            message.status = status.clone();
        }
        if let Some(hidden) = updates.hidden {
            message.hidden = hidden;
        }
        Ok(())
    }

    async fn delete_messages_by_conversation(&self, conv_id: &str) -> Result<(), nomifun_db::DbError> {
        self.messages
            .lock()
            .unwrap()
            .retain(|message| message.conversation_id != conv_id);
        Ok(())
    }

    async fn get_message_by_msg_id(
        &self,
        conv_id: &str,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, nomifun_db::DbError> {
        let messages = self.messages.lock().unwrap();
        Ok(messages
            .iter()
            .find(|message| {
                message.conversation_id == conv_id
                    && message.msg_id.as_deref() == Some(msg_id)
                    && message.r#type == msg_type
            })
            .cloned())
    }

    async fn search_messages(
        &self,
        _user_id: &str,
        _keyword: &str,
        _page: u32,
        _page_size: u32,
    ) -> Result<PaginatedResult<MessageSearchRow>, nomifun_db::DbError> {
        Ok(PaginatedResult {
            items: vec![],
            total: 0,
            has_more: false,
        })
    }

    async fn list_artifacts(&self, conversation_id: &str) -> Result<Vec<ConversationArtifactRow>, nomifun_db::DbError> {
        Ok(self
            .artifacts
            .lock()
            .unwrap()
            .iter()
            .filter(|artifact| artifact.conversation_id == conversation_id)
            .cloned()
            .collect())
    }

    async fn get_artifact(
        &self,
        conversation_id: &str,
        conversation_artifact_id: &str,
    ) -> Result<Option<ConversationArtifactRow>, nomifun_db::DbError> {
        Ok(self
            .artifacts
            .lock()
            .unwrap()
            .iter()
            .find(|artifact| {
                artifact.conversation_id == conversation_id
                    && artifact.conversation_artifact_id == conversation_artifact_id
            })
            .cloned())
    }

    async fn upsert_artifact(
        &self,
        artifact: &ConversationArtifactRow,
    ) -> Result<ConversationArtifactRow, nomifun_db::DbError> {
        let mut artifacts = self.artifacts.lock().unwrap();
        // Mirror the SQLite contract: skill_suggest upserts against
        // (conversation_id, cron_job_id); cron_trigger always inserts fresh.
        // The input `id` is ignored — SQLite allocates the INTEGER PK.
        if artifact.kind == "skill_suggest"
            && let Some(existing) = artifacts.iter_mut().find(|row| {
                row.kind == "skill_suggest"
                    && row.conversation_id == artifact.conversation_id
                    && row.cron_job_id == artifact.cron_job_id
            })
        {
            let conversation_artifact_id = existing.conversation_artifact_id.clone();
            *existing = artifact.clone();
            existing.conversation_artifact_id = conversation_artifact_id;
            return Ok(existing.clone());
        }
        if artifacts.iter().any(|existing| {
            existing.conversation_artifact_id == artifact.conversation_artifact_id
        }) {
            return Err(nomifun_db::DbError::Conflict(format!(
                "Conversation artifact {}",
                artifact.conversation_artifact_id
            )));
        }
        artifacts.push(artifact.clone());
        Ok(artifact.clone())
    }

    async fn update_artifact_status(
        &self,
        conversation_id: &str,
        conversation_artifact_id: &str,
        status: &str,
        updated_at: TimestampMs,
    ) -> Result<Option<ConversationArtifactRow>, nomifun_db::DbError> {
        let mut artifacts = self.artifacts.lock().unwrap();
        let Some(existing) = artifacts
            .iter_mut()
            .find(|artifact| {
                artifact.conversation_id == conversation_id
                    && artifact.conversation_artifact_id == conversation_artifact_id
            })
        else {
            return Ok(None);
        };
        existing.status = status.to_owned();
        existing.updated_at = updated_at;
        Ok(Some(existing.clone()))
    }

    async fn mark_skill_suggest_artifacts_saved(
        &self,
        _user_id: &str,
        cron_job_id: &str,
        updated_at: TimestampMs,
    ) -> Result<Vec<ConversationArtifactRow>, nomifun_db::DbError> {
        let mut artifacts = self.artifacts.lock().unwrap();
        let mut updated = Vec::new();
        for artifact in artifacts
            .iter_mut()
            .filter(|artifact| artifact.cron_job_id.as_deref() == Some(cron_job_id))
        {
            artifact.status = "saved".into();
            artifact.updated_at = updated_at;
            updated.push(artifact.clone());
        }
        Ok(updated)
    }

    async fn delete_artifacts_by_conversation(&self, conversation_id: &str) -> Result<(), nomifun_db::DbError> {
        self.artifacts
            .lock()
            .unwrap()
            .retain(|artifact| artifact.conversation_id != conversation_id);
        Ok(())
    }

}

// ── Helpers ────────────────────────────────────────────────────────

struct StubAgentMetadataRepo;

fn test_acp_agent_metadata() -> AgentMetadataRow {
    AgentMetadataRow {
        id: 1,
        agent_id: TEST_ACP_AGENT_ID.to_owned(),
        icon: None,
        name: "Claude Code".to_owned(),
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("claude".to_owned()),
        agent_type: AgentType::Acp.serde_name().to_owned(),
        agent_source: "builtin".to_owned(),
        agent_source_info: None,
        source_key: Some("agent_builtin_claude".to_owned()),
        enabled: true,
        command: Some("claude".to_owned()),
        args: Some("[]".to_owned()),
        env: Some("[]".to_owned()),
        native_skills_dirs: Some(r#"[".claude/skills"]"#.to_owned()),
        behavior_policy: None,
        yolo_id: Some("bypassPermissions".to_owned()),
        agent_capabilities: None,
        auth_methods: None,
        config_options: None,
        available_modes: None,
        available_models: None,
        available_commands: None,
        sort_order: 0,
        created_at: 1,
        updated_at: 1,
    }
}

#[async_trait::async_trait]
impl IAgentMetadataRepository for StubAgentMetadataRepo {
    async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
        Ok(vec![test_acp_agent_metadata()])
    }
    async fn get(&self, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok((id == TEST_ACP_AGENT_ID).then(test_acp_agent_metadata))
    }
    async fn find_by_source_and_name(
        &self,
        _agent_source: &str,
        _name: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
    }
    async fn find_builtin_by_backend(&self, _backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
    }
    async fn upsert(&self, _params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
        Err(DbError::Init("stub".into()))
    }
    async fn apply_handshake(
        &self,
        _id: &str,
        _params: &UpdateAgentHandshakeParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
    }
    async fn set_enabled(&self, _id: &str, _enabled: bool) -> Result<bool, DbError> {
        Ok(false)
    }
    async fn delete(&self, _id: &str) -> Result<bool, DbError> {
        Ok(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeStateSaveCall {
    conversation_id: String,
    current_model_id: Option<Option<String>>,
}

#[derive(Default)]
struct StubAcpSessionRepo {
    runtime_state_saves: Mutex<Vec<RuntimeStateSaveCall>>,
    cleared_session_ids: Mutex<Vec<String>>,
}

impl StubAcpSessionRepo {
    fn runtime_state_saves(&self) -> Vec<RuntimeStateSaveCall> {
        self.runtime_state_saves.lock().unwrap().clone()
    }

    fn cleared_session_ids(&self) -> Vec<String> {
        self.cleared_session_ids.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl IAcpSessionRepository for StubAcpSessionRepo {
    async fn get(&self, _conversation_id: &str) -> Result<Option<AcpSessionRow>, DbError> {
        Ok(None)
    }
    async fn create(&self, params: &CreateAcpSessionParams<'_>) -> Result<AcpSessionRow, DbError> {
        // Return a synthetic row so `ConversationService::create` can
        // succeed for ACP conversations in unit tests.
        Ok(AcpSessionRow {
            id: 0,
            conversation_id: params.conversation_id.to_owned(),
            agent_backend: params.agent_backend.to_owned(),
            agent_source: params.agent_source.to_owned(),
            agent_id: params.agent_id.to_owned(),
            acp_session_id: None,
            session_status: "idle".into(),
            session_config: "{}".into(),
            last_active_at: None,
            suspended_at: None,
        })
    }
    async fn update_session_id(&self, _conversation_id: &str, _session_id: &str) -> Result<bool, DbError> {
        Ok(false)
    }
    async fn clear_session_id(&self, conversation_id: &str) -> Result<bool, DbError> {
        self.cleared_session_ids
            .lock()
            .unwrap()
            .push(conversation_id.to_owned());
        Ok(true)
    }
    async fn delete(&self, _conversation_id: &str) -> Result<bool, DbError> {
        Ok(false)
    }
    async fn load_runtime_state(&self, _conversation_id: &str) -> Result<Option<PersistedSessionState>, DbError> {
        Ok(Some(PersistedSessionState {
            current_model_id: Some("deepseek-v4-pro".to_owned()),
            ..Default::default()
        }))
    }
    async fn save_runtime_state(
        &self,
        conversation_id: &str,
        params: &SaveRuntimeStateParams<'_>,
    ) -> Result<bool, DbError> {
        self.runtime_state_saves.lock().unwrap().push(RuntimeStateSaveCall {
            conversation_id: conversation_id.to_owned(),
            current_model_id: params.current_model_id.map(|outer| outer.map(ToOwned::to_owned)),
        });
        Ok(true)
    }
}

fn make_service() -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<MockRepo>,
    Arc<dyn AgentRuntimeRegistry>,
) {
    make_service_with_resolver(Arc::new(FixedSkillResolver { names: vec![] }))
}

fn make_service_with_resolver(
    skill_resolver: Arc<dyn crate::skill_resolver::SkillResolver>,
) -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<MockRepo>,
    Arc<dyn AgentRuntimeRegistry>,
) {
    make_service_with_resolver_and_acp_session_repo(skill_resolver, Arc::new(StubAcpSessionRepo::default()))
}

fn make_service_with_resolver_and_acp_session_repo(
    skill_resolver: Arc<dyn crate::skill_resolver::SkillResolver>,
    acp_session_repo: Arc<dyn IAcpSessionRepository>,
) -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<MockRepo>,
    Arc<dyn AgentRuntimeRegistry>,
) {
    let repo = Arc::new(MockRepo::new());
    let broadcaster = Arc::new(MockBroadcaster::new());
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo);
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());
    let svc = ConversationService::new(
        Arc::<str>::from(TEST_USER_1),
        std::env::temp_dir(),
        broadcaster.clone(),
        skill_resolver,
        runtime_registry.clone(),
        repo.clone(),
        agent_metadata_repo,
        acp_session_repo,
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    (svc, broadcaster, repo, runtime_registry)
}

fn make_service_with_workspace_root(
    workspace_root: PathBuf,
) -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<MockRepo>,
    Arc<dyn AgentRuntimeRegistry>,
) {
    let repo = Arc::new(MockRepo::new());
    let broadcaster = Arc::new(MockBroadcaster::new());
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(StubAgentMetadataRepo);
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> =
        Arc::new(MockAgentRuntimeRegistry::new());
    let svc = ConversationService::new(
        Arc::<str>::from(TEST_USER_1),
        workspace_root,
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        agent_metadata_repo,
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    (svc, broadcaster, repo, runtime_registry)
}

fn make_create_req() -> CreateConversationRequest {
    let workspace = isolated_test_workspace("acp");
    serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "agent_id": TEST_ACP_AGENT_ID,
            "workspace": workspace
        }
    }))
    .unwrap()
}

/// Runtime-building tests need a real canonicalizable workspace. Keep the
/// unique TempDir for the lifetime of the test process because many service
/// calls return after spawning a turn owner that may still touch the workspace.
/// Production workspace validation remains unchanged.
fn isolated_test_workspace(label: &str) -> PathBuf {
    tempfile::Builder::new()
        .prefix(&format!("nomifun-conversation-{label}-"))
        .tempdir()
        .expect("create isolated conversation test workspace")
        .keep()
}

fn cross_platform_mock_workspace() -> &'static str {
    static WORKSPACE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    WORKSPACE
        .get_or_init(|| std::env::temp_dir().to_string_lossy().into_owned())
        .as_str()
}

// ── Create tests ───────────────────────────────────────────────────

#[tokio::test]
async fn create_returns_conversation_with_defaults() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();

    let resp = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    assert!(ConversationId::try_from(resp.conversation_id.as_str()).is_ok());
    assert_eq!(resp.r#type, AgentType::Acp);
    assert_eq!(resp.status, ConversationStatus::Pending);
    assert_eq!(resp.source, Some(ConversationSource::Nomifun));
    assert!(!resp.pinned);
    assert!(resp.pinned_at.is_none());
    assert!(
        Path::new(resp.extra["workspace"].as_str().unwrap()).is_dir(),
        "the default ACP fixture must carry a real isolated workspace"
    );
    assert!(resp.created_at > 0);
    assert_eq!(resp.created_at, resp.modified_at);

    // Should have broadcast a listChanged(created) event
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "conversation.listChanged");
    assert_eq!(events[0].data["action"], "created");
    assert_eq!(events[0].data["conversation_id"], resp.conversation_id);
    assert_eq!(events[0].data["source"], "nomifun");
}

#[tokio::test]
async fn create_rejects_backend_only_acp_identity_before_persisting() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let req = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "backend": "claude",
            "workspace": "/project"
        }
    }))
    .unwrap();

    let error = svc.create(TEST_USER_1, req).await.unwrap_err();
    assert!(matches!(
        error,
        AppError::BadRequest(message) if message.contains("extra.agent_id")
    ));
    assert!(repo.rows.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_rejects_every_backend_owned_lifecycle_extra_key_before_persisting() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();

    for key in crate::service::BACKEND_OWNED_LIFECYCLE_EXTRA_KEYS {
        let mut req = make_create_req();
        req.extra
            .as_object_mut()
            .expect("fixture extra must be an object")
            .insert(key.to_owned(), json!("forged-client-authority"));

        let error = svc.create(TEST_USER_1, req).await.unwrap_err();
        assert!(
            matches!(
                error,
                AppError::BadRequest(ref message)
                    if message.contains(key) && message.contains("backend-owned")
            ),
            "create accepted or misclassified backend-owned extra key {key}: {error:?}"
        );
        assert!(
            repo.rows.lock().unwrap().is_empty(),
            "create persisted a partial row after rejecting backend-owned extra key {key}"
        );
    }
}

#[tokio::test]
async fn create_rejects_missing_acp_agent_parent_before_persisting() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let missing_agent_id = ConversationId::new().into_string();
    let req = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "agent_id": missing_agent_id,
            "workspace": "/project"
        }
    }))
    .unwrap();

    let error = svc.create(TEST_USER_1, req).await.unwrap_err();
    assert!(matches!(
        error,
        AppError::BadRequest(message) if message.contains("does not exist")
    ));
    assert!(repo.rows.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_rejects_acp_backend_that_disagrees_with_agent_parent() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let req = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "agent_id": TEST_ACP_AGENT_ID,
            "backend": "codex",
            "workspace": "/project"
        }
    }))
    .unwrap();

    let error = svc.create(TEST_USER_1, req).await.unwrap_err();
    assert!(matches!(
        error,
        AppError::BadRequest(message) if message.contains("does not match agent")
    ));
    assert!(repo.rows.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_rolls_back_row_and_managed_workspace_when_post_create_write_fails() {
    let workspace_root = std::env::temp_dir().join(format!(
        "nomifun-conversation-create-rollback-{}",
        ConversationId::new()
    ));
    let (svc, broadcaster, repo, _runtime_registry) =
        make_service_with_workspace_root(workspace_root.clone());
    repo.fail_next_mcp_selection_write();
    let req = serde_json::from_value(json!({
        "type": "nomi",
        "extra": {
            "selected_mcp_server_ids": ["0190f5fe-7c00-7a00-8000-000000000123"]
        }
    }))
    .unwrap();

    let error = svc.create(TEST_USER_1, req).await.unwrap_err();
    assert!(
        error.to_string().contains("injected MCP selection write failure"),
        "error = {error:?}"
    );
    assert!(repo.rows.lock().unwrap().is_empty());
    assert!(broadcaster.take_events().is_empty());

    let conversations_root = workspace_root.join("conversations");
    if conversations_root.exists() {
        assert!(
            std::fs::read_dir(&conversations_root)
                .unwrap()
                .next()
                .is_none(),
            "managed workspace must not survive a failed create"
        );
    }
    let _ = std::fs::remove_dir_all(workspace_root);
}

#[tokio::test]
async fn create_rejects_numeric_session_mcp_ids() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "agent_id": TEST_ACP_AGENT_ID,
            "workspace": "/project",
            "selected_session_mcp_servers": [{
                "id": 3,
                "name": "everything",
                "transport": {
                    "type": "stdio",
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-everything"]
                }
            }]
        }
    }))
    .unwrap();

    let error = svc.create(TEST_USER_1, req).await.unwrap_err();
    assert!(matches!(
        error,
        AppError::BadRequest(message) if message.contains("Invalid selected_session_mcp_servers")
    ));
}

#[tokio::test]
async fn create_rejects_non_string_session_mcp_ids() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "agent_id": TEST_ACP_AGENT_ID,
            "workspace": "/project",
            "selected_session_mcp_servers": [{
                "id": true,
                "name": "everything",
                "transport": {
                    "type": "stdio",
                    "command": "npx"
                }
            }]
        }
    }))
    .unwrap();

    let error = svc.create(TEST_USER_1, req).await.unwrap_err();

    assert!(matches!(
        error,
        AppError::BadRequest(message) if message.contains("Invalid selected_session_mcp_servers")
    ));
}

#[tokio::test]
async fn create_rejects_workspace_with_trailing_whitespace_in_request() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let dir = std::env::temp_dir().join(format!("nomifun-test-{}", nomifun_common::generate_id()));
    std::fs::create_dir(&dir).unwrap();
    let workspace = dir.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace_with_trailing_space = format!("{} ", workspace.to_string_lossy());

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": workspace_with_trailing_space }
    }))
    .unwrap();
    let err = svc.create(TEST_USER_1, req).await.unwrap_err();

    assert!(matches!(
        err,
        AppError::WorkspacePathEdgeWhitespace(message)
            if message == workspace_with_trailing_space
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn create_accepts_workspace_with_interior_whitespace_segment() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let dir = std::env::temp_dir().join(format!("nomifun-test-{}", nomifun_common::generate_id()));
    // Mirrors the macOS per-user data dir layout ("Application Support"):
    // interior whitespace in a directory name is a normal, supported path.
    let workspace = dir.join("Application Support").join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": workspace.to_string_lossy() }
    }))
    .unwrap();
    let resp = svc.create(TEST_USER_1, req).await.unwrap();

    assert_eq!(resp.extra["workspace"], workspace.to_string_lossy().as_ref());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn create_with_custom_name_and_source() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "Custom Name",
        "source": "telegram",
        "channel_chat_id": "chat:123",
        "extra": { "agent_id": TEST_ACP_AGENT_ID }
    }))
    .unwrap();

    let resp = svc.create(TEST_USER_1, req).await.unwrap();

    assert_eq!(resp.name, "Custom Name");
    assert_eq!(resp.r#type, AgentType::Acp);
    assert_eq!(resp.source, Some(ConversationSource::Telegram));
    assert_eq!(resp.channel_chat_id.as_deref(), Some("chat:123"));
}

#[tokio::test]
async fn create_stores_model_as_json() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();

    // Top-level model is only valid for nomi conversations.
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
        "extra": { "workspace": "/project" }
    }))
    .unwrap();
    let resp = svc.create(TEST_USER_1, req).await.unwrap();

    let model = resp.model.unwrap();
    assert_eq!(model.provider_id, PROVIDER_ID_1);
    assert_eq!(model.model, "m1");
}

// ── Get tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn message_read_immediately_projects_orphaned_writeback_as_retryable() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let conversation = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let message_id = nomifun_common::generate_id();
    repo.insert_message(&MessageRow {
        id: 0,
        message_id: message_id.clone(),
        conversation_id: conversation.conversation_id.clone(),
        msg_id: Some(message_id.clone()),
        r#type: "text".into(),
        content: serde_json::json!({
            "content": "durable answer",
            "knowledge_writeback": {
                "status": "writing",
                "attempt_id": nomifun_common::generate_id(),
                "started_at": now_ms(),
                "updated_at": now_ms(),
                "retryable": false
            }
        })
        .to_string(),
        position: Some("left".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: now_ms(),
    })
    .await
    .unwrap();

    let response = svc
        .get_message(TEST_USER_1, &conversation.conversation_id, &message_id)
        .await
        .unwrap();

    assert_eq!(response.content["knowledge_writeback"]["status"], "interrupted");
    assert_eq!(response.content["knowledge_writeback"]["retryable"], true);
    assert!(
        response.content["knowledge_writeback"]["finished_at"].is_i64(),
        "an orphaned running state must become retryable immediately after restart, not after a heuristic delay"
    );
}

#[tokio::test]
async fn get_existing_conversation() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let created = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let fetched = svc.get(TEST_USER_1, &created.conversation_id).await.unwrap();
    assert_eq!(fetched.conversation_id, created.conversation_id);
    assert_eq!(fetched.name, created.name);
    assert!(fetched.runtime.is_some());
}

#[tokio::test]
async fn get_reports_idle_runtime_when_only_persisted_status_is_running() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let created = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.update(
        &created.conversation_id,
        &ConversationRowUpdate {
            status: Some("running".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let fetched = svc.get(TEST_USER_1, &created.conversation_id).await.unwrap();
    let runtime = fetched.runtime.expect("runtime summary should be present");

    assert_eq!(fetched.status, ConversationStatus::Running);
    assert_eq!(runtime.state, nomifun_api_types::ConversationRuntimeStateKind::Idle);
    assert!(runtime.can_send_message);
}

#[tokio::test]
async fn get_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let err = svc.get(TEST_USER_1, "non-existent").await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

// ── List tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn list_empty() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let result = svc.list(TEST_USER_1, ListConversationsQuery::default(), false).await.unwrap();
    assert!(result.items.is_empty());
    assert_eq!(result.total, 0);
    assert!(!result.has_more);
}

#[tokio::test]
async fn list_returns_created_conversations() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let result = svc.list(TEST_USER_1, ListConversationsQuery::default(), false).await.unwrap();
    assert_eq!(result.items.len(), 2);
    assert_eq!(result.total, 2);
}

#[tokio::test]
async fn list_filters_by_user() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let secondary_model_only_req = serde_json::from_value(json!({
        "type": "nomi",
        "extra": {}
    }))
    .unwrap();
    svc.create(TEST_USER_2, secondary_model_only_req).await.unwrap();

    let result = svc.list(TEST_USER_1, ListConversationsQuery::default(), false).await.unwrap();
    assert_eq!(result.items.len(), 1);
}

#[tokio::test]
async fn list_with_source_filter() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let telegram_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "source": "telegram",
        "extra": { "agent_id": TEST_ACP_AGENT_ID }
    }))
    .unwrap();
    svc.create(TEST_USER_1, telegram_req).await.unwrap();

    let query = ListConversationsQuery {
        source: Some("telegram".into()),
        ..Default::default()
    };
    let result = svc.list(TEST_USER_1, query, false).await.unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].source, Some(ConversationSource::Telegram));
}

#[tokio::test]
async fn list_with_pinned_filter() {
    let (svc, _broadcaster, _repo, runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    // Pin the first one
    let update_req: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": true })).unwrap();
    svc.update(TEST_USER_1, &conv.conversation_id, update_req, &runtime_registry).await.unwrap();

    let query = ListConversationsQuery {
        pinned: Some(true),
        ..Default::default()
    };
    let result = svc.list(TEST_USER_1, query, false).await.unwrap();
    assert_eq!(result.items.len(), 1);
    assert!(result.items[0].pinned);
}

// ── Update tests ───────────────────────────────────────────────────

#[tokio::test]
async fn update_name() {
    let (svc, broadcaster, _repo, runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    broadcaster.take_events(); // clear create event

    let req: UpdateConversationRequest = serde_json::from_value(json!({ "name": "New Name" })).unwrap();
    let updated = svc.update(TEST_USER_1, &conv.conversation_id, req, &runtime_registry).await.unwrap();

    assert_eq!(updated.name, "New Name");
    assert!(updated.modified_at >= conv.modified_at);

    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["action"], "updated");
}

#[tokio::test]
async fn update_pin() {
    let (svc, _broadcaster, _repo, runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    assert!(!conv.pinned);

    let req: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": true })).unwrap();
    let updated = svc.update(TEST_USER_1, &conv.conversation_id, req, &runtime_registry).await.unwrap();
    assert!(updated.pinned);
    assert!(updated.pinned_at.is_some());
}

#[tokio::test]
async fn update_unpin_clears_pinned_at() {
    let (svc, _broadcaster, _repo, runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    // Pin first
    let pin_req: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": true })).unwrap();
    let pinned = svc.update(TEST_USER_1, &conv.conversation_id, pin_req, &runtime_registry).await.unwrap();
    assert!(pinned.pinned);
    assert!(pinned.pinned_at.is_some());

    // Unpin
    let unpin_req: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": false })).unwrap();
    let unpinned = svc.update(TEST_USER_1, &conv.conversation_id, unpin_req, &runtime_registry).await.unwrap();
    assert!(!unpinned.pinned);
    assert!(unpinned.pinned_at.is_none());
}

#[tokio::test]
async fn update_extra_merge() {
    let (svc, _broadcaster, _repo, runtime_registry) = make_service();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": "/old", "contextFileName": "ctx.md" }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, req).await.unwrap();

    // Update only workspace — contextFileName should be preserved
    let update_req: UpdateConversationRequest =
        serde_json::from_value(json!({ "extra": { "workspace": "/new" } })).unwrap();
    let updated = svc.update(TEST_USER_1, &conv.conversation_id, update_req, &runtime_registry).await.unwrap();

    assert_eq!(updated.extra["workspace"], "/new");
    assert_eq!(updated.extra["contextFileName"], "ctx.md");
}

#[tokio::test]
async fn update_rejects_acp_agent_identity_patch() {
    let (svc, _broadcaster, repo, runtime_registry) = make_service();
    let conversation = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let before = repo.get(&conversation.conversation_id).await.unwrap().unwrap().extra;
    let req = serde_json::from_value(json!({
        "extra": { "agent_id": "0190f5fe-7c00-7a00-8000-000000000102" }
    }))
    .unwrap();

    let error = svc
        .update(TEST_USER_1, &conversation.conversation_id, req, &runtime_registry)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AppError::BadRequest(message) if message.contains("immutable after creation")
    ));
    assert_eq!(repo.get(&conversation.conversation_id).await.unwrap().unwrap().extra, before);
}

#[tokio::test]
async fn public_update_rejects_every_backend_owned_lifecycle_extra_key_without_mutation() {
    let (svc, _broadcaster, repo, runtime_registry) = make_service();
    let conversation = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let before = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap()
        .extra;

    for key in crate::service::BACKEND_OWNED_LIFECYCLE_EXTRA_KEYS {
        let patch = serde_json::Value::Object(serde_json::Map::from_iter([(
            key.to_owned(),
            json!("forged-client-authority"),
        )]));
        let req: UpdateConversationRequest =
            serde_json::from_value(json!({ "extra": patch })).unwrap();

        let error = svc
            .update(
                TEST_USER_1,
                &conversation.conversation_id,
                req,
                &runtime_registry,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                AppError::BadRequest(ref message)
                    if message.contains(key) && message.contains("backend-owned")
            ),
            "public update accepted or misclassified backend-owned extra key {key}: {error:?}"
        );
        assert_eq!(
            repo.get(&conversation.conversation_id)
                .await
                .unwrap()
                .unwrap()
                .extra,
            before,
            "public update mutated the row while rejecting backend-owned extra key {key}"
        );
    }
}

#[tokio::test]
async fn internal_update_extra_rejects_every_backend_owned_lifecycle_key_without_mutation() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let conversation = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let before = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap()
        .extra;

    for key in crate::service::BACKEND_OWNED_LIFECYCLE_EXTRA_KEYS {
        let patch = serde_json::Value::Object(serde_json::Map::from_iter([(
            key.to_owned(),
            json!("forged-internal-authority"),
        )]));
        let error = svc
            .update_extra(&conversation.conversation_id, patch)
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                AppError::BadRequest(ref message)
                    if message.contains(key) && message.contains("backend-owned")
            ),
            "update_extra accepted or misclassified backend-owned key {key}: {error:?}"
        );
        assert_eq!(
            repo.get(&conversation.conversation_id)
                .await
                .unwrap()
                .unwrap()
                .extra,
            before,
            "update_extra mutated the row while rejecting backend-owned key {key}"
        );
    }
}

#[tokio::test]
async fn update_model() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();

    // Top-level model updates are only valid on nomi conversations
    // (Task 8 enforces the nomi-only rule in update).
    let create_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
        "extra": { "workspace": "/project" }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, create_req).await.unwrap();

    let req: UpdateConversationRequest = serde_json::from_value(json!({
        "model": { "provider_id": PROVIDER_ID_2, "model": "new-model" }
    }))
    .unwrap();
    let mock = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = mock.clone();
    let updated = svc.update(TEST_USER_1, &conv.conversation_id, req, &runtime_registry).await.unwrap();

    let model = updated.model.unwrap();
    assert_eq!(model.provider_id, PROVIDER_ID_2);
    assert_eq!(model.model, "new-model");
    assert_eq!(
        mock.termination_wait_count(),
        1,
        "model update must await old agent teardown"
    );
}

#[tokio::test]
async fn update_workspace_change_recycles_agent() {
    // Binding a session to a different working directory must recycle the
    // cached agent so the new cwd (and its surface-scoped file authority) takes
    // effect on the next message — same rationale as the model-change recycle.
    // A no-op workspace update must NOT recycle.
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();

    let create_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
        "extra": { "workspace": "/project" }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, create_req).await.unwrap();

    // A fresh mock passed to `update` receives the termination (update uses the passed
    // runtime registry, not the service's internal one).
    let mock = Arc::new(MockAgentRuntimeRegistry::new());
    let mgr: Arc<dyn AgentRuntimeRegistry> = mock.clone();

    let repoint: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "workspace": "/other/project" }
    }))
    .unwrap();
    let updated = svc.update(TEST_USER_1, &conv.conversation_id, repoint, &mgr).await.unwrap();
    assert_eq!(updated.extra["workspace"], "/other/project");
    assert_eq!(mock.termination_count(), 1, "workspace change must recycle the agent");

    // Re-applying the SAME workspace is a no-op → no further recycle.
    let same: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "workspace": "/other/project" }
    }))
    .unwrap();
    svc.update(TEST_USER_1, &conv.conversation_id, same, &mgr).await.unwrap();
    assert_eq!(mock.termination_count(), 1, "no-op workspace update must not recycle");

    // A non-workspace extra change must also not recycle.
    let other: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "some_flag": true }
    }))
    .unwrap();
    svc.update(TEST_USER_1, &conv.conversation_id, other, &mgr).await.unwrap();
    assert_eq!(mock.termination_count(), 1, "non-workspace extra change must not recycle");
}

#[tokio::test]
async fn update_not_found() {
    let (svc, _broadcaster, _repo, runtime_registry) = make_service();
    let req: UpdateConversationRequest = serde_json::from_value(json!({ "name": "x" })).unwrap();
    let err = svc.update(TEST_USER_1, "non-existent", req, &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

// ── Delete tests ───────────────────────────────────────────────────

#[tokio::test]
async fn delete_conversation() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    broadcaster.take_events();

    svc.delete(TEST_USER_1, &conv.conversation_id).await.unwrap();

    // Should be gone
    let err = svc.get(TEST_USER_1, &conv.conversation_id).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));

    // Should broadcast deleted
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["action"], "deleted");
    assert_eq!(events[0].data["conversation_id"], conv.conversation_id);
}

#[tokio::test]
async fn delete_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let err = svc.delete(TEST_USER_1, "non-existent").await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn delete_invokes_registered_hook() {
    use nomifun_common::OnConversationDelete;

    struct RecordingHook(Mutex<Vec<(String, String)>>);
    #[async_trait::async_trait]
    impl OnConversationDelete for RecordingHook {
        async fn on_conversation_deleted(&self, user_id: &str, conversation_id: &str) {
            self.0
                .lock()
                .unwrap()
                .push((user_id.to_owned(), conversation_id.to_owned()));
        }
    }

    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let hook = Arc::new(RecordingHook(Mutex::new(vec![])));
    svc.with_delete_hook(hook.clone());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    svc.delete(TEST_USER_1, &conv.conversation_id).await.unwrap();

    let calls = hook.0.lock().unwrap();
    assert_eq!(calls.as_slice(), &[(TEST_USER_1.to_owned(), conv.conversation_id)]);
}

async fn make_sqlite_projection_service() -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<SqliteConversationRepository>,
    nomifun_db::Database,
) {
    let database = init_database_memory().await.unwrap();
    let repository = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> =
        Arc::new(MockAgentRuntimeRegistry::new());
    let service = ConversationService::new(
        Arc::<str>::from(SQLITE_TEST_OWNER),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry,
        repository.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    (service, broadcaster, repository, database)
}

const PROJECTION_OWNER: &str = SQLITE_TEST_OWNER;

#[tokio::test]
async fn assistant_projection_is_one_durable_row_and_rebroadcasts_stable_final_content() {
    let (service, broadcaster, repository, _database) = make_sqlite_projection_service().await;
    let conversation = service
        .create(
            PROJECTION_OWNER,
            serde_json::from_value(json!({
                "type": "nomi",
                "name": "lead",
                "extra": { "workspace": "/project" }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    broadcaster.take_events();

    let operation_id = "exec-lead-report:exec_1:event:7";
    let first = service
        .project_assistant_message_idempotent(
            PROJECTION_OWNER,
            &conversation.conversation_id,
            operation_id,
            "final synthesis",
            "agent_execution_report",
        )
        .await
        .unwrap();
    let replay = service
        .project_assistant_message_idempotent(
            PROJECTION_OWNER,
            &conversation.conversation_id,
            operation_id,
            "final synthesis",
            "agent_execution_report",
        )
        .await
        .unwrap();
    assert_eq!(first, replay);

    let messages = repository
        .get_messages(&conversation.conversation_id, 1, 20, SortOrder::Asc)
        .await
        .unwrap();
    assert_eq!(messages.items.len(), 1);
    assert_eq!(messages.items[0].message_id, first);
    assert_eq!(messages.items[0].position.as_deref(), Some("left"));
    assert_eq!(messages.items[0].status.as_deref(), Some("finish"));

    // Replays rebroadcast after the durable transaction so a crash between
    // commit and the first WebSocket publish is healed with the same msg_id.
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|event| event.name == "message.stream"));
    assert!(events.iter().all(|event| event.data["msg_id"] == first));
    assert!(events.iter().all(|event| event.data["type"] == "content"));
    assert!(events.iter().all(|event| event.data["replace"] == true));
    assert!(events.iter().all(|event| event.data["stream_complete"] == true));
    assert!(!events.iter().any(|event| {
        matches!(event.name.as_str(), "turn.started" | "turn.completed")
    }));

    assert!(service
        .project_assistant_message_idempotent(
            PROJECTION_OWNER,
            &conversation.conversation_id,
            operation_id,
            "different content",
            "agent_execution_report",
        )
        .await
        .is_err());
    assert!(service
        .project_assistant_message_idempotent(
            "other-owner",
            &conversation.conversation_id,
            "other-op",
            "content",
            "agent_execution_report",
        )
        .await
        .is_err());
}

#[tokio::test]
async fn assistant_projection_reuses_companion_and_channel_wire_markers() {
    let (service, broadcaster, _repository, _database) = make_sqlite_projection_service().await;
    let conversation = service
        .create(
            PROJECTION_OWNER,
            serde_json::from_value(json!({
                "type": "nomi",
                "name": "companion lead",
                "extra": {
                    "workspace": "/project",
                    "companion_session": true,
                    "companion_id": "0190f5fe-7c00-7a00-8abc-012345678941",
                    "channel_platform": "telegram"
                }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    broadcaster.take_events();

    service
        .project_assistant_message_idempotent(
            PROJECTION_OWNER,
            &conversation.conversation_id,
            "exec-lead-report:exec_2:event:9",
            "companion synthesis",
            "agent_execution_report",
        )
        .await
        .unwrap();
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["companion"], true);
    assert_eq!(events[0].data["companion_id"], "0190f5fe-7c00-7a00-8abc-012345678941");
    assert_eq!(events[0].data["channel_platform"], "telegram");
}

#[tokio::test]
async fn two_user_private_events_are_owner_scoped() {
    use nomifun_db::IUserRepository;

    let (service, user_events, _repository, database) = make_sqlite_projection_service().await;
    let owner_b = nomifun_db::SqliteUserRepository::new(database.pool().clone())
        .create_user("owner-b", "test-password-hash")
        .await
        .unwrap()
        .user_id
        .into_string();
    let request = || {
        serde_json::from_value(json!({
            "type": "nomi",
            "name": "private conversation",
            "extra": { "workspace": "/project" }
        }))
        .unwrap()
    };

    let owner_a_conversation = service.create(PROJECTION_OWNER, request()).await.unwrap();
    user_events.take_events();
    user_events.take_deliveries();

    let owner_b_conversation = service.create(&owner_b, request()).await.unwrap();
    let owner_b_deliveries = user_events.take_deliveries();
    assert_eq!(owner_b_deliveries.len(), 1);
    assert_eq!(owner_b_deliveries[0].0, owner_b.as_str());
    assert_eq!(owner_b_deliveries[0].1.name, "conversation.listChanged");
    assert_eq!(
        owner_b_deliveries[0].1.data["conversation_id"],
        owner_b_conversation.conversation_id
    );
    assert!(
        owner_b_deliveries
            .iter()
            .all(|(owner, _)| owner != PROJECTION_OWNER)
    );
    user_events.take_events();

    service
        .project_assistant_message_idempotent(
            PROJECTION_OWNER,
            &owner_a_conversation.conversation_id,
            "exec-lead-report:owner-a:event:1",
            "owner A terminal report",
            "agent_execution_report",
        )
        .await
        .unwrap();

    let owner_a_deliveries = user_events.take_deliveries();
    assert_eq!(owner_a_deliveries.len(), 1);
    assert_eq!(owner_a_deliveries[0].0, PROJECTION_OWNER);
    assert_eq!(owner_a_deliveries[0].1.name, "message.stream");
    assert_eq!(
        owner_a_deliveries[0].1.data["conversation_id"],
        owner_a_conversation.conversation_id
    );
    assert_eq!(owner_a_deliveries[0].1.data["replace"], true);
    assert!(
        owner_a_deliveries
            .iter()
            .all(|(owner, _)| owner != owner_b.as_str())
    );
}

#[tokio::test]
async fn delete_rejects_soft_deleted_execution_attempt_transcript() {
    const USER_ID: &str = SQLITE_TEST_OWNER;

    let database = init_database_memory().await.unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO providers (\
            provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
            capabilities, created_at, updated_at\
         ) VALUES (?1, 'openai', 'test', 'https://example.invalid', \
                   'encrypted', '[\"model_test\"]', 1, '[]', 1, 1)",
    )
    .bind(PROVIDER_ID_1)
    .execute(database.pool())
    .await
    .unwrap();
    let conversation_repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let execution_repo = Arc::new(SqliteAgentExecutionRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let runtime_registry_impl = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = runtime_registry_impl.clone();
    let svc = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        conversation_repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(RepositoryExecutionConversationBoundary::new(
            execution_repo.clone(),
        )),
    );

    let request = |name: &str| {
        let workspace = isolated_test_workspace("retained-execution");
        serde_json::from_value(json!({
            "type": "nomi",
            "name": name,
            "model": {
                "provider_id": PROVIDER_ID_1,
                "model": "model_test",
                "use_model": "model_test"
            },
            "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": workspace }
        }))
        .unwrap()
    };
    let lead = svc.create(USER_ID, request("lead")).await.unwrap();
    let attempt_conversation = svc.create(USER_ID, request("attempt")).await.unwrap();
    let participant_id = ConversationId::new().into_string();
    let step_id = ConversationId::new().into_string();
    let event = |event_type: AgentExecutionEventKind| NewAgentExecutionEvent {
        event_type,
        step_id: None,
        attempt_id: None,
        actor: nomifun_common::AgentExecutionActor::system(),
        payload: "{}".to_owned(),
    };
    let participant = NewAgentExecutionParticipant {
        participant_id: participant_id.clone(),
        source_agent_id: TEST_NOMI_AGENT_ID.to_owned(),
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        provider_id: Some(PROVIDER_ID_1.to_owned()),
        model: Some("model_test".to_owned()),
        role: Some("builder".to_owned()),
        capability: Some(r#"{"coding":true}"#.to_owned()),
        constraints: Some("{}".to_owned()),
        description: None,
        system_prompt: None,
        enabled_skills: "[]".to_owned(),
        disabled_builtin_skills: "[]".to_owned(),
        sort_order: 0,
    };
    let execution = execution_repo
        .create_execution_with_participants(
            USER_ID,
            &CreateAgentExecutionParams {
                goal: "retain attempt transcript".to_owned(),
                status: AgentExecutionStatus::Planning,
                plan_gate: PlanGate::Automatic,
                adaptation_policy: AdaptationPolicy::Fixed,
                decision_policy: DecisionPolicy::Automatic,
                delegation_policy: DelegationPolicy::Automatic,
                max_parallel: 1,
                work_dir: None,
                lead_conversation_id: Some(lead.conversation_id.clone()),
                initial_plan_input: r#"{"mode":"automatic"}"#.to_owned(),
            },
            &[participant],
            &event(AgentExecutionEventKind::Created),
        )
        .await
        .unwrap();
    let planned = execution_repo
        .reconcile_plan(
            USER_ID,
            &execution.execution_id,
            0,
            &ReconcileAgentExecutionPlanParams {
                goal: None,
                plan_gate: None,
                adaptation_policy: None,
                decision_policy: None,
                delegation_policy: None,
                keep_step_ids: Vec::new(),
                new_participants: Vec::new(),
                retire_participant_ids: Vec::new(),
                new_steps: vec![NewAgentExecutionStep {
                    step_id: step_id.clone(),
                    title: "attempt".to_owned(),
                    spec: "execute attempt".to_owned(),
                    role: Some("builder".to_owned()),
                    tool_policy: AgentToolPolicy::Full,
                    kind: ExecutionStepKind::Agent,
                    agent_mode: Some(AgentStepMode::Normal),
                    profile: Some("{}".to_owned()),
                    fanout_group: None,
                    control_policy: None,
                    status: ExecutionStepStatus::Pending,
                    assigned_participant_id: Some(participant_id.clone()),
                    assignment_score: Some(1.0),
                    assignment_rationale: Some("test".to_owned()),
                    assignment_source: Some(ParticipantAssignmentSource::Planner),
                    assignment_locked: false,
                    failure_policy: StepFailurePolicy::FailExecution,
                    preset_prompt: None,
                    graph_x: None,
                    graph_y: None,
                }],
                new_dependencies: Vec::new(),
                execution_status: AgentExecutionStatus::Running,
            },
            &event(AgentExecutionEventKind::PlanChanged),
        )
        .await
        .unwrap();
    assert_eq!(
        planned
            .participants
            .first()
            .expect("participant materialized")
            .participant_id,
        participant_id
    );
    assert_eq!(
        planned.steps.first().expect("step materialized").step_id,
        step_id
    );
    let execution_lease = AgentExecutionLeaseToken::new(
        "conversation-service-test:execution-generation".to_owned(),
    );
    execution_repo
        .try_acquire_lease(
            &execution.execution_id,
            planned.execution.version,
            execution_lease.owner(),
            now_ms() + 60_000,
        )
        .await
        .unwrap()
        .expect("test scheduler lease");
    let queued = execution_repo
        .create_attempt(
            USER_ID,
            &execution.execution_id,
            &step_id,
            0,
            Some(&execution_lease),
            &CreateAgentExecutionAttemptParams {
                participant_id: Some(participant_id.clone()),
                start_immediately: false,
                trigger_reason: "initial".to_owned(),
                effective_config: "{}".to_owned(),
                retry_after: None,
                runtime_state: None,
            },
            &event(AgentExecutionEventKind::AttemptChanged),
        )
        .await
        .unwrap();
    let attempt_id = queued.current_attempt.unwrap().attempt.attempt_id;
    let started = execution_repo
        .start_attempt(
            USER_ID,
            &execution.execution_id,
            &step_id,
            1,
            &attempt_id,
            0,
            &attempt_conversation.conversation_id,
            Some(&execution_lease),
            &event(AgentExecutionEventKind::AttemptChanged),
        )
        .await
        .unwrap();
    let started_attempt = started
        .current_attempt
        .as_ref()
        .expect("started attempt detail");
    let initial_turn_authority = AgentExecutionTurnAuthority {
        execution_id: execution.execution_id.clone(),
        step_id: step_id.clone(),
        attempt_id: attempt_id.clone(),
        expected_step_version: started.step.version,
        expected_attempt_version: started_attempt.attempt.version,
        lease_owner: execution_lease.owner().to_owned(),
    };

    let ordinary_send = svc
        .send_message(
            USER_ID,
            &attempt_conversation.conversation_id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap_err();
    assert!(matches!(ordinary_send, AppError::Conflict(_)));
    let idempotent_public_send = svc
        .send_message_with_idempotency_key(
            USER_ID,
            &attempt_conversation.conversation_id,
            "retained-attempt-public-send",
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(idempotent_public_send, AppError::Conflict(_)),
        "a public receipt is replay identity, not trusted execution authority"
    );
    let ordinary_update = svc
        .update(
            USER_ID,
            &attempt_conversation.conversation_id,
            serde_json::from_value(json!({ "name": "must not mutate" })).unwrap(),
            &runtime_registry,
        )
        .await
        .unwrap_err();
    assert!(matches!(ordinary_update, AppError::Conflict(_)));
    let ordinary_cancel = svc
        .cancel(
            USER_ID,
            &attempt_conversation.conversation_id,
            &runtime_registry,
        )
        .await
        .unwrap_err();
    assert!(matches!(ordinary_cancel, AppError::Conflict(_)));
    assert!(!svc.user_cancelled_since(&attempt_conversation.conversation_id, 0));
    let ordinary_warmup = svc
        .warmup_for_view(
            USER_ID,
            &attempt_conversation.conversation_id,
            &runtime_registry,
        )
        .await
        .unwrap_err();
    assert!(matches!(ordinary_warmup, AppError::Conflict(_)));
    let view_warmup = svc
        .warmup_for_view(
            USER_ID,
            &attempt_conversation.conversation_id,
            &runtime_registry,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(view_warmup, AppError::Conflict(_)),
        "view preparation must enforce retained-transcript ownership before lifecycle checks"
    );
    assert_eq!(runtime_registry_impl.active_runtime_count(), 0);
    assert_eq!(runtime_registry_impl.termination_count(), 0);
    assert!(
        conversation_repo
            .get_messages(&attempt_conversation.conversation_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .items
            .is_empty(),
        "public rejection happens before transcript or runtime side effects"
    );

    let execution_port = svc.agent_execution_port(runtime_registry.clone());
    let delivery = execution_port
        .deliver_turn(
            USER_ID,
            &attempt_conversation.conversation_id,
            "execution:test:initial",
            initial_turn_authority,
            make_send_req(),
        )
        .await
        .unwrap();
    assert!(MessageId::try_from(delivery.message_id.as_str()).is_ok());
    wait_for_turn_released(&svc, &attempt_conversation.conversation_id).await;
    assert_eq!(
        conversation_repo
            .get_messages(&attempt_conversation.conversation_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .items
            .len(),
        1,
        "only the trusted Agent Execution port may deliver an Attempt turn"
    );
    assert!(
        svc.list_confirmations(
            USER_ID,
            &attempt_conversation.conversation_id,
            &runtime_registry,
        )
        .await
        .unwrap()
        .is_empty(),
        "read-only confirmation inspection remains available"
    );
    let projected_lead = svc.get(USER_ID, &lead.conversation_id).await.unwrap();
    assert_eq!(
        projected_lead.linked_execution_id.as_deref(),
        Some(execution.execution_id.as_str())
    );
    assert!(projected_lead.execution_step_id.is_none());
    assert!(projected_lead.execution_attempt_id.is_none());

    let projected_attempt = svc
        .get(USER_ID, &attempt_conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(
        projected_attempt.linked_execution_id.as_deref(),
        Some(execution.execution_id.as_str())
    );
    assert_eq!(projected_attempt.execution_step_id.as_deref(), Some(step_id.as_str()));
    assert_eq!(
        projected_attempt.execution_attempt_id.as_deref(),
        Some(attempt_id.as_str())
    );
    execution_repo
        .settle_attempt(
            USER_ID,
            &execution.execution_id,
            &step_id,
            2,
            &attempt_id,
            1,
            None,
            &SettleAgentExecutionAttemptParams {
                attempt_status: ExecutionAttemptStatus::WaitingInput,
                step_status: ExecutionStepStatus::WaitingInput,
                execution_status: Some(AgentExecutionStatus::WaitingInput),
                question: Some(Some("choose a continuation".to_owned())),
                error: None,
                output_summary: None,
                output_files: None,
                tokens: None,
                retry_after: None,
                runtime_state: None,
                started_at: None,
                finished_at: None,
                loop_repeat_reset: None,
            },
            &event(AgentExecutionEventKind::StatusChanged),
        )
        .await
        .unwrap();

    assert!(matches!(
        svc.send_message(
            USER_ID,
            &attempt_conversation.conversation_id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap_err(),
        AppError::Conflict(_)
    ));
    let resumed = execution_repo
        .resume_waiting_attempt(
            USER_ID,
            &execution.execution_id,
            4,
            &step_id,
            3,
            &attempt_id,
            2,
            &AttemptConversationEffectParams {
                runtime_state: None,
            },
            &event(AgentExecutionEventKind::DecisionAnswered),
        )
        .await
        .unwrap();
    let resumed_attempt = resumed
        .detail
        .current_attempt
        .as_ref()
        .expect("resumed attempt detail");
    let continuation = execution_port
        .deliver_turn(
            USER_ID,
            &attempt_conversation.conversation_id,
            "execution:test:continuation",
            AgentExecutionTurnAuthority {
                execution_id: execution.execution_id.clone(),
                step_id: step_id.clone(),
                attempt_id: attempt_id.clone(),
                expected_step_version: resumed.detail.step.version,
                expected_attempt_version: resumed_attempt.attempt.version,
                lease_owner: execution_lease.owner().to_owned(),
            },
            serde_json::from_value(json!({ "content": "continue" })).unwrap(),
        )
        .await
        .unwrap();
    assert!(MessageId::try_from(continuation.message_id.as_str()).is_ok());
    wait_for_turn_released(&svc, &attempt_conversation.conversation_id).await;
    execution_repo
        .settle_attempt(
            USER_ID,
            &execution.execution_id,
            &step_id,
            4,
            &attempt_id,
            3,
            Some(&execution_lease),
            &SettleAgentExecutionAttemptParams {
                attempt_status: ExecutionAttemptStatus::Completed,
                step_status: ExecutionStepStatus::Completed,
                execution_status: Some(AgentExecutionStatus::Completed),
                question: None,
                error: None,
                output_summary: Some(Some("done".to_owned())),
                output_files: Some("[]".to_owned()),
                tokens: None,
                retry_after: None,
                runtime_state: None,
                started_at: None,
                finished_at: None,
                loop_repeat_reset: None,
            },
            &event(AgentExecutionEventKind::StatusChanged),
        )
        .await
        .unwrap();

    let settled_attempt = svc
        .get(USER_ID, &attempt_conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(
        settled_attempt.linked_execution_id.as_deref(),
        Some(execution.execution_id.as_str())
    );
    assert_eq!(settled_attempt.execution_step_id.as_deref(), Some(step_id.as_str()));
    assert_eq!(
        settled_attempt.execution_attempt_id.as_deref(),
        Some(attempt_id.as_str())
    );
    assert!(matches!(
        svc.send_message(
            USER_ID,
            &attempt_conversation.conversation_id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap_err(),
        AppError::Conflict(_)
    ));
    svc.update_extra(
        &attempt_conversation.conversation_id,
        json!({ "execution_cleanup_state": "retained" }),
    )
    .await
    .unwrap();
    svc.cancel_for_execution(
        USER_ID,
        &attempt_conversation.conversation_id,
        &runtime_registry,
    )
    .await
    .unwrap();

    let cleanup = execution_repo
        .list_pending_conversation_cleanups(Some(&execution.execution_id), 10)
        .await
        .unwrap();
    assert_eq!(cleanup.len(), 1);
    assert!(
        execution_repo
            .mark_conversation_cleanup_completed(
                &cleanup[0].execution_id,
                &cleanup[0].conversation_id,
                now_ms(),
            )
            .await
            .unwrap()
    );
    let cleaned_attempt = svc
        .get(USER_ID, &attempt_conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(cleaned_attempt.execution_step_id.as_deref(), Some(step_id.as_str()));
    assert_eq!(
        cleaned_attempt.execution_attempt_id.as_deref(),
        Some(attempt_id.as_str())
    );
    assert!(
        execution_repo
            .delete_execution(
                USER_ID,
                &execution.execution_id,
                6,
                &event(AgentExecutionEventKind::Deleted),
            )
            .await
            .unwrap()
    );

    let tombstoned_attempt = svc
        .get(USER_ID, &attempt_conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(
        tombstoned_attempt.linked_execution_id, None,
        "a retained transcript must not expose a soft-deleted execution route"
    );
    assert_eq!(
        tombstoned_attempt.execution_step_id,
        Some(step_id)
    );
    assert_eq!(tombstoned_attempt.execution_attempt_id, Some(attempt_id));
    assert!(matches!(
        svc.send_message(
            USER_ID,
            &attempt_conversation.conversation_id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert!(matches!(
        svc.update(
            USER_ID,
            &attempt_conversation.conversation_id,
            serde_json::from_value(json!({ "name": "still immutable" })).unwrap(),
            &runtime_registry,
        )
        .await
        .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert!(matches!(
        svc.cancel(
            USER_ID,
            &attempt_conversation.conversation_id,
            &runtime_registry,
        )
        .await
        .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert!(matches!(
        svc.warmup_for_view(
            USER_ID,
            &attempt_conversation.conversation_id,
            &runtime_registry,
        )
        .await
        .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert_eq!(
        conversation_repo
            .get_messages(&attempt_conversation.conversation_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .items
            .len(),
        2
    );

    let error = svc
        .delete(USER_ID, &attempt_conversation.conversation_id)
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::Conflict(_)));
    assert!(
        conversation_repo
            .get(&attempt_conversation.conversation_id)
            .await
            .unwrap()
            .is_some(),
        "the attempt transcript must remain physically present"
    );
}

// ── Broadcast payload tests ────────────────────────────────────────

#[tokio::test]
async fn broadcast_includes_source_on_delete() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "source": "telegram",
        "extra": { "agent_id": TEST_ACP_AGENT_ID }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, req).await.unwrap();
    broadcaster.take_events();

    svc.delete(TEST_USER_1, &conv.conversation_id).await.unwrap();
    let events = broadcaster.take_events();
    assert_eq!(events[0].data["source"], "telegram");
}

#[tokio::test]
async fn all_crud_operations_broadcast() {
    let (svc, broadcaster, _repo, runtime_registry) = make_service();

    // Create
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let events = broadcaster.take_events();
    assert_eq!(events[0].data["action"], "created");

    // Update
    let req: UpdateConversationRequest = serde_json::from_value(json!({ "name": "x" })).unwrap();
    svc.update(TEST_USER_1, &conv.conversation_id, req, &runtime_registry).await.unwrap();
    let events = broadcaster.take_events();
    assert_eq!(events[0].data["action"], "updated");

    // Delete
    svc.delete(TEST_USER_1, &conv.conversation_id).await.unwrap();
    let events = broadcaster.take_events();
    assert_eq!(events[0].data["action"], "deleted");
}

// ── Ownership tests (M-3) ─────────────────────────────────────────

#[tokio::test]
async fn get_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let err = svc.get(TEST_USER_2, &conv.conversation_id).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn update_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let req: UpdateConversationRequest = serde_json::from_value(json!({ "name": "hacked" })).unwrap();
    let err = svc.update(TEST_USER_2, &conv.conversation_id, req, &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));

    // Original should be unchanged
    let original = svc.get(TEST_USER_1, &conv.conversation_id).await.unwrap();
    assert_ne!(original.name, "hacked");
}

#[tokio::test]
async fn delete_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let err = svc.delete(TEST_USER_2, &conv.conversation_id).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));

    // Should still exist
    let still_exists = svc.get(TEST_USER_1, &conv.conversation_id).await.unwrap();
    assert_eq!(still_exists.conversation_id, conv.conversation_id);
}

// ── Clone tests ───────────────────────────────────────────────────

#[tokio::test]
async fn clone_without_source_creates_isolated_workspace_and_session_state() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();

    let req: CloneConversationRequest = serde_json::from_value(json!({
        "conversation": {
            "type": "acp",
            "name": "Cloned",
            "extra": {
                "agent_id": TEST_ACP_AGENT_ID,
                "backend": "claude",
                "workspace": "/old",
                "custom_workspace": true,
                "is_temporary_workspace": false,
                "temp_workspace_id": "ws_old",
                "workspace_id": "workspace_old",
                "acp_session_id": "session-old",
                "acp_session_conversation_id": 77,
                "acp_session_updated_at": 123,
                "current_mode_id": "plan",
                "current_model_id": "old-model",
                "cached_config_options": [{"id": "mode"}],
                "pending_config_options": {"mode": "plan"},
                "sessionKey": "old-session",
                "runtimeValidation": {"expectedWorkspace": "/old"}
            }
        }
    }))
    .unwrap();

    let resp = svc.clone_create(TEST_USER_1, req).await.unwrap();
    assert_eq!(resp.name, "Cloned");
    let workspace = resp.extra["workspace"].as_str().expect("clone should receive a fresh workspace");
    assert_ne!(workspace, "/old");
    assert!(PathBuf::from(workspace).is_dir());
    assert!(resp.extra["temp_workspace_id"].as_str().is_some());
    assert_eq!(resp.extra["backend"], "claude");
    for key in [
        "custom_workspace",
        "workspace_id",
        "acp_session_id",
        "acp_session_conversation_id",
        "acp_session_updated_at",
        "current_mode_id",
        "current_model_id",
        "cached_config_options",
        "pending_config_options",
        "sessionKey",
        "runtimeValidation",
    ] {
        assert!(resp.extra.get(key).is_none(), "clone leaked source field {key}");
    }

    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["action"], "created");
}

// ── Reset tests ───────────────────────────────────────────────────

async fn seed_reset_aggregate(repo: &MockRepo, conversation_id: &str) {
    let message_id = MessageId::new().into_string();
    repo.insert_message(&MessageRow {
        id: 0,
        message_id: message_id.clone(),
        conversation_id: conversation_id.to_owned(),
        msg_id: Some(message_id),
        r#type: "text".to_owned(),
        content: json!({ "content": "must survive rejected reset" }).to_string(),
        position: Some("right".to_owned()),
        status: Some("finish".to_owned()),
        hidden: false,
        created_at: 1000,
    })
    .await
    .unwrap();
    let cron_job_id = nomifun_common::CronJobId::new().into_string();
    repo.upsert_artifact(&ConversationArtifactRow {
        conversation_artifact_id: nomifun_common::generate_id(),
        conversation_id: conversation_id.to_owned(),
        cron_job_id: Some(cron_job_id.clone()),
        kind: "skill_suggest".to_owned(),
        status: "pending".to_owned(),
        payload: json!({ "cron_job_id": cron_job_id, "name": "reset-fixture" }).to_string(),
        created_at: 1000,
        updated_at: 1000,
    })
    .await
    .unwrap();
}

async fn assert_reset_aggregate_intact(
    repo: &MockRepo,
    conversation_id: &str,
    expected_status: &str,
) {
    assert_eq!(
        repo.get(conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some(expected_status)
    );
    assert_eq!(
        repo.get_messages(conversation_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .total,
        1
    );
    assert_eq!(repo.list_artifacts(conversation_id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn reset_sets_status_to_pending() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    svc.reset(TEST_USER_1, &conv.conversation_id).await.unwrap();

    let fetched = svc.get(TEST_USER_1, &conv.conversation_id).await.unwrap();
    assert_eq!(fetched.status, ConversationStatus::Pending);
}

#[tokio::test]
async fn reset_clears_conversation_artifacts() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let cron_job_id = nomifun_common::CronJobId::new().into_string();
    repo.upsert_artifact(&ConversationArtifactRow {
        conversation_artifact_id: nomifun_common::generate_id(),
        conversation_id: conv.conversation_id.clone(),
        cron_job_id: Some(cron_job_id.clone()),
        kind: "skill_suggest".into(),
        status: "pending".into(),
        payload: json!({ "cron_job_id": cron_job_id, "name": "daily-report" }).to_string(),
        created_at: 1000,
        updated_at: 1000,
    })
    .await
    .unwrap();

    svc.reset(TEST_USER_1, &conv.conversation_id).await.unwrap();

    let artifacts = repo.list_artifacts(&conv.conversation_id).await.unwrap();
    assert!(artifacts.is_empty());
}

#[tokio::test]
async fn clear_messages_uses_terminal_aggregate_fence_and_preserves_status() {
    let (svc, _broadcaster, repo, runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    seed_reset_aggregate(&repo, &conv.conversation_id).await;
    svc.warmup_for_view(TEST_USER_1, &conv.conversation_id, &runtime_registry)
        .await
        .unwrap();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("finished".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    svc.clear_messages(
        TEST_USER_1,
        &conv.conversation_id,
        &runtime_registry,
    )
    .await
    .unwrap();

    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    assert_eq!(
        repo.get_messages(&conv.conversation_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .total,
        0
    );
    assert!(repo.list_artifacts(&conv.conversation_id).await.unwrap().is_empty());
    assert!(
        runtime_registry
            .get_runtime(&conv.conversation_id)
            .is_none(),
        "clear must terminate the old runtime before deleting projections"
    );
}

#[tokio::test]
async fn clear_messages_rejects_running_turn_without_partial_delete() {
    let (svc, _broadcaster, repo, runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    seed_reset_aggregate(&repo, &conv.conversation_id).await;
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("running".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let _turn = svc
        .runtime_state()
        .try_acquire_turn(&conv.conversation_id)
        .unwrap();

    assert!(matches!(
        svc.clear_messages(
            TEST_USER_1,
            &conv.conversation_id,
            &runtime_registry,
        )
        .await
        .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert_reset_aggregate_intact(&repo, &conv.conversation_id, "running").await;
}

#[tokio::test]
async fn reset_rejects_active_turn_without_mutating_aggregate() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    seed_reset_aggregate(&repo, &conv.conversation_id).await;
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("running".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let _turn = svc
        .runtime_state()
        .try_acquire_turn(&conv.conversation_id)
        .unwrap();

    assert!(matches!(
        svc.reset(TEST_USER_1, &conv.conversation_id)
            .await
            .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert_reset_aggregate_intact(&repo, &conv.conversation_id, "running").await;
}

#[tokio::test]
async fn reset_rejects_completion_owner_until_completion_finishes() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    seed_reset_aggregate(&repo, &conv.conversation_id).await;
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("finished".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let runtime_state = svc.runtime_state();
    let mut turn = runtime_state
        .try_acquire_turn(&conv.conversation_id)
        .unwrap();
    let completion = runtime_state
        .begin_turn_completion(&conv.conversation_id, turn.turn_id())
        .unwrap()
        .unwrap();
    assert!(turn.release());

    assert!(matches!(
        svc.reset(TEST_USER_1, &conv.conversation_id)
            .await
            .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert_reset_aggregate_intact(&repo, &conv.conversation_id, "finished").await;

    drop(completion);
    svc.reset(TEST_USER_1, &conv.conversation_id).await.unwrap();
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("pending")
    );
    assert_eq!(
        repo.get_messages(&conv.conversation_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .total,
        0
    );
    assert!(repo.list_artifacts(&conv.conversation_id).await.unwrap().is_empty());
}

#[tokio::test]
async fn reset_tears_down_warmed_runtime_and_allows_fresh_view_warmup() {
    let (svc, _broadcaster, repo, runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    svc.warmup_for_view(TEST_USER_1, &conv.conversation_id, &runtime_registry)
        .await
        .unwrap();
    let old_runtime = runtime_registry
        .get_runtime(&conv.conversation_id)
        .expect("initial view warmup built a runtime");
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("finished".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    svc.reset(TEST_USER_1, &conv.conversation_id).await.unwrap();
    assert!(
        runtime_registry.get_runtime(&conv.conversation_id).is_none(),
        "reset must not leave the warmed runtime registered"
    );
    svc.warmup_for_view(TEST_USER_1, &conv.conversation_id, &runtime_registry)
        .await
        .unwrap();
    let fresh_runtime = runtime_registry
        .get_runtime(&conv.conversation_id)
        .expect("empty Pending aggregate may warm a fresh runtime");
    assert!(
        !std::ptr::eq(old_runtime.as_runtime(), fresh_runtime.as_runtime()),
        "post-reset warmup must not reuse the pre-reset runtime instance"
    );
}

#[tokio::test]
async fn reset_nomi_clears_exact_persisted_session_generation_before_db_commit() {
    let repo = Arc::new(MockRepo::new());
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry_impl = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry_impl.clone();
    let svc = ConversationService::new(
        Arc::<str>::from(TEST_USER_1),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry,
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let request: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
        "extra": { "workspace": "/project" }
    }))
    .unwrap();
    let conversation = svc.create(TEST_USER_1, request).await.unwrap();
    let created_at = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap()
        .created_at;
    repo.update(
        &conversation.conversation_id,
        &ConversationRowUpdate {
            status: Some("finished".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    svc.reset(TEST_USER_1, &conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(
        registry_impl.nomi_reset_records(),
        vec![(conversation.conversation_id.clone(), created_at)],
        "service must clear only the exact conversation generation's persisted Nomi session"
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("pending")
    );
}

#[tokio::test]
async fn companion_archive_context_reset_clears_finished_cold_nomi_without_runtime_build() {
    let repo = Arc::new(MockRepo::new());
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry_impl = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry_impl.clone();
    let svc = ConversationService::new(
        Arc::<str>::from(TEST_USER_1),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let request: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
        "extra": { "workspace": "/project" }
    }))
    .unwrap();
    let conversation = svc.create(TEST_USER_1, request).await.unwrap();
    let created_at = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap()
        .created_at;
    repo.update(
        &conversation.conversation_id,
        &ConversationRowUpdate {
            status: Some("finished".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    registry_impl.seed_persisted_nomi_context(
        &conversation.conversation_id,
        created_at,
        vec!["archived context must not be resumed".to_owned()],
    );
    broadcaster.take_events();

    // ConversationArchivePort::reset_context delegates directly to this seam.
    // A cold archive reset must erase persistence without using warmup/factory.
    svc.clear_context(TEST_USER_1, &conversation.conversation_id)
    .await
    .unwrap();

    assert_eq!(
        registry_impl.build_count(),
        0,
        "archive maintenance must never construct a cold runtime"
    );
    assert_eq!(
        registry_impl.active_runtime_count(),
        0,
        "context reset must leave a cold conversation cold"
    );
    assert!(
        registry_impl
            .persisted_nomi_context(&conversation.conversation_id, created_at)
            .is_empty(),
        "the exact persisted Nomi session generation must be empty"
    );
    assert_eq!(
        registry_impl.nomi_reset_records(),
        vec![(conversation.conversation_id.clone(), created_at)],
        "the conversation created_at owner token must fence persisted reset"
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished"),
        "archive context reset must not reopen the durable lifecycle"
    );
    assert!(
        broadcaster.take_events().iter().all(|event| {
            !matches!(event.name.as_str(), "turn.started" | "turn.completed")
        }),
        "archive maintenance is not a business turn"
    );
}

#[tokio::test]
async fn clear_context_clears_finished_cold_acp_resume_identity_without_runtime_build() {
    let repo = Arc::new(MockRepo::new());
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry_impl = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry_impl.clone();
    let acp_sessions = Arc::new(StubAcpSessionRepo::default());
    let svc = ConversationService::new(
        Arc::<str>::from(TEST_USER_1),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        acp_sessions.clone(),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conversation = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.update(
        &conversation.conversation_id,
        &ConversationRowUpdate {
            status: Some("finished".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    broadcaster.take_events();

    svc.clear_context(TEST_USER_1, &conversation.conversation_id)
    .await
    .unwrap();

    assert_eq!(registry_impl.build_count(), 0);
    assert_eq!(
        acp_sessions.cleared_session_ids(),
        vec![conversation.conversation_id.clone()]
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    assert!(
        broadcaster.take_events().iter().all(|event| {
            !matches!(event.name.as_str(), "turn.started" | "turn.completed")
        })
    );
}

#[tokio::test(start_paused = true)]
async fn clear_context_retains_reset_fence_past_old_teardown_threshold() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let conversation = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.update(
        &conversation.conversation_id,
        &ConversationRowUpdate {
            status: Some("finished".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Model a factory/tool preparation future that cooperatively observes
    // cancellation but cannot release its lease before its own cleanup is
    // complete. The former clear-context implementation returned Conflict at
    // seven seconds and dropped the reset tombstone while this lease was still
    // capable of side effects.
    let lingering_build = svc
        .begin_runtime_build(&conversation.conversation_id)
        .expect("test runtime-build lease");
    let build_cancelled = lingering_build.cancellation_token();
    let clear_task = {
        let service = svc.clone();
        let conversation_id = conversation.conversation_id.clone();
        tokio::spawn(async move {
            service
                .clear_context(TEST_USER_1, &conversation_id)
                .await
        })
    };
    build_cancelled.cancelled().await;

    tokio::time::advance(Duration::from_secs(8)).await;
    tokio::task::yield_now().await;
    assert!(
        !clear_task.is_finished(),
        "elapsed teardown time is not quiescence proof; context clear must retain its reset fence"
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished"),
        "the durable terminal aggregate must remain unchanged while preparation still owns side effects"
    );
    assert!(
        svc.begin_runtime_build(&conversation.conversation_id).is_err(),
        "the context-clear reset tombstone must continue rejecting successor builds"
    );

    drop(lingering_build);
    clear_task
        .await
        .expect("clear-context task")
        .expect("clear context completes after exact build quiescence");
    assert!(
        svc.begin_runtime_build(&conversation.conversation_id).is_ok(),
        "success reopens admission only after the cancelled build lease releases"
    );
}

#[tokio::test]
async fn reset_nomi_persistence_failure_leaves_durable_aggregate_untouched() {
    let repo = Arc::new(MockRepo::new());
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry_impl = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry_impl.clone();
    let svc = ConversationService::new(
        Arc::<str>::from(TEST_USER_1),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry,
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let request: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
        "extra": { "workspace": "/project" }
    }))
    .unwrap();
    let conversation = svc.create(TEST_USER_1, request).await.unwrap();
    seed_reset_aggregate(&repo, &conversation.conversation_id).await;
    repo.update(
        &conversation.conversation_id,
        &ConversationRowUpdate {
            status: Some("finished".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    registry_impl.fail_next_nomi_reset("injected Nomi session persistence failure");

    assert!(matches!(
        svc.reset(TEST_USER_1, &conversation.conversation_id)
            .await
            .unwrap_err(),
        AppError::Internal(message) if message.contains("injected Nomi session persistence failure")
    ));
    assert_reset_aggregate_intact(&repo, &conversation.conversation_id, "finished").await;

    svc.reset(TEST_USER_1, &conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("pending")
    );
}

#[tokio::test]
async fn reset_runtime_teardown_failure_leaves_durable_aggregate_untouched() {
    let repo = Arc::new(MockRepo::new());
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry_impl = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry_impl.clone();
    let svc = ConversationService::new(
        Arc::<str>::from(TEST_USER_1),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    svc.warmup_for_view(TEST_USER_1, &conv.conversation_id, &runtime_registry)
        .await
        .unwrap();
    seed_reset_aggregate(&repo, &conv.conversation_id).await;
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("finished".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    registry_impl.fail_next_termination_wait("injected reset teardown failure");

    assert!(matches!(
        svc.reset(TEST_USER_1, &conv.conversation_id)
            .await
            .unwrap_err(),
        AppError::Internal(message) if message.contains("injected reset teardown failure")
    ));
    assert_reset_aggregate_intact(&repo, &conv.conversation_id, "finished").await;
    assert!(
        runtime_registry.get_runtime(&conv.conversation_id).is_some(),
        "failed teardown must not pretend the old runtime exited"
    );

    // The reset guard is reversible on failure, so a later proven teardown can
    // safely retry and commit the aggregate reset.
    svc.reset(TEST_USER_1, &conv.conversation_id).await.unwrap();
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("pending")
    );
}

#[tokio::test]
async fn reset_rejects_stale_running_row_without_clearing_aggregate() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    seed_reset_aggregate(&repo, &conv.conversation_id).await;
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("running".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert!(matches!(
        svc.reset(TEST_USER_1, &conv.conversation_id)
            .await
            .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert_reset_aggregate_intact(&repo, &conv.conversation_id, "running").await;
}

#[tokio::test]
async fn reset_absorbs_accepted_internal_turn_so_replay_never_builds() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    const OPERATION_ID: &str = "execution:reset-absorbed-turn";

    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry_impl = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(1)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry_impl.clone();
    let make_service = || {
        ConversationService::new(
            Arc::<str>::from(USER_ID),
            std::env::temp_dir(),
            broadcaster.clone(),
            Arc::new(FixedSkillResolver { names: vec![] }),
            runtime_registry.clone(),
            repo.clone(),
            Arc::new(StubAgentMetadataRepo),
            Arc::new(StubAcpSessionRepo::default()),
            Arc::new(crate::NoExecutionConversationBoundary),
        )
    };
    let svc = make_service();
    let request: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": "/project" }
    }))
    .unwrap();
    let conversation = svc.create(USER_ID, request).await.unwrap();
    let send_request = make_send_req();
    let request_payload = json!({
        "content": &send_request.content,
        "files": &send_request.files,
        "inject_skills": &send_request.inject_skills,
        "hidden": send_request.hidden,
        "origin": &send_request.origin,
        "channel_platform": &send_request.channel_platform,
    })
    .to_string();
    let accepted = repo
        .claim_delivery_receipt(
            USER_ID,
            &conversation.conversation_id,
            OPERATION_ID,
            "turn",
            &request_payload,
            now_ms(),
        )
        .await
        .unwrap();
    repo.insert_message(&MessageRow {
        id: 0,
        message_id: accepted.message_id.clone(),
        conversation_id: conversation.conversation_id.clone(),
        msg_id: None,
        r#type: "text".to_owned(),
        content: serde_json::json!({"content": &send_request.content}).to_string(),
        position: Some("right".to_owned()),
        status: Some("finish".to_owned()),
        hidden: false,
        created_at: now_ms(),
    })
    .await
    .unwrap();
    svc.reset(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    let absorbed = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, OPERATION_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(absorbed.status, "completed");
    assert_eq!(absorbed.result_ok, Some(false));
    assert_eq!(absorbed.result_error.as_deref(), Some("conversation reset"));

    // Simulate a new service process receiving the same durable internal
    // operation after reset. The completed failure receipt is absorbing.
    let replay_service = make_service();
    let replay = replay_service
        .send_message_idempotent(
            USER_ID,
            &conversation.conversation_id,
            OPERATION_ID,
            send_request,
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_eq!(replay.message_id, accepted.message_id);
    assert!(replay.completed);
    assert_eq!(replay.result_ok, Some(false));
    assert_eq!(replay.result_error.as_deref(), Some("conversation reset"));
    assert_eq!(
        registry_impl.build_calls(),
        0,
        "absorbed internal replay must never construct or execute a runtime"
    );
    assert_eq!(
        repo.get_messages(&conversation.conversation_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .total,
        0
    );
}

#[tokio::test]
async fn history_reverifies_committed_local_artifact_after_replace_and_delete() {
    use nomifun_ai_agent::artifact_store::ArtifactStore;

    const ONE_PIXEL_PNG: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    let data_root = std::env::temp_dir().join(format!(
        "nomifun-history-artifact-test-{}",
        MessageId::new().into_string()
    ));
    let workspace = data_root.join("custom-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let (svc, _broadcaster, repo, _runtime_registry) =
        make_service_with_workspace_root(data_root.clone());
    let conv = svc
        .create(
            TEST_USER_1,
            serde_json::from_value(json!({
                "type": "acp",
                "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": workspace.to_string_lossy() }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let artifact = ArtifactStore::new(&workspace)
        .persist_images([("image/png", ONE_PIXEL_PNG)])
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let original_bytes = std::fs::read(&artifact.path).unwrap();
    let message_id = MessageId::new().into_string();
    repo.insert_message(&MessageRow {
        id: 0,
        message_id: message_id.clone(),
        conversation_id: conv.conversation_id.clone(),
        msg_id: None,
        r#type: "tool_call".into(),
        content: json!({
            "call_id": "historical-image",
            "name": "ImageGeneration",
            "status": "completed",
            "artifact_delivery_committed": true,
            "artifacts": [artifact.clone()],
        })
        .to_string(),
        position: Some("left".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: 1000,
    })
    .await
    .unwrap();

    let valid = svc
        .list_messages(TEST_USER_1, &conv.conversation_id, ListMessagesQuery::default())
        .await
        .unwrap();
    assert_eq!(valid.items[0].status, Some(nomifun_common::MessageStatus::Finish));
    assert_eq!(valid.items[0].content["status"], "completed");
    assert_eq!(valid.items[0].content["artifacts"].as_array().map(Vec::len), Some(1));

    std::fs::write(&artifact.path, b"replacement bytes at the same locator").unwrap();
    let replaced = svc
        .list_messages(
            TEST_USER_1,
            &conv.conversation_id,
            ListMessagesQuery {
                cursor: Some(String::new()),
                content_mode: Some("compact".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        replaced.items[0].status,
        Some(nomifun_common::MessageStatus::Error)
    );
    assert_eq!(replaced.items[0].content["status"], "error");
    assert_eq!(replaced.items[0].content["artifact_delivery_committed"], false);
    assert_eq!(replaced.items[0].content["artifacts"], json!([]));

    // The downgrade is a read projection, not an irreversible DB rewrite: if
    // the exact committed bytes are restored, the next load can verify them.
    std::fs::write(&artifact.path, &original_bytes).unwrap();
    let restored = svc
        .get_message(TEST_USER_1, &conv.conversation_id, &message_id)
        .await
        .unwrap();
    assert_eq!(restored.status, Some(nomifun_common::MessageStatus::Finish));
    assert_eq!(restored.content["status"], "completed");

    std::fs::remove_file(&artifact.path).unwrap();
    let deleted = svc
        .get_message(TEST_USER_1, &conv.conversation_id, &message_id)
        .await
        .unwrap();
    assert_eq!(deleted.status, Some(nomifun_common::MessageStatus::Error));
    assert_eq!(deleted.content["status"], "error");
    assert_eq!(deleted.content["artifacts"], json!([]));
    std::fs::remove_dir_all(&data_root).unwrap();
}

#[tokio::test]
async fn history_without_workspace_fails_closed_for_acp_local_artifact_batch() {
    use nomifun_ai_agent::artifact_store::ArtifactStore;

    const ONE_PIXEL_PNG: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    let data_root = std::env::temp_dir().join(format!(
        "nomifun-history-artifact-no-workspace-test-{}",
        MessageId::new().into_string()
    ));
    let workspace = data_root.join("custom-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let (svc, _broadcaster, repo, _runtime_registry) =
        make_service_with_workspace_root(data_root.clone());
    let conv = svc
        .create(
            TEST_USER_1,
            serde_json::from_value(json!({
                "type": "acp",
                "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": workspace.to_string_lossy() }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let artifact = ArtifactStore::new(&workspace)
        .persist_images([("image/png", ONE_PIXEL_PNG)])
        .unwrap()
        .into_iter()
        .next()
        .unwrap();

    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            extra: Some("{}".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    repo.insert_message(&MessageRow {
        id: 0,
        message_id: MessageId::new().into_string(),
        conversation_id: conv.conversation_id.clone(),
        msg_id: None,
        r#type: "acp_tool_call".into(),
        content: json!({
            "session_id": "session-history",
            "artifact_delivery_committed": true,
            "update": {
                "session_update": "tool_call_update",
                "tool_call_id": "historical-acp-image",
                "status": "completed",
                "content": [
                    { "type": "artifact", "artifact": artifact },
                    { "type": "resource_link", "name": "remote copy", "uri": "https://example.com/copy.png" }
                ]
            }
        })
        .to_string(),
        position: Some("left".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: 1000,
    })
    .await
    .unwrap();

    let history = svc
        .list_messages(TEST_USER_1, &conv.conversation_id, ListMessagesQuery::default())
        .await
        .unwrap();
    let message = &history.items[0];
    assert_eq!(message.status, Some(nomifun_common::MessageStatus::Error));
    assert_eq!(message.content["update"]["status"], "failed");
    assert_eq!(message.content["artifact_delivery_committed"], false);
    let items = message.content["update"]["content"].as_array().unwrap();
    assert!(items.iter().all(|item| {
        !matches!(
            item["type"].as_str(),
            Some("artifact" | "resource_link")
        )
    }));
    assert!(items.iter().any(|item| item["type"] == "artifact_error"));
    std::fs::remove_dir_all(&data_root).unwrap();
}

#[tokio::test]
async fn legacy_completed_artifact_tool_without_required_receipts_is_not_green() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let fake_receipt = json!({
        "id": "artifact-history-contract",
        "kind": "image",
        "mime_type": "image/png",
        "path": "/project/nomifun-artifacts/history.png",
        "relative_path": "nomifun-artifacts/history.png",
        "size_bytes": 10,
        "sha256": "a".repeat(64),
    });
    let mut wrong_mime_receipt = fake_receipt.clone();
    wrong_mime_receipt["mime_type"] = json!("application/pdf");
    let rows = [
        (
            "image_gen",
            json!({
                "call_id": "legacy-empty-image",
                "name": "image_gen",
                "args": { "prompt": "cat" },
                "status": "completed",
                "artifacts": [],
            }),
        ),
        (
            "image_count_short",
            json!({
                "call_id": "legacy-count-short",
                "name": "image_gen",
                "args": { "prompt": "cats", "n": 2 },
                "status": "completed",
                "artifacts": [fake_receipt.clone()],
            }),
        ),
        (
            "image_wrong_mime",
            json!({
                "call_id": "legacy-wrong-mime",
                "name": "image_gen",
                "args": { "prompt": "cat" },
                "status": "completed",
                "artifacts": [wrong_mime_receipt],
            }),
        ),
        (
            "ordinary_read",
            json!({
                "call_id": "ordinary-read",
                "name": "read_file",
                "args": { "path": "README.md" },
                "status": "completed",
                "artifacts": [],
            }),
        ),
    ];
    for (index, (_, content)) in rows.iter().enumerate() {
        repo.insert_message(&MessageRow {
            id: 0,
            message_id: MessageId::new().into_string(),
            conversation_id: conv.conversation_id.clone(),
            msg_id: None,
            r#type: "tool_call".into(),
            content: content.to_string(),
            position: Some("left".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: index as i64,
        })
        .await
        .unwrap();
    }

    let history = svc
        .list_messages(TEST_USER_1, &conv.conversation_id, ListMessagesQuery::default())
        .await
        .unwrap();
    assert_eq!(history.items.len(), 4);
    for message in &history.items[..3] {
        assert_eq!(message.status, Some(nomifun_common::MessageStatus::Error));
        assert_eq!(message.content["status"], "error");
        assert_eq!(message.content["artifacts"], json!([]));
        assert_eq!(message.content["artifact_delivery_committed"], false);
    }
    let ordinary_read = &history.items[3];
    assert_eq!(
        ordinary_read.status,
        Some(nomifun_common::MessageStatus::Finish)
    );
    assert_eq!(ordinary_read.content["status"], "completed");
    assert!(ordinary_read
        .content
        .get("artifact_delivery_committed")
        .is_none());
}

#[tokio::test]
async fn legacy_completed_high_signal_tool_group_without_receipts_is_not_green() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.insert_message(&MessageRow {
        id: 0,
        message_id: MessageId::new().into_string(),
        conversation_id: conv.conversation_id.clone(),
        msg_id: None,
        r#type: "tool_group".into(),
        content: json!([
            {
                "call_id": "legacy-group-image",
                "name": "image_gen",
                "status": "completed",
                "description": "generated"
            },
            {
                "call_id": "legacy-group-export",
                "name": "export_pdf",
                "status": "completed",
                "description": "exported",
                "result_display": { "path": "/project/report.pdf" }
            },
            {
                "call_id": "legacy-group-read",
                "name": "read_file",
                "status": "completed",
                "description": "read"
            }
        ])
        .to_string(),
        position: Some("left".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: 1000,
    })
    .await
    .unwrap();

    let history = svc
        .list_messages(TEST_USER_1, &conv.conversation_id, ListMessagesQuery::default())
        .await
        .unwrap();
    assert_eq!(history.items.len(), 1);
    let message = &history.items[0];
    assert_eq!(message.status, Some(nomifun_common::MessageStatus::Error));
    let entries = message.content.as_array().expect("tool group content");
    assert_eq!(entries[0]["status"], "error");
    assert_eq!(entries[1]["status"], "error");
    assert!(entries[1].get("result_display").is_none());
    assert_eq!(entries[2]["status"], "completed");
}

#[tokio::test]
async fn legacy_completed_acp_artifact_tool_without_required_receipts_is_not_green() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let fake_receipt = json!({
        "id": "acp-artifact-history-contract",
        "kind": "image",
        "mime_type": "image/png",
        "path": "/project/nomifun-artifacts/history.png",
        "relative_path": "nomifun-artifacts/history.png",
        "size_bytes": 10,
        "sha256": "a".repeat(64),
    });
    let mut wrong_mime_receipt = fake_receipt.clone();
    wrong_mime_receipt["mime_type"] = json!("application/pdf");
    let identity_free_first = fake_receipt.clone();
    let mut identity_free_duplicate_id = fake_receipt.clone();
    identity_free_duplicate_id["path"] =
        json!("/project/nomifun-artifacts/history-second.png");
    identity_free_duplicate_id["relative_path"] =
        json!("nomifun-artifacts/history-second.png");
    let updates = [
        json!({
            "session_update": "tool_call_update",
            "tool_call_id": "acp-empty-image",
            "title": "ImageGeneration",
            "status": "completed",
            "raw_input": { "prompt": "cat" },
            "content": [],
        }),
        json!({
            "session_update": "tool_call_update",
            "tool_call_id": "acp-count-short",
            "title": "image_gen",
            "status": "completed",
            "raw_input": { "prompt": "cats", "n": 2 },
            "content": [{ "type": "artifact", "artifact": fake_receipt }],
        }),
        json!({
            "session_update": "tool_call_update",
            "tool_call_id": "acp-wrong-mime",
            "title": "image_gen",
            "status": "completed",
            "raw_input": { "prompt": "cat" },
            "content": [{ "type": "artifact", "artifact": wrong_mime_receipt }],
        }),
        json!({
            "session_update": "tool_call_update",
            "tool_call_id": "acp-identity-free-duplicate-id",
            "status": "completed",
            "content": [
                { "type": "artifact", "artifact": identity_free_first },
                { "type": "artifact", "artifact": identity_free_duplicate_id }
            ],
        }),
        json!({
            "session_update": "tool_call_update",
            "tool_call_id": "acp-ordinary-read",
            "title": "Read",
            "status": "completed",
            "raw_input": { "path": "README.md" },
            "content": [],
        }),
    ];
    for (index, update) in updates.iter().enumerate() {
        repo.insert_message(&MessageRow {
            id: 0,
            message_id: MessageId::new().into_string(),
            conversation_id: conv.conversation_id.clone(),
            msg_id: None,
            r#type: "acp_tool_call".into(),
            content: json!({
                "session_id": "legacy-acp-history",
                "update": update,
            })
            .to_string(),
            position: Some("left".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: index as i64,
        })
        .await
        .unwrap();
    }

    let history = svc
        .list_messages(TEST_USER_1, &conv.conversation_id, ListMessagesQuery::default())
        .await
        .unwrap();
    assert_eq!(history.items.len(), 5);
    for message in &history.items[..4] {
        assert_eq!(message.status, Some(nomifun_common::MessageStatus::Error));
        assert_eq!(message.content["update"]["status"], "failed");
        assert_eq!(message.content["artifact_delivery_committed"], false);
        let items = message.content["update"]["content"].as_array().unwrap();
        assert!(items.iter().any(|item| item["type"] == "artifact_error"));
        assert!(items.iter().all(|item| item["type"] != "artifact"));
    }
    let ordinary_read = &history.items[4];
    assert_eq!(
        ordinary_read.status,
        Some(nomifun_common::MessageStatus::Finish)
    );
    assert_eq!(ordinary_read.content["update"]["status"], "completed");
    assert!(ordinary_read
        .content
        .get("artifact_delivery_committed")
        .is_none());
}

#[tokio::test]
async fn reset_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let err = svc.reset(TEST_USER_1, "no-such-id").await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn reset_wrong_user() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let err = svc.reset(TEST_USER_2, &conv.conversation_id).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

// ── Search validation tests ───────────────────────────────────────

#[tokio::test]
async fn search_messages_empty_keyword_returns_bad_request() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();

    let query = SearchMessagesQuery {
        keyword: "".into(),
        page: None,
        page_size: None,
    };
    let err = svc.search_messages(TEST_USER_1, query).await.unwrap_err();
    assert!(matches!(err, AppError::BadRequest(_)));
}

#[tokio::test]
async fn search_messages_whitespace_keyword_returns_bad_request() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();

    let query = SearchMessagesQuery {
        keyword: "   ".into(),
        page: None,
        page_size: None,
    };
    let err = svc.search_messages(TEST_USER_1, query).await.unwrap_err();
    assert!(matches!(err, AppError::BadRequest(_)));
}

// ── Mock Agent ───────────────────────────────────────────────────

struct MockAgent {
    conversation_id: String,
    event_tx: broadcast::Sender<AgentStreamEvent>,
    stopped: Mutex<bool>,
    confirmations: Mutex<Vec<Confirmation>>,
    approval_memory: Mutex<std::collections::HashMap<String, bool>>,
    allow_direct_confirm: bool,
    /// Optional workspace override. Simple mocks fall back to the host's real
    /// temporary directory so workspace canonicalization behaves identically
    /// on Windows, Linux, and macOS.
    workspace_override: Option<String>,
}

impl MockAgent {
    fn new(conversation_id: &str) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            conversation_id: conversation_id.to_owned(),
            event_tx,
            stopped: Mutex::new(false),
            confirmations: Mutex::new(vec![]),
            approval_memory: Mutex::new(std::collections::HashMap::new()),
            allow_direct_confirm: false,
            workspace_override: None,
        }
    }

    fn with_confirmations(conversation_id: &str, confirmations: Vec<Confirmation>) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            conversation_id: conversation_id.to_owned(),
            event_tx,
            stopped: Mutex::new(false),
            confirmations: Mutex::new(confirmations),
            approval_memory: Mutex::new(std::collections::HashMap::new()),
            allow_direct_confirm: false,
            workspace_override: None,
        }
    }

    fn with_direct_confirm(conversation_id: &str) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            conversation_id: conversation_id.to_owned(),
            event_tx,
            stopped: Mutex::new(false),
            confirmations: Mutex::new(vec![]),
            approval_memory: Mutex::new(std::collections::HashMap::new()),
            allow_direct_confirm: true,
            workspace_override: None,
        }
    }
}

#[async_trait::async_trait]
impl AgentRuntimeControl for MockAgent {
    fn agent_type(&self) -> AgentType {
        AgentType::Acp
    }
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    fn workspace(&self) -> &str {
        match self.workspace_override.as_deref() {
            Some(workspace) => workspace,
            None => cross_platform_mock_workspace(),
        }
    }
    fn status(&self) -> Option<ConversationStatus> {
        None
    }
    fn is_transport_healthy(&self) -> bool {
        true
    }
    fn last_activity_at(&self) -> TimestampMs {
        0
    }
    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }
    async fn send_message(&self, _data: SendMessageData) -> Result<(), AgentSendError> {
        // Emit finish event so the relay task completes
        let _ = self.event_tx.send(AgentStreamEvent::Finish(
            nomifun_ai_agent::protocol::events::FinishEventData::default(),
        ));
        Ok(())
    }
    async fn cancel(&self) -> Result<(), AppError> {
        *self.stopped.lock().unwrap() = true;
        Ok(())
    }
    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl MockAgentRuntime for MockAgent {
    fn get_confirmations(&self) -> Vec<Confirmation> {
        self.confirmations.lock().unwrap().clone()
    }
    fn check_approval(&self, action: &str, command_type: Option<&str>) -> bool {
        let key = match command_type {
            Some(ct) => format!("{action}:{ct}"),
            None => action.to_owned(),
        };
        self.approval_memory.lock().unwrap().get(&key).copied().unwrap_or(false)
    }
    fn confirm(
        &self,
        _msg_id: &str,
        call_id: &str,
        _data: serde_json::Value,
        always_allow: bool,
    ) -> Result<(), AppError> {
        let mut confs = self.confirmations.lock().unwrap();
        let existed = confs.iter().any(|c| c.call_id == call_id);
        if !existed && !self.allow_direct_confirm {
            return Err(AppError::NotFound(format!("Confirmation {call_id} not found")));
        }
        if always_allow && let Some(conf) = confs.iter().find(|c| c.call_id == call_id) {
            let key = match (conf.action.as_deref(), conf.command_type.as_deref()) {
                (Some(a), Some(ct)) => format!("{a}:{ct}"),
                (Some(a), None) => a.to_owned(),
                _ => String::new(),
            };
            self.approval_memory.lock().unwrap().insert(key, true);
        }
        confs.retain(|c| c.call_id != call_id);
        Ok(())
    }
}

// ── Mock AgentRuntimeRegistry ──────────────────────────────────────

struct MockAgentRuntimeRegistry {
    agents: Mutex<std::collections::HashMap<String, AgentRuntimeHandle>>,
    workspace_bindings:
        Mutex<std::collections::HashMap<String, nomifun_knowledge::WorkspaceBindingLease>>,
    build_count: AtomicUsize,
    termination_records: Mutex<Vec<(String, Option<AgentKillReason>)>>,
    termination_count: AtomicUsize,
    termination_wait_count: AtomicUsize,
    fail_next_termination_wait: Mutex<Option<String>>,
    block_termination_wait: AtomicBool,
    nomi_reset_records: Mutex<Vec<(String, TimestampMs)>>,
    persisted_nomi_context:
        Mutex<std::collections::HashMap<(String, TimestampMs), Vec<String>>>,
    fail_next_nomi_reset: Mutex<Option<String>>,
}

impl MockAgentRuntimeRegistry {
    fn new() -> Self {
        Self {
            agents: Mutex::new(std::collections::HashMap::new()),
            workspace_bindings: Mutex::new(std::collections::HashMap::new()),
            build_count: AtomicUsize::new(0),
            termination_records: Mutex::new(Vec::new()),
            termination_count: AtomicUsize::new(0),
            termination_wait_count: AtomicUsize::new(0),
            fail_next_termination_wait: Mutex::new(None),
            block_termination_wait: AtomicBool::new(false),
            nomi_reset_records: Mutex::new(Vec::new()),
            persisted_nomi_context: Mutex::new(std::collections::HashMap::new()),
            fail_next_nomi_reset: Mutex::new(None),
        }
    }

    fn insert_agent(&self, conversation_id: &str, agent: AgentRuntimeHandle) {
        self.agents.lock().unwrap().insert(conversation_id.to_owned(), agent);
    }

    fn termination_count(&self) -> usize {
        self.termination_count.load(Ordering::SeqCst)
    }

    fn termination_wait_count(&self) -> usize {
        self.termination_wait_count.load(Ordering::SeqCst)
    }

    fn termination_records(&self) -> Vec<(String, Option<AgentKillReason>)> {
        self.termination_records.lock().unwrap().clone()
    }

    fn fail_next_termination_wait(&self, error: impl Into<String>) {
        *self.fail_next_termination_wait.lock().unwrap() = Some(error.into());
    }

    fn block_termination_wait(&self, blocked: bool) {
        self.block_termination_wait
            .store(blocked, Ordering::SeqCst);
    }

    fn nomi_reset_records(&self) -> Vec<(String, TimestampMs)> {
        self.nomi_reset_records.lock().unwrap().clone()
    }

    fn build_count(&self) -> usize {
        self.build_count.load(Ordering::SeqCst)
    }

    fn seed_persisted_nomi_context(
        &self,
        conversation_id: &str,
        conversation_created_at: TimestampMs,
        messages: Vec<String>,
    ) {
        self.persisted_nomi_context.lock().unwrap().insert(
            (conversation_id.to_owned(), conversation_created_at),
            messages,
        );
    }

    fn persisted_nomi_context(
        &self,
        conversation_id: &str,
        conversation_created_at: TimestampMs,
    ) -> Vec<String> {
        self.persisted_nomi_context
            .lock()
            .unwrap()
            .get(&(conversation_id.to_owned(), conversation_created_at))
            .cloned()
            .unwrap_or_default()
    }

    fn fail_next_nomi_reset(&self, error: impl Into<String>) {
        *self.fail_next_nomi_reset.lock().unwrap() = Some(error.into());
    }
}

struct FailingAgentRuntimeRegistry {
    error: String,
}

impl FailingAgentRuntimeRegistry {
    fn new(error: impl Into<String>) -> Self {
        Self { error: error.into() }
    }
}

#[async_trait::async_trait]
impl AgentRuntimeRegistry for FailingAgentRuntimeRegistry {
    fn get_runtime(&self, _conversation_id: &str) -> Option<AgentRuntimeHandle> {
        None
    }

    async fn get_or_create_runtime(
        &self,
        _conversation_id: &str,
        _options: AgentRuntimeBuildOptions,
    ) -> Result<AgentRuntimeHandle, AppError> {
        Err(AppError::BadGateway(self.error.clone()))
    }

    fn terminate(&self, _conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        Ok(())
    }

    fn terminate_and_wait(
        &self,
        _conversation_id: &str,
        _reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }

    fn terminate_and_wait_result(
        &self,
        _conversation_id: &str,
        _reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AppError>> + Send>> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn terminate_all(&self) {}

    fn active_runtime_count(&self) -> usize {
        0
    }

    fn collect_idle_runtimes(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

#[async_trait::async_trait]
impl AgentRuntimeRegistry for MockAgentRuntimeRegistry {
    fn get_runtime(&self, conversation_id: &str) -> Option<AgentRuntimeHandle> {
        self.agents.lock().unwrap().get(conversation_id).cloned()
    }

    async fn get_or_create_runtime(
        &self,
        conversation_id: &str,
        mut options: AgentRuntimeBuildOptions,
    ) -> Result<AgentRuntimeHandle, AppError> {
        self.build_count.fetch_add(1, Ordering::SeqCst);
        let workspace = options.workspace.clone();
        if let Some(requested) = options.workspace_binding_lease.take() {
            let mut bindings = self.workspace_bindings.lock().unwrap();
            if let Some(current) = bindings.get(conversation_id)
                && !current.same_binding(&requested)
            {
                return Err(AppError::Conflict(format!(
                    "conversation {conversation_id} test runtime has a different workspace binding"
                )));
            }
            bindings.insert(conversation_id.to_owned(), requested);
        }
        let mut agents = self.agents.lock().unwrap();
        if let Some(existing) = agents.get(conversation_id) {
            return Ok(existing.clone());
        }
        let mut agent = MockAgent::new(conversation_id);
        agent.workspace_override = Some(workspace);
        let instance = AgentRuntimeHandle::Mock(Arc::new(agent));
        agents.insert(conversation_id.to_owned(), instance.clone());
        Ok(instance)
    }

    fn terminate(&self, conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        self.termination_count.fetch_add(1, Ordering::SeqCst);
        self.termination_records
            .lock()
            .unwrap()
            .push((conversation_id.to_owned(), _reason));
        self.agents.lock().unwrap().remove(conversation_id);
        self.workspace_bindings
            .lock()
            .unwrap()
            .remove(conversation_id);
        Ok(())
    }

    fn terminate_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        self.termination_wait_count.fetch_add(1, Ordering::SeqCst);
        let _ = self.terminate(conversation_id, reason);
        Box::pin(std::future::ready(()))
    }

    fn terminate_and_wait_result(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AppError>> + Send>> {
        self.termination_wait_count.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.fail_next_termination_wait.lock().unwrap().take() {
            return Box::pin(std::future::ready(Err(AppError::Internal(error))));
        }
        if self.block_termination_wait.load(Ordering::SeqCst) {
            return Box::pin(std::future::ready(Err(AppError::Internal(
                "injected persistent runtime teardown failure".to_owned(),
            ))));
        }
        let result = self.terminate(conversation_id, reason);
        Box::pin(std::future::ready(result))
    }

    fn reset_persisted_nomi_session(
        &self,
        conversation_id: &str,
        conversation_created_at: TimestampMs,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<NomiSessionResetOutcome, AppError>>
                + Send,
        >,
    > {
        self.nomi_reset_records
            .lock()
            .unwrap()
            .push((conversation_id.to_owned(), conversation_created_at));
        if let Some(error) = self.fail_next_nomi_reset.lock().unwrap().take() {
            return Box::pin(std::future::ready(Err(AppError::Internal(error))));
        }
        self.persisted_nomi_context
            .lock()
            .unwrap()
            .remove(&(conversation_id.to_owned(), conversation_created_at));
        Box::pin(std::future::ready(Ok(
            NomiSessionResetOutcome::AlreadyAbsent,
        )))
    }

    fn terminate_all(&self) {
        self.agents.lock().unwrap().clear();
        self.workspace_bindings.lock().unwrap().clear();
    }

    fn active_runtime_count(&self) -> usize {
        self.agents.lock().unwrap().len()
    }

    fn collect_idle_runtimes(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

struct SlowAgentRuntimeRegistry {
    delay: Duration,
    built: AtomicBool,
    build_calls: AtomicUsize,
}

impl SlowAgentRuntimeRegistry {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            built: AtomicBool::new(false),
            build_calls: AtomicUsize::new(0),
        }
    }

    fn was_built(&self) -> bool {
        self.built.load(Ordering::SeqCst)
    }

    fn build_calls(&self) -> usize {
        self.build_calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl AgentRuntimeRegistry for SlowAgentRuntimeRegistry {
    fn get_runtime(&self, _conversation_id: &str) -> Option<AgentRuntimeHandle> {
        None
    }

    async fn get_or_create_runtime(
        &self,
        conversation_id: &str,
        options: AgentRuntimeBuildOptions,
    ) -> Result<AgentRuntimeHandle, AppError> {
        self.build_calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.built.store(true, Ordering::SeqCst);
        let mut agent = MockAgent::new(conversation_id);
        agent.workspace_override = Some(options.workspace);
        Ok(AgentRuntimeHandle::Mock(Arc::new(agent)))
    }

    fn terminate(&self, _conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        Ok(())
    }

    fn terminate_and_wait(
        &self,
        _conversation_id: &str,
        _reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }

    fn terminate_and_wait_result(
        &self,
        _conversation_id: &str,
        _reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AppError>> + Send>> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn terminate_all(&self) {}

    fn active_runtime_count(&self) -> usize {
        usize::from(self.was_built())
    }

    fn collect_idle_runtimes(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

/// A variant of MockAgentRuntimeRegistry that always builds agents with a specific workspace.
struct MockAgentRuntimeRegistryWithWorkspace {
    workspace: String,
    agents: Mutex<std::collections::HashMap<String, AgentRuntimeHandle>>,
}

impl MockAgentRuntimeRegistryWithWorkspace {
    fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_owned(),
            agents: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl AgentRuntimeRegistry for MockAgentRuntimeRegistryWithWorkspace {
    fn get_runtime(&self, conversation_id: &str) -> Option<AgentRuntimeHandle> {
        self.agents.lock().unwrap().get(conversation_id).cloned()
    }

    async fn get_or_create_runtime(
        &self,
        conversation_id: &str,
        _options: AgentRuntimeBuildOptions,
    ) -> Result<AgentRuntimeHandle, AppError> {
        let workspace = self.workspace.clone();
        let mut agents = self.agents.lock().unwrap();
        if let Some(existing) = agents.get(conversation_id) {
            return Ok(existing.clone());
        }
        let mut agent = MockAgent::new(conversation_id);
        agent.workspace_override = Some(workspace);
        let instance = AgentRuntimeHandle::Mock(Arc::new(agent));
        agents.insert(conversation_id.to_owned(), instance.clone());
        Ok(instance)
    }

    fn terminate(&self, conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        self.agents.lock().unwrap().remove(conversation_id);
        Ok(())
    }

    fn terminate_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = self.terminate(conversation_id, reason);
        Box::pin(std::future::ready(()))
    }

    fn terminate_and_wait_result(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AppError>> + Send>> {
        let result = self.terminate(conversation_id, reason);
        Box::pin(std::future::ready(result))
    }

    fn terminate_all(&self) {
        self.agents.lock().unwrap().clear();
    }

    fn active_runtime_count(&self) -> usize {
        self.agents.lock().unwrap().len()
    }

    fn collect_idle_runtimes(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

struct ScriptedAgent {
    conversation_id: String,
    agent_type: AgentType,
    workspace: String,
    event_tx: broadcast::Sender<AgentStreamEvent>,
    scripts: Mutex<VecDeque<Vec<AgentStreamEvent>>>,
    sent_contents: Mutex<Vec<String>>,
}

impl ScriptedAgent {
    fn new(conversation_id: &str, scripts: Vec<Vec<AgentStreamEvent>>) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            conversation_id: conversation_id.to_owned(),
            agent_type: AgentType::Acp,
            workspace: cross_platform_mock_workspace().to_owned(),
            event_tx,
            scripts: Mutex::new(VecDeque::from(scripts)),
            sent_contents: Mutex::new(vec![]),
        }
    }

    fn with_agent_type(mut self, agent_type: AgentType) -> Self {
        self.agent_type = agent_type;
        self
    }

    fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = workspace.into();
        self
    }

    fn sent_contents(&self) -> Vec<String> {
        self.sent_contents.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl AgentRuntimeControl for ScriptedAgent {
    fn agent_type(&self) -> AgentType {
        self.agent_type
    }

    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn workspace(&self) -> &str {
        &self.workspace
    }

    fn status(&self) -> Option<ConversationStatus> {
        Some(ConversationStatus::Finished)
    }

    fn is_transport_healthy(&self) -> bool {
        true
    }

    fn last_activity_at(&self) -> TimestampMs {
        0
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }

    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        self.sent_contents.lock().unwrap().push(data.content);
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![AgentStreamEvent::Finish(FinishEventData::default())]);
        for event in script {
            let _ = self.event_tx.send(event);
        }
        Ok(())
    }

    async fn cancel(&self) -> Result<(), AppError> {
        Ok(())
    }

    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        Ok(())
    }
}

impl MockAgentRuntime for ScriptedAgent {}

/// A mock that models a LIVE, steerable turn for `steer_message` tests.
///
/// It records whether `steer()` vs `send_message()` was invoked so a test can
/// prove which path `steer_message` took (mid-turn injection vs. fall-back to
/// a fresh `send_message`). `status()` and the `steer()` return value are both
/// configurable so a single mock covers the happy path (`Running` + `Ok(true)`)
/// and the racy "turn ended" path (`Running` + `Ok(false)`).
struct SteerableAgent {
    conversation_id: String,
    event_tx: broadcast::Sender<AgentStreamEvent>,
    status: Option<ConversationStatus>,
    steer_result: bool,
    /// When set, `steer()` returns `Err(BadRequest)` (the non-Nomi
    /// `steer_unsupported` path) instead of `Ok(steer_result)`.
    steer_err: bool,
    steered: Mutex<Vec<String>>,
    sent_contents: Mutex<Vec<String>>,
    confirmations: Mutex<Vec<Confirmation>>,
    confirmed_call_ids: Mutex<Vec<String>>,
}

impl SteerableAgent {
    fn new(conversation_id: &str, status: Option<ConversationStatus>, steer_result: bool) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            conversation_id: conversation_id.to_owned(),
            event_tx,
            status,
            steer_result,
            steer_err: false,
            steered: Mutex::new(vec![]),
            sent_contents: Mutex::new(vec![]),
            confirmations: Mutex::new(vec![]),
            confirmed_call_ids: Mutex::new(vec![]),
        }
    }

    /// A live (Running) turn whose `steer()` rejects with `BadRequest`,
    /// modelling a non-Nomi engine (the `steer_unsupported` route maps this
    /// to a client-side queue fallback).
    fn new_steer_err(conversation_id: &str) -> Self {
        Self {
            steer_err: true,
            ..Self::new(conversation_id, Some(ConversationStatus::Running), false)
        }
    }

    fn steered(&self) -> Vec<String> {
        self.steered.lock().unwrap().clone()
    }

    fn with_confirmation(self, confirmation: Confirmation) -> Self {
        self.confirmations.lock().unwrap().push(confirmation);
        self
    }

    fn confirmed_call_ids(&self) -> Vec<String> {
        self.confirmed_call_ids.lock().unwrap().clone()
    }

    fn sent_contents(&self) -> Vec<String> {
        self.sent_contents.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl AgentRuntimeControl for SteerableAgent {
    fn agent_type(&self) -> AgentType {
        AgentType::Nomi
    }
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    fn workspace(&self) -> &str {
        cross_platform_mock_workspace()
    }
    fn status(&self) -> Option<ConversationStatus> {
        self.status
    }
    fn is_transport_healthy(&self) -> bool {
        true
    }
    fn last_activity_at(&self) -> TimestampMs {
        0
    }
    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }
    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        self.sent_contents.lock().unwrap().push(data.content);
        let _ = self
            .event_tx
            .send(AgentStreamEvent::Finish(FinishEventData::default()));
        Ok(())
    }
    async fn cancel(&self) -> Result<(), AppError> {
        Ok(())
    }
    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl MockAgentRuntime for SteerableAgent {
    fn get_confirmations(&self) -> Vec<Confirmation> {
        self.confirmations.lock().unwrap().clone()
    }

    fn steer(&self, text: String) -> Result<bool, AppError> {
        self.steered.lock().unwrap().push(text);
        if self.steer_err {
            return Err(AppError::BadRequest("Steering is not supported for this agent type".into()));
        }
        Ok(self.steer_result)
    }

    fn confirm(
        &self,
        _msg_id: &str,
        call_id: &str,
        _data: serde_json::Value,
        _always_allow: bool,
    ) -> Result<(), AppError> {
        let mut confirmations = self.confirmations.lock().unwrap();
        if !confirmations
            .iter()
            .any(|confirmation| confirmation.call_id == call_id)
        {
            return Err(AppError::NotFound(format!(
                "Confirmation {call_id} not found"
            )));
        }
        confirmations.retain(|confirmation| confirmation.call_id != call_id);
        self.confirmed_call_ids
            .lock()
            .unwrap()
            .push(call_id.to_owned());
        Ok(())
    }
}

struct MockCronContinuationService;

#[async_trait::async_trait]
impl ICronService for MockCronContinuationService {
    async fn create_job(&self, _user_id: &str, _conversation_id: &str, params: &CronCreateParams) -> CronCommandResult {
        CronCommandResult {
            success: true,
            message: format!("Created cron job '{}'", params.name),
        }
    }

    async fn update_job(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _params: &CronUpdateParams,
    ) -> CronCommandResult {
        CronCommandResult {
            success: true,
            message: "Updated cron job".into(),
        }
    }

    async fn list_jobs(&self, _user_id: &str, _conversation_id: &str) -> CronCommandResult {
        CronCommandResult {
            success: true,
            message: "No scheduled tasks".into(),
        }
    }

    async fn delete_job(&self, _user_id: &str, _job_id: &str) -> CronCommandResult {
        CronCommandResult {
            success: true,
            message: "Deleted cron job".into(),
        }
    }
}

struct RecordingKnowledgeCompleter {
    response: String,
    prompts: Mutex<Vec<(String, String)>>,
    models: Mutex<Vec<(String, String)>>,
}

impl RecordingKnowledgeCompleter {
    fn new(response: String) -> Self {
        Self {
            response,
            prompts: Mutex::new(Vec::new()),
            models: Mutex::new(Vec::new()),
        }
    }

    fn prompts(&self) -> Vec<(String, String)> {
        self.prompts.lock().unwrap().clone()
    }

    fn models(&self) -> Vec<(String, String)> {
        self.models.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl KnowledgeCompleter for RecordingKnowledgeCompleter {
    async fn complete(&self, system: &str, user: &str) -> Result<String, AppError> {
        self.prompts.lock().unwrap().push((system.to_owned(), user.to_owned()));
        Ok(self.response.clone())
    }

    async fn complete_with(
        &self,
        system: &str,
        user: &str,
        provider_id: &str,
        model: &str,
    ) -> Result<String, AppError> {
        self.models
            .lock()
            .unwrap()
            .push((provider_id.to_owned(), model.to_owned()));
        self.complete(system, user).await
    }
}

struct BlockingFirstKnowledgeCompleter {
    first_response: String,
    subsequent_response: String,
    prompts: Mutex<Vec<(String, String)>>,
    calls: AtomicUsize,
    has_started: AtomicBool,
    started: Notify,
    release: Notify,
}

impl BlockingFirstKnowledgeCompleter {
    fn new_sequence(first_response: String, subsequent_response: String) -> Self {
        Self {
            first_response,
            subsequent_response,
            prompts: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            has_started: AtomicBool::new(false),
            started: Notify::new(),
            release: Notify::new(),
        }
    }

    async fn wait_started(&self) {
        loop {
            let notified = self.started.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.has_started.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn release(&self) {
        self.release.notify_waiters();
    }
}

#[async_trait::async_trait]
impl KnowledgeCompleter for BlockingFirstKnowledgeCompleter {
    async fn complete(&self, system: &str, user: &str) -> Result<String, AppError> {
        self.prompts.lock().unwrap().push((system.to_owned(), user.to_owned()));
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            self.has_started.store(true, Ordering::Release);
            self.started.notify_waiters();
            self.release.notified().await;
        }
        Ok(if call == 0 {
            self.first_response.clone()
        } else {
            self.subsequent_response.clone()
        })
    }
}

fn unique_test_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("nomifun-{label}-{}-{nanos}", std::process::id()))
}

// ── send_message tests ──────────────────────────────────────────

fn make_send_req() -> SendMessageRequest {
    serde_json::from_value(json!({
        "content": "Hello"
    }))
    .unwrap()
}

async fn send_message_with_test_key(
    service: &ConversationService,
    user_id: &str,
    conversation_id: &str,
    idempotency_key: &str,
    request: SendMessageRequest,
    runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
) -> Result<String, AppError> {
    service
        .send_message_with_idempotency_key(
            user_id,
            conversation_id,
            idempotency_key,
            request,
            runtime_registry,
        )
        .await
        .map(|delivery| delivery.message_id)
}

fn make_execution_steer_req(content: &str) -> SendMessageRequest {
    serde_json::from_value(json!({
        "content": content,
        "origin": "agent_execution"
    }))
    .unwrap()
}

async fn make_execution_steer_service(
) -> (
    ConversationService,
    Arc<SqliteConversationRepository>,
    Arc<MockBroadcaster>,
    Arc<MockAgentRuntimeRegistry>,
    String,
) {
    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(SQLITE_TEST_OWNER),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry,
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(ActiveRetainedExecutionBoundary),
    );
    let conversation = service
        .create(SQLITE_TEST_OWNER, make_create_req())
        .await
        .unwrap();
    (
        service,
        repo,
        broadcaster,
        registry,
        conversation.conversation_id,
    )
}

async fn make_public_steer_service(
) -> (
    ConversationService,
    Arc<SqliteConversationRepository>,
    Arc<MockBroadcaster>,
    Arc<MockAgentRuntimeRegistry>,
    String,
) {
    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(SQLITE_TEST_OWNER),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry,
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conversation = service
        .create(SQLITE_TEST_OWNER, make_create_req())
        .await
        .unwrap();
    (
        service,
        repo,
        broadcaster,
        registry,
        conversation.conversation_id,
    )
}

async fn wait_for_turn_released(svc: &ConversationService, conversation_id: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !svc.runtime_state().has_active_turn(conversation_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("turn should release its runtime handle");
    assert!(
        svc.runtime_state()
            .wait_for_cleanup_fences(conversation_id, Duration::from_secs(2))
            .await,
        "turn completion should publish and release its cleanup fence"
    );
}

#[tokio::test]
async fn send_message_returns_accepted() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let msg_id = send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "send-message-returns-accepted",
        make_send_req(),
        &runtime_registry,
    )
        .await
        .unwrap();

    assert!(!msg_id.is_empty(), "msg_id must be non-empty");
    MessageId::parse(&msg_id).expect("msg_id should be a canonical UUIDv7");
}

#[tokio::test]
async fn send_message_rejects_pathological_workspace_with_runtime_error_code() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let legacy_workspace = "/tmp/my project ".to_owned();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            extra: Some(json!({ "workspace": legacy_workspace }).to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let err = send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "pathological-workspace-runtime-error",
        make_send_req(),
        &runtime_registry,
    )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        AppError::WorkspacePathEdgeWhitespaceRuntimeUnsupported(message) if message == "/tmp/my project "
    ));

    let messages = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let messages = repo.get_messages(&conv.conversation_id, 1, 20, SortOrder::Asc).await.unwrap().items;
            if messages.iter().any(|message| message.r#type == "tips") {
                return messages;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("legacy workspace failure should persist an error tip");

    let error_tip = messages
        .iter()
        .find(|message| message.r#type == "tips")
        .expect("legacy workspace failure should persist an error tips message");
    let content: serde_json::Value = serde_json::from_str(&error_tip.content).unwrap();
    assert_eq!(
        content["code"],
        "WORKSPACE_PATH_EDGE_WHITESPACE_RUNTIME_UNSUPPORTED"
    );
    assert_eq!(content["details"]["workspace_path"], "/tmp/my project ");
    assert_eq!(
        content["error"]["code"],
        "WORKSPACE_PATH_EDGE_WHITESPACE_RUNTIME_UNSUPPORTED"
    );
    assert_eq!(content["error"]["workspacePath"], "/tmp/my project ");
}

#[tokio::test]
async fn send_message_broadcasts_user_created_event() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    // Clear events from create
    broadcaster.take_events();

    let msg_id = send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "broadcast-user-created",
        make_send_req(),
        &runtime_registry,
    )
        .await
        .unwrap();

    let events = broadcaster.take_events();
    let user_created = events
        .iter()
        .find(|e| e.name == "message.userCreated")
        .expect("should broadcast message.userCreated event");

    assert_eq!(user_created.data["conversation_id"], conv.conversation_id);
    assert_eq!(user_created.data["msg_id"], msg_id);
    assert_eq!(user_created.data["content"], "Hello");
    assert_eq!(user_created.data["position"], "right");
    // No companion_session in extra → markers default off.
    assert_eq!(user_created.data["companion"], false);
    assert!(user_created.data["companion_id"].is_null());
    assert!(user_created.data["channel_platform"].is_null());
}

#[tokio::test]
async fn send_message_broadcasts_turn_started_with_processing_runtime() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    broadcaster.take_events();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "broadcast-turn-started-runtime",
        make_send_req(),
        &runtime_registry,
    )
        .await
        .unwrap();

    let events = broadcaster.take_events();
    let turn_started = events
        .iter()
        .find(|e| e.name == "turn.started")
        .expect("should broadcast turn.started event as soon as a turn is acquired");

    assert_eq!(turn_started.data["conversation_id"], conv.conversation_id);
    assert_eq!(turn_started.data["conversation_id"], conv.conversation_id);
    assert_eq!(turn_started.data["status"], "running");
    assert_eq!(turn_started.data["runtime"]["state"], "starting");
    assert_eq!(turn_started.data["runtime"]["is_processing"], true);
    assert_eq!(turn_started.data["runtime"]["can_send_message"], false);
    assert_eq!(
        turn_started.data["runtime"]["active_turn_id"],
        turn_started.data["turn_id"],
        "turn.started must carry the exact runtime admission identity"
    );
    assert!(
        turn_started.data["runtime"]["processing_started_at"].is_number(),
        "turn.started runtime should expose a stable processing start timestamp"
    );
}

#[tokio::test]
async fn send_message_broadcasts_companion_markers_for_companion_conversation() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let workspace = isolated_test_workspace("companion");
    let create_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": workspace, "companion_session": true, "companion_id": "0190f5fe-7c00-7a00-8abc-012345678942" }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, create_req).await.unwrap();
    broadcaster.take_events();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "broadcast-companion-markers",
        make_send_req(),
        &runtime_registry,
    )
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.conversation_id).await;

    let events = broadcaster.take_events();
    let user_created = events
        .iter()
        .find(|e| e.name == "message.userCreated")
        .expect("should broadcast message.userCreated event");
    assert_eq!(user_created.data["companion"], true);
    assert_eq!(user_created.data["companion_id"], "0190f5fe-7c00-7a00-8abc-012345678942");

    let turn_completed = events
        .iter()
        .find(|e| e.name == "turn.completed")
        .expect("should broadcast turn.completed event");
    assert_eq!(turn_completed.data["companion"], true);
    assert_eq!(turn_completed.data["companion_id"], "0190f5fe-7c00-7a00-8abc-012345678942");
}

#[tokio::test]
async fn send_message_stamps_channel_platform_for_channel_agent_conversation() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    // A Channel Agent conversation (see nomifun-channel's
    // apply_master_agent_extra): companion_session + companion_id + channel_platform.
    let workspace = isolated_test_workspace("channel-agent");
    let create_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": workspace, "companion_session": true, "companion_id": "0190f5fe-7c00-7a00-8abc-012345678942", "channel_platform": "telegram" }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, create_req).await.unwrap();
    broadcaster.take_events();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "broadcast-channel-platform",
        make_send_req(),
        &runtime_registry,
    )
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.conversation_id).await;

    let events = broadcaster.take_events();
    let user_created = events
        .iter()
        .find(|e| e.name == "message.userCreated")
        .expect("should broadcast message.userCreated event");
    assert_eq!(user_created.data["channel_platform"], "telegram");
    assert_eq!(user_created.data["companion"], true);
    assert_eq!(user_created.data["companion_id"], "0190f5fe-7c00-7a00-8abc-012345678942");

    let turn_completed = events
        .iter()
        .find(|e| e.name == "turn.completed")
        .expect("should broadcast turn.completed event");
    assert_eq!(turn_completed.data["channel_platform"], "telegram");
}

#[tokio::test]
async fn send_message_with_origin_stamps_origin_on_turn_events() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    broadcaster.take_events();

    let req: SendMessageRequest = serde_json::from_value(json!({
        "content": "请创建报表任务",
        "origin": "companion"
    }))
    .unwrap();
    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "broadcast-origin-markers",
        req,
        &runtime_registry,
    )
    .await
    .unwrap();
    wait_for_turn_released(&svc, &conv.conversation_id).await;

    let events = broadcaster.take_events();
    let user_created = events
        .iter()
        .find(|e| e.name == "message.userCreated")
        .expect("should broadcast message.userCreated event");
    assert_eq!(user_created.data["origin"], "companion");

    // The whole turn is origin-marked: turn.completed carries it too, so the
    // companion collector can drop agent-driven reply buffers off the wire.
    let turn_completed = events
        .iter()
        .find(|e| e.name == "turn.completed")
        .expect("should broadcast turn.completed event");
    assert_eq!(turn_completed.data["origin"], "companion");
}

#[tokio::test]
async fn send_message_without_origin_keeps_turn_events_unmarked() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    broadcaster.take_events();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "broadcast-no-origin-markers",
        make_send_req(),
        &runtime_registry,
    )
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.conversation_id).await;

    let events = broadcaster.take_events();
    let user_created = events
        .iter()
        .find(|e| e.name == "message.userCreated")
        .expect("should broadcast message.userCreated event");
    assert!(user_created.data["origin"].is_null());
    let turn_completed = events
        .iter()
        .find(|e| e.name == "turn.completed")
        .expect("should broadcast turn.completed event");
    assert!(turn_completed.data["origin"].is_null());
}

#[tokio::test]
async fn send_message_returns_before_cold_agent_build_completes() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let slow_runtime_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(500)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_runtime_registry.clone();

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let msg_id = tokio::time::timeout(
        Duration::from_millis(50),
        send_message_with_test_key(
            &svc,
            TEST_USER_1,
            &conv.conversation_id,
            "returns-before-cold-build",
            make_send_req(),
            &runtime_registry,
        ),
    )
    .await
    .expect("send_message should return before cold agent build finishes")
    .unwrap();

    assert!(!msg_id.is_empty(), "msg_id must be non-empty");
    assert!(
        !slow_runtime_registry.was_built(),
        "cold agent build should continue in the background after send_message returns"
    );

    let updated = repo.get(&conv.conversation_id).await.unwrap().unwrap();
    assert_eq!(
        updated.status.as_deref(),
        Some("running"),
        "HTTP acceptance requires a durable Running transition before background execution"
    );
    assert!(
        svc.runtime_state().has_active_turn(&conv.conversation_id),
        "turn handle must cover the cold Agent build window"
    );
}

#[tokio::test]
async fn terminal_finalize_failure_retains_running_turn_until_durable_commit() {
    let (svc, broadcaster, repo, runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    broadcaster.take_events();
    repo.block_turn_finalization(true);

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "terminal-finalize-retry",
        make_send_req(),
        &runtime_registry,
    )
    .await
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        repo.wait_for_turn_finalization_attempt(),
    )
    .await
    .expect("turn owner should attempt durable finalization");

    assert!(
        svc.runtime_state().has_active_turn(&conv.conversation_id),
        "a failed terminal DB write must retain exact local turn ownership"
    );
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );
    assert!(
        !broadcaster
            .take_events()
            .into_iter()
            .any(|event| event.name == "turn.completed"),
        "turn.completed must be withheld until durable finalize commits"
    );

    repo.block_turn_finalization(false);
    wait_for_turn_released(&svc, &conv.conversation_id).await;
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    assert!(
        broadcaster
            .take_events()
            .into_iter()
            .any(|event| event.name == "turn.completed")
    );
}

#[tokio::test]
async fn idempotent_send_replay_reuses_pending_turn_and_completed_receipt() {
    const USER_ID: &str = SQLITE_TEST_OWNER;

    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(250)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let svc = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let workspace = isolated_test_workspace("idempotent-send");
    let request: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": workspace }
    }))
    .unwrap();
    let conversation = svc.create(USER_ID, request).await.unwrap();
    let operation_id = "execution:decision:stable";

    let first = svc
        .send_message_idempotent(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(!first.completed);
    let replay_while_pending = svc
        .send_message_idempotent(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_eq!(replay_while_pending.message_id, first.message_id);
    assert!(!replay_while_pending.completed);

    wait_for_turn_released(&svc, &conversation.conversation_id).await;
    assert_eq!(
        slow_registry.build_calls(),
        1,
        "a replay while the stable user transcript is processing must not start another model turn"
    );
    let completed_replay = svc
        .send_message_idempotent(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(completed_replay.completed);
    assert_eq!(slow_registry.build_calls(), 1);

    let user_messages = repo
        .get_messages(&conversation.conversation_id, 1, 20, SortOrder::Asc)
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|message| message.position.as_deref() == Some("right"))
        .collect::<Vec<_>>();
    assert_eq!(user_messages.len(), 1);
    nomifun_common::MessageId::parse(&user_messages[0].message_id)
        .expect("idempotent user transcript row has a canonical message ID");
    assert_eq!(user_messages[0].message_id, first.message_id);
}

#[tokio::test]
async fn public_idempotent_send_reuses_one_turn_and_never_restarts_after_completion() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    const CLIENT_KEY: &str = "0190f5fe-7c00-7a00-8000-000000000777";

    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(250)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let svc = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let request = || -> CreateConversationRequest {
        let workspace = isolated_test_workspace("public-idempotent-send");
        serde_json::from_value(json!({
            "type": "acp",
            "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": workspace }
        }))
        .unwrap()
    };
    let conversation = svc.create(USER_ID, request()).await.unwrap();
    broadcaster.take_events();
    assert_eq!(
        svc.public_turn_delivery_state(
            USER_ID,
            &conversation.conversation_id,
            CLIENT_KEY,
        )
        .await
        .unwrap(),
        PublicTurnDeliveryState::Missing
    );

    let first = svc
        .send_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            CLIENT_KEY,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(!first.replayed);
    assert!(!first.completed);
    let replay_while_pending = svc
        .send_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            CLIENT_KEY,
            make_send_req(),
            &runtime_registry,
    )
    .await
    .unwrap();
    assert_eq!(replay_while_pending.message_id, first.message_id);
    assert!(replay_while_pending.replayed);
    assert!(!replay_while_pending.completed);

    let operation_id = format!(
        "public-turn:v1:{USER_ID}:{}:{CLIENT_KEY}",
        conversation.conversation_id
    );
    assert_eq!(
        repo.get_delivery_receipt(USER_ID, &conversation.conversation_id, &operation_id)
            .await
            .unwrap()
            .expect("accepted public receipt")
            .status,
        "accepted"
    );
    assert_eq!(
        svc.public_turn_delivery_state(
            USER_ID,
            &conversation.conversation_id,
            CLIENT_KEY,
        )
        .await
        .unwrap(),
        PublicTurnDeliveryState::Accepted {
            message_id: first.message_id.clone(),
        }
    );
    let restarted_svc = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let accepted_after_restart = restarted_svc
        .send_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            CLIENT_KEY,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_eq!(accepted_after_restart.message_id, first.message_id);
    assert!(accepted_after_restart.replayed);
    assert!(!accepted_after_restart.completed);

    wait_for_turn_released(&svc, &conversation.conversation_id).await;
    assert_eq!(
        slow_registry.build_calls(),
        1,
        "a pending replay must share the first receipt owner and model turn"
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    assert_eq!(
        repo.get_delivery_receipt(USER_ID, &conversation.conversation_id, &operation_id)
            .await
            .unwrap()
            .expect("atomically finalized public receipt")
            .status,
        "completed"
    );
    assert!(matches!(
        svc.public_turn_delivery_state(
            USER_ID,
            &conversation.conversation_id,
            CLIENT_KEY,
        )
        .await
        .unwrap(),
        PublicTurnDeliveryState::Completed(delivery)
            if delivery.message_id == first.message_id
                && delivery.replayed
                && delivery.completed
    ));
    assert!(
        !svc
            .runtime_summary_for(&conversation.conversation_id)
            .await
            .is_processing
    );

    // Simulate a fresh service process reading the durable completed receipt.
    // Hold its first asynchronous preflight open so the runtime summary can be
    // inspected while the replay is in flight, not only before/after it.
    let replay_boundary = Arc::new(BlockingNoExecutionBoundary::default());
    let replay_svc = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        replay_boundary.clone(),
    );
    broadcaster.take_events();
    let replay_task = {
        let replay_svc = replay_svc.clone();
        let runtime_registry = runtime_registry.clone();
        let conversation_id = conversation.conversation_id.clone();
        tokio::spawn(async move {
            replay_svc
                .send_message_with_idempotency_key(
                    USER_ID,
                    &conversation_id,
                    CLIENT_KEY,
                    make_send_req(),
                    &runtime_registry,
                )
                .await
        })
    };
    tokio::time::timeout(
        Duration::from_secs(1),
        replay_boundary.entered_retention_check.notified(),
    )
    .await
    .expect("completed replay reached its asynchronous preparation fence");
    let in_flight_summary = replay_svc
        .runtime_summary_for(&conversation.conversation_id)
        .await;
    assert_eq!(
        in_flight_summary.state,
        nomifun_api_types::ConversationRuntimeStateKind::Idle
    );
    assert!(!in_flight_summary.is_processing);
    assert_eq!(slow_registry.build_calls(), 1);

    replay_boundary.release_retention_check.notify_one();
    let completed_replay = replay_task
        .await
        .expect("completed replay task")
        .unwrap();
    assert_eq!(completed_replay.message_id, first.message_id);
    assert!(completed_replay.replayed);
    assert!(completed_replay.completed);
    // This fixture emits Finish without any assistant text. The durable
    // terminal contract therefore reports a completed-but-empty result rather
    // than inventing a successful payload.
    assert_eq!(completed_replay.result_ok, Some(false));
    assert_eq!(completed_replay.result_text, None);
    assert_eq!(completed_replay.result_error, None);
    assert_eq!(
        slow_registry.build_calls(),
        1,
        "a completed public receipt is an absorbing boundary: no runtime rebuild"
    );
    let replay_summary = replay_svc
        .runtime_summary_for(&conversation.conversation_id)
        .await;
    assert_eq!(
        replay_summary.state,
        nomifun_api_types::ConversationRuntimeStateKind::Idle
    );
    assert!(!replay_summary.is_processing);
    assert!(
        broadcaster.take_events().is_empty(),
        "completed replay must not emit user/turn/stream lifecycle events"
    );

    let conflicting_request: SendMessageRequest =
        serde_json::from_value(json!({ "content": "different payload" })).unwrap();
    replay_boundary.release_retention_check.notify_one();
    assert!(matches!(
        replay_svc.send_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            CLIENT_KEY,
            conflicting_request,
            &runtime_registry,
        )
        .await
        .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert_eq!(slow_registry.build_calls(), 1);
    assert!(
        !replay_svc
            .runtime_summary_for(&conversation.conversation_id)
            .await
            .is_processing
    );

    // Client keys are scoped per Conversation. Reusing the same opaque key on
    // a different Conversation must not collide with the receipt above's
    // globally unique operation_id.
    let second = svc.create(USER_ID, request()).await.unwrap();
    let second_message = svc
        .send_message_with_idempotency_key(
            USER_ID,
            &second.conversation_id,
            CLIENT_KEY,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_ne!(second_message.message_id, first.message_id);
    assert!(!second_message.replayed);
    wait_for_turn_released(&svc, &second.conversation_id).await;
    assert_eq!(slow_registry.build_calls(), 2);
}

#[tokio::test]
async fn delayed_initial_delivery_cannot_cross_a_completed_turn_generation() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    const WINNER_KEY: &str = "explicit-turn-wins-before-initial";
    const STALE_INITIAL_KEY: &str = "stale-initial-auto-delivery";

    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(1)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conversation = service
        .create(
            USER_ID,
            serde_json::from_value(json!({
                "type": "acp",
                "extra": {
                    "agent_id": TEST_ACP_AGENT_ID,
                    "workspace": isolated_test_workspace("initial-delivery-toctou")
                }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

    // This is the stale UI preflight: creation generation, Pending, empty
    // transcript. It grants no execution authority.
    let observed = service
        .get(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(observed.status, ConversationStatus::Pending);
    assert_eq!(
        repo.get_messages(
            &conversation.conversation_id,
            1,
            20,
            SortOrder::Asc,
        )
        .await
        .unwrap()
        .total,
        0
    );

    service
        .send_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            WINNER_KEY,
            serde_json::from_value(json!({"content": "explicit winner"}))
                .unwrap(),
            &runtime_registry,
        )
        .await
        .unwrap();
    wait_for_turn_released(&service, &conversation.conversation_id).await;
    assert_eq!(registry.build_calls(), 1);
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    broadcaster.take_events();

    let error = service
        .send_initial_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            STALE_INITIAL_KEY,
            serde_json::from_value(json!({"content": "must never execute"}))
                .unwrap(),
            &runtime_registry,
        )
        .await
        .expect_err("stale initial auto-delivery must fail closed");
    assert!(matches!(error, AppError::Conflict(_)));
    assert_eq!(
        registry.build_calls(),
        1,
        "initial-only rejection must occur before runtime/model construction"
    );
    let stale_operation = ConversationService::public_turn_operation_id(
        USER_ID,
        &conversation.conversation_id,
        STALE_INITIAL_KEY,
    );
    assert!(
        repo.get_delivery_receipt(
            USER_ID,
            &conversation.conversation_id,
            &stale_operation,
        )
        .await
        .unwrap()
        .is_none(),
        "a rejected initial claim must roll back its candidate receipt"
    );
    let user_messages = repo
        .get_messages(
            &conversation.conversation_id,
            1,
            20,
            SortOrder::Asc,
        )
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|message| message.position.as_deref() == Some("right"))
        .collect::<Vec<_>>();
    assert_eq!(user_messages.len(), 1);
    assert!(user_messages[0].content.contains("explicit winner"));
    assert!(
        broadcaster.take_events().is_empty(),
        "rejected initial delivery must emit no model or lifecycle projection"
    );
}

#[tokio::test]
async fn fresh_initial_delivery_is_exactly_once_and_replayable() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    const INITIAL_KEY: &str = "fresh-initial-auto-delivery";

    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(1)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        Arc::new(MockBroadcaster::new()),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo,
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conversation = service
        .create(
            USER_ID,
            serde_json::from_value(json!({
                "type": "acp",
                "extra": {
                    "agent_id": TEST_ACP_AGENT_ID,
                    "workspace": isolated_test_workspace("fresh-initial-delivery")
                }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let request = || -> SendMessageRequest {
        serde_json::from_value(json!({"content": "run once"})).unwrap()
    };

    let first = service
        .send_initial_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            INITIAL_KEY,
            request(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(!first.replayed);
    wait_for_turn_released(&service, &conversation.conversation_id).await;
    let replay = service
        .send_initial_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            INITIAL_KEY,
            request(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert!(replay.completed);
    assert_eq!(replay.message_id, first.message_id);
    assert_eq!(registry.build_calls(), 1);
}

#[tokio::test]
async fn successor_pending_generation_cannot_impersonate_creation_for_initial_delivery() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    const STALE_INITIAL_KEY: &str = "reset-generation-initial-auto-delivery";

    let database = init_database_memory().await.unwrap();
    let pool = database.pool().clone();
    let repo = Arc::new(SqliteConversationRepository::new(pool.clone()));
    let registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(1)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        Arc::new(MockBroadcaster::new()),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conversation = service
        .create(
            USER_ID,
            serde_json::from_value(json!({
                "type": "acp",
                "extra": {
                    "agent_id": TEST_ACP_AGENT_ID,
                    "workspace": isolated_test_workspace("successor-initial-delivery")
                }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

    // Model the externally indistinguishable state left by a reset: Pending
    // and empty, but no longer the creation generation. We deliberately leave
    // no turn receipt or message so admission_epoch is the only rejecting
    // predicate exercised by this regression.
    nomifun_db::sqlx::query(
        "UPDATE conversations SET admission_epoch = 2 WHERE conversation_id = ? AND user_id = ?",
    )
    .bind(&conversation.conversation_id)
    .bind(USER_ID)
    .execute(&pool)
    .await
    .unwrap();
    let successor = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(successor.status.as_deref(), Some("pending"));
    let successor_epoch: i64 = nomifun_db::sqlx::query_scalar(
        "SELECT admission_epoch FROM conversations WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(successor_epoch, 2);
    assert_eq!(
        repo.get_messages(
            &conversation.conversation_id,
            1,
            20,
            SortOrder::Asc,
        )
        .await
        .unwrap()
        .total,
        0
    );

    let error = service
        .send_initial_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            STALE_INITIAL_KEY,
            serde_json::from_value(json!({"content": "must never execute"}))
                .unwrap(),
            &runtime_registry,
        )
        .await
        .expect_err("a successor generation cannot regain initial-only authority");
    assert!(matches!(error, AppError::Conflict(_)));
    assert_eq!(
        registry.build_calls(),
        0,
        "generation rejection must occur before runtime/model construction"
    );
    let stale_operation = ConversationService::public_turn_operation_id(
        USER_ID,
        &conversation.conversation_id,
        STALE_INITIAL_KEY,
    );
    assert!(
        repo.get_delivery_receipt(
            USER_ID,
            &conversation.conversation_id,
            &stale_operation,
        )
        .await
        .unwrap()
        .is_none(),
        "generation rejection must roll back the candidate receipt"
    );
}

#[tokio::test]
async fn completed_replay_repairs_finished_row_that_still_carries_its_active_operation() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    const CLIENT_KEY: &str = "0190f5fe-7c00-7a00-8000-000000000778";

    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(25)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let make_service = || {
        ConversationService::new(
            Arc::<str>::from(USER_ID),
            std::env::temp_dir(),
            broadcaster.clone(),
            Arc::new(FixedSkillResolver { names: vec![] }),
            runtime_registry.clone(),
            repo.clone(),
            Arc::new(StubAgentMetadataRepo),
            Arc::new(StubAcpSessionRepo::default()),
            Arc::new(crate::NoExecutionConversationBoundary),
        )
    };
    let service = make_service();
    let workspace = isolated_test_workspace("finished-active-partial-replay");
    let request: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": workspace }
    }))
    .unwrap();
    let conversation = service.create(USER_ID, request).await.unwrap();
    let first = service
        .send_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            CLIENT_KEY,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    wait_for_turn_released(&service, &conversation.conversation_id).await;
    let operation_id = format!(
        "public-turn:v1:{USER_ID}:{}:{CLIENT_KEY}",
        conversation.conversation_id
    );
    let terminal_epoch = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap()
        .epoch;

    // Reproduce the historical partial commit exactly: the durable result and
    // terminal status landed, but the aggregate still names operation A.
    nomifun_db::sqlx::query(
        "UPDATE conversations SET active_turn_operation_id = ? \
         WHERE conversation_id = ? AND user_id = ? AND status = 'finished'",
    )
    .bind(&operation_id)
    .bind(&conversation.conversation_id)
    .bind(USER_ID)
    .execute(database.pool())
    .await
    .unwrap();

    let restarted_service = make_service();
    let replay = restarted_service
        .send_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            CLIENT_KEY,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_eq!(replay.message_id, first.message_id);
    assert!(replay.replayed);
    assert!(replay.completed);
    assert_eq!(
        slow_registry.build_calls(),
        1,
        "repairing a completed partial commit must not execute the model again"
    );

    let repaired = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(repaired.active_operation_id, None);
    assert_eq!(repaired.epoch, terminal_epoch + 1);
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
}

struct ClaimCommitReturnBarrierRepository {
    inner: Arc<SqliteConversationRepository>,
    committed_before_return: Arc<Notify>,
    explicit_claim_error: bool,
    abandon_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl IConversationRepository for ClaimCommitReturnBarrierRepository {
    async fn get(&self, conversation_id: &str) -> Result<Option<ConversationRow>, DbError> {
        self.inner.get(conversation_id).await
    }

    async fn create(&self, row: &ConversationRow) -> Result<String, DbError> {
        self.inner.create(row).await
    }

    async fn update(
        &self,
        conversation_id: &str,
        updates: &ConversationRowUpdate,
    ) -> Result<(), DbError> {
        self.inner.update(conversation_id, updates).await
    }

    async fn delete(&self, conversation_id: &str) -> Result<(), DbError> {
        self.inner.delete(conversation_id).await
    }

    async fn list_paginated(
        &self,
        user_id: &str,
        filters: &ConversationFilters,
    ) -> Result<PaginatedResult<ConversationRow>, DbError> {
        self.inner.list_paginated(user_id, filters).await
    }

    async fn find_by_source_and_chat(
        &self,
        user_id: &str,
        source: &str,
        chat_id: &str,
        agent_type: &str,
    ) -> Result<Option<ConversationRow>, DbError> {
        self.inner
            .find_by_source_and_chat(user_id, source, chat_id, agent_type)
            .await
    }

    async fn list_by_cron_job(
        &self,
        user_id: &str,
        cron_job_id: &str,
    ) -> Result<Vec<ConversationRow>, DbError> {
        self.inner.list_by_cron_job(user_id, cron_job_id).await
    }

    async fn list_associated(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<ConversationRow>, DbError> {
        self.inner.list_associated(user_id, conversation_id).await
    }

    async fn get_messages(
        &self,
        conversation_id: &str,
        page: u32,
        page_size: u32,
        order: SortOrder,
    ) -> Result<PaginatedResult<MessageRow>, DbError> {
        self.inner
            .get_messages(conversation_id, page, page_size, order)
            .await
    }

    async fn insert_message(&self, message: &MessageRow) -> Result<(), DbError> {
        self.inner.insert_message(message).await
    }

    async fn update_message(
        &self,
        message_id: &str,
        updates: &MessageRowUpdate,
    ) -> Result<(), DbError> {
        self.inner.update_message(message_id, updates).await
    }

    async fn delete_messages_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<(), DbError> {
        self.inner
            .delete_messages_by_conversation(conversation_id)
            .await
    }

    async fn get_message_by_msg_id(
        &self,
        conversation_id: &str,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, DbError> {
        self.inner
            .get_message_by_msg_id(conversation_id, msg_id, msg_type)
            .await
    }

    async fn search_messages(
        &self,
        user_id: &str,
        keyword: &str,
        page: u32,
        page_size: u32,
    ) -> Result<PaginatedResult<MessageSearchRow>, DbError> {
        self.inner
            .search_messages(user_id, keyword, page, page_size)
            .await
    }

    async fn get_turn_admission_state(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<nomifun_db::ConversationTurnAdmissionState, DbError> {
        self.inner
            .get_turn_admission_state(user_id, conversation_id)
            .await
    }

    async fn finalize_exact_turn_operation(
        &self,
        user_id: &str,
        conversation_id: &str,
        completion: &TurnReceiptCompletion,
        completed_at: i64,
    ) -> Result<TurnLifecycleTransition, DbError> {
        self.inner
            .finalize_exact_turn_operation(
                user_id,
                conversation_id,
                completion,
                completed_at,
            )
            .await
    }

    async fn claim_turn_delivery_receipt_and_admit_with_candidate(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        candidate_message_id: &str,
        request_payload: &str,
        expected_admission_epoch: i64,
        now: i64,
    ) -> Result<nomifun_db::ConversationDeliveryReceiptClaim, DbError> {
        if self.explicit_claim_error {
            return Err(DbError::Conflict(
                "injected claim transaction rollback".to_owned(),
            ));
        }
        let committed = self
            .inner
            .claim_turn_delivery_receipt_and_admit_with_candidate(
                user_id,
                conversation_id,
                operation_id,
                candidate_message_id,
                request_payload,
                expected_admission_epoch,
                now,
            )
            .await?;
        self.committed_before_return.notify_one();
        std::future::pending::<()>().await;
        #[allow(unreachable_code)]
        Ok(committed)
    }

    async fn abandon_exact_turn_admission(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        candidate_message_id: &str,
        request_payload: &str,
        expected_admitted_epoch: i64,
        reason: &str,
        completed_at: i64,
    ) -> Result<TurnLifecycleTransition, DbError> {
        self.abandon_calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .abandon_exact_turn_admission(
                user_id,
                conversation_id,
                operation_id,
                candidate_message_id,
                request_payload,
                expected_admitted_epoch,
                reason,
                completed_at,
            )
            .await
    }

    async fn get_delivery_receipt(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
    ) -> Result<Option<ConversationDeliveryReceiptRow>, DbError> {
        self.inner
            .get_delivery_receipt(user_id, conversation_id, operation_id)
            .await
    }

    async fn has_accepted_delivery_receipt_operation_prefix(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id_prefix: &str,
    ) -> Result<bool, DbError> {
        self.inner
            .has_accepted_delivery_receipt_operation_prefix(
                user_id,
                conversation_id,
                operation_id_prefix,
            )
            .await
    }

    async fn recover_unadmitted_edit_resubmit_reservation(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        candidate_message_id: &str,
        request_payload: &str,
        expected_admission_epoch: i64,
        reason: &str,
        now: i64,
    ) -> Result<TurnLifecycleTransition, DbError> {
        self.inner
            .recover_unadmitted_edit_resubmit_reservation(
                user_id,
                conversation_id,
                operation_id,
                candidate_message_id,
                request_payload,
                expected_admission_epoch,
                reason,
                now,
            )
            .await
    }
}

async fn public_admission_cutpoint_fixture(
    label: &str,
) -> (
    ConversationService,
    Arc<SqliteConversationRepository>,
    Arc<SlowAgentRuntimeRegistry>,
    Arc<dyn AgentRuntimeRegistry>,
    String,
) {
    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_secs(2)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(SQLITE_TEST_OWNER),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conversation = service
        .create(
            SQLITE_TEST_OWNER,
            serde_json::from_value(json!({
                "type": "acp",
                "extra": {
                    "agent_id": TEST_ACP_AGENT_ID,
                    "workspace": isolated_test_workspace(label)
                }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    (
        service,
        repo,
        slow_registry,
        runtime_registry,
        conversation.conversation_id,
    )
}

struct AgentExecutionAdmissionCutpointFixture {
    service: ConversationService,
    repo: Arc<SqliteConversationRepository>,
    slow_registry: Arc<SlowAgentRuntimeRegistry>,
    runtime_registry: Arc<dyn AgentRuntimeRegistry>,
    boundary: Arc<ControlledExecutionBoundary>,
    conversation_id: String,
    operation_id: String,
    authority: AgentExecutionTurnAuthority,
}

async fn agent_execution_admission_cutpoint_fixture(
    label: &str,
    behavior: ControlledExecutionClaimBehavior,
) -> AgentExecutionAdmissionCutpointFixture {
    let database = init_database_memory().await.unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO providers (\
            provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
            capabilities, created_at, updated_at\
         ) VALUES (?1, 'openai', 'test', 'https://example.invalid', \
                   'encrypted', '[\"model_test\"]', 1, '[]', 1, 1)",
    )
    .bind(PROVIDER_ID_1)
    .execute(database.pool())
    .await
    .unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let execution_repo = Arc::new(SqliteAgentExecutionRepository::new(database.pool().clone()));
    let repository_boundary: Arc<dyn crate::ExecutionConversationBoundary> =
        Arc::new(RepositoryExecutionConversationBoundary::new(
            execution_repo.clone(),
        ));
    let boundary = Arc::new(ControlledExecutionBoundary::new(
        repository_boundary,
        behavior,
    ));
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::ZERO));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(SQLITE_TEST_OWNER),
        std::env::temp_dir(),
        Arc::new(MockBroadcaster::new()),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        boundary.clone(),
    );
    let conversation = service
        .create(
            SQLITE_TEST_OWNER,
            serde_json::from_value(json!({
                "type": "nomi",
                "name": format!("Agent admission {label}"),
                "model": {
                    "provider_id": PROVIDER_ID_1,
                    "model": "model_test",
                    "use_model": "model_test"
                },
                "extra": {
                    "agent_id": TEST_NOMI_AGENT_ID,
                    "workspace": isolated_test_workspace(label)
                }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

    let participant_id = ConversationId::new().into_string();
    let step_id = ConversationId::new().into_string();
    let event = |event_type: AgentExecutionEventKind| NewAgentExecutionEvent {
        event_type,
        step_id: None,
        attempt_id: None,
        actor: nomifun_common::AgentExecutionActor::system(),
        payload: "{}".to_owned(),
    };
    let execution = execution_repo
        .create_execution_with_participants(
            SQLITE_TEST_OWNER,
            &CreateAgentExecutionParams {
                goal: "exercise exact admission custodian".to_owned(),
                status: AgentExecutionStatus::Planning,
                plan_gate: PlanGate::Automatic,
                adaptation_policy: AdaptationPolicy::Fixed,
                decision_policy: DecisionPolicy::Automatic,
                delegation_policy: DelegationPolicy::Automatic,
                max_parallel: 1,
                work_dir: None,
                lead_conversation_id: None,
                initial_plan_input: r#"{"mode":"automatic"}"#.to_owned(),
            },
            &[NewAgentExecutionParticipant {
                participant_id: participant_id.clone(),
                source_agent_id: TEST_NOMI_AGENT_ID.to_owned(),
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                provider_id: Some(PROVIDER_ID_1.to_owned()),
                model: Some("model_test".to_owned()),
                role: Some("builder".to_owned()),
                capability: Some(r#"{"coding":true}"#.to_owned()),
                constraints: Some("{}".to_owned()),
                description: None,
                system_prompt: None,
                enabled_skills: "[]".to_owned(),
                disabled_builtin_skills: "[]".to_owned(),
                sort_order: 0,
            }],
            &event(AgentExecutionEventKind::Created),
        )
        .await
        .unwrap();
    let planned = execution_repo
        .reconcile_plan(
            SQLITE_TEST_OWNER,
            &execution.execution_id,
            execution.version,
            &ReconcileAgentExecutionPlanParams {
                goal: None,
                plan_gate: None,
                adaptation_policy: None,
                decision_policy: None,
                delegation_policy: None,
                keep_step_ids: Vec::new(),
                new_participants: Vec::new(),
                retire_participant_ids: Vec::new(),
                new_steps: vec![NewAgentExecutionStep {
                    step_id: step_id.clone(),
                    title: "attempt".to_owned(),
                    spec: "execute attempt".to_owned(),
                    role: Some("builder".to_owned()),
                    tool_policy: AgentToolPolicy::Full,
                    kind: ExecutionStepKind::Agent,
                    agent_mode: Some(AgentStepMode::Normal),
                    profile: Some("{}".to_owned()),
                    fanout_group: None,
                    control_policy: None,
                    status: ExecutionStepStatus::Pending,
                    assigned_participant_id: Some(participant_id.clone()),
                    assignment_score: Some(1.0),
                    assignment_rationale: Some("test".to_owned()),
                    assignment_source: Some(ParticipantAssignmentSource::Planner),
                    assignment_locked: false,
                    failure_policy: StepFailurePolicy::FailExecution,
                    preset_prompt: None,
                    graph_x: None,
                    graph_y: None,
                }],
                new_dependencies: Vec::new(),
                execution_status: AgentExecutionStatus::Running,
            },
            &event(AgentExecutionEventKind::PlanChanged),
        )
        .await
        .unwrap();
    let lease =
        AgentExecutionLeaseToken::new(format!("service-test:{label}:execution-generation"));
    execution_repo
        .try_acquire_lease(
            &execution.execution_id,
            planned.execution.version,
            lease.owner(),
            now_ms() + 60_000,
        )
        .await
        .unwrap()
        .expect("test scheduler lease");
    let queued = execution_repo
        .create_attempt(
            SQLITE_TEST_OWNER,
            &execution.execution_id,
            &step_id,
            planned.steps[0].version,
            Some(&lease),
            &CreateAgentExecutionAttemptParams {
                participant_id: Some(participant_id),
                start_immediately: false,
                trigger_reason: "initial".to_owned(),
                effective_config: "{}".to_owned(),
                retry_after: None,
                runtime_state: None,
            },
            &event(AgentExecutionEventKind::AttemptChanged),
        )
        .await
        .unwrap();
    let queued_attempt = queued
        .current_attempt
        .as_ref()
        .expect("queued attempt detail");
    let attempt_id = queued_attempt.attempt.attempt_id.clone();
    let started = execution_repo
        .start_attempt(
            SQLITE_TEST_OWNER,
            &execution.execution_id,
            &step_id,
            queued.step.version,
            &attempt_id,
            queued_attempt.attempt.version,
            &conversation.conversation_id,
            Some(&lease),
            &event(AgentExecutionEventKind::AttemptChanged),
        )
        .await
        .unwrap();
    let started_attempt = started
        .current_attempt
        .as_ref()
        .expect("started attempt detail");
    let authority = AgentExecutionTurnAuthority {
        execution_id: execution.execution_id,
        step_id,
        attempt_id: attempt_id.clone(),
        expected_step_version: started.step.version,
        expected_attempt_version: started_attempt.attempt.version,
        lease_owner: lease.owner().to_owned(),
    };

    AgentExecutionAdmissionCutpointFixture {
        service,
        repo,
        slow_registry,
        runtime_registry,
        boundary,
        conversation_id: conversation.conversation_id,
        operation_id: format!("{attempt_id}:initial-turn"),
        authority,
    }
}

async fn background_reconciliation_fixture(
    label: &str,
    boundary: Arc<dyn crate::ExecutionConversationBoundary>,
) -> (
    ConversationService,
    Arc<SqliteConversationRepository>,
    Arc<SlowAgentRuntimeRegistry>,
    Arc<dyn AgentRuntimeRegistry>,
    nomifun_db::Database,
    String,
) {
    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::ZERO));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(SQLITE_TEST_OWNER),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        boundary,
    );
    let conversation = service
        .create(
            SQLITE_TEST_OWNER,
            serde_json::from_value(json!({
                "type": "acp",
                "extra": {
                    "agent_id": TEST_ACP_AGENT_ID,
                    "workspace": isolated_test_workspace(label)
                }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    (
        service,
        repo,
        slow_registry,
        runtime_registry,
        database,
        conversation.conversation_id,
    )
}

async fn claim_background_turn_for_test(
    repo: &SqliteConversationRepository,
    conversation_id: &str,
    key: &str,
) -> (String, String, String, i64) {
    let operation_id = ConversationService::public_turn_operation_id(
        SQLITE_TEST_OWNER,
        conversation_id,
        key,
    );
    let candidate_message_id = MessageId::new().into_string();
    let request_payload = json!({"background_test": key}).to_string();
    let initial = repo
        .get_turn_admission_state(SQLITE_TEST_OWNER, conversation_id)
        .await
        .unwrap();
    let claim = repo
        .claim_turn_delivery_receipt_and_admit_with_candidate(
            SQLITE_TEST_OWNER,
            conversation_id,
            &operation_id,
            &candidate_message_id,
            &request_payload,
            initial.epoch,
            now_ms(),
        )
        .await
        .unwrap();
    assert!(claim.claimed_new);
    (
        operation_id,
        candidate_message_id,
        request_payload,
        initial.epoch + 1,
    )
}

async fn finish_exact_sqlite_turn_for_test(
    repo: &SqliteConversationRepository,
    conversation_id: &str,
    key: &str,
) {
    let (operation_id, _, request_payload, _) =
        claim_background_turn_for_test(repo, conversation_id, key).await;
    assert_eq!(
        repo.finalize_exact_turn_operation(
            SQLITE_TEST_OWNER,
            conversation_id,
            &TurnReceiptCompletion {
                operation_id,
                kind: "turn".to_owned(),
                request_payload,
                result_ok: false,
                result_text: None,
                result_error: Some("fixture terminal proof".to_owned()),
            },
            now_ms(),
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );
}

#[derive(Debug, Clone, Copy)]
enum WritebackRetryLifecycleMutation {
    Stop,
    Clear,
    Reset,
    Delete,
}

impl WritebackRetryLifecycleMutation {
    fn label(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Clear => "clear",
            Self::Reset => "reset",
            Self::Delete => "delete",
        }
    }
}

#[tokio::test]
async fn writeback_retry_is_linearized_against_stop_clear_reset_and_delete() {
    const USER_ID: &str = SQLITE_TEST_OWNER;

    struct LifecycleConversationFixture {
        conversation_id: String,
    }

    for mutation in [
        WritebackRetryLifecycleMutation::Stop,
        WritebackRetryLifecycleMutation::Clear,
        WritebackRetryLifecycleMutation::Reset,
        WritebackRetryLifecycleMutation::Delete,
    ] {
        let label = mutation.label();
        let database = init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
        let broadcaster = Arc::new(MockBroadcaster::new());
        let registry = Arc::new(MockAgentRuntimeRegistry::new());
        let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
        let service = ConversationService::new(
            Arc::<str>::from(USER_ID),
            std::env::temp_dir(),
            broadcaster.clone(),
            Arc::new(FixedSkillResolver { names: vec![] }),
            runtime_registry.clone(),
            repo.clone(),
            Arc::new(StubAgentMetadataRepo),
            Arc::new(StubAcpSessionRepo::default()),
            Arc::new(crate::NoExecutionConversationBoundary),
        );
        let conversation = if matches!(mutation, WritebackRetryLifecycleMutation::Delete) {
            // Delete correctly rejects Conversations retained by delivery
            // history. Seed the orthogonal legal case directly: a terminal
            // transcript with no receipt/Execution retention, so this race
            // exercises deletion quiescence rather than retention policy.
            let conversation_id = ConversationId::new().into_string();
            let created_at = now_ms();
            nomifun_db::sqlx::query(
                "INSERT INTO conversations (\
                    conversation_id, user_id, name, type, status, extra, created_at, updated_at\
                 ) VALUES (?1, ?2, 'writeback retry delete fixture', 'acp', 'finished', '{}', ?3, ?3)",
            )
            .bind(&conversation_id)
            .bind(USER_ID)
            .bind(created_at)
            .execute(database.pool())
            .await
            .unwrap();
            LifecycleConversationFixture { conversation_id }
        } else {
            LifecycleConversationFixture {
                conversation_id: service
                    .create(USER_ID, make_create_req())
                    .await
                    .unwrap()
                    .conversation_id,
            }
        };
        let source_message_id = MessageId::new().into_string();
        let assistant_message_id = MessageId::new().into_string();
        let expected_attempt_id = format!("{label}-retryable-attempt");
        let created_at = now_ms();
        repo.insert_message(&MessageRow {
            id: 0,
            message_id: source_message_id.clone(),
            conversation_id: conversation.conversation_id.clone(),
            msg_id: Some(source_message_id.clone()),
            r#type: "text".to_owned(),
            content: json!({ "content": "durable source prompt" }).to_string(),
            position: Some("right".to_owned()),
            status: Some("finish".to_owned()),
            hidden: false,
            created_at,
        })
        .await
        .unwrap();
        repo.insert_message(&MessageRow {
            id: 0,
            message_id: assistant_message_id.clone(),
            conversation_id: conversation.conversation_id.clone(),
            msg_id: Some(assistant_message_id.clone()),
            r#type: "text".to_owned(),
            content: json!({
                "content": "durable assistant answer",
                "knowledge_writeback": {
                    "status": "failed",
                    "retryable": true,
                    "attempt_id": &expected_attempt_id,
                    "attempt_generation": 1,
                    "source_message_id": &source_message_id,
                    "scope": &conversation.conversation_id,
                    "assistant_text": "durable assistant answer",
                    "finished_at": created_at + 1,
                    "failures": [{
                        "kb_id": null,
                        "rel_path": null,
                        "error": "retry fixture"
                    }],
                    "written": []
                }
            })
            .to_string(),
            position: Some("left".to_owned()),
            status: Some("finish".to_owned()),
            hidden: false,
            created_at: created_at + 1,
        })
        .await
        .unwrap();
        if !matches!(mutation, WritebackRetryLifecycleMutation::Delete) {
            finish_exact_sqlite_turn_for_test(
                repo.as_ref(),
                &conversation.conversation_id,
                &format!("{label}-retry-lifecycle-fixture"),
            )
            .await;
        }
        broadcaster.take_events();

        // Pause after retry has become a tracked preparation owner, but before
        // it acquires the preparation gate or registers a write-back worker.
        // This is the exact gap where an early lifecycle drain used to be able
        // to observe an empty worker map and let the retry register afterward.
        let (entered, release) = service.install_public_admission_cutpoint(
            crate::service::PublicAdmissionCutpoint::AfterWritebackRetryPreparationLease,
            false,
        );
        let retry_task = {
            let service = service.clone();
            let conversation_id = conversation.conversation_id.clone();
            let assistant_message_id = assistant_message_id.clone();
            let expected_attempt_id = expected_attempt_id.clone();
            tokio::spawn(async move {
                service
                    .retry_knowledge_writeback(
                        USER_ID,
                        &conversation_id,
                        &assistant_message_id,
                        &expected_attempt_id,
                    )
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap_or_else(|_| panic!("{label}: retry must reach its preparation cutpoint"));

        // Register through the production guard map before lifecycle
        // linearization. The test owner deliberately holds this guard after
        // cancellation so transcript mutation cannot pass merely because the
        // cancellation token was signalled.
        let (writeback_cancelled, writeback_release) = service
            .install_blocking_turn_writeback_run_for_test(
                &conversation.conversation_id,
                &assistant_message_id,
            );
        let stop_cutpoint = matches!(mutation, WritebackRetryLifecycleMutation::Stop).then(|| {
            service.install_public_admission_cutpoint(
                crate::service::PublicAdmissionCutpoint::AfterPublicPreparationCancelCaptured,
                false,
            )
        });
        let lifecycle_task = {
            let service = service.clone();
            let conversation_id = conversation.conversation_id.clone();
            let runtime_registry = runtime_registry.clone();
            tokio::spawn(async move {
                match mutation {
                    WritebackRetryLifecycleMutation::Stop => {
                        service
                            .cancel(USER_ID, &conversation_id, &runtime_registry)
                            .await
                    }
                    WritebackRetryLifecycleMutation::Clear => {
                        service
                            .clear_messages(USER_ID, &conversation_id, &runtime_registry)
                            .await
                    }
                    WritebackRetryLifecycleMutation::Reset => {
                        service.reset(USER_ID, &conversation_id).await
                    }
                    WritebackRetryLifecycleMutation::Delete => {
                        service.delete(USER_ID, &conversation_id).await
                    }
                }
            })
        };
        if let Some((stop_entered, _)) = stop_cutpoint.as_ref() {
            tokio::time::timeout(Duration::from_secs(2), stop_entered.notified())
                .await
                .unwrap_or_else(|_| {
                    panic!("{label}: requester stop must capture the preparation lease")
                });
        } else {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if service
                        .runtime_summary_for(&conversation.conversation_id)
                        .await
                        .is_processing
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{label}: lifecycle tombstone must become observable"));
        }
        assert!(
            !lifecycle_task.is_finished(),
            "{label}: lifecycle must retain ownership until the captured retry lease quiesces"
        );

        // A retry that starts after the tombstone linearization point must be
        // rejected synchronously and must never join the lifecycle's captured
        // lease set.
        let late_retry = tokio::time::timeout(
            Duration::from_secs(2),
            service.retry_knowledge_writeback(
                USER_ID,
                &conversation.conversation_id,
                &assistant_message_id,
                &expected_attempt_id,
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{label}: late retry must be rejected without waiting"));
        assert!(
            matches!(
                late_retry,
                Err(AppError::Conflict(_)) | Err(AppError::NotFound(_))
            ),
            "{label}: retry after tombstone must fail closed: {late_retry:?}"
        );

        release.notify_one();
        let retry_result = tokio::time::timeout(Duration::from_secs(2), retry_task)
            .await
            .unwrap_or_else(|_| panic!("{label}: captured retry must observe cancellation"))
            .unwrap_or_else(|error| panic!("{label}: retry task panicked: {error}"));
        assert!(
            matches!(retry_result, Err(AppError::Conflict(_))),
            "{label}: retry admitted before tombstone must lose through its cancelled lease: {retry_result:?}"
        );
        if let Some((_, stop_release)) = stop_cutpoint {
            stop_release.notify_one();
        }
        tokio::time::timeout(Duration::from_secs(2), writeback_cancelled.notified())
            .await
            .unwrap_or_else(|_| panic!("{label}: lifecycle must cancel the registered guard"));
        assert!(
            !lifecycle_task.is_finished(),
            "{label}: cancellation alone must not bypass write-back owner quiescence"
        );
        assert!(
            repo.get_message(&conversation.conversation_id, &assistant_message_id)
                .await
                .unwrap()
                .is_some(),
            "{label}: transcript mutation must remain behind the registered write-back guard"
        );
        writeback_release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), lifecycle_task)
            .await
            .unwrap_or_else(|_| panic!("{label}: lifecycle must finish after retry lease quiesces"))
            .unwrap_or_else(|error| panic!("{label}: lifecycle task panicked: {error}"))
            .unwrap_or_else(|error| panic!("{label}: lifecycle failed: {error}"));

        assert_eq!(
            registry.build_count(),
            0,
            "{label}: retry/lifecycle race must not build or restart an agent runtime"
        );
        assert!(
            broadcaster
                .take_events()
                .iter()
                .all(|event| event.name != "knowledge.writeback"),
            "{label}: no write-back attempt may start across the lifecycle boundary"
        );
        match mutation {
            WritebackRetryLifecycleMutation::Stop => {
                assert!(
                    repo.get_message(&conversation.conversation_id, &assistant_message_id)
                        .await
                        .unwrap()
                        .is_some(),
                    "stop keeps terminal history but must not restart or late-write it"
                );
                assert_eq!(
                    repo.get(&conversation.conversation_id)
                        .await
                        .unwrap()
                        .unwrap()
                        .status
                        .as_deref(),
                    Some("finished")
                );
            }
            WritebackRetryLifecycleMutation::Clear => {
                assert!(
                    repo.get_message(&conversation.conversation_id, &assistant_message_id)
                        .await
                        .unwrap()
                        .is_none(),
                    "clear must remove the retryable transcript only after guard quiescence"
                );
                assert_eq!(
                    repo.get(&conversation.conversation_id)
                        .await
                        .unwrap()
                        .unwrap()
                        .status
                        .as_deref(),
                    Some("finished")
                );
            }
            WritebackRetryLifecycleMutation::Reset => {
                assert!(
                    repo.get_message(&conversation.conversation_id, &assistant_message_id)
                        .await
                        .unwrap()
                        .is_none(),
                    "reset must remove the retryable transcript only after guard quiescence"
                );
                assert_eq!(
                    repo.get(&conversation.conversation_id)
                        .await
                        .unwrap()
                        .unwrap()
                        .status
                        .as_deref(),
                    Some("pending")
                );
            }
            WritebackRetryLifecycleMutation::Delete => {
                assert!(
                    repo.get_message(&conversation.conversation_id, &assistant_message_id)
                        .await
                        .unwrap()
                        .is_none(),
                    "delete must remove the retryable transcript only after guard quiescence"
                );
                assert!(
                    repo.get(&conversation.conversation_id)
                        .await
                        .unwrap()
                        .is_none()
                );
            }
        }
    }
}

#[tokio::test]
async fn background_reconcile_process_local_orphan_without_queryable_proof_is_quarantined() {
    const CLIENT_KEY: &str = "background-local-orphan";
    let (service, repo, slow_registry, runtime_registry, _database, conversation_id) =
        background_reconciliation_fixture(
            "background-local-orphan",
            Arc::new(crate::NoExecutionConversationBoundary),
        )
        .await;
    let (operation_id, _, _, _) =
        claim_background_turn_for_test(repo.as_ref(), &conversation_id, CLIENT_KEY).await;

    let disposition = service
        .reconcile_quiescent_running_turn_for_background(
            SQLITE_TEST_OWNER,
            &conversation_id,
            CLIENT_KEY,
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_eq!(
        disposition,
        BackgroundTurnReconciliationDisposition::ExternalProofRequiredFailClosed
    );
    let after_first = repo
        .get_turn_admission_state(SQLITE_TEST_OWNER, &conversation_id)
        .await
        .unwrap();
    assert_eq!(
        after_first.active_operation_id.as_deref(),
        Some(operation_id.as_str())
    );
    assert_eq!(
        repo.get(&conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );
    let receipt = repo
        .get_delivery_receipt(SQLITE_TEST_OWNER, &conversation_id, &operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, "accepted");
    assert_eq!(receipt.result_ok, None);
    assert_eq!(slow_registry.build_calls(), 0);

    let replay = service
        .reconcile_quiescent_running_turn_for_background(
            SQLITE_TEST_OWNER,
            &conversation_id,
            CLIENT_KEY,
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_eq!(
        replay,
        BackgroundTurnReconciliationDisposition::ExternalProofRequiredFailClosed
    );
    assert_eq!(
        repo.get_turn_admission_state(SQLITE_TEST_OWNER, &conversation_id)
            .await
            .unwrap()
            .epoch,
        after_first.epoch,
        "quarantine must not mutate the exact running generation"
    );
    assert_eq!(slow_registry.build_calls(), 0);
}

#[tokio::test]
async fn background_reconcile_exact_live_owner_waits_without_settling_or_building() {
    const CLIENT_KEY: &str = "background-live-exact-owner";
    let (service, repo, slow_registry, runtime_registry, _database, conversation_id) =
        background_reconciliation_fixture(
            "background-live-exact-owner",
            Arc::new(crate::NoExecutionConversationBoundary),
        )
        .await;
    let (operation_id, _, _, _) =
        claim_background_turn_for_test(repo.as_ref(), &conversation_id, CLIENT_KEY).await;
    let _live_owner = service
        .runtime_state()
        .try_acquire_turn(&conversation_id)
        .unwrap();

    let disposition = service
        .reconcile_quiescent_running_turn_for_background(
            SQLITE_TEST_OWNER,
            &conversation_id,
            CLIENT_KEY,
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_eq!(
        disposition,
        BackgroundTurnReconciliationDisposition::LiveExactOwnerWait
    );
    assert_eq!(
        repo.get_delivery_receipt(SQLITE_TEST_OWNER, &conversation_id, &operation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "accepted"
    );
    let admission = repo
        .get_turn_admission_state(SQLITE_TEST_OWNER, &conversation_id)
        .await
        .unwrap();
    assert_eq!(admission.active_operation_id.as_deref(), Some(operation_id.as_str()));
    assert_eq!(slow_registry.build_calls(), 0);
}

#[tokio::test]
async fn background_reconcile_external_backends_fail_closed_without_mutation() {
    for (index, backend) in [
        AgentType::Nomi.serde_name(),
        AgentType::Nanobot.serde_name(),
        AgentType::Remote.serde_name(),
        AgentType::OpenclawGateway.serde_name(),
    ]
    .into_iter()
    .enumerate()
    {
        let key = format!("background-external-{index}");
        let (service, repo, slow_registry, runtime_registry, database, conversation_id) =
            background_reconciliation_fixture(
                &key,
                Arc::new(crate::NoExecutionConversationBoundary),
            )
            .await;
        nomifun_db::sqlx::query(
            "UPDATE conversations SET type = ? WHERE conversation_id = ? AND user_id = ?",
        )
        .bind(backend)
        .bind(&conversation_id)
        .bind(SQLITE_TEST_OWNER)
        .execute(database.pool())
        .await
        .unwrap();
        let (operation_id, _, _, _) =
            claim_background_turn_for_test(repo.as_ref(), &conversation_id, &key).await;

        let disposition = service
            .reconcile_quiescent_running_turn_for_background(
                SQLITE_TEST_OWNER,
                &conversation_id,
                &key,
                &runtime_registry,
            )
            .await
            .unwrap();
        assert_eq!(
            disposition,
            BackgroundTurnReconciliationDisposition::ExternalProofRequiredFailClosed,
            "{backend}"
        );
        assert_eq!(
            repo.get_delivery_receipt(SQLITE_TEST_OWNER, &conversation_id, &operation_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "accepted",
            "{backend}"
        );
        assert_eq!(
            repo.get(&conversation_id)
                .await
                .unwrap()
                .unwrap()
                .status
                .as_deref(),
            Some("running"),
            "{backend}"
        );
        assert_eq!(slow_registry.build_calls(), 0, "{backend}");
    }
}

#[tokio::test]
async fn background_reconcile_stale_operation_cannot_settle_or_terminate_successor() {
    const KEY_A: &str = "background-stale-a";
    const KEY_B: &str = "background-successor-b";
    let (service, repo, slow_registry, runtime_registry, database, conversation_id) =
        background_reconciliation_fixture(
            "background-stale-successor",
            Arc::new(crate::NoExecutionConversationBoundary),
        )
        .await;
    let (operation_a, _, payload_a, _) =
        claim_background_turn_for_test(repo.as_ref(), &conversation_id, KEY_A).await;
    repo.finalize_exact_turn_operation(
        SQLITE_TEST_OWNER,
        &conversation_id,
        &TurnReceiptCompletion {
            operation_id: operation_a.clone(),
            kind: "turn".to_owned(),
            request_payload: payload_a,
            result_ok: false,
            result_text: None,
            result_error: Some("settled A".to_owned()),
        },
        now_ms(),
    )
    .await
    .unwrap();
    let (operation_b, _, _, _) =
        claim_background_turn_for_test(repo.as_ref(), &conversation_id, KEY_B).await;

    // A completed receipt is absorbing even while B is the exact successor.
    // The physical trigger must reject any attempt to manufacture the old
    // accepted observer that historically enabled duplicate recovery.
    let rollback_error = nomifun_db::sqlx::query(
        "UPDATE conversation_delivery_receipts \
         SET status = 'accepted', result_ok = NULL, result_text = NULL, \
             result_error = NULL, completed_at = NULL \
         WHERE operation_id = ?",
    )
    .bind(&operation_a)
    .execute(database.pool())
    .await
    .expect_err("completed receipt rollback must be rejected");
    assert!(
        rollback_error
            .to_string()
            .contains("absorbing and terminal outcomes are immutable")
    );

    let disposition = service
        .reconcile_quiescent_running_turn_for_background(
            SQLITE_TEST_OWNER,
            &conversation_id,
            KEY_A,
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_eq!(
        disposition,
        BackgroundTurnReconciliationDisposition::ReconciledOrTerminalReRead
    );
    let admission = repo
        .get_turn_admission_state(SQLITE_TEST_OWNER, &conversation_id)
        .await
        .unwrap();
    assert_eq!(admission.active_operation_id.as_deref(), Some(operation_b.as_str()));
    assert_eq!(
        repo.get_delivery_receipt(SQLITE_TEST_OWNER, &conversation_id, &operation_b)
            .await
            .unwrap()
            .unwrap()
            .status,
        "accepted"
    );
    assert_eq!(
        repo.get_delivery_receipt(SQLITE_TEST_OWNER, &conversation_id, &operation_a)
            .await
            .unwrap()
            .unwrap()
            .status,
        "completed"
    );
    assert_eq!(slow_registry.build_calls(), 0);
}

#[tokio::test]
async fn background_finished_active_partial_is_quarantined_and_boot_skips_retained_attempts() {
    const CLIENT_KEY: &str = "background-finished-active";
    let (service, repo, slow_registry, runtime_registry, database, conversation_id) =
        background_reconciliation_fixture(
            "background-finished-active",
            Arc::new(crate::NoExecutionConversationBoundary),
        )
        .await;
    let (operation_id, _, _, admitted_epoch) =
        claim_background_turn_for_test(repo.as_ref(), &conversation_id, CLIENT_KEY).await;
    nomifun_db::sqlx::query("DROP TRIGGER trg_conversations_running_exit_guard")
        .execute(database.pool())
        .await
        .unwrap();
    nomifun_db::sqlx::query(
        "UPDATE conversations SET status = 'finished' \
         WHERE conversation_id = ? AND user_id = ?",
    )
    .bind(&conversation_id)
    .bind(SQLITE_TEST_OWNER)
    .execute(database.pool())
    .await
    .unwrap();

    let disposition = service
        .reconcile_quiescent_running_turn_for_background(
            SQLITE_TEST_OWNER,
            &conversation_id,
            CLIENT_KEY,
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_eq!(
        disposition,
        BackgroundTurnReconciliationDisposition::ExternalProofRequiredFailClosed
    );
    let quarantined = repo
        .get_turn_admission_state(SQLITE_TEST_OWNER, &conversation_id)
        .await
        .unwrap();
    assert_eq!(
        quarantined.active_operation_id.as_deref(),
        Some(operation_id.as_str()),
        "Finished plus an accepted active operation is not terminal proof"
    );
    assert_eq!(
        quarantined.epoch, admitted_epoch,
        "quarantine must retain the exact unresolved generation"
    );
    assert_eq!(
        repo.get_delivery_receipt(SQLITE_TEST_OWNER, &conversation_id, &operation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "accepted"
    );
    assert_eq!(slow_registry.build_calls(), 0);

    let (
        initial_service,
        retained_repo,
        retained_registry,
        retained_runtime_registry,
        _retained_database,
        retained_conversation_id,
    ) = background_reconciliation_fixture(
        "boot-retained-skip",
        Arc::new(crate::NoExecutionConversationBoundary),
    )
    .await;
    claim_background_turn_for_test(
        retained_repo.as_ref(),
        &retained_conversation_id,
        "boot-retained",
    )
    .await;
    let retained_service = ConversationService::new(
        Arc::<str>::from(SQLITE_TEST_OWNER),
        std::env::temp_dir(),
        Arc::new(MockBroadcaster::new()),
        Arc::new(FixedSkillResolver { names: vec![] }),
        retained_runtime_registry.clone(),
        retained_repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(AlwaysRetainedExecutionBoundary),
    );
    drop(initial_service);
    assert_eq!(
        retained_service
            .reconcile_locally_quiescent_orphan_on_boot(
                SQLITE_TEST_OWNER,
                &retained_conversation_id,
                &retained_runtime_registry,
            )
            .await
            .unwrap(),
        QuiescentOrphanReconciliation::RetainedExecutionSkipped
    );
    assert_eq!(
        retained_repo
            .get(&retained_conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );
    assert_eq!(retained_registry.build_calls(), 0);
}

#[tokio::test]
async fn boot_reconcile_quarantines_every_current_backend_without_terminal_proof() {
    for (index, backend) in [
        AgentType::Nomi.serde_name(),
        AgentType::Acp.serde_name(),
        AgentType::Nanobot.serde_name(),
        AgentType::Remote.serde_name(),
        AgentType::OpenclawGateway.serde_name(),
    ]
    .into_iter()
    .enumerate()
    {
        let key = format!("boot-unproven-{index}");
        let (service, repo, slow_registry, runtime_registry, database, conversation_id) =
            background_reconciliation_fixture(
                &key,
                Arc::new(crate::NoExecutionConversationBoundary),
            )
            .await;
        nomifun_db::sqlx::query(
            "UPDATE conversations SET type = ? WHERE conversation_id = ? AND user_id = ?",
        )
        .bind(backend)
        .bind(&conversation_id)
        .bind(SQLITE_TEST_OWNER)
        .execute(database.pool())
        .await
        .unwrap();
        let (operation_id, _, _, admitted_epoch) =
            claim_background_turn_for_test(repo.as_ref(), &conversation_id, &key).await;

        let error = service
            .reconcile_locally_quiescent_orphan_on_boot(
                SQLITE_TEST_OWNER,
                &conversation_id,
                &runtime_registry,
            )
            .await
            .expect_err("an unproven restart orphan must stay quarantined");
        assert!(matches!(error, AppError::Conflict(_)), "{backend}: {error}");

        let row = repo.get(&conversation_id).await.unwrap().unwrap();
        assert_eq!(row.status.as_deref(), Some("running"), "{backend}");
        let admission = repo
            .get_turn_admission_state(SQLITE_TEST_OWNER, &conversation_id)
            .await
            .unwrap();
        assert_eq!(admission.epoch, admitted_epoch, "{backend}");
        assert_eq!(
            admission.active_operation_id.as_deref(),
            Some(operation_id.as_str()),
            "{backend}"
        );
        let receipt = repo
            .get_delivery_receipt(SQLITE_TEST_OWNER, &conversation_id, &operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.status, "accepted", "{backend}");
        assert_eq!(receipt.result_ok, None, "{backend}");
        assert_eq!(
            slow_registry.build_calls(),
            0,
            "{backend} quarantine must never construct or restart a runtime"
        );
    }
}

#[tokio::test]
async fn boot_reconcile_treats_exact_terminal_generation_as_noop_without_building() {
    const KEY: &str = "boot-already-terminal";
    let (service, repo, slow_registry, runtime_registry, _database, conversation_id) =
        background_reconciliation_fixture(
            KEY,
            Arc::new(crate::NoExecutionConversationBoundary),
        )
        .await;
    let (operation_id, _, request_payload, admitted_epoch) =
        claim_background_turn_for_test(repo.as_ref(), &conversation_id, KEY).await;
    let transition = repo
        .finalize_exact_turn_operation(
            SQLITE_TEST_OWNER,
            &conversation_id,
            &TurnReceiptCompletion {
                operation_id: operation_id.clone(),
                kind: "turn".to_owned(),
                request_payload,
                result_ok: true,
                result_text: Some("completed before restart".to_owned()),
                result_error: None,
            },
            now_ms(),
        )
        .await
        .unwrap();
    assert_eq!(transition, TurnLifecycleTransition::Committed);

    assert_eq!(
        service
            .reconcile_locally_quiescent_orphan_on_boot(
                SQLITE_TEST_OWNER,
                &conversation_id,
                &runtime_registry,
            )
            .await
            .unwrap(),
        QuiescentOrphanReconciliation::AlreadyTerminal
    );
    let terminal = repo.get(&conversation_id).await.unwrap().unwrap();
    assert_eq!(terminal.status.as_deref(), Some("finished"));
    let admission = repo
        .get_turn_admission_state(SQLITE_TEST_OWNER, &conversation_id)
        .await
        .unwrap();
    assert_eq!(admission.epoch, admitted_epoch + 1);
    assert!(admission.active_operation_id.is_none());
    assert_eq!(
        repo.get_delivery_receipt(SQLITE_TEST_OWNER, &conversation_id, &operation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "completed"
    );
    assert_eq!(
        slow_registry.build_calls(),
        0,
        "an absorbing terminal generation is read-only during boot"
    );
}

async fn wait_for_public_admission_terminal(
    repo: &SqliteConversationRepository,
    user_id: &str,
    conversation_id: &str,
    operation_id: &str,
) -> ConversationDeliveryReceiptRow {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let receipt = repo
                .get_delivery_receipt(user_id, conversation_id, operation_id)
                .await
                .unwrap();
            let row = repo.get(conversation_id).await.unwrap();
            let admission = repo
                .get_turn_admission_state(user_id, conversation_id)
                .await
                .unwrap();
            if let (Some(receipt), Some(row)) = (receipt, row)
                && receipt.status == "completed"
                && row.status.as_deref() == Some("finished")
                && admission.active_operation_id.is_none()
            {
                return receipt;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("abandoned public admission must reach one exact durable terminal state")
}

async fn wait_for_public_admission_process_guards_released(
    service: &ConversationService,
    user_id: &str,
    conversation_id: &str,
    operation_id: &str,
) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !service.has_durable_operation_guard(user_id, conversation_id, operation_id)
                && !service.runtime_state().has_active_turn(conversation_id)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("exact local admission and turn guards must be released after durable terminal proof");
}

async fn assert_dropped_agent_execution_at_common_cutpoint(
    stage: crate::service::PublicAdmissionCutpoint,
    label: &str,
    expected_user_messages: usize,
) {
    let fixture = agent_execution_admission_cutpoint_fixture(
        label,
        ControlledExecutionClaimBehavior::Normal,
    )
    .await;
    let (entered, _release) = fixture
        .service
        .install_public_admission_cutpoint(stage, false);
    let delivery_task = {
        let port = fixture
            .service
            .agent_execution_port(fixture.runtime_registry.clone());
        let conversation_id = fixture.conversation_id.clone();
        let operation_id = fixture.operation_id.clone();
        let authority = fixture.authority.clone();
        tokio::spawn(async move {
            port.deliver_turn(
                SQLITE_TEST_OWNER,
                &conversation_id,
                &operation_id,
                authority,
                make_send_req(),
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("Agent Execution delivery must reach the selected ownership cutpoint");
    assert_eq!(
        fixture
            .repo
            .get(&fixture.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );
    assert_eq!(
        fixture
            .repo
            .get_delivery_receipt(
                SQLITE_TEST_OWNER,
                &fixture.conversation_id,
                &fixture.operation_id,
            )
            .await
            .unwrap()
            .unwrap()
            .status,
        "accepted"
    );

    delivery_task.abort();
    assert!(delivery_task.await.unwrap_err().is_cancelled());
    let receipt = wait_for_public_admission_terminal(
        fixture.repo.as_ref(),
        SQLITE_TEST_OWNER,
        &fixture.conversation_id,
        &fixture.operation_id,
    )
    .await;
    wait_for_public_admission_process_guards_released(
        &fixture.service,
        SQLITE_TEST_OWNER,
        &fixture.conversation_id,
        &fixture.operation_id,
    )
    .await;
    assert_eq!(receipt.result_ok, Some(false));
    assert!(
        receipt
            .result_error
            .as_deref()
            .is_some_and(|error| error.contains("Agent Execution turn request was dropped"))
    );
    assert_eq!(
        fixture.boundary.abandon_calls.load(Ordering::SeqCst),
        1,
        "the exact typed custodian must be the sole pre-owner terminalizer"
    );
    let user_message_count = fixture
        .repo
        .get_messages(&fixture.conversation_id, 1, 20, SortOrder::Asc)
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|message| message.position.as_deref() == Some("right"))
        .count();
    assert_eq!(user_message_count, expected_user_messages);

    let builds_before_replay = fixture.slow_registry.build_calls();
    let replay = fixture
        .service
        .agent_execution_port(fixture.runtime_registry.clone())
        .deliver_turn(
            SQLITE_TEST_OWNER,
            &fixture.conversation_id,
            &fixture.operation_id,
            fixture.authority.clone(),
            make_send_req(),
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert!(replay.completed);
    assert_eq!(replay.message_id, receipt.message_id);
    assert_eq!(replay.result_ok, Some(false));
    assert_eq!(
        fixture.slow_registry.build_calls(),
        builds_before_replay,
        "terminal replay must not rebuild or restart an Agent runtime"
    );
    assert_eq!(
        fixture
            .repo
            .get_turn_admission_state(SQLITE_TEST_OWNER, &fixture.conversation_id)
            .await
            .unwrap()
            .active_operation_id,
        None
    );
}

#[tokio::test]
async fn agent_execution_abort_after_claim_commit_uses_exact_candidate_custodian() {
    assert_dropped_agent_execution_at_common_cutpoint(
        crate::service::PublicAdmissionCutpoint::AfterClaimCommit,
        "agent-execution-drop-after-claim",
        0,
    )
    .await;
}

#[tokio::test]
async fn agent_execution_abort_before_owner_spawn_never_restarts_terminal_receipt() {
    assert_dropped_agent_execution_at_common_cutpoint(
        crate::service::PublicAdmissionCutpoint::BeforeOwnerSpawn,
        "agent-execution-drop-before-owner",
        1,
    )
    .await;
}

#[tokio::test]
async fn agent_execution_panic_before_owner_spawn_is_terminalized_exactly_once() {
    let fixture = agent_execution_admission_cutpoint_fixture(
        "agent-execution-panic-before-owner",
        ControlledExecutionClaimBehavior::Normal,
    )
    .await;
    let (entered, _release) = fixture.service.install_public_admission_cutpoint(
        crate::service::PublicAdmissionCutpoint::BeforeOwnerSpawn,
        true,
    );
    let delivery_task = {
        let port = fixture
            .service
            .agent_execution_port(fixture.runtime_registry.clone());
        let conversation_id = fixture.conversation_id.clone();
        let operation_id = fixture.operation_id.clone();
        let authority = fixture.authority.clone();
        tokio::spawn(async move {
            port.deliver_turn(
                SQLITE_TEST_OWNER,
                &conversation_id,
                &operation_id,
                authority,
                make_send_req(),
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("Agent Execution delivery must reach the panic cutpoint");
    let panic = delivery_task
        .await
        .expect_err("the injected pre-owner cutpoint must panic");
    assert!(panic.is_panic());

    let receipt = wait_for_public_admission_terminal(
        fixture.repo.as_ref(),
        SQLITE_TEST_OWNER,
        &fixture.conversation_id,
        &fixture.operation_id,
    )
    .await;
    wait_for_public_admission_process_guards_released(
        &fixture.service,
        SQLITE_TEST_OWNER,
        &fixture.conversation_id,
        &fixture.operation_id,
    )
    .await;
    assert_eq!(receipt.result_ok, Some(false));
    assert_eq!(
        fixture.boundary.abandon_calls.load(Ordering::SeqCst),
        1
    );
    let builds_before_replay = fixture.slow_registry.build_calls();
    let replay = fixture
        .service
        .agent_execution_port(fixture.runtime_registry.clone())
        .deliver_turn(
            SQLITE_TEST_OWNER,
            &fixture.conversation_id,
            &fixture.operation_id,
            fixture.authority,
            make_send_req(),
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert!(replay.completed);
    assert_eq!(replay.message_id, receipt.message_id);
    assert_eq!(
        fixture.slow_registry.build_calls(),
        builds_before_replay
    );
}

#[tokio::test]
async fn agent_execution_commit_before_poll_ready_drop_is_reconciled_by_candidate() {
    let fixture = agent_execution_admission_cutpoint_fixture(
        "agent-execution-commit-before-ready",
        ControlledExecutionClaimBehavior::BlockCommittedReturnOnce,
    )
    .await;
    let delivery_task = {
        let port = fixture
            .service
            .agent_execution_port(fixture.runtime_registry.clone());
        let conversation_id = fixture.conversation_id.clone();
        let operation_id = fixture.operation_id.clone();
        let authority = fixture.authority.clone();
        tokio::spawn(async move {
            port.deliver_turn(
                SQLITE_TEST_OWNER,
                &conversation_id,
                &operation_id,
                authority,
                make_send_req(),
            )
            .await
        })
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.boundary.committed_before_return.notified(),
    )
    .await
    .expect("SQLite must commit before the boundary returns Poll::Ready");
    assert!(!delivery_task.is_finished());
    assert_eq!(
        fixture
            .repo
            .get_delivery_receipt(
                SQLITE_TEST_OWNER,
                &fixture.conversation_id,
                &fixture.operation_id,
            )
            .await
            .unwrap()
            .unwrap()
            .status,
        "accepted"
    );

    delivery_task.abort();
    assert!(delivery_task.await.unwrap_err().is_cancelled());
    let receipt = wait_for_public_admission_terminal(
        fixture.repo.as_ref(),
        SQLITE_TEST_OWNER,
        &fixture.conversation_id,
        &fixture.operation_id,
    )
    .await;
    wait_for_public_admission_process_guards_released(
        &fixture.service,
        SQLITE_TEST_OWNER,
        &fixture.conversation_id,
        &fixture.operation_id,
    )
    .await;
    assert_eq!(receipt.result_ok, Some(false));
    assert_eq!(
        fixture.boundary.abandon_calls.load(Ordering::SeqCst),
        1
    );
    assert_eq!(fixture.slow_registry.build_calls(), 0);

    let replay = fixture
        .service
        .agent_execution_port(fixture.runtime_registry.clone())
        .deliver_turn(
            SQLITE_TEST_OWNER,
            &fixture.conversation_id,
            &fixture.operation_id,
            fixture.authority,
            make_send_req(),
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert!(replay.completed);
    assert_eq!(replay.message_id, receipt.message_id);
    assert_eq!(fixture.slow_registry.build_calls(), 0);
}

#[tokio::test]
async fn agent_execution_explicit_claim_error_disarms_uncommitted_custodian() {
    let fixture = agent_execution_admission_cutpoint_fixture(
        "agent-execution-explicit-claim-error",
        ControlledExecutionClaimBehavior::ExplicitError,
    )
    .await;
    let error = fixture
        .service
        .agent_execution_port(fixture.runtime_registry.clone())
        .deliver_turn(
            SQLITE_TEST_OWNER,
            &fixture.conversation_id,
            &fixture.operation_id,
            fixture.authority,
            make_send_req(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AppError::Conflict(message)
            if message.contains("injected Agent Execution claim transaction rollback")
    ));
    tokio::task::yield_now().await;
    assert_eq!(
        fixture.boundary.abandon_calls.load(Ordering::SeqCst),
        0,
        "a proven rollback must disarm rather than launch ambiguity recovery"
    );
    assert!(
        fixture
            .repo
            .get_delivery_receipt(
                SQLITE_TEST_OWNER,
                &fixture.conversation_id,
                &fixture.operation_id,
            )
            .await
            .unwrap()
            .is_none()
    );
    let admission = fixture
        .repo
        .get_turn_admission_state(SQLITE_TEST_OWNER, &fixture.conversation_id)
        .await
        .unwrap();
    assert_eq!(admission.epoch, 0);
    assert!(admission.active_operation_id.is_none());
    assert_eq!(
        fixture
            .repo
            .get(&fixture.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("pending")
    );
    assert_eq!(fixture.slow_registry.build_calls(), 0);
}

#[tokio::test]
async fn dropped_while_sqlite_commit_result_is_not_yet_returned_abandons_exact_candidate() {
    const CLIENT_KEY: &str = "drop-before-claim-await-return";
    let database = init_database_memory().await.unwrap();
    let inner = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let committed_before_return = Arc::new(Notify::new());
    let abandon_calls = Arc::new(AtomicUsize::new(0));
    let repository: Arc<dyn IConversationRepository> =
        Arc::new(ClaimCommitReturnBarrierRepository {
            inner: inner.clone(),
            committed_before_return: committed_before_return.clone(),
            explicit_claim_error: false,
            abandon_calls: abandon_calls.clone(),
        });
    let broadcaster = Arc::new(MockBroadcaster::new());
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_secs(2)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(SQLITE_TEST_OWNER),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repository,
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conversation = service
        .create(
            SQLITE_TEST_OWNER,
            serde_json::from_value(json!({
                "type": "acp",
                "extra": {
                    "agent_id": TEST_ACP_AGENT_ID,
                    "workspace": isolated_test_workspace("drop-before-claim-await-return")
                }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let operation_id = ConversationService::public_turn_operation_id(
        SQLITE_TEST_OWNER,
        &conversation.conversation_id,
        CLIENT_KEY,
    );
    let send_task = {
        let service = service.clone();
        let runtime_registry = runtime_registry.clone();
        let conversation_id = conversation.conversation_id.clone();
        tokio::spawn(async move {
            service
                .send_message_with_idempotency_key(
                    SQLITE_TEST_OWNER,
                    &conversation_id,
                    CLIENT_KEY,
                    make_send_req(),
                    &runtime_registry,
                )
                .await
        })
    };

    tokio::time::timeout(
        Duration::from_secs(2),
        committed_before_return.notified(),
    )
    .await
    .expect("SQLite must commit while the repository future still withholds Poll::Ready");
    assert!(
        !send_task.is_finished(),
        "the service claim await has not received the committed result"
    );
    assert_eq!(
        inner
            .get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );
    assert_eq!(
        inner
            .get_delivery_receipt(
                SQLITE_TEST_OWNER,
                &conversation.conversation_id,
                &operation_id,
            )
            .await
            .unwrap()
            .unwrap()
            .status,
        "accepted"
    );

    send_task.abort();
    assert!(send_task.await.unwrap_err().is_cancelled());
    let receipt = wait_for_public_admission_terminal(
        inner.as_ref(),
        SQLITE_TEST_OWNER,
        &conversation.conversation_id,
        &operation_id,
    )
    .await;
    assert_eq!(receipt.result_ok, Some(false));
    assert_eq!(
        abandon_calls.load(Ordering::SeqCst),
        1,
        "an unknown commit-return drop must invoke the exact custodian"
    );
    assert_eq!(slow_registry.build_calls(), 0);

    let replay = service
        .send_message_with_idempotency_key(
            SQLITE_TEST_OWNER,
            &conversation.conversation_id,
            CLIENT_KEY,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert!(replay.completed);
    assert_eq!(replay.message_id, receipt.message_id);
    assert_eq!(slow_registry.build_calls(), 0);
}

#[tokio::test]
async fn explicit_claim_error_disarms_ambiguity_custodian_after_proven_rollback() {
    const CLIENT_KEY: &str = "explicit-claim-rollback";
    let database = init_database_memory().await.unwrap();
    let inner = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let abandon_calls = Arc::new(AtomicUsize::new(0));
    let repository: Arc<dyn IConversationRepository> =
        Arc::new(ClaimCommitReturnBarrierRepository {
            inner: inner.clone(),
            committed_before_return: Arc::new(Notify::new()),
            explicit_claim_error: true,
            abandon_calls: abandon_calls.clone(),
        });
    let broadcaster = Arc::new(MockBroadcaster::new());
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_secs(2)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(SQLITE_TEST_OWNER),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repository,
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conversation = service
        .create(
            SQLITE_TEST_OWNER,
            serde_json::from_value(json!({
                "type": "acp",
                "extra": {
                    "agent_id": TEST_ACP_AGENT_ID,
                    "workspace": isolated_test_workspace("explicit-claim-rollback")
                }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

    eprintln!("edit-failure-test: before edit");
    let error = service
        .send_message_with_idempotency_key(
            SQLITE_TEST_OWNER,
            &conversation.conversation_id,
            CLIENT_KEY,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AppError::Conflict(message)
            if message.contains("injected claim transaction rollback")
    ));
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(
        abandon_calls.load(Ordering::SeqCst),
        0,
        "an explicit repository rollback must not start ambiguous-commit recovery"
    );
    assert_eq!(
        inner
            .get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("pending")
    );
    let operation_id = ConversationService::public_turn_operation_id(
        SQLITE_TEST_OWNER,
        &conversation.conversation_id,
        CLIENT_KEY,
    );
    assert!(
        inner
            .get_delivery_receipt(
                SQLITE_TEST_OWNER,
                &conversation.conversation_id,
                &operation_id,
            )
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(slow_registry.build_calls(), 0);
}

#[tokio::test]
async fn dropped_request_after_claim_commit_abandons_exact_admission_without_warmup_restart() {
    const CLIENT_KEY: &str = "drop-after-claim-commit";
    let (service, repo, slow_registry, runtime_registry, conversation_id) =
        public_admission_cutpoint_fixture("drop-after-claim-commit").await;
    let operation_id = ConversationService::public_turn_operation_id(
        SQLITE_TEST_OWNER,
        &conversation_id,
        CLIENT_KEY,
    );
    let (entered, _release) = service.install_public_admission_cutpoint(
        crate::service::PublicAdmissionCutpoint::AfterClaimCommit,
        false,
    );

    let send_task = {
        let service = service.clone();
        let runtime_registry = runtime_registry.clone();
        let conversation_id = conversation_id.clone();
        tokio::spawn(async move {
            service
                .send_message_with_idempotency_key(
                    SQLITE_TEST_OWNER,
                    &conversation_id,
                    CLIENT_KEY,
                    make_send_req(),
                    &runtime_registry,
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("request must pause after the SQLite claim committed");
    assert_eq!(
        repo.get(&conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );
    assert_eq!(
        repo.get_delivery_receipt(SQLITE_TEST_OWNER, &conversation_id, &operation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "accepted"
    );

    let warmup_task = {
        let service = service.clone();
        let runtime_registry = runtime_registry.clone();
        let conversation_id = conversation_id.clone();
        tokio::spawn(async move {
            service
                .warmup_for_view(SQLITE_TEST_OWNER, &conversation_id, &runtime_registry)
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(
        !warmup_task.is_finished(),
        "view warmup must wait behind the admission owner's preparation gate"
    );

    send_task.abort();
    assert!(
        send_task.await.unwrap_err().is_cancelled(),
        "the injected request future must be dropped at the commit-return cutpoint"
    );
    let receipt = wait_for_public_admission_terminal(
        repo.as_ref(),
        SQLITE_TEST_OWNER,
        &conversation_id,
        &operation_id,
    )
    .await;
    wait_for_public_admission_process_guards_released(
        &service,
        SQLITE_TEST_OWNER,
        &conversation_id,
        &operation_id,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(2), warmup_task)
        .await
        .expect("warmup must resume after the exact admission owner drops")
        .expect("warmup task must not panic")
        .expect("warmup must observe the terminalized admission");

    assert_eq!(receipt.result_ok, Some(false));
    assert!(
        receipt
            .result_error
            .as_deref()
            .is_some_and(|error| error.contains("dropped before detached execution ownership"))
    );
    assert_eq!(
        slow_registry.build_calls(),
        0,
        "neither the dropped request nor navigation may construct an Agent runtime"
    );

    let replay = service
        .send_message_with_idempotency_key(
            SQLITE_TEST_OWNER,
            &conversation_id,
            CLIENT_KEY,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert!(replay.completed);
    assert_eq!(replay.message_id, receipt.message_id);
    assert_eq!(replay.result_ok, Some(false));
    assert_eq!(slow_registry.build_calls(), 0);
}

#[tokio::test]
async fn dropped_request_before_detached_owner_spawn_abandons_exact_admission_without_warmup_restart()
{
    const CLIENT_KEY: &str = "drop-before-detached-owner";
    let (service, repo, slow_registry, runtime_registry, conversation_id) =
        public_admission_cutpoint_fixture("drop-before-detached-owner").await;
    let operation_id = ConversationService::public_turn_operation_id(
        SQLITE_TEST_OWNER,
        &conversation_id,
        CLIENT_KEY,
    );
    let (entered, _release) = service.install_public_admission_cutpoint(
        crate::service::PublicAdmissionCutpoint::BeforeOwnerSpawn,
        false,
    );

    let send_task = {
        let service = service.clone();
        let runtime_registry = runtime_registry.clone();
        let conversation_id = conversation_id.clone();
        tokio::spawn(async move {
            service
                .send_message_with_idempotency_key(
                    SQLITE_TEST_OWNER,
                    &conversation_id,
                    CLIENT_KEY,
                    make_send_req(),
                    &runtime_registry,
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("request must pause after acquiring the local turn");
    assert!(
        service.runtime_state().has_active_turn(&conversation_id),
        "the cutpoint is after exact local turn acquisition"
    );
    assert_eq!(slow_registry.build_calls(), 0);

    let warmup_task = {
        let service = service.clone();
        let runtime_registry = runtime_registry.clone();
        let conversation_id = conversation_id.clone();
        tokio::spawn(async move {
            service
                .warmup_for_view(SQLITE_TEST_OWNER, &conversation_id, &runtime_registry)
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(
        !warmup_task.is_finished(),
        "view warmup must not observe a false idle gap before detached ownership"
    );

    send_task.abort();
    assert!(send_task.await.unwrap_err().is_cancelled());
    let receipt = wait_for_public_admission_terminal(
        repo.as_ref(),
        SQLITE_TEST_OWNER,
        &conversation_id,
        &operation_id,
    )
    .await;
    wait_for_public_admission_process_guards_released(
        &service,
        SQLITE_TEST_OWNER,
        &conversation_id,
        &operation_id,
    )
    .await;
    let warmup_error = tokio::time::timeout(Duration::from_secs(2), warmup_task)
        .await
        .expect("warmup must resume after terminal proof")
        .expect("warmup task must not panic")
        .expect_err("view cannot infer prior process-tree exit from a cold Running aggregate");
    assert!(matches!(warmup_error, AppError::Conflict(_)));
    assert_eq!(
        repo.get(&conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished"),
        "only the exact admission custodian may finish its abandoned generation; navigation remains read-only"
    );

    assert_eq!(receipt.result_ok, Some(false));
    assert_eq!(slow_registry.build_calls(), 0);
    let user_messages = repo
        .get_messages(&conversation_id, 1, 20, SortOrder::Asc)
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|message| message.position.as_deref() == Some("right"))
        .collect::<Vec<_>>();
    assert_eq!(
        user_messages.len(),
        1,
        "the admitted transcript remains stable and is never re-executed"
    );

    let replay = service
        .send_message_with_idempotency_key(
            SQLITE_TEST_OWNER,
            &conversation_id,
            CLIENT_KEY,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert!(replay.completed);
    assert_eq!(replay.message_id, receipt.message_id);
    assert_eq!(slow_registry.build_calls(), 0);
    assert_eq!(
        repo.get(&conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished"),
        "the completed receipt replay is exact terminal evidence and may repair only its own generation"
    );
}

#[tokio::test]
async fn panic_before_detached_owner_spawn_is_terminalized_by_admission_custodian() {
    const CLIENT_KEY: &str = "panic-before-detached-owner";
    let (service, repo, slow_registry, runtime_registry, conversation_id) =
        public_admission_cutpoint_fixture("panic-before-detached-owner").await;
    let operation_id = ConversationService::public_turn_operation_id(
        SQLITE_TEST_OWNER,
        &conversation_id,
        CLIENT_KEY,
    );
    let (entered, _release) = service.install_public_admission_cutpoint(
        crate::service::PublicAdmissionCutpoint::BeforeOwnerSpawn,
        true,
    );
    let send_task = {
        let service = service.clone();
        let runtime_registry = runtime_registry.clone();
        let conversation_id = conversation_id.clone();
        tokio::spawn(async move {
            service
                .send_message_with_idempotency_key(
                    SQLITE_TEST_OWNER,
                    &conversation_id,
                    CLIENT_KEY,
                    make_send_req(),
                    &runtime_registry,
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("request must reach the injected panic cutpoint");
    let panic = send_task
        .await
        .expect_err("the injected cutpoint must panic the request task");
    assert!(panic.is_panic());

    let receipt = wait_for_public_admission_terminal(
        repo.as_ref(),
        SQLITE_TEST_OWNER,
        &conversation_id,
        &operation_id,
    )
    .await;
    wait_for_public_admission_process_guards_released(
        &service,
        SQLITE_TEST_OWNER,
        &conversation_id,
        &operation_id,
    )
    .await;
    assert_eq!(receipt.result_ok, Some(false));
    assert_eq!(slow_registry.build_calls(), 0);

    let replay = service
        .send_message_with_idempotency_key(
            SQLITE_TEST_OWNER,
            &conversation_id,
            CLIENT_KEY,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert!(replay.completed);
    assert_eq!(replay.message_id, receipt.message_id);
    assert_eq!(slow_registry.build_calls(), 0);
}

#[tokio::test]
async fn completed_turn_receipt_read_boundaries_adopt_its_still_active_running_generation() {
    let (service, repo, slow_registry, runtime_registry, conversation_id) =
        public_admission_cutpoint_fixture("completed-receipt-adoption").await;
    let request = make_send_req();
    let request_payload = json!({
        "content": &request.content,
        "files": &request.files,
        "inject_skills": &request.inject_skills,
        "hidden": request.hidden,
        "origin": &request.origin,
        "channel_platform": &request.channel_platform,
    })
    .to_string();

    let public_key = "completed-public-preflight";
    let public_operation = ConversationService::public_turn_operation_id(
        SQLITE_TEST_OWNER,
        &conversation_id,
        public_key,
    );
    let initial_epoch = repo
        .get_turn_admission_state(SQLITE_TEST_OWNER, &conversation_id)
        .await
        .unwrap()
        .epoch;
    let public_claim = repo
        .claim_turn_delivery_receipt_and_admit_with_candidate(
            SQLITE_TEST_OWNER,
            &conversation_id,
            &public_operation,
            &MessageId::new().into_string(),
            &request_payload,
            initial_epoch,
            now_ms(),
        )
        .await
        .unwrap();
    assert!(public_claim.claimed_new);
    assert!(
        repo.complete_delivery_receipt(
            SQLITE_TEST_OWNER,
            &conversation_id,
            &public_operation,
            true,
            Some("authoritative public result"),
            None,
            now_ms(),
        )
        .await
        .unwrap()
    );
    assert_eq!(
        repo.get(&conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running"),
        "the fixture models a receipt-only commit crash window"
    );

    let public_replay = service
        .idempotent_delivery_result_with_idempotency_key(
            SQLITE_TEST_OWNER,
            &conversation_id,
            public_key,
            &request,
        )
        .await
        .unwrap()
        .expect("completed public receipt must be visible");
    assert!(public_replay.completed);
    assert_eq!(
        public_replay.result_text.as_deref(),
        Some("authoritative public result")
    );
    assert_eq!(
        repo.get(&conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished"),
        "public replay preflight must adopt the terminal receipt into Conversation"
    );

    let internal_operation = "execution:completed-receipt-read-boundary";
    let internal_epoch = repo
        .get_turn_admission_state(SQLITE_TEST_OWNER, &conversation_id)
        .await
        .unwrap()
        .epoch;
    repo.claim_turn_delivery_receipt_and_admit_with_candidate(
        SQLITE_TEST_OWNER,
        &conversation_id,
        internal_operation,
        &MessageId::new().into_string(),
        r#"{"source":"agent-execution"}"#,
        internal_epoch,
        now_ms(),
    )
    .await
    .unwrap();
    assert!(
        repo.complete_delivery_receipt(
            SQLITE_TEST_OWNER,
            &conversation_id,
            internal_operation,
            false,
            None,
            Some("authoritative internal result"),
            now_ms(),
        )
        .await
        .unwrap()
    );
    let internal_replay = service
        .idempotent_delivery_result(
            SQLITE_TEST_OWNER,
            &conversation_id,
            internal_operation,
        )
        .await
        .unwrap()
        .expect("completed internal receipt must be visible");
    assert!(internal_replay.completed);
    assert_eq!(
        internal_replay.result_error.as_deref(),
        Some("authoritative internal result")
    );
    assert_eq!(
        repo.get(&conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );

    let proof_key = "completed-proof-read-boundary";
    let proof_operation = ConversationService::public_turn_operation_id(
        SQLITE_TEST_OWNER,
        &conversation_id,
        proof_key,
    );
    let proof_epoch = repo
        .get_turn_admission_state(SQLITE_TEST_OWNER, &conversation_id)
        .await
        .unwrap()
        .epoch;
    repo.claim_turn_delivery_receipt_and_admit_with_candidate(
        SQLITE_TEST_OWNER,
        &conversation_id,
        &proof_operation,
        &MessageId::new().into_string(),
        r#"{"source":"proof"}"#,
        proof_epoch,
        now_ms(),
    )
    .await
    .unwrap();
    assert!(
        repo.complete_delivery_receipt(
            SQLITE_TEST_OWNER,
            &conversation_id,
            &proof_operation,
            true,
            Some("authoritative proof result"),
            None,
            now_ms(),
        )
        .await
        .unwrap()
    );
    assert!(
        !service
            .prove_no_turn_admission_with_idempotency_key(
                SQLITE_TEST_OWNER,
                &conversation_id,
                proof_key,
            )
            .await
            .unwrap()
    );
    assert_eq!(
        repo.get(&conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );

    let edit_key = "completed-edit-read-boundary";
    let edit_operation = format!(
        "public-edit-resubmit:v1:{}:{}:{}",
        SQLITE_TEST_OWNER, conversation_id, edit_key
    );
    let edit_request = make_send_req();
    let edit_payload = json!({
        "workflow": "edit-resubmit",
        "target_message_id": MESSAGE_ID_1,
        "content": &edit_request.content,
        "files": &edit_request.files,
        "inject_skills": &edit_request.inject_skills,
        "hidden": edit_request.hidden,
        "origin": &edit_request.origin,
        "channel_platform": &edit_request.channel_platform,
    })
    .to_string();
    let edit_epoch = repo
        .get_turn_admission_state(SQLITE_TEST_OWNER, &conversation_id)
        .await
        .unwrap()
        .epoch;
    repo.claim_turn_delivery_receipt_and_admit_with_candidate(
        SQLITE_TEST_OWNER,
        &conversation_id,
        &edit_operation,
        &MessageId::new().into_string(),
        &edit_payload,
        edit_epoch,
        now_ms(),
    )
    .await
    .unwrap();
    assert!(
        repo.complete_delivery_receipt(
            SQLITE_TEST_OWNER,
            &conversation_id,
            &edit_operation,
            true,
            Some("authoritative edit result"),
            None,
            now_ms(),
        )
        .await
        .unwrap()
    );
    let edit_replay = service
        .edit_and_resubmit_with_idempotency_key(
            SQLITE_TEST_OWNER,
            &conversation_id,
            MESSAGE_ID_1,
            edit_key,
            edit_request,
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(edit_replay.replayed);
    assert!(edit_replay.completed);
    assert_eq!(
        edit_replay.result_text.as_deref(),
        Some("authoritative edit result")
    );
    assert_eq!(
        repo.get(&conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    assert_eq!(
        slow_registry.build_calls(),
        0,
        "all completed-receipt read boundaries are terminal adoption only"
    );
}

#[tokio::test]
async fn background_receipt_preflight_absorbs_replay_before_runtime_or_mount_and_rejects_payload_drift() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    const CLIENT_KEY: &str = "autowork:0190f5fe-7c00-7a00-8000-000000000901:5:turn";

    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(250)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry,
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let workspace = tempfile::tempdir().unwrap();
    let conversation = service
        .create(
            USER_ID,
            serde_json::from_value(json!({
                "type": "acp",
                "extra": {
                    "agent_id": TEST_ACP_AGENT_ID,
                    "workspace": workspace.path().to_string_lossy()
                }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    broadcaster.take_events();

    // AutoWork acquires this non-processing fence before its durable
    // Requirement claim. An idle poll (no Requirement) must remain observably
    // Finished/Idle and must not publish a synthetic Starting transition.
    let idle_poll_lease = service
        .begin_public_runtime_preparation(&conversation.conversation_id, USER_ID)
        .expect("AutoWork preparation lease");
    let idle_poll_summary = service
        .runtime_summary_for(&conversation.conversation_id)
        .await;
    assert_eq!(
        idle_poll_summary.state,
        nomifun_api_types::ConversationRuntimeStateKind::Idle
    );
    assert!(!idle_poll_summary.is_processing);
    assert_eq!(idle_poll_summary.processing_started_at, None);
    assert!(broadcaster.take_events().is_empty());

    let request = make_send_req();
    let request_payload = json!({
        "content": &request.content,
        "files": &request.files,
        "inject_skills": &request.inject_skills,
        "hidden": request.hidden,
        "origin": &request.origin,
        "channel_platform": &request.channel_platform,
    })
    .to_string();
    let operation_id = format!(
        "public-turn:v1:{USER_ID}:{}:{CLIENT_KEY}",
        conversation.conversation_id
    );
    let claim = repo
        .claim_delivery_receipt_once(
            USER_ID,
            &conversation.conversation_id,
            &operation_id,
            "turn",
            &request_payload,
            now_ms(),
        )
        .await
        .unwrap();
    assert!(claim.claimed_new);

    let replay = service
        .idempotent_delivery_result_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            CLIENT_KEY,
            &request,
        )
        .await
        .unwrap()
        .expect("accepted receipt must be visible to background preflight");
    assert!(replay.replayed);
    assert!(!replay.completed);
    let replay_summary = service
        .runtime_summary_for(&conversation.conversation_id)
        .await;
    assert_eq!(
        replay_summary.state,
        nomifun_api_types::ConversationRuntimeStateKind::Idle
    );
    assert!(
        !replay_summary.is_processing,
        "an accepted receipt replay must not transiently project Starting"
    );
    assert_eq!(
        slow_registry.build_calls(),
        0,
        "receipt replay must be absorbed before constructing an Agent runtime"
    );
    assert!(
        !workspace.path().join(".nomi").exists(),
        "receipt replay must not activate knowledge or attachment mounts"
    );
    assert!(broadcaster.take_events().is_empty());
    drop(idle_poll_lease);

    let retained_runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let retained_service = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        retained_runtime_registry,
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(AlwaysRetainedExecutionBoundary),
    );
    assert!(matches!(
        retained_service
            .idempotent_delivery_result_with_idempotency_key(
                USER_ID,
                &conversation.conversation_id,
                CLIENT_KEY,
                &request,
            )
            .await
            .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert_eq!(
        slow_registry.build_calls(),
        0,
        "retained Agent Execution transcripts must be rejected before runtime construction"
    );
    assert!(!workspace.path().join(".nomi").exists());

    let mut drifted = request;
    drifted.content.push_str(" drift");
    assert!(matches!(
        service
            .idempotent_delivery_result_with_idempotency_key(
                USER_ID,
                &conversation.conversation_id,
                CLIENT_KEY,
                &drifted,
            )
            .await
            .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert_eq!(slow_registry.build_calls(), 0);
    assert!(!workspace.path().join(".nomi").exists());

    let edit_operation_id = format!(
        "public-edit-resubmit:v1:{USER_ID}:{}:edit-crash-window",
        conversation.conversation_id
    );
    repo.claim_delivery_receipt_once(
        USER_ID,
        &conversation.conversation_id,
        &edit_operation_id,
        "turn",
        r#"{"workflow":"edit-resubmit"}"#,
        now_ms(),
    )
    .await
    .unwrap();
    assert!(matches!(
        service
            .idempotent_delivery_result_with_idempotency_key(
                USER_ID,
                &conversation.conversation_id,
                "autowork:0190f5fe-7c00-7a00-8000-000000000901:6:turn",
                &make_send_req(),
            )
            .await
            .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert_eq!(
        slow_registry.build_calls(),
        0,
        "accepted edit-and-resubmit crash evidence must fence background preflight"
    );
    assert!(!workspace.path().join(".nomi").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_idempotent_send_has_one_execution_owner_across_independent_sqlite_pools() {
    const CLIENT_KEY: &str = "gateway-response-loss-cross-service-v1";

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let test_root = std::env::temp_dir().join(format!(
        "nomifun-cross-service-receipt-{}-{nonce}",
        std::process::id()
    ));
    let db_path = test_root.join("shared.sqlite3");
    let database_a = nomifun_db::init_database(&db_path).await.unwrap();
    let user_id = nomifun_db::installation_owner_id(database_a.pool())
        .await
        .unwrap();
    // A second init opens a genuinely independent SqlitePool over the same
    // durable file, matching two backend processes rather than two Arc clones.
    let database_b = nomifun_db::init_database(&db_path).await.unwrap();
    let repo_a = Arc::new(SqliteConversationRepository::new(
        database_a.pool().clone(),
    ));
    let repo_b = Arc::new(SqliteConversationRepository::new(
        database_b.pool().clone(),
    ));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_secs(2)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let svc_a = ConversationService::new(
        Arc::<str>::from(user_id.clone()),
        test_root.clone(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo_a.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let workspace = test_root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let request: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "agent_id": TEST_ACP_AGENT_ID,
            "workspace": workspace.to_string_lossy()
        }
    }))
    .unwrap();
    let conversation = svc_a.create(&user_id, request).await.unwrap();
    let svc_b = ConversationService::new(
        Arc::<str>::from(user_id.clone()),
        test_root.clone(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo_b.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );

    let start = Arc::new(tokio::sync::Barrier::new(3));
    let first = {
        let svc = svc_a.clone();
        let runtime_registry = runtime_registry.clone();
        let start = start.clone();
        let user_id = user_id.clone();
        let conversation_id = conversation.conversation_id.clone();
        tokio::spawn(async move {
            start.wait().await;
            svc.send_message_with_idempotency_key(
                &user_id,
                &conversation_id,
                CLIENT_KEY,
                make_send_req(),
                &runtime_registry,
            )
            .await
        })
    };
    let replay = {
        let svc = svc_b.clone();
        let runtime_registry = runtime_registry.clone();
        let start = start.clone();
        let user_id = user_id.clone();
        let conversation_id = conversation.conversation_id.clone();
        tokio::spawn(async move {
            start.wait().await;
            svc.send_message_with_idempotency_key(
                &user_id,
                &conversation_id,
                CLIENT_KEY,
                make_send_req(),
                &runtime_registry,
            )
            .await
        })
    };
    start.wait().await;
    let first_delivery = first.await.unwrap().unwrap();
    let replay_delivery = replay.await.unwrap().unwrap();
    assert_eq!(
        replay_delivery.message_id, first_delivery.message_id,
        "both services must observe the one canonical receipt identity"
    );
    assert_ne!(
        replay_delivery.replayed, first_delivery.replayed,
        "exactly one independent service may be the INSERT winner"
    );
    let operation_id = format!(
        "public-turn:v1:{user_id}:{}:{CLIENT_KEY}",
        conversation.conversation_id
    );
    assert_eq!(
        repo_a
            .get_delivery_receipt(
                &user_id,
                &conversation.conversation_id,
                &operation_id,
            )
            .await
            .unwrap()
            .expect("the INSERT winner persisted its receipt")
            .status,
        "accepted",
        "the long-running winner keeps the durable receipt in its absorbing accepted state"
    );
    let accepted_replay = svc_b
        .send_message_with_idempotency_key(
            &user_id,
            &conversation.conversation_id,
            CLIENT_KEY,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_eq!(accepted_replay.message_id, first_delivery.message_id);
    assert!(accepted_replay.replayed);
    assert!(!accepted_replay.completed);
    assert_eq!(
        slow_registry.build_calls(),
        1,
        "an old accepted receipt is never treated as takeover authority"
    );

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let finished = repo_a
                .get(&conversation.conversation_id)
                .await
                .unwrap()
                .and_then(|row| row.status)
                .as_deref()
                == Some("finished");
            if finished && slow_registry.build_calls() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the single receipt owner should finish its turn");
    assert_eq!(
        slow_registry.build_calls(),
        1,
        "only the INSERT winner may build/execute the model turn"
    );
    let user_messages = repo_b
        .get_messages(&conversation.conversation_id, 1, 20, SortOrder::Asc)
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|message| message.position.as_deref() == Some("right"))
        .collect::<Vec<_>>();
    assert_eq!(
        user_messages.len(),
        1,
        "the losing service must not persist a duplicate user transcript"
    );
    assert_eq!(user_messages[0].message_id, first_delivery.message_id);

    drop(svc_a);
    drop(svc_b);
    drop(repo_a);
    drop(repo_b);
    database_a.close().await;
    database_b.close().await;
    let _ = std::fs::remove_dir_all(&test_root);
}

#[tokio::test]
async fn public_idempotency_receipt_never_grants_execution_attempt_authority() {
    let repo = Arc::new(MockRepo::new());
    let broadcaster = Arc::new(MockBroadcaster::new());
    let runtime_registry_impl = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = runtime_registry_impl.clone();
    let svc = ConversationService::new(
        Arc::<str>::from(TEST_USER_1),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(AlwaysRetainedExecutionBoundary),
    );
    let conversation = svc
        .create(TEST_USER_1, make_create_req())
        .await
        .unwrap();

    let error = svc
        .send_message_with_idempotency_key(
            TEST_USER_1,
            &conversation.conversation_id,
            "public-replay-is-not-engine-authority",
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::Conflict(_)));
    assert_eq!(runtime_registry_impl.active_runtime_count(), 0);
    assert!(
        repo.get_messages(&conversation.conversation_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .items
            .is_empty(),
        "retention rejection precedes receipt, transcript, and runtime effects"
    );
    assert!(
        !svc
            .runtime_summary_for(&conversation.conversation_id)
            .await
            .is_processing
    );
}

#[tokio::test]
async fn public_idempotent_send_remains_owner_cancellable_during_runtime_startup() {
    const USER_ID: &str = SQLITE_TEST_OWNER;

    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_secs(2)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let svc = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo,
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let workspace = isolated_test_workspace("cancellable-send");
    let request: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": workspace }
    }))
    .unwrap();
    let conversation = svc.create(USER_ID, request).await.unwrap();

    let first_delivery = svc
        .send_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            "owner-cancellable-public-turn",
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(!first_delivery.replayed);
    assert!(
        svc.runtime_state()
            .has_active_turn(&conversation.conversation_id)
    );

    tokio::time::timeout(
        Duration::from_secs(12),
        svc.cancel(USER_ID, &conversation.conversation_id, &runtime_registry),
    )
    .await
    .expect("owner cancellation must remain bounded during the cold runtime build")
    .unwrap();
    wait_for_turn_released(&svc, &conversation.conversation_id).await;
    let build_calls_after_cancel = slow_registry.build_calls();
    let cancelled_replay = svc
        .send_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            "owner-cancellable-public-turn",
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_eq!(cancelled_replay.message_id, first_delivery.message_id);
    assert!(cancelled_replay.replayed);
    assert!(cancelled_replay.completed);
    assert_eq!(cancelled_replay.result_ok, Some(false));
    assert_eq!(
        slow_registry.build_calls(),
        build_calls_after_cancel,
        "replaying a user-cancelled public receipt must not resurrect its turn"
    );
    assert!(
        !svc
            .runtime_summary_for(&conversation.conversation_id)
            .await
            .is_processing
    );
}

#[tokio::test]
async fn send_message_persists_hidden_user_message_when_requested() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let req: SendMessageRequest = serde_json::from_value(json!({
        "content": "Hidden cron prompt",
        "hidden": true
    }))
    .unwrap();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "persist-hidden-user-message",
        req,
        &runtime_registry,
    )
    .await
    .unwrap();

    let messages = repo.get_messages(&conv.conversation_id, 1, 20, SortOrder::Asc).await.unwrap().items;
    // The user message is the only hidden text row written by the service.
    let user_message = messages
        .iter()
        .find(|message| message.r#type == "text" && message.position.as_deref() == Some("right"))
        .expect("user message should be persisted");
    assert!(user_message.hidden);
    // msg_id is server-generated and must be non-empty for frontend routing.
    assert!(user_message.msg_id.as_deref().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn send_message_persists_error_tip_when_agent_build_fails() {
    let (svc, broadcaster, repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> =
        Arc::new(FailingAgentRuntimeRegistry::new("ACP init failed: config file is invalid"));

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    broadcaster.take_events();

    let msg_id = send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "persist-agent-build-error",
        make_send_req(),
        &runtime_registry,
    )
        .await
        .unwrap();

    assert!(!msg_id.is_empty(), "msg_id must be non-empty");

    let messages = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let messages = repo.get_messages(&conv.conversation_id, 1, 20, SortOrder::Asc).await.unwrap().items;
            if messages.iter().any(|message| message.r#type == "tips") {
                return messages;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("agent build failure should persist an error tip");
    assert_eq!(messages.len(), 2, "user message and error tip should be persisted");

    let error_tip = messages
        .iter()
        .find(|message| message.r#type == "tips")
        .expect("agent build failure should persist an error tips message");
    assert_eq!(error_tip.status.as_deref(), Some("error"));
    assert_eq!(error_tip.position.as_deref(), Some("center"));

    let content: serde_json::Value = serde_json::from_str(&error_tip.content).unwrap();
    assert_eq!(content["type"], "error");
    assert_eq!(content["source"], "send_failed");
    assert_eq!(content["code"], "BAD_GATEWAY");
    assert_eq!(content["error"]["code"], "UNKNOWN_UPSTREAM_ERROR");
    assert_eq!(content["error"]["ownership"], "unknown_upstream");
    assert_eq!(content["error"]["retryable"], true);
    assert_eq!(content["error"]["feedback_recommended"], true);
    assert_eq!(content["error"]["detail"], "ACP init failed: config file is invalid");
    assert_eq!(
        content["content"],
        "The upstream Agent failed while handling the request"
    );

    let updated = repo.get(&conv.conversation_id).await.unwrap().unwrap();
    assert_eq!(updated.status.as_deref(), Some("finished"));
    assert!(
        !svc.runtime_state().has_active_turn(&conv.conversation_id),
        "turn handle must be released after a failed turn"
    );

    let events = broadcaster.take_events();
    let error_tip_event = events
        .iter()
        .find(|event| event.name == "message.stream" && event.data["type"] == "tips")
        .expect("agent build failure should broadcast the error tips message");
    assert_eq!(error_tip_event.data["status"], "error");
    assert_eq!(error_tip_event.data["data"]["code"], "BAD_GATEWAY");
}

#[tokio::test]
async fn send_message_empty_content_returns_bad_request() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let req: SendMessageRequest = serde_json::from_value(json!({
        "content": ""
    }))
    .unwrap();

    let err = svc.send_message(TEST_USER_1, &conv.conversation_id, req, &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::BadRequest(_)));
}

#[tokio::test]
async fn send_message_whitespace_content_returns_bad_request() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let req: SendMessageRequest = serde_json::from_value(json!({
        "content": "   "
    }))
    .unwrap();

    let err = svc.send_message(TEST_USER_1, &conv.conversation_id, req, &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::BadRequest(_)));
}

#[tokio::test]
async fn send_message_conversation_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let err = svc
        .send_message(TEST_USER_1, "no-such-id", make_send_req(), &runtime_registry)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn send_message_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let err = svc
        .send_message(TEST_USER_2, &conv.conversation_id, make_send_req(), &runtime_registry)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn send_message_keeps_cold_acp_orphan_running_without_restarting_it() {
    let (svc, broadcaster, repo, _runtime_registry) = make_service();
    let runtime_registry_impl = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = runtime_registry_impl.clone();

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let update = ConversationRowUpdate {
        status: Some("running".into()),
        ..Default::default()
    };
    repo.update(&conv.conversation_id, &update).await.unwrap();
    broadcaster.take_events();

    let error = svc
        .send_message(
            TEST_USER_1,
            &conv.conversation_id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .expect_err("a cold Running row must remain quarantined, not resumed");
    assert!(matches!(error, AppError::Conflict(_)));
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );
    assert_eq!(
        runtime_registry_impl.active_runtime_count(),
        0,
        "orphan quarantine must not build a replacement runtime"
    );
    assert!(
        repo.messages.lock().unwrap().is_empty(),
        "the rejected stale admission must not persist a new user turn"
    );
    assert!(
        !broadcaster
            .take_events()
            .into_iter()
            .any(|event| event.name == "turn.completed"),
        "cold restart quarantine cannot publish terminal completion"
    );
}

#[tokio::test]
async fn send_message_keeps_external_gateway_orphan_running_until_terminal_is_proven() {
    let (svc, broadcaster, repo, _runtime_registry) = make_service();
    let runtime_registry_impl = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = runtime_registry_impl.clone();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    {
        let mut rows = repo.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|row| row.conversation_id == conv.conversation_id)
            .unwrap();
        row.r#type = AgentType::Remote.serde_name().to_owned();
        row.status = Some("running".to_owned());
    }
    broadcaster.take_events();

    let error = svc
        .send_message(
            TEST_USER_1,
            &conv.conversation_id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .expect_err("an unproven remote turn must remain fenced");

    assert!(matches!(error, AppError::Conflict(_)));
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running"),
        "absence from this process must not finalize work that may still run remotely"
    );
    assert_eq!(runtime_registry_impl.active_runtime_count(), 0);
    assert!(
        !broadcaster
            .take_events()
            .iter()
            .any(|event| event.name == "turn.completed"),
        "no completion may be published without a remote terminal proof"
    );
}

#[tokio::test]
async fn send_message_rejects_active_turn() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let _turn_handle = svc
        .runtime_state()
        .try_acquire_turn(&conv.conversation_id)
        .expect("test turn handle should be acquired");

    let err = svc
        .send_message(TEST_USER_1, &conv.conversation_id, make_send_req(), &runtime_registry)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::Conflict(_)));
}

#[tokio::test]
async fn send_message_missing_managed_workspace_identity_fails_closed() {
    let (svc, _broadcaster, repo, _default_runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> =
        Arc::new(MockAgentRuntimeRegistryWithWorkspace::new("/tmp/factory-resolved"));

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, req).await.unwrap();

    let invalid_extra = ConversationRowUpdate {
        extra: Some(r#"{"workspace":""}"#.to_owned()),
        ..Default::default()
    };
    repo.update(&conv.conversation_id, &invalid_extra).await.unwrap();

    let error = send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "missing-managed-workspace-identity",
        make_send_req(),
        &runtime_registry,
    )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AppError::Internal(message)
            if message.contains("neither a custom workspace nor a canonical temp_workspace_id")
    ));
}

#[tokio::test]
async fn durable_turn_preflight_failure_atomically_finishes_conversation_and_receipt() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> =
        Arc::new(MockAgentRuntimeRegistryWithWorkspace::new("/tmp/factory-resolved"));
    let svc = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let request: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID }
    }))
    .unwrap();
    let conversation = svc.create(USER_ID, request).await.unwrap();
    repo.update(
        &conversation.conversation_id,
        &ConversationRowUpdate {
            extra: Some(r#"{"workspace":""}"#.to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let key = "preflight-failure";
    let error = svc
        .send_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            key,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::Internal(_)));
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    let operation_id = format!(
        "public-turn:v1:{USER_ID}:{}:{key}",
        conversation.conversation_id
    );
    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, &operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, "completed");
    assert_eq!(receipt.result_ok, Some(false));
}

#[tokio::test]
async fn build_runtime_options_rebases_managed_workspace_after_restore() {
    let destination_root =
        std::env::temp_dir().join(format!("nomifun-rebase-{}", nomifun_common::generate_id()));
    let (svc, _broadcaster, repo, _runtime_registry) =
        make_service_with_workspace_root(destination_root.clone());
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "backend": "claude" }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, req).await.unwrap();
    let temp_workspace_id = conv.extra["temp_workspace_id"]
        .as_str()
        .expect("create must stamp temp_workspace_id")
        .to_owned();

    let restored_extra = json!({
        "agent_id": TEST_ACP_AGENT_ID,
        "agent_source": "builtin",
        "backend": "claude",
        "temp_workspace_id": temp_workspace_id,
        "workspace": "/source-install/conversations/0190f5fe-7c00-7a00-8abc-000000000000",
        "skills": []
    });
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            extra: Some(restored_extra.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let response = svc.get(TEST_USER_1, &conv.conversation_id).await.unwrap();
    let expected = destination_root
        .join("conversations")
        .join(&temp_workspace_id);
    assert_eq!(
        PathBuf::from(response.extra["workspace"].as_str().unwrap()),
        expected
    );
    let row = repo.get(&conv.conversation_id).await.unwrap().unwrap();

    let options = svc.build_runtime_options(&row).unwrap();
    assert_eq!(PathBuf::from(options.workspace), expected);
    assert_ne!(
        options.extra["workspace"],
        "/source-install/conversations/0190f5fe-7c00-7a00-8abc-000000000000"
    );
    assert_eq!(
        options.extra["temp_workspace_id"],
        temp_workspace_id
    );

    let _ = std::fs::remove_dir_all(destination_root);
}

#[tokio::test]
async fn build_runtime_options_preserves_explicit_custom_workspace() {
    let destination_root =
        std::env::temp_dir().join(format!("nomifun-custom-{}", nomifun_common::generate_id()));
    let custom_workspace =
        std::env::temp_dir().join(format!("nomifun-project-{}", nomifun_common::generate_id()));
    std::fs::create_dir_all(&custom_workspace).unwrap();
    let (svc, _broadcaster, repo, _runtime_registry) =
        make_service_with_workspace_root(destination_root.clone());
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "agent_id": TEST_ACP_AGENT_ID,
            "backend": "claude",
            "workspace": custom_workspace.to_string_lossy()
        }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, req).await.unwrap();
    let row = repo.get(&conv.conversation_id).await.unwrap().unwrap();

    let options = svc.build_runtime_options(&row).unwrap();
    assert_eq!(
        PathBuf::from(options.workspace),
        custom_workspace
    );
    assert!(options.extra.get("temp_workspace_id").is_none());

    let _ = std::fs::remove_dir_all(destination_root);
    let _ = std::fs::remove_dir_all(custom_workspace);
}

#[tokio::test]
async fn send_message_continues_cron_system_responses() {
    let (svc, broadcaster, _repo, _default_runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let scripted_agent = Arc::new(ScriptedAgent::new(
        &conv.conversation_id,
        vec![
            vec![
                AgentStreamEvent::Text(TextEventData {
                    content: "I'll check. [CRON_LIST]".into(),
                }),
                AgentStreamEvent::Finish(FinishEventData::default()),
            ],
            vec![
                AgentStreamEvent::Text(TextEventData {
                    content: "[CRON_CREATE]\nname: Daily Greeting\nschedule: 0 9 * * *\nschedule_description: Daily at 9:00 AM\nmessage: Say good morning\n[/CRON_CREATE]".into(),
                }),
                AgentStreamEvent::Finish(FinishEventData::default()),
            ],
            vec![
                AgentStreamEvent::Thinking(ThinkingEventData {
                    content: "Plan the final response first.".into(),
                    subject: None,
                    duration: None,
                    status: Some("thinking".into()),
                }),
                AgentStreamEvent::Text(TextEventData {
                    content: "Done. The task is scheduled.".into(),
                }),
                AgentStreamEvent::Finish(FinishEventData::default()),
            ],
        ],
    ));
    runtime_registry.insert_agent(&conv.conversation_id, AgentRuntimeHandle::Mock(scripted_agent.clone()));
    svc.with_cron_service(Some(Arc::new(MockCronContinuationService)));

    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    let req: SendMessageRequest = serde_json::from_value(json!({
        "content": "Create the task now"
    }))
    .unwrap();

    svc.send_message_with_idempotency_key(
        TEST_USER_1,
        &conv.conversation_id,
        "turn-final-writeback-after-cron-continuation",
        req,
        &runtime_registry_dyn,
    )
    .await
    .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if scripted_agent.sent_contents().len() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let sends = scripted_agent.sent_contents();
    assert_eq!(sends.len(), 3);
    assert_eq!(sends[0], "Create the task now");
    assert_eq!(sends[1], "[System: No scheduled tasks]");
    assert_eq!(sends[2], "[System: Created cron job 'Daily Greeting']");

    wait_for_turn_released(&svc, &conv.conversation_id).await;

    let finished = svc.get(TEST_USER_1, &conv.conversation_id).await.unwrap();
    assert_eq!(finished.status, ConversationStatus::Finished);

    let events = broadcaster.take_events();
    let turn_events: Vec<_> = events.iter().filter(|evt| evt.name == "turn.completed").collect();
    assert_eq!(turn_events.len(), 1);
    assert_eq!(turn_events[0].data["runtime"]["is_processing"], false);
    assert_eq!(turn_events[0].data["runtime"]["can_send_message"], true);
}

#[tokio::test]
async fn send_message_turn_writeback_runs_after_system_continuation_final_answer() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let workspace = unique_test_dir("conv-knowledge-workspace");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    let data_dir = unique_test_dir("conv-knowledge-data");

    let knowledge_db = nomifun_db::init_database_memory().await.unwrap();
    let knowledge_owner = nomifun_db::installation_owner_id(knowledge_db.pool()).await.unwrap();
    let knowledge_repo: Arc<dyn nomifun_db::IKnowledgeRepository> = Arc::new(
        nomifun_db::SqliteKnowledgeRepository::new(knowledge_db.pool().clone()),
    );
    let knowledge = Arc::new(KnowledgeService::new(
        knowledge_repo,
        &data_dir,
        KnowledgeEventEmitter::new(broadcaster.clone(), Arc::from(TEST_USER_1)),
    ));
    svc.with_knowledge_service(knowledge.clone());

    let create_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
        "extra": { "workspace": workspace }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, create_req).await.unwrap();
    let kb = knowledge.create_base("turn-final", "", None, None).await.unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO conversations (conversation_id, user_id, name, type, status, created_at, updated_at) \
         VALUES (?, ?, 'turn-final', 'acp', 'pending', 1, 1)",
    )
    .bind(&conv.conversation_id)
    .bind(&knowledge_owner)
    .execute(knowledge_db.pool())
    .await
    .unwrap();
    let workpath_key =
        nomifun_knowledge::session_workpath_key(&workspace, &std::env::temp_dir());
    knowledge
        .set_binding(
            "workpath",
            &workpath_key,
            KnowledgeBinding {
                enabled: true,
                writeback: true,
                writeback_mode: "staged".into(),
                writeback_eagerness: "aggressive".into(),
                kb_ids: vec![kb.knowledge_base_id.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let candidate = format!(
        r##"{{"candidates":[{{"kb_id":"{}","rel_path":"patterns/cron-final.md","content":"# Final scheduling lesson\n\nThe final answer after cron continuation is durable."}}]}}"##,
        kb.knowledge_base_id
    );
    let completer = Arc::new(RecordingKnowledgeCompleter::new(candidate));
    knowledge.set_completer(completer.clone());

    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    svc.warmup_for_view(
        TEST_USER_1,
        &conv.conversation_id,
        &runtime_registry_dyn,
    )
    .await
    .expect("warmup must seed the exact knowledge binding lease and runtime signature");

    let scripted_agent = Arc::new(
        ScriptedAgent::new(
            &conv.conversation_id,
            vec![
            vec![
                AgentStreamEvent::Text(TextEventData {
                    content: "I'll check. [CRON_LIST]".into(),
                }),
                AgentStreamEvent::Finish(FinishEventData::default()),
            ],
            vec![
                AgentStreamEvent::Thinking(ThinkingEventData {
                    content: "Plan the final writeback answer first.".into(),
                    subject: None,
                    duration: None,
                    status: Some("thinking".into()),
                }),
                AgentStreamEvent::Text(TextEventData {
                    content: "Done. The task is scheduled.".into(),
                }),
                AgentStreamEvent::Finish(FinishEventData::default()),
            ],
            ],
        )
        .with_agent_type(AgentType::Nomi)
        .with_workspace(workspace.to_string_lossy().into_owned()),
    );
    runtime_registry.insert_agent(&conv.conversation_id, AgentRuntimeHandle::Mock(scripted_agent.clone()));
    svc.with_cron_service(Some(Arc::new(MockCronContinuationService)));
    broadcaster.take_events();

    let req: SendMessageRequest = serde_json::from_value(json!({
        "content": "Create the task now"
    }))
    .unwrap();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "turn-final-writeback-after-cron-continuation",
        req,
        &runtime_registry_dyn,
    )
    .await
    .unwrap();
    wait_for_turn_released(&svc, &conv.conversation_id).await;

    let mut events = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            events.extend(broadcaster.take_events());
            if events
                .iter()
                .any(|evt| evt.name == "knowledge.writeback" && evt.data["status"] == "written")
                && events.iter().any(|evt| evt.name == "turn.completed")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let turn_idx = events
        .iter()
        .position(|evt| evt.name == "turn.completed")
        .expect("turn.completed should be broadcast");
    let first_writeback_idx = events
        .iter()
        .position(|evt| evt.name == "knowledge.writeback")
        .expect("knowledge.writeback should be broadcast");
    assert!(
        first_writeback_idx < turn_idx,
        "turn completion must remain behind durable turn-final knowledge writeback"
    );
    let final_writeback_idx = events
        .iter()
        .position(|evt| evt.name == "knowledge.writeback" && evt.data["status"] == "written")
        .expect("written knowledge.writeback should be broadcast");
    let writeback_statuses: Vec<_> = events
        .iter()
        .filter(|evt| evt.name == "knowledge.writeback")
        .filter_map(|evt| evt.data["status"].as_str())
        .collect();
    assert!(writeback_statuses.contains(&"started"), "{writeback_statuses:?}");
    assert!(writeback_statuses.contains(&"extracting"), "{writeback_statuses:?}");
    assert!(writeback_statuses.contains(&"writing"), "{writeback_statuses:?}");
    assert!(writeback_statuses.contains(&"written"), "{writeback_statuses:?}");
    let writeback = &events[final_writeback_idx];
    assert_eq!(writeback.data["status"], "written");
    let msg_id = writeback.data["msg_id"].as_str().expect("writeback msg_id");
    let stored_msg = repo
        .get_message(&conv.conversation_id, msg_id)
        .await
        .unwrap()
        .expect("assistant message row should persist writeback state");
    assert_eq!(
        stored_msg.r#type, "text",
        "turn-final writeback state must be attached to the final assistant text message, not the turn's thinking segment"
    );
    let stored_content: serde_json::Value = serde_json::from_str(&stored_msg.content).unwrap();
    assert_eq!(stored_content["knowledge_writeback"]["status"], "written");
    assert_eq!(stored_content["knowledge_writeback"]["retryable"], false);
    let persisted_messages = repo.messages.lock().unwrap();
    let thinking_with_writeback: Vec<_> = persisted_messages
        .iter()
        .filter(|message| message.conversation_id == conv.conversation_id && message.r#type == "thinking")
        .filter(|message| {
            serde_json::from_str::<serde_json::Value>(&message.content)
                .ok()
                .and_then(|content| content.get("knowledge_writeback").cloned())
                .is_some()
        })
        .collect();
    assert!(
        thinking_with_writeback.is_empty(),
        "thinking messages must not own turn-final knowledge writeback state"
    );
    let rel_path = writeback.data["written"][0]["rel_path"]
        .as_str()
        .expect("written rel_path");
    assert!(rel_path.starts_with(&format!("_inbox/{}/", conv.conversation_id)));
    assert!(rel_path.ends_with("/patterns/cron-final.md"));
    let staged = knowledge
        .read_file(&kb.knowledge_base_id, rel_path)
        .await
        .unwrap();
    assert!(staged.content.contains("final answer after cron continuation"));

    let prompts = completer.prompts();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].1.contains("Create the task now"));
    assert!(prompts[0].1.contains("Done. The task is scheduled."));
    assert!(
        !prompts[0].1.contains("[System: No scheduled tasks]"),
        "hidden system continuation text must not replace the human turn input"
    );
    assert_eq!(
        completer.models(),
        vec![(PROVIDER_ID_1.to_owned(), "m1".to_owned())],
        "without an explicit knowledge model, write-back must use the conversation model"
    );

    let _ = tokio::fs::remove_dir_all(&workspace).await;
    let _ = tokio::fs::remove_dir_all(&data_dir).await;
}

#[tokio::test]
async fn stop_during_slow_turn_writeback_keeps_exact_turn_fenced_until_child_quiesces() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    const FIRST_KEY: &str = "slow-turn-final-writeback-first";
    const SECOND_KEY: &str = "slow-turn-final-writeback-second";
    const STOP_RACE_KEY: &str = "slow-turn-final-writeback-stop-race";

    let database = init_database_memory().await.unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO providers (\
            provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
            capabilities, created_at, updated_at\
         ) VALUES (?1, 'openai', 'writeback fixture', 'https://example.invalid', \
                   'encrypted', '[\"m1\"]', 1, '[]', 1, 1)",
    )
    .bind(PROVIDER_ID_1)
    .execute(database.pool())
    .await
    .unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    let svc = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        Arc::clone(&runtime_registry_dyn),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let workspace = unique_test_dir("conv-knowledge-workspace-slow-writeback");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    let data_dir = unique_test_dir("conv-knowledge-data-slow-writeback");

    let knowledge_db = nomifun_db::init_database_memory().await.unwrap();
    let knowledge_owner = nomifun_db::installation_owner_id(knowledge_db.pool()).await.unwrap();
    let knowledge_repo: Arc<dyn nomifun_db::IKnowledgeRepository> = Arc::new(
        nomifun_db::SqliteKnowledgeRepository::new(knowledge_db.pool().clone()),
    );
    let knowledge = Arc::new(KnowledgeService::new(
        knowledge_repo,
        &data_dir,
        KnowledgeEventEmitter::new(
            Arc::new(MockBroadcaster::new()),
            Arc::from(USER_ID),
        ),
    ));
    svc.with_knowledge_service(knowledge.clone());

    let create_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
        "extra": { "workspace": workspace }
    }))
    .unwrap();
    let conv = svc.create(USER_ID, create_req).await.unwrap();
    let kb = knowledge.create_base("turn-final", "", None, None).await.unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO conversations (conversation_id, user_id, name, type, status, created_at, updated_at) \
         VALUES (?, ?, 'turn-final', 'acp', 'pending', 1, 1)",
    )
    .bind(&conv.conversation_id)
    .bind(&knowledge_owner)
    .execute(knowledge_db.pool())
    .await
    .unwrap();
    let workpath_key =
        nomifun_knowledge::session_workpath_key(&workspace, &std::env::temp_dir());
    knowledge
        .set_binding(
            "workpath",
            &workpath_key,
            KnowledgeBinding {
                enabled: true,
                writeback: true,
                writeback_mode: "staged".into(),
                writeback_eagerness: "aggressive".into(),
                kb_ids: vec![kb.knowledge_base_id.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let completer = Arc::new(BlockingFirstKnowledgeCompleter::new_sequence(
        "not valid JSON".into(),
        r#"{"candidates":[]}"#.into(),
    ));
    knowledge.set_completer(completer.clone());

    svc.warmup_for_view(
        USER_ID,
        &conv.conversation_id,
        &runtime_registry_dyn,
    )
    .await
    .expect("warmup must seed the exact knowledge binding lease and runtime signature");

    let scripted_agent = Arc::new(
        ScriptedAgent::new(
            &conv.conversation_id,
            vec![
            vec![
                AgentStreamEvent::Text(TextEventData {
                    content: "First answer.".into(),
                }),
                AgentStreamEvent::Finish(FinishEventData::default()),
            ],
            vec![
                AgentStreamEvent::Text(TextEventData {
                    content: "Second answer.".into(),
                }),
                AgentStreamEvent::Finish(FinishEventData::default()),
            ],
            ],
        )
        .with_workspace(workspace.to_string_lossy().into_owned()),
    );
    runtime_registry.insert_agent(&conv.conversation_id, AgentRuntimeHandle::Mock(scripted_agent));

    let first_req: SendMessageRequest = serde_json::from_value(json!({ "content": "first" })).unwrap();
    svc.send_message_with_idempotency_key(
        USER_ID,
        &conv.conversation_id,
        FIRST_KEY,
        first_req,
        &runtime_registry_dyn,
    )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), completer.wait_started())
        .await
        .expect("turn-final writeback should start");
    let first_writeback_msg_id = broadcaster
        .take_events()
        .into_iter()
        .find(|event| {
            event.name == "knowledge.writeback" && event.data["status"] == "started"
        })
        .and_then(|event| event.data["msg_id"].as_str().map(ToOwned::to_owned))
        .expect("first writeback started event should identify its assistant row");

    let second_req: SendMessageRequest = serde_json::from_value(json!({ "content": "second" })).unwrap();
    let second = svc
        .send_message_with_idempotency_key(
            USER_ID,
            &conv.conversation_id,
            SECOND_KEY,
            second_req,
            &runtime_registry_dyn,
        )
        .await;

    assert!(
        matches!(second, Err(AppError::Conflict(_))),
        "slow turn-final writeback must retain exact turn ownership: {second:?}"
    );
    let running = svc.runtime_summary_for(&conv.conversation_id).await;
    assert!(running.is_processing);
    assert!(!running.can_send_message);
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );
    assert!(
        !broadcaster
            .take_events()
            .into_iter()
            .any(|event| event.name == "turn.completed"),
        "turn.completed must not precede writeback terminal persistence"
    );

    // Elapsed time has no authority to fabricate a terminal result or release
    // the exact turn while the real knowledge worker is still alive.
    tokio::time::sleep(Duration::from_millis(5_200)).await;
    let events_before_release = broadcaster.take_events();
    assert!(
        !events_before_release.iter().any(|event| {
            event.name == "knowledge.writeback"
                && event.data["msg_id"].as_str()
                    == Some(first_writeback_msg_id.as_str())
                && event.data["status"] == "interrupted"
        }),
        "crossing the old five-second grace must not fabricate an interrupted terminal"
    );

    let cancel_task = {
        let service = svc.clone();
        let conversation_id = conv.conversation_id.clone();
        let runtime_registry = Arc::clone(&runtime_registry_dyn);
        tokio::spawn(async move {
            service
                .cancel(USER_ID, &conversation_id, &runtime_registry)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime_registry.termination_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stop should prove backend teardown while writeback remains blocked");
    assert!(
        !cancel_task.is_finished(),
        "backend exit alone must not bypass the tracked writeback fence"
    );

    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running"),
        "stop must retain durable Running until writeback publication quiesces"
    );
    let operation_id = format!(
        "public-turn:v1:{USER_ID}:{}:{FIRST_KEY}",
        conv.conversation_id
    );
    let accepted = repo
        .get_delivery_receipt(USER_ID, &conv.conversation_id, &operation_id)
        .await
        .unwrap()
        .expect("the blocked exact turn must retain its durable receipt");
    assert_eq!(accepted.status, "accepted");
    let events_during_stop = broadcaster.take_events();
    assert!(
        !events_during_stop
            .iter()
            .any(|event| event.name == "turn.completed"),
        "stop must not publish completion while its writeback child is active"
    );

    let stop_race = tokio::time::timeout(
        Duration::from_secs(2),
        svc.send_message_with_idempotency_key(
            USER_ID,
            &conv.conversation_id,
            STOP_RACE_KEY,
            serde_json::from_value(json!({ "content": "must stay fenced" })).unwrap(),
            &runtime_registry_dyn,
        ),
    )
    .await
    .expect("the stop tombstone should reject a successor without waiting");
    assert!(
        matches!(stop_race, Err(AppError::Conflict(_))),
        "a replacement send must remain fenced while stop awaits writeback: {stop_race:?}"
    );

    completer.release();
    tokio::time::timeout(Duration::from_secs(6), cancel_task)
    .await
    .expect("stop should finish after the real writeback terminal")
    .expect("stop task should not panic")
    .expect("stop should finalize the exact generation");

    let mut terminal_events = events_during_stop;
    terminal_events.extend(broadcaster.take_events());
    let writeback_index = terminal_events
        .iter()
        .position(|event| {
            event.name == "knowledge.writeback"
                && event.data["msg_id"].as_str() == Some(first_writeback_msg_id.as_str())
                && event.data["status"] == "failed"
        })
        .unwrap_or_else(|| {
            panic!(
                "released writeback should reach its real terminal state: {:?}",
                terminal_events
                    .iter()
                    .filter(|event| event.name == "knowledge.writeback")
                    .map(|event| (
                        event.data["msg_id"].as_str(),
                        event.data["status"].as_str(),
                        event.data["retryable"].as_bool(),
                    ))
                    .collect::<Vec<_>>()
            )
        });
    let completion_index = terminal_events
        .iter()
        .position(|event| event.name == "turn.completed")
        .expect("stop should publish one completion after durable closure");
    assert!(
        writeback_index < completion_index,
        "knowledge writeback terminal must precede turn.completed"
    );
    let failed = &terminal_events[writeback_index];
    assert_eq!(failed.data["retryable"], true);
    wait_for_turn_released(&svc, &conv.conversation_id).await;
    let idle = svc.runtime_summary_for(&conv.conversation_id).await;
    assert!(!idle.is_processing);
    assert!(idle.can_send_message);
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    let completed_receipt = repo
        .get_delivery_receipt(USER_ID, &conv.conversation_id, &operation_id)
        .await
        .unwrap()
        .expect("stop must settle the accepted exact-turn receipt");
    assert_eq!(completed_receipt.status, "completed");
    assert_eq!(completed_receipt.result_ok, Some(false));

    let stored = repo
        .get_message(&conv.conversation_id, &first_writeback_msg_id)
        .await
        .unwrap()
        .expect("assistant message should retain terminal writeback state");
    let stored_content: serde_json::Value = serde_json::from_str(&stored.content).unwrap();
    assert_eq!(
        stored_content["knowledge_writeback"]["status"],
        "failed"
    );
    assert_eq!(stored_content["knowledge_writeback"]["retryable"], true);
    assert!(
        stored_content["knowledge_writeback"]["source_message_id"].is_string(),
        "manual retry needs the exact originating user message"
    );
    assert!(
        stored_content["knowledge_writeback"]["scope"].is_string(),
        "manual retry needs the original idempotency scope"
    );
    assert!(
        stored_content["knowledge_writeback"]["finished_at"]
            .as_i64()
            .is_some()
    );
    assert_eq!(
        stored_content["knowledge_writeback"].get("interrupted_at"),
        None,
        "a normally released worker must not be projected as interrupted"
    );

    let first_attempt_id = stored_content["knowledge_writeback"]["attempt_id"]
        .as_str()
        .expect("failed writeback attempt id")
        .to_owned();
    svc.retry_knowledge_writeback(
        USER_ID,
        &conv.conversation_id,
        &first_writeback_msg_id,
        &first_attempt_id,
    )
    .await
    .expect("retryable terminal state should start one manual retry");
    let retried = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(event) = broadcaster.take_events().into_iter().find(|event| {
                event.name == "knowledge.writeback"
                    && event.data["status"] == "no_candidate"
                    && event.data["msg_id"].as_str()
                        == Some(first_writeback_msg_id.as_str())
            }) {
                break event;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("manual retry should publish its independent terminal state");
    assert_ne!(
        retried.data["attempt_id"].as_str().unwrap(),
        first_attempt_id,
        "manual retry must use a fresh attempt generation"
    );
    let retried_stored = repo
        .get_message(&conv.conversation_id, &first_writeback_msg_id)
        .await
        .unwrap()
        .expect("assistant message should retain retried writeback state");
    let retried_content: serde_json::Value =
        serde_json::from_str(&retried_stored.content).unwrap();
    assert_eq!(
        retried_content["knowledge_writeback"]["status"],
        "no_candidate"
    );
    assert_eq!(
        retried_content["knowledge_writeback"]["retryable"],
        false
    );
    assert_eq!(
        retried_content["knowledge_writeback"]["attempt_id"],
        retried.data["attempt_id"]
    );

    // Simulate a remount/restart with an empty runtime registry. The completed
    // receipt is absorbing and must be returned without constructing an Agent.
    let replay_registry_impl = Arc::new(MockAgentRuntimeRegistry::new());
    let replay_registry: Arc<dyn AgentRuntimeRegistry> = replay_registry_impl.clone();
    let replay_service = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        Arc::new(MockBroadcaster::new()),
        Arc::new(FixedSkillResolver { names: vec![] }),
        Arc::clone(&replay_registry),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let replay = replay_service
        .send_message_with_idempotency_key(
            USER_ID,
            &conv.conversation_id,
            FIRST_KEY,
            serde_json::from_value(json!({ "content": "first" })).unwrap(),
            &replay_registry,
        )
        .await
        .expect("completed receipt should be an absorbing replay");
    assert!(replay.replayed);
    assert!(replay.completed);
    assert_eq!(
        replay_registry_impl.build_count(),
        0,
        "remount replay must not rebuild or restart the completed turn"
    );

    let _ = tokio::fs::remove_dir_all(&workspace).await;
    let _ = tokio::fs::remove_dir_all(&data_dir).await;
}

// ── IDMM exact-turn continuation tests ──────────────────────────

fn make_idmm_continuation(content: &str) -> SendMessageRequest {
    serde_json::from_value(json!({
        "content": content,
        "hidden": true,
        "origin": "idmm"
    }))
    .unwrap()
}

fn unavailable_idmm_scope() -> crate::IdmmTurnScope {
    crate::IdmmTurnScope {
        wire_turn_id: MessageId::new().into_string(),
        generation: u64::MAX,
    }
}

#[tokio::test]
async fn idmm_continuation_of_finished_conversation_cannot_build_or_persist() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("finished".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    broadcaster.take_events();

    let error = svc
        .idmm_continue_active_turn(
            TEST_USER_1,
            &conv.conversation_id,
            &unavailable_idmm_scope(),
            make_idmm_continuation("Please continue."),
            &runtime_registry,
        )
        .await
        .expect_err("Finished is absorbing for an IDMM continuation");
    assert!(matches!(error, AppError::Conflict(_)));
    assert_eq!(registry.active_runtime_count(), 0);
    assert!(repo.messages.lock().unwrap().is_empty());
    assert!(
        broadcaster
            .take_events()
            .iter()
            .all(|event| event.name != "message.userCreated")
    );
}

#[tokio::test]
async fn idmm_continuation_missing_runtime_never_falls_back_to_fresh_send() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("running".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let wire_turn_id = MessageId::new().into_string();
    let turn = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner(
            &conv.conversation_id,
            Some(wire_turn_id.clone()),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(TEST_USER_1.to_owned()),
            true,
            None,
        )
        .unwrap();
    broadcaster.take_events();

    let error = svc
        .idmm_continue_active_turn(
            TEST_USER_1,
            &conv.conversation_id,
            &crate::IdmmTurnScope {
                wire_turn_id,
                generation: turn.turn_id(),
            },
            make_idmm_continuation("Please continue."),
            &runtime_registry,
        )
        .await
        .expect_err("a missing runtime cannot accept exact-turn continuation");
    assert!(matches!(error, AppError::Conflict(_)));
    assert_eq!(
        registry.active_runtime_count(),
        0,
        "the no-runtime branch must not call get_or_create"
    );
    assert!(repo.messages.lock().unwrap().is_empty());
    assert!(
        broadcaster
            .take_events()
            .iter()
            .all(|event| event.name != "message.userCreated")
    );
    drop(turn);
}

#[tokio::test]
async fn idmm_continuation_steers_and_persists_only_the_exact_running_turn() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("running".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let wire_turn_id = MessageId::new().into_string();
    let turn = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner(
            &conv.conversation_id,
            Some(wire_turn_id),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(TEST_USER_1.to_owned()),
            true,
            None,
        )
        .unwrap();
    let agent = Arc::new(SteerableAgent::new(
        &conv.conversation_id,
        Some(ConversationStatus::Running),
        true,
    ));
    registry.insert_agent(
        &conv.conversation_id,
        AgentRuntimeHandle::Mock(agent.clone()),
    );
    broadcaster.take_events();

    let scope = svc
        .idmm_active_turn_scope(
            TEST_USER_1,
            &conv.conversation_id,
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_eq!(scope.generation, turn.turn_id());
    let message_id = svc
        .idmm_continue_active_turn(
            TEST_USER_1,
            &conv.conversation_id,
            &scope,
            make_idmm_continuation("continue this exact turn"),
            &runtime_registry,
        )
        .await
        .unwrap();
    MessageId::parse(&message_id).unwrap();
    assert_eq!(agent.steered(), vec!["continue this exact turn".to_owned()]);
    assert!(
        agent.sent_contents().is_empty(),
        "IDMM continuation must never invoke fresh-turn send_message"
    );
    assert_eq!(registry.active_runtime_count(), 1);

    let messages = repo.messages.lock().unwrap().clone();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].message_id, message_id);
    assert!(messages[0].hidden);
    assert_eq!(messages[0].position.as_deref(), Some("right"));
    let events = broadcaster.take_events();
    let created = events
        .iter()
        .filter(|event| event.name == "message.userCreated")
        .collect::<Vec<_>>();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].data["origin"], "idmm");
    assert!(
        events
            .iter()
            .all(|event| !matches!(event.name.as_str(), "turn.started" | "turn.completed"))
    );
    drop(turn);
}

#[tokio::test]
async fn idmm_scope_reserved_for_turn_a_cannot_steer_turn_b() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("running".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let turn_a = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner(
            &conv.conversation_id,
            Some(MessageId::new().into_string()),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(TEST_USER_1.to_owned()),
            true,
            None,
        )
        .unwrap();
    let agent_a = Arc::new(SteerableAgent::new(
        &conv.conversation_id,
        Some(ConversationStatus::Running),
        true,
    ));
    registry.insert_agent(
        &conv.conversation_id,
        AgentRuntimeHandle::Mock(agent_a.clone()),
    );
    let scope_a = svc
        .idmm_active_turn_scope(
            TEST_USER_1,
            &conv.conversation_id,
            &runtime_registry,
        )
        .await
        .unwrap();
    drop(turn_a);

    let turn_b = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner(
            &conv.conversation_id,
            Some(MessageId::new().into_string()),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(TEST_USER_1.to_owned()),
            true,
            None,
        )
        .unwrap();
    let agent_b = Arc::new(SteerableAgent::new(
        &conv.conversation_id,
        Some(ConversationStatus::Running),
        true,
    ));
    registry.insert_agent(
        &conv.conversation_id,
        AgentRuntimeHandle::Mock(agent_b.clone()),
    );
    broadcaster.take_events();

    let error = svc
        .idmm_continue_active_turn(
            TEST_USER_1,
            &conv.conversation_id,
            &scope_a,
            make_idmm_continuation("must remain on turn A"),
            &runtime_registry,
        )
        .await
        .expect_err("a reservation for turn A must not target turn B");
    assert!(matches!(error, AppError::Conflict(_)));
    assert!(agent_a.steered().is_empty());
    assert!(agent_b.steered().is_empty());
    assert!(repo.messages.lock().unwrap().is_empty());
    assert!(
        broadcaster
            .take_events()
            .iter()
            .all(|event| event.name != "message.userCreated")
    );
    drop(turn_b);
}

#[tokio::test]
async fn idmm_confirm_requires_pending_call_on_exact_reserved_turn() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("running".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let turn = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner(
            &conv.conversation_id,
            Some(MessageId::new().into_string()),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(TEST_USER_1.to_owned()),
            true,
            None,
        )
        .unwrap();
    let agent = Arc::new(
        SteerableAgent::new(
            &conv.conversation_id,
            Some(ConversationStatus::Running),
            true,
        )
        .with_confirmation(Confirmation {
            id: "confirmation-idmm-exact".to_owned(),
            call_id: "call-idmm-exact".to_owned(),
            title: Some("Allow exact action?".to_owned()),
            action: Some("edit_file".to_owned()),
            description: "exact scoped confirmation".to_owned(),
            command_type: None,
            options: vec![],
            screenshot: None,
        }),
    );
    registry.insert_agent(
        &conv.conversation_id,
        AgentRuntimeHandle::Mock(agent.clone()),
    );
    let scope = svc
        .idmm_active_turn_scope(
            TEST_USER_1,
            &conv.conversation_id,
            &runtime_registry,
        )
        .await
        .unwrap();
    broadcaster.take_events();
    let req: ConfirmRequest = serde_json::from_value(json!({
        "msg_id": MessageId::new().into_string(),
        "data": { "value": "allow" },
        "always_allow": false
    }))
    .unwrap();

    svc.idmm_confirm_active_turn(
        TEST_USER_1,
        &conv.conversation_id,
        &scope,
        "call-idmm-exact",
        req,
        &runtime_registry,
    )
    .await
    .unwrap();
    assert_eq!(
        agent.confirmed_call_ids(),
        vec!["call-idmm-exact".to_owned()]
    );
    let removed = broadcaster
        .take_events()
        .into_iter()
        .filter(|event| event.name == "confirmation.remove")
        .collect::<Vec<_>>();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].data["id"], "confirmation-idmm-exact");

    let missing_req: ConfirmRequest = serde_json::from_value(json!({
        "msg_id": MessageId::new().into_string(),
        "data": { "value": "allow" }
    }))
    .unwrap();
    assert!(matches!(
        svc.idmm_confirm_active_turn(
            TEST_USER_1,
            &conv.conversation_id,
            &scope,
            "call-idmm-exact",
            missing_req,
            &runtime_registry,
        )
        .await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(agent.confirmed_call_ids().len(), 1);
    drop(turn);
}

#[tokio::test]
async fn completed_public_steer_replay_is_absorbing_without_finishing_its_parent_turn() {
    const CLIENT_KEY: &str = "public-steer-response-loss-v1";
    const PARENT_OPERATION_ID: &str = "test:public-steer-parent-turn";
    const PARENT_REQUEST_PAYLOAD: &str = r#"{"test":"public-steer-parent"}"#;
    let (svc, repo, broadcaster, registry, conversation_id) =
        make_public_steer_service().await;
    let initial_epoch = repo
        .get_turn_admission_state(SQLITE_TEST_OWNER, &conversation_id)
        .await
        .unwrap()
        .epoch;
    let parent_claim = repo
        .claim_turn_delivery_receipt_and_admit(
            SQLITE_TEST_OWNER,
            &conversation_id,
            PARENT_OPERATION_ID,
            PARENT_REQUEST_PAYLOAD,
            initial_epoch,
            now_ms(),
        )
        .await
        .unwrap();
    assert!(parent_claim.claimed_new);
    let parent_admission_epoch = initial_epoch.checked_add(1).unwrap();
    let turn = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner_with_persistent_generation(
            &conversation_id,
            Some(MessageId::new().into_string()),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(SQLITE_TEST_OWNER.to_owned()),
            false,
            None,
            Some((parent_admission_epoch, PARENT_OPERATION_ID.to_owned())),
        )
        .unwrap();
    let agent = Arc::new(SteerableAgent::new(
        &conversation_id,
        Some(ConversationStatus::Running),
        true,
    ));
    registry.insert_agent(
        &conversation_id,
        AgentRuntimeHandle::Mock(agent.clone()),
    );
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry;
    broadcaster.take_events();

    let first = svc
        .steer_message_with_idempotency_key(
            SQLITE_TEST_OWNER,
            &conversation_id,
            CLIENT_KEY,
            make_execution_steer_req("deliver exactly once"),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(!first.replayed);
    assert!(
        first.completed,
        "steer receipt is terminal after the control effect is persisted"
    );
    assert_eq!(agent.steered(), vec!["deliver exactly once".to_owned()]);
    assert_eq!(
        repo.get_messages(&conversation_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .items
            .len(),
        1
    );

    let replay = svc
        .steer_message_with_idempotency_key(
            SQLITE_TEST_OWNER,
            &conversation_id,
            CLIENT_KEY,
            make_execution_steer_req("deliver exactly once"),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert!(replay.completed);
    assert_eq!(replay.message_id, first.message_id);
    assert_eq!(
        agent.steered(),
        vec!["deliver exactly once".to_owned()],
        "a completed receipt replay must never invoke runtime steer again"
    );
    assert_eq!(
        repo.get_messages(&conversation_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .items
            .len(),
        1,
        "a completed receipt replay must preserve the canonical persisted interjection"
    );
    assert_eq!(
        repo.get(&conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running"),
        "steer receipt completion is not parent turn completion"
    );
    assert!(svc.runtime_state().has_active_turn(&conversation_id));
    assert_eq!(
        broadcaster
            .take_events()
            .iter()
            .filter(|event| event.name == "message.userCreated")
            .count(),
        1,
        "replay must not broadcast a second user message"
    );
    drop(turn);
}

#[tokio::test]
async fn agent_execution_steer_rejects_finished_db_even_with_cached_running_runtime() {
    let (svc, repo, _broadcaster, registry, conversation_id) =
        make_execution_steer_service().await;
    finish_exact_sqlite_turn_for_test(
        repo.as_ref(),
        &conversation_id,
        "execution-steer-finished-fixture",
    )
    .await;
    let stale_turn = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner(
            &conversation_id,
            Some(MessageId::new().into_string()),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(SQLITE_TEST_OWNER.to_owned()),
            false,
            None,
        )
        .unwrap();
    let agent = Arc::new(SteerableAgent::new(
        &conversation_id,
        Some(ConversationStatus::Running),
        true,
    ));
    registry.insert_agent(
        &conversation_id,
        AgentRuntimeHandle::Mock(agent.clone()),
    );
    let operation_id = "execution:steer:finished-cached-runtime";
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry;
    let error = svc
        .agent_execution_port(runtime_registry)
        .steer_turn(
            SQLITE_TEST_OWNER,
            &conversation_id,
            operation_id,
            make_execution_steer_req("must not reopen"),
        )
        .await
        .expect_err("durable Finished must dominate cached runtime state");
    assert!(matches!(error, AppError::Conflict(_)));
    assert!(agent.steered().is_empty());
    assert!(agent.sent_contents().is_empty());
    assert!(
        repo.get_delivery_receipt(
            SQLITE_TEST_OWNER,
            &conversation_id,
            operation_id,
        )
        .await
        .unwrap()
        .is_none(),
        "terminal rejection must occur before receipt election or runtime effects"
    );
    drop(stale_turn);
}

#[tokio::test]
async fn agent_execution_steer_operation_cannot_cross_turn_generation() {
    let (svc, repo, _broadcaster, registry, conversation_id) =
        make_execution_steer_service().await;
    let (parent_a, _, parent_payload_a, parent_epoch_a) =
        claim_background_turn_for_test(
            repo.as_ref(),
            &conversation_id,
            "execution-steer-parent-a",
        )
        .await;
    let wire_a = MessageId::new().into_string();
    let turn_a = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner_with_persistent_generation(
            &conversation_id,
            Some(wire_a.clone()),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(SQLITE_TEST_OWNER.to_owned()),
            false,
            None,
            Some((parent_epoch_a, parent_a.clone())),
        )
        .unwrap();
    let request = make_execution_steer_req("scope-owned control");
    let operation_id = "execution:steer:turn-a-only";
    let request_payload = json!({
        "content": &request.content,
        "files": &request.files,
        "inject_skills": &request.inject_skills,
        "hidden": request.hidden,
        "origin": &request.origin,
        "channel_platform": &request.channel_platform,
        "turn_scope": {
            "wire_turn_id": wire_a,
            "generation": turn_a.turn_id(),
        }
    })
    .to_string();
    repo.claim_delivery_receipt_once(
        SQLITE_TEST_OWNER,
        &conversation_id,
        operation_id,
        "steer",
        &request_payload,
        now_ms(),
    )
    .await
    .unwrap();
    drop(turn_a);

    assert_eq!(
        repo.finalize_exact_turn_operation(
            SQLITE_TEST_OWNER,
            &conversation_id,
            &TurnReceiptCompletion {
                operation_id: parent_a,
                kind: "turn".to_owned(),
                request_payload: parent_payload_a,
                result_ok: false,
                result_text: None,
                result_error: Some("turn A ended".to_owned()),
            },
            now_ms(),
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );
    let (parent_b, _, _, parent_epoch_b) =
        claim_background_turn_for_test(
            repo.as_ref(),
            &conversation_id,
            "execution-steer-parent-b",
        )
        .await;
    let turn_b = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner_with_persistent_generation(
            &conversation_id,
            Some(MessageId::new().into_string()),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(SQLITE_TEST_OWNER.to_owned()),
            false,
            None,
            Some((parent_epoch_b, parent_b)),
        )
        .unwrap();
    let agent_b = Arc::new(SteerableAgent::new(
        &conversation_id,
        Some(ConversationStatus::Running),
        true,
    ));
    registry.insert_agent(
        &conversation_id,
        AgentRuntimeHandle::Mock(agent_b.clone()),
    );
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry;
    let error = svc
        .agent_execution_port(runtime_registry)
        .steer_turn(
            SQLITE_TEST_OWNER,
            &conversation_id,
            operation_id,
            request,
        )
        .await
        .expect_err("turn A operation identity must not target turn B");
    assert!(matches!(error, AppError::Conflict(_)));
    assert!(agent_b.steered().is_empty());
    assert!(agent_b.sent_contents().is_empty());
    assert!(
        repo.get_messages(&conversation_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    drop(turn_b);
}

#[tokio::test]
async fn agent_execution_steer_receipt_replay_is_absorbing_without_runtime_effect() {
    let (svc, repo, broadcaster, registry, conversation_id) =
        make_execution_steer_service().await;
    let (parent_operation, _, _, parent_epoch) =
        claim_background_turn_for_test(
            repo.as_ref(),
            &conversation_id,
            "execution-steer-replay-parent",
        )
        .await;
    let wire_turn_id = MessageId::new().into_string();
    let turn = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner_with_persistent_generation(
            &conversation_id,
            Some(wire_turn_id.clone()),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(SQLITE_TEST_OWNER.to_owned()),
            false,
            None,
            Some((parent_epoch, parent_operation)),
        )
        .unwrap();
    let agent = Arc::new(SteerableAgent::new(
        &conversation_id,
        Some(ConversationStatus::Running),
        true,
    ));
    registry.insert_agent(
        &conversation_id,
        AgentRuntimeHandle::Mock(agent.clone()),
    );
    let request = make_execution_steer_req("already elected");
    let operation_id = "execution:steer:absorbing-replay";
    let request_payload = json!({
        "content": &request.content,
        "files": &request.files,
        "inject_skills": &request.inject_skills,
        "hidden": request.hidden,
        "origin": &request.origin,
        "channel_platform": &request.channel_platform,
        "turn_scope": {
            "wire_turn_id": wire_turn_id,
            "generation": turn.turn_id(),
        }
    })
    .to_string();
    let claim = repo
        .claim_delivery_receipt_once(
            SQLITE_TEST_OWNER,
            &conversation_id,
            operation_id,
            "steer",
            &request_payload,
            now_ms(),
        )
        .await
        .unwrap();
    assert!(claim.claimed_new);
    broadcaster.take_events();

    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry;
    let replayed_message_id = svc
        .agent_execution_port(runtime_registry)
        .steer_turn(
            SQLITE_TEST_OWNER,
            &conversation_id,
            operation_id,
            request,
        )
        .await
        .unwrap();
    assert_eq!(replayed_message_id, claim.receipt.message_id);
    assert!(agent.steered().is_empty());
    assert!(agent.sent_contents().is_empty());
    assert!(
        repo.get_messages(&conversation_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(broadcaster.take_events().is_empty());
    drop(turn);
}

#[tokio::test]
async fn agent_execution_steer_delivers_once_to_exact_active_turn() {
    let (svc, repo, broadcaster, registry, conversation_id) =
        make_execution_steer_service().await;
    let (parent_operation, _, _, parent_epoch) =
        claim_background_turn_for_test(
            repo.as_ref(),
            &conversation_id,
            "execution-steer-delivery-parent",
        )
        .await;
    let wire_turn_id = MessageId::new().into_string();
    let turn = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner_with_persistent_generation(
            &conversation_id,
            Some(wire_turn_id.clone()),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(SQLITE_TEST_OWNER.to_owned()),
            false,
            None,
            Some((parent_epoch, parent_operation)),
        )
        .unwrap();
    let agent = Arc::new(SteerableAgent::new(
        &conversation_id,
        Some(ConversationStatus::Running),
        true,
    ));
    registry.insert_agent(
        &conversation_id,
        AgentRuntimeHandle::Mock(agent.clone()),
    );
    broadcaster.take_events();
    let operation_id = "execution:steer:exact-active";
    let request = make_execution_steer_req("steer this exact turn");
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry;
    let port = svc.agent_execution_port(runtime_registry);
    let message_id = port
        .steer_turn(
            SQLITE_TEST_OWNER,
            &conversation_id,
            operation_id,
            make_execution_steer_req("steer this exact turn"),
        )
        .await
        .unwrap();
    let replayed = port
        .steer_turn(
            SQLITE_TEST_OWNER,
            &conversation_id,
            operation_id,
            request,
        )
        .await
        .unwrap();
    assert_eq!(replayed, message_id);
    assert_eq!(agent.steered(), vec!["steer this exact turn".to_owned()]);
    assert!(
        agent.sent_contents().is_empty(),
        "trusted steer must never fall back to a fresh model turn"
    );

    let receipt = repo
        .get_delivery_receipt(
            SQLITE_TEST_OWNER,
            &conversation_id,
            operation_id,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, "completed");
    assert_eq!(receipt.message_id, message_id);
    let payload: serde_json::Value =
        serde_json::from_str(&receipt.request_payload).unwrap();
    assert_eq!(payload["turn_scope"]["wire_turn_id"], wire_turn_id);
    assert_eq!(payload["turn_scope"]["generation"], turn.turn_id());
    let messages = repo
        .get_messages(&conversation_id, 1, 20, SortOrder::Asc)
        .await
        .unwrap()
        .items;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].message_id, message_id);
    assert_eq!(
        broadcaster
            .take_events()
            .into_iter()
            .filter(|event| event.name == "message.userCreated")
            .count(),
        1
    );
    drop(turn);
}

// ── steer_message tests ─────────────────────────────────────────

/// Happy path: a live, steerable turn → `steer_message` injects mid-turn and
/// does NOT take the normal send path (no fresh turn acquired, no `send_message`
/// on the agent), while still persisting the interjection as a right-bubble
/// user message and broadcasting `message.userCreated`.
#[tokio::test]
async fn steer_message_injects_into_running_turn() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let conv_id = conv.conversation_id.clone();

    // A live turn: status Running, steer accepts (Ok(true)).
    let agent = Arc::new(SteerableAgent::new(&conv_id, Some(ConversationStatus::Running), true));
    runtime_registry.insert_agent(&conv_id, AgentRuntimeHandle::Mock(agent.clone()));

    let req: SendMessageRequest = serde_json::from_value(json!({ "content": "actually, focus on the tests" })).unwrap();
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    let msg_id = svc
        .steer_message(TEST_USER_1, &conv_id, req, &runtime_registry_dyn)
        .await
        .unwrap();

    // Returned a real, persisted user-message id.
    assert!(!msg_id.is_empty(), "msg_id must be non-empty");
    MessageId::parse(&msg_id).expect("msg_id should be a canonical UUIDv7");

    // Routed through the steering inbox, NOT a fresh send.
    assert_eq!(agent.steered(), vec!["actually, focus on the tests".to_owned()]);
    assert!(
        agent.sent_contents().is_empty(),
        "steering must not invoke the agent's send_message (no new turn)"
    );
    // No turn was acquired in runtime state (mid-turn injection, not a new turn).
    assert!(
        !svc.runtime_state().has_active_turn(&conv_id),
        "steering must not acquire a fresh turn"
    );

    // Persisted as a right-bubble user message.
    let stored = repo.messages.lock().unwrap().clone();
    assert_eq!(stored.len(), 1, "the interjection must be persisted exactly once");
    assert_eq!(stored[0].message_id, msg_id);
    assert_eq!(stored[0].position.as_deref(), Some("right"));
    assert_eq!(stored[0].status.as_deref(), Some("finish"));
    assert!(stored[0].content.contains("actually, focus on the tests"));

    // Broadcast message.userCreated for the interjection.
    let events = broadcaster.take_events();
    let created: Vec<_> = events.iter().filter(|e| e.name == "message.userCreated").collect();
    assert_eq!(created.len(), 1, "expected exactly one message.userCreated");
    assert_eq!(created[0].data["msg_id"], msg_id);
    assert_eq!(created[0].data["position"], "right");
    assert_eq!(created[0].data["content"], "actually, focus on the tests");
}

/// Fallback: no live turn (`get_runtime` returns None) → `steer_message` routes
/// through the normal `send_message` path (a fresh turn is acquired + run, the
/// MockAgentRuntimeRegistry builds an agent), so it behaves exactly like `send_message`.
#[tokio::test]
async fn steer_message_without_live_turn_falls_back_to_send() {
    let (svc, _broadcaster, repo, _default_runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let conv_id = conv.conversation_id.clone();

    // No agent registered → get_runtime() returns None → fall back to send_message.
    let req: SendMessageRequest = serde_json::from_value(json!({ "content": "start working" })).unwrap();
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    let msg_id = send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv_id,
        "steer-client-fallback-no-live-turn",
        req,
        &runtime_registry_dyn,
    )
        .await
        .unwrap();
    MessageId::parse(&msg_id).expect("fallback must return a canonical UUIDv7");

    // The fallback's spawned turn acquires then releases its turn handle — proof
    // we went through send_message (steering never acquires a turn). Wait for it to
    // run to completion so the build below has finished.
    wait_for_turn_released(&svc, &conv_id).await;

    // The send path builds an agent for the conversation (MockAgentRuntimeRegistry's
    // get_or_create_runtime) — further proof we took send_message, not steering.
    assert!(
        runtime_registry.get_runtime(&conv_id).is_some(),
        "fallback must run the normal send path (agent built for the turn)"
    );

    // The user message was persisted as a right bubble (send_message shape).
    let stored = repo.messages.lock().unwrap().clone();
    assert!(
        stored
            .iter()
            .any(|m| m.message_id == msg_id && m.position.as_deref() == Some("right")),
        "fallback must persist the user message via send_message"
    );
}

/// Fallback (racy): a live agent that is NOT Running → `steer_message` must
/// NOT attempt to steer and instead fall back to `send_message`.
#[tokio::test]
async fn steer_message_with_non_running_agent_falls_back_to_send() {
    let (svc, _broadcaster, _repo, _default_runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let conv_id = conv.conversation_id.clone();

    // Live agent but status is Finished (turn already over) → no steering.
    let agent = Arc::new(SteerableAgent::new(&conv_id, Some(ConversationStatus::Finished), true));
    runtime_registry.insert_agent(&conv_id, AgentRuntimeHandle::Mock(agent.clone()));

    let req: SendMessageRequest = serde_json::from_value(json!({ "content": "anything" })).unwrap();
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv_id,
        "steer-client-fallback-non-running",
        req,
        &runtime_registry_dyn,
    )
        .await
        .unwrap();

    // Never steered (status was not Running) — it took the send path instead.
    assert!(
        agent.steered().is_empty(),
        "a non-Running agent must not be steered"
    );
    // send_message reuses the existing agent → it received the turn's content.
    wait_for_turn_released(&svc, &conv_id).await;
    assert!(
        !agent.sent_contents().is_empty(),
        "fallback must send the message through the existing agent"
    );
}

/// Race-tail fallback (the duplicate-persist bug): a live agent that IS Running
/// at the status check but whose `steer()` returns `Ok(false)` (the turn ended
/// between the check and the steer). `steer_message` must fall back to
/// `send_message` AND persist the interjection EXACTLY ONCE — `send_message`
/// already persists its own user row, so persisting before the steer (the old
/// ordering) double-wrote. This test fails against persist-first ordering.
#[tokio::test]
async fn steer_message_race_tail_falls_back_and_persists_once() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let conv_id = conv.conversation_id.clone();

    // Running at the status check, but steer() reports the turn already ended.
    let agent = Arc::new(SteerableAgent::new(&conv_id, Some(ConversationStatus::Running), false));
    runtime_registry.insert_agent(&conv_id, AgentRuntimeHandle::Mock(agent.clone()));

    let req: SendMessageRequest =
        serde_json::from_value(json!({ "content": "race-tail interjection" })).unwrap();
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    let legacy_error = svc
        .steer_message(TEST_USER_1, &conv_id, req, &runtime_registry_dyn)
        .await
        .expect_err("race-tail fallback cannot create an unkeyed Running turn");
    assert!(matches!(legacy_error, AppError::Conflict(_)));
    let msg_id = send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv_id,
        "steer-client-race-tail-fallback",
        serde_json::from_value(json!({ "content": "race-tail interjection" })).unwrap(),
        &runtime_registry_dyn,
    )
    .await
    .unwrap();
    MessageId::parse(&msg_id).expect("fallback must return a canonical UUIDv7");

    // steer() was attempted (status was Running) and reported Ok(false)…
    assert_eq!(
        agent.steered(),
        vec!["race-tail interjection".to_owned()],
        "steer must have been attempted (status was Running)"
    );
    // …so it fell back to the normal send path (existing agent received the turn).
    wait_for_turn_released(&svc, &conv_id).await;
    assert!(
        !agent.sent_contents().is_empty(),
        "Ok(false) must fall back to send_message through the existing agent"
    );

    // The interjection is persisted EXACTLY ONCE (send_message's row only).
    // Persist-first ordering would leave two rows with this content.
    let stored = repo.messages.lock().unwrap().clone();
    let with_content: Vec<_> = stored
        .iter()
        .filter(|m| m.content.contains("race-tail interjection"))
        .collect();
    assert_eq!(
        with_content.len(),
        1,
        "the interjection must be persisted exactly once (no double-write); rows = {:?}",
        with_content.iter().map(|m| &m.id).collect::<Vec<_>>()
    );

    // And broadcast exactly once for this content (no duplicate userCreated).
    let events = broadcaster.take_events();
    let created: Vec<_> = events
        .iter()
        .filter(|e| e.name == "message.userCreated" && e.data["content"] == "race-tail interjection")
        .collect();
    assert_eq!(
        created.len(),
        1,
        "expected exactly one message.userCreated for the interjection"
    );
}

/// Non-steerable (`steer_unsupported`) path: a live Running agent whose
/// `steer()` returns `Err(BadRequest)` (non-Nomi engine). `steer_message` must
/// propagate the error and persist NOTHING itself — the client falls back to
/// the pending queue, which sends (and persists) later. Persisting before the
/// steer (the old ordering) would leave an orphan row that the queue then
/// duplicates.
#[tokio::test]
async fn steer_message_unsupported_propagates_and_persists_nothing() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let conv_id = conv.conversation_id.clone();

    // Live Running turn whose engine cannot be steered (Err path).
    let agent = Arc::new(SteerableAgent::new_steer_err(&conv_id));
    runtime_registry.insert_agent(&conv_id, AgentRuntimeHandle::Mock(agent.clone()));

    let req: SendMessageRequest =
        serde_json::from_value(json!({ "content": "unsupported interjection" })).unwrap();
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    let err = svc
        .steer_message(TEST_USER_1, &conv_id, req, &runtime_registry_dyn)
        .await
        .expect_err("a non-steerable engine must surface an error (steer_unsupported)");
    assert!(
        matches!(err, AppError::BadRequest(_)),
        "steer_unsupported must be a BadRequest, got {err:?}"
    );

    // steer() was attempted (status Running) and rejected.
    assert_eq!(agent.steered(), vec!["unsupported interjection".to_owned()]);
    // It did NOT fall back to a fresh send (the client owns the queue fallback).
    assert!(
        agent.sent_contents().is_empty(),
        "the Err path must not send through the agent"
    );
    assert!(
        !svc.runtime_state().has_active_turn(&conv_id),
        "the Err path must not acquire a fresh turn"
    );

    // Persisted NOTHING — the row only exists if/when the caller later sends.
    let stored = repo.messages.lock().unwrap().clone();
    assert!(
        stored.is_empty(),
        "steer_message must persist nothing on the Err path; rows = {:?}",
        stored.iter().map(|m| &m.id).collect::<Vec<_>>()
    );
    // No userCreated broadcast either.
    let events = broadcaster.take_events();
    assert!(
        !events.iter().any(|e| e.name == "message.userCreated"),
        "the Err path must not broadcast message.userCreated"
    );
}

#[tokio::test]
async fn steer_message_with_attachments_is_queued_by_the_client_instead_of_dropped() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let conv_id = conv.conversation_id.clone();
    let agent = Arc::new(SteerableAgent::new(
        &conv_id,
        Some(ConversationStatus::Running),
        true,
    ));
    runtime_registry.insert_agent(&conv_id, AgentRuntimeHandle::Mock(agent.clone()));

    let req: SendMessageRequest = serde_json::from_value(json!({
        "content": "look at this",
        "files": ["C:\\images\\sample.png"]
    }))
    .unwrap();
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry;
    let err = svc
        .steer_message(TEST_USER_1, &conv_id, req, &runtime_registry_dyn)
        .await
        .expect_err("live steering must not silently discard attachments");

    assert!(matches!(
        err,
        AppError::BadRequest(ref message) if message.contains("steer_unsupported")
    ));
    assert!(agent.steered().is_empty());
    assert!(agent.sent_contents().is_empty());
    assert!(repo.messages.lock().unwrap().is_empty());
    assert!(
        !broadcaster
            .take_events()
            .iter()
            .any(|event| event.name == "message.userCreated")
    );
}

#[tokio::test]
async fn send_message_keeps_acp_task_after_normal_finish() {
    let (svc, _broadcaster, _repo, _default_runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let scripted_agent = Arc::new(ScriptedAgent::new(
        &conv.conversation_id,
        vec![vec![AgentStreamEvent::Finish(FinishEventData::default())]],
    ));
    runtime_registry.insert_agent(&conv.conversation_id, AgentRuntimeHandle::Mock(scripted_agent));

    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "keep-acp-runtime-after-finish",
        make_send_req(),
        &runtime_registry_dyn,
    )
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.conversation_id).await;

    assert_eq!(runtime_registry.termination_count(), 0);
    assert_eq!(runtime_registry.active_runtime_count(), 1);
}

#[tokio::test]
async fn send_message_does_not_evict_non_acp_task_after_terminal_error() {
    let (svc, _broadcaster, _repo, _default_runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let scripted_agent = Arc::new(
        ScriptedAgent::new(
            &conv.conversation_id,
            vec![vec![AgentStreamEvent::Error(ErrorEventData::legacy(
                "nomi terminal error",
                Some(AgentErrorCode::UnknownUpstreamError),
            ))]],
        )
        .with_agent_type(AgentType::Nomi),
    );
    runtime_registry.insert_agent(&conv.conversation_id, AgentRuntimeHandle::Mock(scripted_agent));

    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "keep-non-acp-runtime-after-error",
        make_send_req(),
        &runtime_registry_dyn,
    )
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.conversation_id).await;

    assert_eq!(runtime_registry.termination_count(), 0);
    assert_eq!(runtime_registry.active_runtime_count(), 1);
}

// ── stop_stream tests ───────────────────────────────────────────

#[tokio::test]
async fn stop_stream_with_active_agent() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    // Build agent via send_message
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> =
        runtime_registry.clone();
    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv.conversation_id,
        "stop-stream-active-agent",
        make_send_req(),
        &runtime_registry_dyn,
    )
    .await
    .unwrap();

    // Stop should succeed since agent exists
    let result = svc
        .cancel(TEST_USER_1, &conv.conversation_id, &(runtime_registry as Arc<dyn AgentRuntimeRegistry>))
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn stop_stream_conversation_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let err = svc.cancel(TEST_USER_1, "no-such-id", &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn stop_stream_no_active_agent_is_idempotent() {
    let (svc, broadcaster, repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    broadcaster.take_events();

    let result = svc.cancel(TEST_USER_1, &conv.conversation_id, &runtime_registry).await;
    assert!(result.is_ok());
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("pending"),
        "an idle Pending conversation has no business turn to complete"
    );
    assert!(
        !broadcaster
            .take_events()
            .into_iter()
            .any(|event| event.name == "turn.completed"),
        "an idle Pending stop must not fabricate a completion event"
    );
}

#[tokio::test]
async fn cold_running_orphan_rejects_user_stop_and_delete_without_mutation() {
    const CLIENT_KEY: &str = "cold-running-orphan-stop-delete";
    let (service, repo, registry, runtime_registry, _database, conversation_id) =
        background_reconciliation_fixture(
            "cold-running-orphan-stop-delete",
            Arc::new(crate::NoExecutionConversationBoundary),
        )
        .await;
    let (operation_id, message_id, _payload, admitted_epoch) =
        claim_background_turn_for_test(repo.as_ref(), &conversation_id, CLIENT_KEY).await;
    let cancel_probe_started_at = now_ms();

    let stop_error = service
        .cancel(
            SQLITE_TEST_OWNER,
            &conversation_id,
            &runtime_registry,
        )
        .await
        .expect_err("a cold Running orphan has no local stop authority");
    assert!(matches!(stop_error, AppError::Conflict(_)));
    assert!(
        !service.user_cancelled_since(&conversation_id, cancel_probe_started_at),
        "a rejected orphan stop must not create a user-cancel stamp"
    );

    let delete_error = service
        .delete(SQLITE_TEST_OWNER, &conversation_id)
        .await
        .expect_err("delete must not bypass restart-orphan quarantine");
    assert!(matches!(delete_error, AppError::Conflict(_)));

    let row = repo
        .get(&conversation_id)
        .await
        .unwrap()
        .expect("quarantined Conversation must remain present");
    assert_eq!(row.status.as_deref(), Some("running"));
    let admission = repo
        .get_turn_admission_state(SQLITE_TEST_OWNER, &conversation_id)
        .await
        .unwrap();
    assert_eq!(admission.epoch, admitted_epoch);
    assert_eq!(
        admission.active_operation_id.as_deref(),
        Some(operation_id.as_str())
    );
    let receipt = repo
        .get_delivery_receipt(
            SQLITE_TEST_OWNER,
            &conversation_id,
            &operation_id,
        )
        .await
        .unwrap()
        .expect("accepted receipt must remain the exact quarantine authority");
    assert_eq!(receipt.status, "accepted");
    assert_eq!(receipt.message_id, message_id);
    assert_eq!(receipt.result_ok, None);
    assert_eq!(registry.build_calls(), 0);
    assert!(!service.runtime_state().has_active_turn(&conversation_id));
}

#[tokio::test]
async fn stop_keeps_running_fenced_until_runtime_exit_is_proven() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let conv_id = conv.conversation_id.clone();

    repo.update(
        &conv_id,
        &ConversationRowUpdate {
            status: Some("running".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let turn = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner(
            &conv_id,
            Some(MessageId::new().into_string()),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(TEST_USER_1.to_owned()),
            true,
            None,
        )
        .unwrap();
    let cancelled = turn.cancellation_token();
    registry.insert_agent(
        &conv_id,
        AgentRuntimeHandle::Mock(Arc::new(MockAgent::new(&conv_id))),
    );
    registry.block_termination_wait(true);
    broadcaster.take_events();

    let cancel_task = {
        let service = svc.clone();
        let conversation_id = conv_id.clone();
        let runtime_registry = Arc::clone(&runtime_registry);
        tokio::spawn(async move {
            service
                .cancel(TEST_USER_1, &conversation_id, &runtime_registry)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), cancelled.cancelled())
        .await
        .expect("stop should cancel the exact active generation");
    tokio::time::timeout(Duration::from_secs(2), async {
        while registry.termination_wait_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stop should attempt result-bearing runtime teardown");

    assert_eq!(
        repo.get(&conv_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running"),
        "a failed kill is not proof of exit and must not commit Finished"
    );
    assert!(
        svc.runtime_state().has_active_turn(&conv_id),
        "the exact turn owner/release fence must survive teardown failure"
    );
    assert!(
        runtime_registry.has_registered_runtime(&conv_id),
        "the failed runtime must remain registered/quarantined"
    );
    assert!(
        !broadcaster
            .take_events()
            .into_iter()
            .any(|event| event.name == "turn.completed"),
        "turn.completed must be withheld until both exit proof and DB commit"
    );

    // Let the turn owner quiesce, then allow the next teardown retry to prove
    // process exit. Only that retry may advance Running -> Finished/release.
    drop(turn);
    registry.block_termination_wait(false);
    tokio::time::timeout(Duration::from_secs(6), cancel_task)
        .await
        .expect("stop should finish after teardown recovery")
        .expect("stop task should not panic")
        .expect("stop should succeed after teardown recovery");

    assert_eq!(
        repo.get(&conv_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    assert!(!svc.runtime_state().has_active_turn(&conv_id));
    assert!(!runtime_registry.has_registered_runtime(&conv_id));
    assert_eq!(
        broadcaster
            .take_events()
            .into_iter()
            .filter(|event| event.name == "turn.completed")
            .count(),
        1,
        "the terminal event is emitted exactly once after durable closure"
    );
}

#[tokio::test]
async fn stop_repairs_accepted_turn_receipt_in_same_terminal_commit() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    const OPERATION_ID: &str = "turn:stop-repairs-accepted";

    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry;
    let svc = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    let conv_id = conv.conversation_id.clone();
    let request_payload = r#"{"content":"accepted but interrupted"}"#;
    let initial = repo
        .get_turn_admission_state(USER_ID, &conv_id)
        .await
        .unwrap();
    let claim = repo
        .claim_turn_delivery_receipt_and_admit_with_candidate(
            USER_ID,
            &conv_id,
            OPERATION_ID,
            &MessageId::new().into_string(),
            request_payload,
            initial.epoch,
            now_ms(),
        )
        .await
        .unwrap();
    assert!(claim.claimed_new);
    let accepted = claim.receipt;
    assert_eq!(accepted.status, "accepted");
    let turn = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner_with_persistent_generation(
            &conv_id,
            Some(accepted.message_id.clone()),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(USER_ID.to_owned()),
            true,
            None,
            Some((initial.epoch + 1, OPERATION_ID.to_owned())),
        )
        .unwrap();
    let cancelled = turn.cancellation_token();
    broadcaster.take_events();

    let cancel_task = {
        let service = svc.clone();
        let conversation_id = conv_id.clone();
        let runtime_registry = Arc::clone(&runtime_registry);
        tokio::spawn(async move {
            service
                .cancel(USER_ID, &conversation_id, &runtime_registry)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), cancelled.cancelled())
        .await
        .expect("stop should cancel the receipt-owning turn");
    drop(turn);
    tokio::time::timeout(Duration::from_secs(6), cancel_task)
        .await
        .expect("stop should complete")
        .expect("stop task should not panic")
        .unwrap();

    let repaired = repo
        .get_delivery_receipt(USER_ID, &conv_id, OPERATION_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repaired.status, "completed");
    assert_eq!(repaired.result_ok, Some(false));
    assert!(
        repaired
            .result_error
            .as_deref()
            .is_some_and(|error| error.contains("cancelled after runtime exit"))
    );
    assert_eq!(
        repo.get(&conv_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    assert_eq!(
        broadcaster
            .take_events()
            .into_iter()
            .filter(|event| event.name == "turn.completed")
            .count(),
        1
    );
}

#[tokio::test]
async fn execution_cleanup_does_not_record_a_user_cancel_stamp() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let since = nomifun_common::now_ms();

    svc.cancel_for_execution(
        TEST_USER_1,
        &conv.conversation_id,
        &runtime_registry,
    )
    .await
    .unwrap();

    assert!(!svc.user_cancelled_since(&conv.conversation_id, since));
}

#[tokio::test]
async fn stop_stream_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let err = svc.cancel(TEST_USER_2, &conv.conversation_id, &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

// ── warmup tests ────────────────────────────────────────────────

#[tokio::test]
async fn view_warmup_creates_agent_runtime_for_empty_pending_conversation() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let workspace = tempfile::tempdir().unwrap();
    let request: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "agent_id": TEST_ACP_AGENT_ID,
            "workspace": workspace.path()
        }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, request).await.unwrap();

    let result = svc
        .warmup_for_view(
            TEST_USER_1,
            &conv.conversation_id,
            &(runtime_registry.clone() as Arc<dyn AgentRuntimeRegistry>),
        )
        .await;
    assert!(result.is_ok(), "pending view warmup failed: {result:?}");

    // Agent should now exist
    assert!(runtime_registry.get_runtime(&conv.conversation_id).is_some());
}

#[tokio::test]
async fn view_warmup_of_finished_conversation_never_builds_or_emits_turn_events() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(500)));

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("finished".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    broadcaster.take_events();

    svc.warmup_for_view(
        TEST_USER_1,
        &conv.conversation_id,
        &(slow_registry.clone() as Arc<dyn AgentRuntimeRegistry>),
    )
    .await
    .unwrap();
    assert_eq!(
        slow_registry.build_calls(),
        0,
        "viewing a completed conversation must not create or resume an Agent runtime"
    );
    assert!(!slow_registry.was_built());

    let fetched = svc.get(TEST_USER_1, &conv.conversation_id).await.unwrap();
    assert_eq!(fetched.status, ConversationStatus::Finished);
    let runtime = fetched.runtime.expect("runtime summary");
    assert_eq!(
        runtime.state,
        nomifun_api_types::ConversationRuntimeStateKind::Idle
    );
    assert!(!runtime.is_processing);
    assert!(runtime.can_send_message);
    assert_eq!(runtime.processing_started_at, None);
    assert!(!svc.runtime_state().has_active_turn(&conv.conversation_id));

    assert!(
        broadcaster
            .take_events()
            .iter()
            .all(|event| !matches!(event.name.as_str(), "turn.started" | "turn.completed")),
        "runtime preparation must not publish business-turn lifecycle events"
    );
}

#[tokio::test]
async fn view_warmup_quarantines_cached_idle_runtime_without_terminal_proof() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conversation = service
        .create(USER_ID, make_create_req())
        .await
        .unwrap();
    let operation_id = "turn:cached-idle-runtime-orphan";
    let request_payload = r#"{"content":"already completed externally"}"#;
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        request_payload,
        0,
        now_ms(),
    )
    .await
    .unwrap();
    registry.insert_agent(
        &conversation.conversation_id,
        AgentRuntimeHandle::Mock(Arc::new(MockAgent::new(&conversation.conversation_id))),
    );
    assert!(!service.runtime_state().has_active_turn(&conversation.conversation_id));
    broadcaster.take_events();

    let error = service
        .warmup_for_view(
            USER_ID,
            &conversation.conversation_id,
            &runtime_registry,
        )
        .await
        .expect_err("an idle cache entry is not durable proof that the prior process tree exited");
    assert!(matches!(error, AppError::Conflict(_)));

    assert_eq!(registry.termination_wait_count(), 0);
    assert!(
        registry.get_runtime(&conversation.conversation_id).is_some(),
        "read-only navigation must leave the ambiguous runtime quarantined"
    );
    let row = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status.as_deref(), Some("running"));
    let state = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(
        state.active_operation_id.as_deref(),
        Some(operation_id)
    );
    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, "accepted");
    assert_eq!(receipt.result_ok, None);
    assert_eq!(
        broadcaster
            .take_events()
            .iter()
            .filter(|event| event.name == "turn.completed")
            .count(),
        0
    );
}

#[tokio::test]
async fn restart_view_recovers_only_the_unadmitted_edit_reservation_cutpoint() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(250)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conversation = service
        .create(USER_ID, make_create_req())
        .await
        .unwrap();
    let target_message_id = MessageId::new().into_string();
    repo.insert_message(&MessageRow {
        id: 0,
        message_id: target_message_id.clone(),
        conversation_id: conversation.conversation_id.clone(),
        msg_id: Some(target_message_id.clone()),
        r#type: "text".to_owned(),
        content: json!({ "content": "original" }).to_string(),
        position: Some("right".to_owned()),
        status: Some("finish".to_owned()),
        hidden: false,
        created_at: now_ms(),
    })
    .await
    .unwrap();
    finish_exact_sqlite_turn_for_test(
        repo.as_ref(),
        &conversation.conversation_id,
        "restart-edit-fixture-finished",
    )
    .await;
    let operation_id = format!(
        "public-edit-resubmit:v1:{USER_ID}:{}:restart-cutpoint",
        conversation.conversation_id
    );
    let request_payload = json!({
        "workflow": "edit-resubmit",
        "target_message_id": &target_message_id,
        "content": "replacement",
    })
    .to_string();
    let initial = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    let candidate_message_id = MessageId::new().into_string();
    repo.claim_edit_resubmit_receipt_and_fence(
        USER_ID,
        &conversation.conversation_id,
        &operation_id,
        &candidate_message_id,
        &request_payload,
        &target_message_id,
        initial.epoch,
        now_ms(),
    )
    .await
    .unwrap();
    let reserved = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    drop(service);

    let restarted = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry,
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    restarted
        .warmup_for_view(
            USER_ID,
            &conversation.conversation_id,
            &(registry.clone() as Arc<dyn AgentRuntimeRegistry>),
        )
        .await
        .unwrap();

    assert_eq!(registry.build_calls(), 0);
    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, &operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, "completed");
    assert_eq!(receipt.result_ok, Some(false));
    let state = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(state.epoch, reserved.epoch + 1);
    assert_eq!(state.active_operation_id, None);
    let row = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status.as_deref(), Some("finished"));
    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
    assert!(extra.get("_edit_resubmit_fence").is_none());
    assert!(
        broadcaster
            .take_events()
            .iter()
            .all(|event| !matches!(event.name.as_str(), "turn.started" | "turn.completed"))
    );
}

#[tokio::test]
async fn edit_rewind_then_transcript_delete_failure_quarantines_runtime_before_fence_release() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    const EDIT_KEY: &str = "edit-delete-failure";
    let database = init_database_memory().await.unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO providers (\
            provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
            capabilities, created_at, updated_at\
         ) VALUES (?1, 'openai', 'edit fixture', 'https://example.invalid', \
                   'encrypted', '[\"m1\"]', 1, '[]', 1, 1)",
    )
    .bind(PROVIDER_ID_1)
    .execute(database.pool())
    .await
    .unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conversation = service
        .create(
            USER_ID,
            serde_json::from_value(json!({
                "type": "nomi",
                "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
                "extra": { "workspace": isolated_test_workspace("edit-delete-failure") }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let target_message_id = MessageId::new().into_string();
    repo.insert_message(&MessageRow {
        id: 0,
        message_id: target_message_id.clone(),
        conversation_id: conversation.conversation_id.clone(),
        msg_id: Some(target_message_id.clone()),
        r#type: "text".to_owned(),
        content: json!({ "content": "original" }).to_string(),
        position: Some("right".to_owned()),
        status: Some("finish".to_owned()),
        hidden: false,
        created_at: now_ms(),
    })
    .await
    .unwrap();
    finish_exact_sqlite_turn_for_test(
        repo.as_ref(),
        &conversation.conversation_id,
        "edit-delete-fixture-finished",
    )
    .await;
    let row = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap();
    registry.insert_agent(
        &conversation.conversation_id,
        AgentRuntimeHandle::Mock(Arc::new(MockAgent::new(&conversation.conversation_id))),
    );
    registry.seed_persisted_nomi_context(
        &conversation.conversation_id,
        row.created_at,
        vec!["old resumable turn".to_owned()],
    );
    nomifun_db::sqlx::query(
        "CREATE TRIGGER inject_edit_transcript_delete_failure \
         BEFORE DELETE ON messages \
         BEGIN SELECT RAISE(ABORT, 'injected edit transcript delete failure'); END",
    )
    .execute(database.pool())
    .await
    .unwrap();

    let error = service
        .edit_and_resubmit_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            &target_message_id,
            EDIT_KEY,
            serde_json::from_value(json!({"content": "replacement"})).unwrap(),
            &runtime_registry,
        )
        .await
        .expect_err("the injected transcript delete must fail after rewind");
    eprintln!("edit-failure-test: after edit");
    assert!(error.to_string().contains("injected edit transcript delete failure"));
    assert!(
        registry.termination_wait_count() >= 1,
        "rewind crossing requires result-bearing process exit proof"
    );
    assert!(
        registry.get_runtime(&conversation.conversation_id).is_none(),
        "the rewound runtime must be removed rather than reused"
    );
    assert_eq!(
        registry.persisted_nomi_context(&conversation.conversation_id, row.created_at),
        Vec::<String>::new(),
        "persisted Nomi recovery authority must be erased before the edit fence opens"
    );
    assert!(
        registry
            .nomi_reset_records()
            .iter()
            .any(|record| record == &(conversation.conversation_id.clone(), row.created_at))
    );
    let operation_id = format!(
        "public-edit-resubmit:v1:{USER_ID}:{}:{EDIT_KEY}",
        conversation.conversation_id
    );
    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, &operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, "completed");
    assert_eq!(receipt.result_ok, Some(false));
    let terminal = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(terminal.status.as_deref(), Some("finished"));
    assert!(
        serde_json::from_str::<serde_json::Value>(&terminal.extra)
            .unwrap()
            .get("_edit_resubmit_fence")
            .is_none()
    );
    assert!(
        repo.get_turn_admission_state(USER_ID, &conversation.conversation_id)
            .await
            .unwrap()
            .active_operation_id
            .is_none()
    );
    assert_eq!(
        registry.build_count(),
        0,
        "the failed edit must not start a replacement model turn"
    );

    eprintln!("edit-failure-test: before fresh send");
    let fresh = service
        .send_message_with_idempotency_key(
            USER_ID,
            &conversation.conversation_id,
            "fresh-after-failed-edit",
            serde_json::from_value(json!({"content": "new explicit turn"})).unwrap(),
            &runtime_registry,
        )
        .await
        .unwrap();
    eprintln!("edit-failure-test: after fresh send");
    assert!(!fresh.replayed);
    wait_for_turn_released(&service, &conversation.conversation_id).await;
    assert_eq!(
        registry.build_count(),
        1,
        "the next explicit turn must rebuild instead of reusing the rewound runtime"
    );
}

#[tokio::test]
async fn view_recovery_cannot_cancel_a_live_edit_request_between_reserve_and_admit() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(250)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let service = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let conversation = service
        .create(USER_ID, make_create_req())
        .await
        .unwrap();
    let target_message_id = MessageId::new().into_string();
    repo.insert_message(&MessageRow {
        id: 0,
        message_id: target_message_id.clone(),
        conversation_id: conversation.conversation_id.clone(),
        msg_id: Some(target_message_id.clone()),
        r#type: "text".to_owned(),
        content: json!({ "content": "original" }).to_string(),
        position: Some("right".to_owned()),
        status: Some("finish".to_owned()),
        hidden: false,
        created_at: now_ms(),
    })
    .await
    .unwrap();
    finish_exact_sqlite_turn_for_test(
        repo.as_ref(),
        &conversation.conversation_id,
        "live-edit-fixture-finished",
    )
    .await;
    let gate_token = tokio_util::sync::CancellationToken::new();
    let live_edit_gate = service
        .runtime_state()
        .acquire_preparation_gate(&conversation.conversation_id, &gate_token)
        .await
        .unwrap();
    let operation_id = format!(
        "public-edit-resubmit:v1:{USER_ID}:{}:live-reservation",
        conversation.conversation_id
    );
    let initial = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    let request_payload = json!({
        "workflow": "edit-resubmit",
        "target_message_id": &target_message_id,
        "content": "replacement",
    })
    .to_string();
    let candidate_message_id = MessageId::new().into_string();
    repo.claim_edit_resubmit_receipt_and_fence(
        USER_ID,
        &conversation.conversation_id,
        &operation_id,
        &candidate_message_id,
        &request_payload,
        &target_message_id,
        initial.epoch,
        now_ms(),
    )
    .await
    .unwrap();
    let reserved = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();

    let warmup_service = service.clone();
    let warmup_registry = runtime_registry.clone();
    let warmup_conversation_id = conversation.conversation_id.clone();
    let warmup = tokio::spawn(async move {
        warmup_service
            .warmup_for_view(
                USER_ID,
                &warmup_conversation_id,
                &warmup_registry,
            )
            .await
    });
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert!(
        !warmup.is_finished(),
        "view recovery must wait behind the live edit request's preparation gate"
    );
    assert!(
        repo.admit_reserved_edit_turn(
            USER_ID,
            &conversation.conversation_id,
            &operation_id,
            &request_payload,
            reserved.epoch,
            now_ms() + 1,
        )
        .await
        .unwrap(),
        "the live reservation must retain admission authority"
    );
    drop(live_edit_gate);

    assert!(matches!(
        warmup.await.unwrap(),
        Err(AppError::Conflict(_))
    ));
    let admitted = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(admitted.active_operation_id.as_deref(), Some(operation_id.as_str()));
    assert_eq!(
        repo.get_delivery_receipt(USER_ID, &conversation.conversation_id, &operation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "accepted",
        "the waiting view must not settle the live edit receipt"
    );
}

#[tokio::test]
async fn view_warmup_of_finished_writeback_session_never_builds_or_reconciles_mounts() {
    const USER_ID: &str = SQLITE_TEST_OWNER;
    const CLIENT_KEY: &str = "finished-writeback-remount-exact-turn";

    let database = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
    let broadcaster = Arc::new(MockBroadcaster::new());
    let registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = registry.clone();
    let workspace = unique_test_dir("finished-writeback-workspace");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    let data_dir = unique_test_dir("finished-writeback-data");
    let svc = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry.clone(),
        repo.clone(),
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    let knowledge_db = nomifun_db::init_database_memory().await.unwrap();
    let knowledge_owner = nomifun_db::installation_owner_id(knowledge_db.pool())
        .await
        .unwrap();
    let knowledge_repo: Arc<dyn nomifun_db::IKnowledgeRepository> = Arc::new(
        nomifun_db::SqliteKnowledgeRepository::new(knowledge_db.pool().clone()),
    );
    let knowledge = Arc::new(KnowledgeService::new(
        knowledge_repo,
        &data_dir,
        KnowledgeEventEmitter::new(broadcaster.clone(), Arc::from(USER_ID)),
    ));
    svc.with_knowledge_service(knowledge.clone());
    svc.with_failover_deps(
        Arc::new(StubProviderRepo::new(vec![test_provider(
            PROVIDER_ID_1,
            &["knowledge-model"],
        )])),
        Arc::new(FixedClientPrefRepo {
            preferences: vec![ClientPreference {
                id: 1,
                key: "knowledge.autogenModel".into(),
                value: json!({
                    "provider_id": PROVIDER_ID_1,
                    "model": "knowledge-model"
                })
                .to_string(),
                updated_at: 1,
            }],
        }),
    );

    let request: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "agent_id": TEST_ACP_AGENT_ID,
            "workspace": workspace
        }
    }))
    .unwrap();
    let conv = svc.create(USER_ID, request).await.unwrap();
    // The knowledge database owns its own installation identity. Seed the
    // corresponding aggregate exactly as production assembly does so binding
    // and writeback persistence exercise the real repository constraints.
    nomifun_db::sqlx::query(
        "INSERT INTO conversations (conversation_id, user_id, name, type, status, created_at, updated_at) \
         VALUES (?, ?, 'finished-writeback', 'acp', 'pending', 1, 1)",
    )
    .bind(&conv.conversation_id)
    .bind(&knowledge_owner)
    .execute(knowledge_db.pool())
    .await
    .unwrap();
    let kb = knowledge
        .create_base("finished-writeback", "", None, None)
        .await
        .unwrap();
    let binding = KnowledgeBinding {
        enabled: true,
        writeback: true,
        writeback_mode: "direct".into(),
        writeback_eagerness: "aggressive".into(),
        kb_ids: vec![kb.knowledge_base_id.clone()],
        ..Default::default()
    };
    let workpath_key =
        nomifun_knowledge::session_workpath_key(&workspace, &std::env::temp_dir());
    knowledge
        .set_binding("workpath", &workpath_key, binding.clone())
        .await
        .unwrap();

    // Materialize the exact mount state that existed while the completed turn
    // was running. The base is still empty, so both its summary and TOC are
    // empty before the turn-final writeback.
    let initial_plan = knowledge
        .prepare_mounts_for_session(&workpath_key, &workspace)
        .await
        .unwrap();
    let initial_signature = initial_plan.binding_signature().to_owned();
    let initial_mounts = initial_plan.outcome().mounts.clone();
    assert_eq!(initial_mounts.len(), 1);
    assert!(initial_mounts[0].summary.is_none());
    assert!(initial_mounts[0].toc.is_empty());
    let (_initial_outcome, initial_lease) = initial_plan
        .activate(&conv.conversation_id)
        .await
        .unwrap();
    drop(initial_lease);

    // Exercise production keyed admission and exact terminal finalization.
    // The scripted runtime emits a real assistant segment, which drives the
    // service-owned turn-final writeback before the accepted receipt and
    // Running aggregate are atomically closed.
    let candidate = format!(
        r##"{{"candidates":[{{"kb_id":"{}","rel_path":"README.md","content":"# Closed loop\n\nDurable writeback summary."}},{{"kb_id":"{}","rel_path":"writeback-evidence.md","content":"# Writeback evidence\n\nThe completed turn was persisted."}}]}}"##,
        kb.knowledge_base_id, kb.knowledge_base_id
    );
    knowledge.set_completer(Arc::new(RecordingKnowledgeCompleter::new(candidate)));
    svc.warmup_for_view(USER_ID, &conv.conversation_id, &runtime_registry)
        .await
        .expect("initial Pending view prepares the exact knowledge/runtime binding");
    let scripted_agent = Arc::new(
        ScriptedAgent::new(
            &conv.conversation_id,
            vec![vec![
                AgentStreamEvent::Text(TextEventData {
                    content: "The task is complete.".into(),
                }),
                AgentStreamEvent::Finish(FinishEventData::default()),
            ]],
        )
        .with_workspace(workspace.to_string_lossy()),
    );
    registry.insert_agent(
        &conv.conversation_id,
        AgentRuntimeHandle::Mock(scripted_agent),
    );
    broadcaster.take_events();
    let delivery = svc
        .send_message_with_idempotency_key(
            USER_ID,
            &conv.conversation_id,
            CLIENT_KEY,
            serde_json::from_value(json!({
                "content": "Record the durable conclusion."
            }))
            .unwrap(),
            &runtime_registry,
        )
        .await;
    let delivery = delivery.expect("production keyed turn admission");
    assert!(!delivery.replayed);
    wait_for_turn_released(&svc, &conv.conversation_id).await;

    let operation_id = format!(
        "public-turn:v1:{USER_ID}:{}:{CLIENT_KEY}",
        conv.conversation_id
    );
    let receipt = repo
        .get_delivery_receipt(USER_ID, &conv.conversation_id, &operation_id)
        .await
        .unwrap()
        .expect("exact turn receipt");
    assert_eq!(receipt.status, "completed");
    assert_eq!(receipt.message_id, delivery.message_id);
    assert!(
        receipt.result_ok.is_some(),
        "the exact finalizer must persist a typed terminal receipt outcome"
    );
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished"),
        "the exact keyed finalizer, not a generic status shortcut, must close the aggregate"
    );

    let mutated_plan = knowledge
        .prepare_mounts_for_session(&workpath_key, &workspace)
        .await
        .unwrap();
    assert_eq!(
        mutated_plan.binding_signature(),
        initial_signature,
        "README/TOC writeback is content mutation, not a mount-binding change"
    );
    let mutated_mount = &mutated_plan.outcome().mounts[0];
    assert_eq!(
        mutated_mount.summary.as_deref(),
        Some("Durable writeback summary.")
    );
    assert!(
        mutated_mount
            .toc
            .iter()
            .any(|line| line.contains("writeback-evidence.md")),
        "{:?}",
        mutated_mount.toc
    );

    // A mount reconciliation sweeps every unrecognized entry. Keeping this
    // canary proves that opening a Finished conversation returned before
    // knowledge preparation/activation, even if a future registry mock were to
    // hide an attempted runtime build.
    let mount_root = workspace.join(nomifun_knowledge::KB_MOUNT_REL_DIR);
    let reconcile_canary = mount_root.join("view-warmup-must-not-reconcile");
    tokio::fs::write(&reconcile_canary, b"closed-loop")
        .await
        .unwrap();
    let builds_before_remount = registry.build_count();
    broadcaster.take_events();

    svc.warmup_for_view(
        USER_ID,
        &conv.conversation_id,
        &runtime_registry,
    )
    .await
    .unwrap();

    assert_eq!(
        registry.build_count(),
        builds_before_remount,
        "viewing a Finished writeback session must not construct a runtime"
    );
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    assert_eq!(
        tokio::fs::read(&reconcile_canary).await.unwrap(),
        b"closed-loop",
        "view warmup must not reconcile or sweep the completed session's mount namespace"
    );
    assert!(
        broadcaster
            .take_events()
            .iter()
            .all(|event| !matches!(event.name.as_str(), "turn.started" | "turn.completed")),
        "viewing a Finished writeback session must not emit turn lifecycle events"
    );

    drop(mutated_plan);
    let _ = tokio::fs::remove_dir_all(&workspace).await;
    let _ = tokio::fs::remove_dir_all(&data_dir).await;
}

#[tokio::test]
async fn view_warmup_keeps_cold_acp_orphan_quarantined_without_building() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::ZERO));
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            status: Some("running".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    svc.warmup_for_view(
        TEST_USER_1,
        &conv.conversation_id,
        &(registry.clone() as Arc<dyn AgentRuntimeRegistry>),
    )
    .await
    .expect_err("view warmup cannot prove a prior ACP process tree is empty");

    assert_eq!(registry.build_calls(), 0);
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running"),
        "a persisted Running row with no queryable process proof stays quarantined"
    );
    assert!(
        !broadcaster
            .take_events()
            .into_iter()
            .any(|event| event.name == "turn.completed"),
        "restart quarantine must not publish a fabricated completion"
    );
}

#[tokio::test]
async fn view_warmup_keeps_external_gateway_orphan_running_until_terminal_is_proven() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::ZERO));
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    {
        let mut rows = repo.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|row| row.conversation_id == conv.conversation_id)
            .unwrap();
        row.r#type = AgentType::OpenclawGateway.serde_name().to_owned();
        row.status = Some("running".to_owned());
    }
    broadcaster.take_events();

    let error = svc
        .warmup_for_view(
            TEST_USER_1,
            &conv.conversation_id,
            &(registry.clone() as Arc<dyn AgentRuntimeRegistry>),
        )
        .await
        .expect_err("view warmup must fail closed for an unproven external gateway turn");

    assert!(matches!(error, AppError::Conflict(_)));
    assert_eq!(registry.build_calls(), 0);
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );
    assert!(
        !broadcaster
            .take_events()
            .iter()
            .any(|event| event.name == "turn.completed")
    );
}

#[tokio::test]
async fn view_warmup_keeps_remote_orphan_running_until_terminal_is_proven() {
    let (svc, broadcaster, repo, _default_runtime_registry) = make_service();
    let registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::ZERO));
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    {
        let mut rows = repo.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|row| row.conversation_id == conv.conversation_id)
            .unwrap();
        row.r#type = AgentType::Remote.serde_name().to_owned();
        row.status = Some("running".to_owned());
    }
    broadcaster.take_events();

    let error = svc
        .warmup_for_view(
            TEST_USER_1,
            &conv.conversation_id,
            &(registry.clone() as Arc<dyn AgentRuntimeRegistry>),
        )
        .await
        .expect_err("view warmup must not rebuild an unproven remote turn");

    assert!(matches!(error, AppError::Conflict(_)));
    assert_eq!(registry.build_calls(), 0);
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );
    assert!(
        !broadcaster
            .take_events()
            .iter()
            .any(|event| event.name == "turn.completed")
    );
}

#[tokio::test]
async fn view_warmup_treats_pending_history_as_started_after_failed_finished_write() {
    let (svc, _broadcaster, repo, _default_runtime_registry) = make_service();
    let registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::ZERO));
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.insert_message(&MessageRow {
        id: 0,
        message_id: MessageId::new().into_string(),
        conversation_id: conv.conversation_id.clone(),
        msg_id: None,
        r#type: "text".into(),
        content: json!({ "content": "durable completed transcript" }).to_string(),
        position: Some("left".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: now_ms(),
    })
    .await
    .unwrap();
    assert_eq!(
        repo.get(&conv.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("pending"),
        "fixture models a failed terminal status write with durable history"
    );

    svc.warmup_for_view(
        TEST_USER_1,
        &conv.conversation_id,
        &(registry.clone() as Arc<dyn AgentRuntimeRegistry>),
    )
    .await
    .unwrap();

    assert_eq!(
        registry.build_calls(),
        0,
        "durable history independently proves this is not a never-started session"
    );
}

#[tokio::test]
async fn view_warmup_fails_closed_when_transcript_emptiness_cannot_be_read() {
    let (svc, _broadcaster, repo, _default_runtime_registry) = make_service();
    let registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::ZERO));
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.fail_next_messages_keyset_read();

    let result = svc
        .warmup_for_view(
            TEST_USER_1,
            &conv.conversation_id,
            &(registry.clone() as Arc<dyn AgentRuntimeRegistry>),
        )
        .await;

    assert!(result.is_err());
    assert_eq!(
        registry.build_calls(),
        0,
        "an unknown transcript state must never be interpreted as empty"
    );
}

#[tokio::test]
async fn view_warmup_runtime_preparation_never_claims_business_processing() {
    let (svc, broadcaster, _repo, _default_runtime_registry) = make_service();
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(500)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    broadcaster.take_events();

    let warmup_service = svc.clone();
    let conversation_id = conv.conversation_id.clone();
    let warmup = tokio::spawn(async move {
        warmup_service
            .warmup_for_view(TEST_USER_1, &conversation_id, &runtime_registry)
            .await
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        while slow_registry.build_calls() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("pending-conversation warmup should enter runtime preparation");
    assert!(!slow_registry.was_built());

    let runtime = svc.runtime_summary_for(&conv.conversation_id).await;
    assert_eq!(
        runtime.state,
        nomifun_api_types::ConversationRuntimeStateKind::Idle
    );
    assert!(!runtime.is_processing);
    assert!(runtime.can_send_message);
    assert_eq!(runtime.processing_started_at, None);
    assert!(!svc.runtime_state().has_active_turn(&conv.conversation_id));

    warmup.await.unwrap().unwrap();
    assert!(
        broadcaster
            .take_events()
            .iter()
            .all(|event| !matches!(event.name.as_str(), "turn.started" | "turn.completed")),
        "runtime preparation must not publish business-turn lifecycle events"
    );
}

#[tokio::test]
async fn view_warmup_and_explicit_send_share_one_preparation_gate() {
    let (svc, _broadcaster, _repo, _default_runtime_registry) = make_service();
    let slow_registry = Arc::new(SlowAgentRuntimeRegistry::new(Duration::from_millis(400)));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = slow_registry.clone();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let view_service = svc.clone();
    let view_conversation_id = conv.conversation_id.clone();
    let view_registry = runtime_registry.clone();
    let view = tokio::spawn(async move {
        view_service
            .warmup_for_view(TEST_USER_1, &view_conversation_id, &view_registry)
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while slow_registry.build_calls() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("view warmup should hold the gate while building");

    let send_service = svc.clone();
    let send_conversation_id = conv.conversation_id.clone();
    let send_registry = runtime_registry.clone();
    let mut send = tokio::spawn(async move {
        send_message_with_test_key(
            &send_service,
            TEST_USER_1,
            &send_conversation_id,
            "view-warmup-shared-preparation-gate",
            make_send_req(),
            &send_registry,
        )
        .await
    });

    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut send)
            .await
            .is_err(),
        "explicit send must wait for view reconciliation/build to leave the shared gate"
    );
    assert!(
        !svc.runtime_state().has_active_turn(&conv.conversation_id),
        "turn admission cannot overtake an in-flight view build"
    );

    view.await.unwrap().unwrap();
    let message_id = tokio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("send should proceed after view releases the gate")
        .unwrap()
        .unwrap();
    assert!(MessageId::try_from(message_id.as_str()).is_ok());
    wait_for_turn_released(&svc, &conv.conversation_id).await;
}

#[tokio::test]
async fn warmup_conversation_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let err = svc.warmup_for_view(TEST_USER_1, "no-such-id", &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn warmup_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let err = svc.warmup_for_view(TEST_USER_2, &conv.conversation_id, &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn warmup_rejects_pathological_workspace_with_runtime_error_code() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let legacy_workspace = "/tmp/my project ".to_owned();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            extra: Some(json!({ "workspace": legacy_workspace }).to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let err = svc.warmup_for_view(TEST_USER_1, &conv.conversation_id, &runtime_registry).await.unwrap_err();
    assert!(matches!(
        err,
        AppError::WorkspacePathEdgeWhitespaceRuntimeUnsupported(message) if message == "/tmp/my project "
    ));
}

// ── Confirmation system tests ────────────────────────────────────

fn make_test_confirmations() -> Vec<Confirmation> {
    vec![
        Confirmation {
            id: "c1".into(),
            call_id: "call-1".into(),
            title: Some("Allow file edit".into()),
            action: Some("edit_file".into()),
            description: "Edit main.rs".into(),
            command_type: Some("bash".into()),
            options: vec![],
            screenshot: None,
        },
        Confirmation {
            id: "c2".into(),
            call_id: "call-2".into(),
            title: Some("Read file".into()),
            action: Some("read_file".into()),
            description: "Read config.toml".into(),
            command_type: None,
            options: vec![],
            screenshot: None,
        },
    ]
}

#[tokio::test]
async fn list_confirmations_empty_when_no_agent() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let result = svc.list_confirmations(TEST_USER_1, &conv.conversation_id, &runtime_registry).await.unwrap();
    assert!(result.is_empty());
}

#[tokio::test]
async fn list_confirmations_returns_items() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let agent = AgentRuntimeHandle::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.conversation_id,
        make_test_confirmations(),
    )));
    runtime_registry.insert_agent(&conv.conversation_id, agent);

    let result = svc
        .list_confirmations(TEST_USER_1, &conv.conversation_id, &(runtime_registry as Arc<dyn AgentRuntimeRegistry>))
        .await
        .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].call_id, "call-1");
    assert_eq!(result[1].call_id, "call-2");
}

#[tokio::test]
async fn list_confirmations_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let err = svc
        .list_confirmations(TEST_USER_1, "no-such-id", &runtime_registry)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn list_confirmations_wrong_user() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let err = svc.list_confirmations(TEST_USER_2, &conv.conversation_id, &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn confirm_removes_confirmation_and_broadcasts() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    broadcaster.take_events(); // clear create event

    let agent = AgentRuntimeHandle::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.conversation_id,
        make_test_confirmations(),
    )));
    runtime_registry.insert_agent(&conv.conversation_id, agent);

    let req = nomifun_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: false,
    };
    svc.confirm(
        TEST_USER_1,
        &conv.conversation_id,
        "call-1",
        req,
        &(runtime_registry.clone() as Arc<dyn AgentRuntimeRegistry>),
    )
    .await
    .unwrap();

    // Confirmation should be removed from the agent
    let remaining = runtime_registry.get_runtime(&conv.conversation_id).unwrap().get_confirmations();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].call_id, "call-2");

    // Should broadcast confirmation.remove event
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "confirmation.remove");
    assert_eq!(events[0].data["conversation_id"], conv.conversation_id);
    assert_eq!(events[0].data["id"], "c1");
}

#[tokio::test]
async fn confirm_with_always_allow_stores_approval() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let agent = AgentRuntimeHandle::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.conversation_id,
        make_test_confirmations(),
    )));
    runtime_registry.insert_agent(&conv.conversation_id, agent);

    let req = nomifun_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: true,
    };
    let runtime_registry_arc: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    svc.confirm(TEST_USER_1, &conv.conversation_id, "call-1", req, &runtime_registry_arc)
        .await
        .unwrap();

    // check_approval should now return true for edit_file:bash
    let agent = runtime_registry.get_runtime(&conv.conversation_id).unwrap();
    assert!(agent.check_approval("edit_file", Some("bash")));
    assert!(!agent.check_approval("delete_file", None));
}

#[tokio::test]
async fn confirm_nonexistent_call_id_returns_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let agent = AgentRuntimeHandle::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.conversation_id,
        make_test_confirmations(),
    )));
    runtime_registry.insert_agent(&conv.conversation_id, agent);

    let req = nomifun_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: false,
    };
    let err = svc
        .confirm(
            TEST_USER_1,
            &conv.conversation_id,
            "nonexistent-call",
            req,
            &(runtime_registry as Arc<dyn AgentRuntimeRegistry>),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn confirm_without_confirmation_state_still_calls_agent() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    broadcaster.take_events();

    let agent = AgentRuntimeHandle::Mock(Arc::new(MockAgent::with_direct_confirm(&conv.conversation_id)));
    runtime_registry.insert_agent(&conv.conversation_id, agent);

    let req = nomifun_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!("allow_once"),
        always_allow: false,
    };
    svc.confirm(
        TEST_USER_1,
        &conv.conversation_id,
        "call-1",
        req,
        &(runtime_registry.clone() as Arc<dyn AgentRuntimeRegistry>),
    )
    .await
    .unwrap();

    assert!(broadcaster.take_events().is_empty());
}

#[tokio::test]
async fn confirm_no_agent_returns_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let req = nomifun_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: false,
    };
    let err = svc
        .confirm(TEST_USER_1, &conv.conversation_id, "call-1", req, &runtime_registry)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn check_approval_returns_false_when_not_set() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let agent = AgentRuntimeHandle::Mock(Arc::new(MockAgent::new(&conv.conversation_id)));
    runtime_registry.insert_agent(&conv.conversation_id, agent);

    let result = svc
        .check_approval(
            TEST_USER_1,
            &conv.conversation_id,
            "edit_file",
            None,
            &(runtime_registry as Arc<dyn AgentRuntimeRegistry>),
        )
        .await
        .unwrap();
    assert!(!result.approved);
}

#[tokio::test]
async fn check_approval_returns_true_after_always_allow() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let agent = AgentRuntimeHandle::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.conversation_id,
        make_test_confirmations(),
    )));
    runtime_registry.insert_agent(&conv.conversation_id, agent);

    // Confirm with always_allow
    let req = nomifun_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: true,
    };
    let runtime_registry_arc: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    svc.confirm(TEST_USER_1, &conv.conversation_id, "call-1", req, &runtime_registry_arc)
        .await
        .unwrap();

    // Now check_approval should return true
    let result = svc
        .check_approval(TEST_USER_1, &conv.conversation_id, "edit_file", Some("bash"), &runtime_registry_arc)
        .await
        .unwrap();
    assert!(result.approved);
}

#[tokio::test]
async fn check_approval_returns_false_when_no_agent() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let result = svc
        .check_approval(TEST_USER_1, &conv.conversation_id, "edit_file", None, &runtime_registry)
        .await
        .unwrap();
    assert!(!result.approved);
}

#[tokio::test]
async fn check_approval_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let err = svc
        .check_approval(TEST_USER_1, "no-such-id", "edit_file", None, &runtime_registry)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

// ── Skill snapshot tests ───────────────────────────────────────────

#[tokio::test]
async fn create_writes_extra_skills_from_auto_inject_and_preset() {
    let resolver = Arc::new(FixedSkillResolver {
        names: vec!["cron".into(), "todo-tracker".into()],
    });
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service_with_resolver(resolver);

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "t",
        "extra": {
            "agent_id": TEST_ACP_AGENT_ID,
            "workspace": "/project",
            "backend": "claude",
            "preset_enabled_skills": ["pdf", "cron"],
            "exclude_auto_inject_skills": ["todo-tracker"],
        },
    }))
    .unwrap();
    let resp = svc.create(TEST_USER_1, req).await.unwrap();

    assert_eq!(resp.extra["skills"], json!(["cron", "pdf"]));
    assert!(resp.extra.get("preset_enabled_skills").is_none());
    assert!(resp.extra.get("exclude_auto_inject_skills").is_none());

    let stored = _repo.get(&resp.conversation_id).await.unwrap().unwrap();
    let stored_extra: serde_json::Value = serde_json::from_str(&stored.extra).unwrap();
    assert_eq!(stored_extra["skills"], json!(["cron", "pdf"]));
    assert!(stored_extra.get("preset_enabled_skills").is_none());
    assert!(stored_extra.get("exclude_auto_inject_skills").is_none());
}

#[tokio::test]
async fn create_writes_empty_skills_when_no_auto_inject_and_no_preset() {
    let resolver = Arc::new(FixedSkillResolver { names: vec![] });
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service_with_resolver(resolver);

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": "/project", "backend": "claude" },
    }))
    .unwrap();
    let resp = svc.create(TEST_USER_1, req).await.unwrap();

    assert_eq!(resp.extra["skills"], json!([]));
}

#[tokio::test]
async fn warmup_restores_skill_links_for_recreated_auto_workspace() {
    let resolver = Arc::new(RecordingSkillResolver::new(vec!["cron".into()]));
    let links = resolver.links.clone();
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service_with_resolver(resolver);

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
        "extra": {},
    }))
    .unwrap();
    let resp = svc.create(TEST_USER_1, req).await.unwrap();
    let workspace = PathBuf::from(resp.extra["workspace"].as_str().unwrap());
    assert!(workspace.join(".nomi/skills/cron").is_dir());

    std::fs::remove_dir_all(&workspace).unwrap();
    assert!(!workspace.exists());
    links.lock().unwrap().clear();

    let runtime_registry: Arc<dyn AgentRuntimeRegistry> =
        Arc::new(MockAgentRuntimeRegistryWithWorkspace::new(workspace.to_str().unwrap()));
    svc.warmup_for_view(TEST_USER_1, &resp.conversation_id, &runtime_registry).await.unwrap();

    assert!(workspace.join(".nomi/skills/cron").is_dir());
    let calls = links.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].workspace, workspace);
    assert_eq!(calls[0].rel_dirs, vec![".nomi/skills"]);
    assert_eq!(calls[0].skill_names, vec!["cron"]);
}

#[tokio::test]
async fn update_rejects_extra_skills() {
    let (svc, _broadcaster, _repo, runtime_registry) = make_service();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": "/project", "backend": "claude" },
    }))
    .unwrap();
    let resp = svc.create(TEST_USER_1, req).await.unwrap();

    let update_req: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "skills": ["cron"] },
    }))
    .unwrap();
    let err = svc
        .update(TEST_USER_1, &resp.conversation_id, update_req, &runtime_registry)
        .await
        .unwrap_err();

    match err {
        AppError::BadRequest(msg) => assert!(msg.contains("skills"), "msg = {msg:?}"),
        other => panic!("expected BadRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn create_rejects_retired_skill_fields_without_interpreting_them() {
    for field in ["enabled_skills", "exclude_builtin_skills", "loaded_skills"] {
        let (svc, _broadcaster, repo, _runtime_registry) = make_service();
        let mut extra = serde_json::Map::from_iter([(
            "workspace".to_owned(),
            serde_json::Value::String("/project".to_owned()),
        )]);
        extra.insert(field.to_owned(), json!(["cron"]));
        let req: CreateConversationRequest = serde_json::from_value(json!({
            "type": "acp",
            "extra": extra,
        }))
        .unwrap();

        let err = svc.create(TEST_USER_1, req).await.unwrap_err();
        match err {
            AppError::BadRequest(msg) => assert!(msg.contains(field), "msg = {msg:?}"),
            other => panic!("expected BadRequest for {field}, got {other:?}"),
        }
        assert!(repo.rows.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn update_rejects_retired_skill_fields_without_removing_them() {
    let (svc, _broadcaster, repo, runtime_registry) = make_service();
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": "/project" },
    }))
    .unwrap();
    let resp = svc.create(TEST_USER_1, req).await.unwrap();
    let before = repo.get(&resp.conversation_id).await.unwrap().unwrap().extra;

    let update_req: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "loaded_skills": [{"name": "stale"}] },
    }))
    .unwrap();
    let err = svc
        .update(TEST_USER_1, &resp.conversation_id, update_req, &runtime_registry)
        .await
        .unwrap_err();

    match err {
        AppError::BadRequest(msg) => assert!(msg.contains("loaded_skills"), "msg = {msg:?}"),
        other => panic!("expected BadRequest, got {other:?}"),
    }
    assert_eq!(repo.get(&resp.conversation_id).await.unwrap().unwrap().extra, before);
}

#[tokio::test]
async fn update_allows_other_extra_fields() {
    let (svc, _broadcaster, _repo, runtime_registry) = make_service();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": "/project", "backend": "claude" },
    }))
    .unwrap();
    let resp = svc.create(TEST_USER_1, req).await.unwrap();

    let update_req: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "current_model_id": "claude-3-5-sonnet" },
    }))
    .unwrap();
    let updated = svc
        .update(TEST_USER_1, &resp.conversation_id, update_req, &runtime_registry)
        .await
        .unwrap();

    assert_eq!(updated.extra["current_model_id"], "claude-3-5-sonnet");
}

// ── Phase 3 model failover (plan D3) integration tests ──────────────
//
// These drive the send loop end to end through a nomi `ScriptedAgent`: turn 1
// emits a (pre-response) provider-fault terminal error, the seam picks the next
// queued model, rebuilds, and resends the SAME content. We assert on the
// `sent_contents` of a PERSISTENT scripted agent (returned across rebuilds), the
// model column written to the row, the termination count, and the provider repo's
// recorded health stamp.

use nomifun_common::ProviderWithModel;
use nomifun_db::models::{ClientPreference, Provider};
use nomifun_db::{
    CreateProviderParams, IClientPreferenceRepository, IProviderRepository, UpdateProviderParams,
};

/// Provider repo stub: serves a fixed candidate set to the picker and records
/// any `model_health` write so a test can assert the unhealthy stamp.
struct StubProviderRepo {
    providers: Vec<Provider>,
    health_writes: Mutex<Vec<(String, String)>>,
}

impl StubProviderRepo {
    fn new(providers: Vec<Provider>) -> Self {
        Self {
            providers,
            health_writes: Mutex::new(vec![]),
        }
    }

    fn health_writes(&self) -> Vec<(String, String)> {
        self.health_writes.lock().unwrap().clone()
    }
}

fn test_provider(id: &str, models: &[&str]) -> Provider {
    Provider {
        id: 0,
        provider_id: id.into(),
        platform: "openai".into(),
        name: id.into(),
        base_url: "https://example.com".into(),
        api_key_encrypted: "x".into(),
        models: serde_json::to_string(models).unwrap(),
        enabled: true,
        capabilities: "[]".into(),
        model_context_limits: None,
        model_protocols: None,
        model_descriptions: None,
        model_enabled: None,
        model_health: None,
        bedrock_config: None,
        is_full_url: false,
        sort_order: 0,
        created_at: 0,
        updated_at: 0,
    }
}

#[async_trait::async_trait]
impl IProviderRepository for StubProviderRepo {
    async fn list(&self) -> Result<Vec<Provider>, DbError> {
        Ok(self.providers.clone())
    }
    async fn find_by_id(&self, id: &str) -> Result<Option<Provider>, DbError> {
        Ok(self
            .providers
            .iter()
            .find(|p| p.provider_id == id)
            .cloned())
    }
    async fn create(&self, _params: CreateProviderParams<'_>) -> Result<Provider, DbError> {
        unimplemented!("not used in failover tests")
    }
    async fn update(&self, id: &str, params: UpdateProviderParams<'_>) -> Result<Provider, DbError> {
        if let Some(Some(health)) = params.model_health {
            self.health_writes.lock().unwrap().push((id.to_owned(), health.to_owned()));
        }
        Ok(self
            .providers
            .iter()
            .find(|p| p.provider_id == id)
            .cloned()
            .ok_or_else(|| DbError::NotFound(format!("provider {id}")))?)
    }
    async fn delete(&self, _id: &str) -> Result<(), DbError> {
        Ok(())
    }
}

/// Client-pref repo stub. The failover tests drive config via the conversation's
/// `extra.model_failover` session override, so the global pref is intentionally
/// empty here.
#[derive(Default)]
struct StubClientPrefRepo;

#[async_trait::async_trait]
impl IClientPreferenceRepository for StubClientPrefRepo {
    async fn get_all(&self) -> Result<Vec<ClientPreference>, DbError> {
        Ok(vec![])
    }
    async fn get_by_keys(&self, _keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
        Ok(vec![])
    }
    async fn upsert_batch(&self, _entries: &[(&str, &str)]) -> Result<(), DbError> {
        Ok(())
    }
    async fn delete_keys(&self, _keys: &[&str]) -> Result<(), DbError> {
        Ok(())
    }
}

struct FixedClientPrefRepo {
    preferences: Vec<ClientPreference>,
}

#[async_trait::async_trait]
impl IClientPreferenceRepository for FixedClientPrefRepo {
    async fn get_all(&self) -> Result<Vec<ClientPreference>, DbError> {
        Ok(self.preferences.clone())
    }

    async fn get_by_keys(&self, keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
        Ok(self
            .preferences
            .iter()
            .filter(|preference| keys.contains(&preference.key.as_str()))
            .cloned()
            .collect())
    }

    async fn upsert_batch(&self, _entries: &[(&str, &str)]) -> Result<(), DbError> {
        Ok(())
    }

    async fn delete_keys(&self, _keys: &[&str]) -> Result<(), DbError> {
        Ok(())
    }
}

#[tokio::test]
async fn explicit_knowledge_model_preference_overrides_the_conversation_model() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    svc.with_failover_deps(
        Arc::new(StubProviderRepo::new(vec![test_provider(
            PROVIDER_ID_2,
            &["knowledge-model"],
        )])),
        Arc::new(FixedClientPrefRepo {
            preferences: vec![ClientPreference {
                id: 1,
                key: "knowledge.autogenModel".into(),
                value: serde_json::json!({
                    "provider_id": PROVIDER_ID_2,
                    "model": "knowledge-model",
                    "use_model": "stale-session-only-override"
                })
                .to_string(),
                updated_at: 1,
            }],
        }),
    );
    let session_model = ProviderWithModel {
        provider_id: PROVIDER_ID_1.into(),
        model: "session-model".into(),
        use_model: Some("session-wire-model".into()),
    };

    let selected = svc
        .resolve_turn_writeback_model(Some(&session_model))
        .await
        .expect("knowledge model resolution should succeed")
        .expect("explicit knowledge model should resolve");

    assert_eq!(selected.provider_id, PROVIDER_ID_2);
    assert_eq!(selected.model, "knowledge-model");
    assert_eq!(selected.use_model, None);
}

#[tokio::test]
async fn invalid_explicit_knowledge_model_never_falls_back_to_session_model() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    svc.with_failover_deps(
        Arc::new(StubProviderRepo::new(vec![test_provider(
            PROVIDER_ID_1,
            &["session-model"],
        )])),
        Arc::new(FixedClientPrefRepo {
            preferences: vec![ClientPreference {
                id: 1,
                key: "knowledge.autogenModel".into(),
                value: "{broken explicit preference".into(),
                updated_at: 1,
            }],
        }),
    );
    let session_model = ProviderWithModel {
        provider_id: PROVIDER_ID_1.into(),
        model: "session-model".into(),
        use_model: Some("session-wire-model".into()),
    };

    let error = svc
        .resolve_turn_writeback_model(Some(&session_model))
        .await
        .unwrap_err();

    assert!(
        error.contains("configured knowledge write-back model is invalid"),
        "{error}"
    );
}

/// Runtime registry that returns ONE persistent scripted Agent across rebuilds, so a
/// failover (termination + recreation) keeps driving the same script queue and records
/// every resend. Counts `kill_and_wait` calls so tests can bound the switches.
struct PersistentScriptedRuntimeRegistry {
    agent: AgentRuntimeHandle,
    scripted: Arc<ScriptedAgent>,
    termination_count: AtomicUsize,
}

impl PersistentScriptedRuntimeRegistry {
    fn new(scripted: Arc<ScriptedAgent>) -> Self {
        Self {
            agent: AgentRuntimeHandle::Mock(scripted.clone()),
            scripted,
            termination_count: AtomicUsize::new(0),
        }
    }

    fn termination_count(&self) -> usize {
        self.termination_count.load(Ordering::SeqCst)
    }

    fn sent_contents(&self) -> Vec<String> {
        self.scripted.sent_contents()
    }
}

#[async_trait::async_trait]
impl AgentRuntimeRegistry for PersistentScriptedRuntimeRegistry {
    fn get_runtime(&self, _conversation_id: &str) -> Option<AgentRuntimeHandle> {
        Some(self.agent.clone())
    }
    async fn get_or_create_runtime(
        &self,
        _conversation_id: &str,
        _options: AgentRuntimeBuildOptions,
    ) -> Result<AgentRuntimeHandle, AppError> {
        Ok(self.agent.clone())
    }
    fn terminate(&self, _conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        Ok(())
    }
    fn terminate_and_wait(
        &self,
        _conversation_id: &str,
        _reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        self.termination_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(()))
    }
    fn terminate_and_wait_result(
        &self,
        _conversation_id: &str,
        _reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AppError>> + Send>> {
        self.termination_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(Ok(())))
    }
    fn terminate_all(&self) {}
    fn active_runtime_count(&self) -> usize {
        1
    }
    fn collect_idle_runtimes(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

/// Seed a nomi conversation row with a model + a session-level `model_failover`
/// override, returning the allocated id. `failover` is merged verbatim into
/// `extra.model_failover`.
async fn seed_nomi_failover_conversation(
    repo: &Arc<MockRepo>,
    failed: ProviderWithModel,
    failover: serde_json::Value,
) -> String {
    let workspace = isolated_test_workspace("failover");
    let row = ConversationRow {
        cron_job_id: None,
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        id: 0,
        conversation_id: ConversationId::new().into_string(),
        user_id: TEST_USER_1.into(),
        name: "failover".into(),
        r#type: "nomi".into(),
        extra: serde_json::to_string(&json!({
            "workspace": workspace,
            "model_failover": failover,
        }))
        .unwrap(),
        delegation_policy: "automatic".into(),
        execution_model_pool: None,
        decision_policy: "automatic".into(),
        execution_template_id: None,
        model: Some(serde_json::to_string(&failed).unwrap()),
        status: Some("pending".into()),
        source: Some("nomifun".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 0,
        updated_at: 0,
    };
    repo.create(&row).await.unwrap()
}

fn pwm(provider_id: &str, model: &str) -> ProviderWithModel {
    ProviderWithModel {
        provider_id: provider_id.into(),
        model: model.into(),
        use_model: None,
    }
}

/// Build a service whose failover deps are wired to the given provider repo.
///
/// Also returns the [`MockBroadcaster`] handle so a test can assert which WS
/// events were (or were NOT) emitted for the turn — the suppressed-error
/// failover invariant (gap #8) needs to confirm no error event reaches the wire.
fn make_failover_service(
    providers: Vec<Provider>,
) -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<MockRepo>,
    Arc<StubProviderRepo>,
) {
    let repo = Arc::new(MockRepo::new());
    let broadcaster = Arc::new(MockBroadcaster::new());
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo);
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());
    let provider_repo = Arc::new(StubProviderRepo::new(providers));
    let svc = ConversationService::new(
        Arc::<str>::from(TEST_USER_1),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        runtime_registry,
        repo.clone(),
        agent_metadata_repo,
        Arc::new(StubAcpSessionRepo::default()),
        Arc::new(crate::NoExecutionConversationBoundary),
    );
    svc.with_failover_deps(provider_repo.clone(), Arc::new(StubClientPrefRepo));
    (svc, broadcaster, repo, provider_repo)
}

fn provider_fault_then_finish_agent(conv_id: &str) -> Arc<ScriptedAgent> {
    Arc::new(
        ScriptedAgent::new(
            conv_id,
            vec![
                // Turn 1: pre-response provider fault (no Text emitted).
                vec![AgentStreamEvent::Error(ErrorEventData::legacy(
                    "rate limited",
                    Some(AgentErrorCode::UserLlmProviderRateLimited),
                ))],
                // Turn 2 (after failover): success.
                vec![
                    AgentStreamEvent::Text(TextEventData {
                        content: "recovered on backup model".into(),
                    }),
                    AgentStreamEvent::Finish(FinishEventData::default()),
                ],
            ],
        )
        .with_agent_type(AgentType::Nomi),
    )
}

#[tokio::test]
async fn failover_pre_response_fault_rebuilds_with_next_model_and_resends() {
    let (svc, _broadcaster, repo, provider_repo) =
        make_failover_service(vec![test_provider(PROVIDER_ID_1, &["m1"]), test_provider(PROVIDER_ID_2, &["m2"])]);
    let conv_id = seed_nomi_failover_conversation(
        &repo,
        pwm(PROVIDER_ID_1, "m1"),
        json!({ "enabled": true, "queue": [{"provider_id": PROVIDER_ID_2, "model": "m2"}] }),
    )
    .await;

    let scripted = provider_fault_then_finish_agent(&conv_id);
    let runtime_registry = Arc::new(PersistentScriptedRuntimeRegistry::new(scripted));
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv_id,
        "failover-pre-response-resend",
        make_send_req(),
        &runtime_registry_dyn,
    )
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv_id).await;

    // The same content was resent to the backup model: two sends, identical body.
    let sends = runtime_registry.sent_contents();
    assert_eq!(sends.len(), 2, "expected original send + one resend after failover");
    assert_eq!(sends[0], "Hello");
    assert_eq!(sends[1], "Hello", "failover must resend the SAME content");

    // Exactly one failover kill_and_wait.
    assert_eq!(runtime_registry.termination_count(), 1);

    // The conversation.model was rewritten to the next queued candidate.
    let row = repo.get(&conv_id).await.unwrap().unwrap();
    let model: ProviderWithModel = serde_json::from_str(row.model.as_deref().unwrap()).unwrap();
    assert_eq!(model.provider_id, PROVIDER_ID_2);
    assert_eq!(model.model, "m2");

    // stamp_unhealthy defaults to true → failed model stamped on its provider.
    let writes = provider_repo.health_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, PROVIDER_ID_1);
    assert!(writes[0].1.contains("\"m1\""), "failed model must be in the health write");
    assert!(writes[0].1.contains("unhealthy"));
}

#[tokio::test]
async fn failover_successful_pre_response_recovery_surfaces_no_error_to_user() {
    // Gap #8 (safety-critical): on a SUCCESSFUL pre-response failover the user
    // must see ONLY the backup model's turn — never the swallowed fault. The
    // relay suppresses the WS error event + the error `tips` row at source for a
    // fault the send loop will fail over, and the loop only re-surfaces it if the
    // picker found no candidate. Here the picker DOES find one (p2), so:
    //   (a) zero WS error events were broadcast,
    //   (b) no error / `tips` message row was persisted,
    //   (c) the resend landed on the backup model.
    let (svc, broadcaster, repo, _provider_repo) =
        make_failover_service(vec![test_provider(PROVIDER_ID_1, &["m1"]), test_provider(PROVIDER_ID_2, &["m2"])]);
    let conv_id = seed_nomi_failover_conversation(
        &repo,
        pwm(PROVIDER_ID_1, "m1"),
        json!({ "enabled": true, "queue": [{"provider_id": PROVIDER_ID_2, "model": "m2"}] }),
    )
    .await;

    let scripted = provider_fault_then_finish_agent(&conv_id);
    let runtime_registry = Arc::new(PersistentScriptedRuntimeRegistry::new(scripted));
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv_id,
        "failover-recovered-no-error",
        make_send_req(),
        &runtime_registry_dyn,
    )
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv_id).await;

    // (c) The same content was resent to the backup model, and the model column
    // was rewritten to the next queued candidate.
    let sends = runtime_registry.sent_contents();
    assert_eq!(sends.len(), 2, "expected original send + one resend after failover");
    assert_eq!(sends[1], "Hello", "failover must resend the SAME content on the backup model");
    let row = repo.get(&conv_id).await.unwrap().unwrap();
    let model: ProviderWithModel = serde_json::from_str(row.model.as_deref().unwrap()).unwrap();
    assert_eq!(model.provider_id, PROVIDER_ID_2, "resend must run on the backup model");

    // (a) No error event ever reached the wire — the only forwarded stream
    // fragments are the backup turn's Text/content, plus the turn lifecycle
    // events. A suppressed pre-response fault never broadcasts `type: "error"`.
    let events = broadcaster.take_events();
    assert!(
        !events
            .iter()
            .any(|evt| evt.name == "message.stream" && evt.data["type"] == "error"),
        "a recovered pre-response failover must not broadcast any WS error event"
    );

    // (b) No error / `tips` row persisted for the turn — the swallowed fault was
    // never written, so the conversation history shows only the recovered reply.
    let messages = repo.get_messages(&conv_id, 1, 50, SortOrder::Asc).await.unwrap().items;
    assert!(
        !messages.iter().any(|message| message.r#type == "tips"),
        "a recovered pre-response failover must not persist an error tips row"
    );
    assert!(
        !messages.iter().any(|message| message.status.as_deref() == Some("error")),
        "a recovered pre-response failover must not persist any error-status row"
    );
    // Sanity: the backup model's reply WAS persisted (only the error was hidden).
    let recovered = messages
        .iter()
        .find(|message| message.r#type == "text" && message.position.as_deref() == Some("left"))
        .expect("the backup model's assistant reply should be persisted");
    let content: serde_json::Value = serde_json::from_str(&recovered.content).unwrap();
    assert_eq!(content["content"], "recovered on backup model");
}

#[tokio::test]
async fn failover_mid_response_fault_does_not_switch_and_surfaces_error() {
    let (svc, _broadcaster, repo, _provider_repo) = make_failover_service(vec![test_provider(PROVIDER_ID_2, &["m2"])]);
    let conv_id = seed_nomi_failover_conversation(
        &repo,
        pwm(PROVIDER_ID_1, "m1"),
        json!({ "enabled": true, "queue": [{"provider_id": PROVIDER_ID_2, "model": "m2"}] }),
    )
    .await;

    // Mid-response: Text is emitted BEFORE the provider fault → no failover.
    let scripted = Arc::new(
        ScriptedAgent::new(
            &conv_id,
            vec![vec![
                AgentStreamEvent::Text(TextEventData {
                    content: "partial answer".into(),
                }),
                AgentStreamEvent::Error(ErrorEventData::legacy(
                    "rate limited",
                    Some(AgentErrorCode::UserLlmProviderRateLimited),
                )),
            ]],
        )
        .with_agent_type(AgentType::Nomi),
    );
    let runtime_registry = Arc::new(PersistentScriptedRuntimeRegistry::new(scripted));
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv_id,
        "failover-mid-response-no-switch",
        make_send_req(),
        &runtime_registry_dyn,
    )
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv_id).await;

    // No resend, no termination, no model change: the original error is surfaced as-is.
    assert_eq!(runtime_registry.sent_contents().len(), 1);
    assert_eq!(runtime_registry.termination_count(), 0);
    let row = repo.get(&conv_id).await.unwrap().unwrap();
    let model: ProviderWithModel = serde_json::from_str(row.model.as_deref().unwrap()).unwrap();
    assert_eq!(model.provider_id, PROVIDER_ID_1, "model must be unchanged on mid-response fault");
}

#[tokio::test]
async fn failover_post_toolcall_fault_does_not_switch_and_surfaces_error() {
    // Gap #3 (duplicate-side-effect guard): a provider fault AFTER a ToolCall is
    // post-response — the relay sets `emitted_response` via the tool arm, so the
    // failover seam must stand down. Failing over here would re-run the tool's
    // side effect (and re-bill it). Mirrors the Text-then-fault case but drives a
    // ToolCall before the fault. Assert: no resend, model unchanged, error surfaced.
    use nomifun_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

    let (svc, _broadcaster, repo, _provider_repo) = make_failover_service(vec![test_provider(PROVIDER_ID_2, &["m2"])]);
    let conv_id = seed_nomi_failover_conversation(
        &repo,
        pwm(PROVIDER_ID_1, "m1"),
        json!({ "enabled": true, "queue": [{"provider_id": PROVIDER_ID_2, "model": "m2"}] }),
    )
    .await;

    // Post-response: a ToolCall (side-effecting action) is emitted BEFORE the
    // provider fault → `emitted_response` is set via the tool arm → no failover.
    let scripted = Arc::new(
        ScriptedAgent::new(
            &conv_id,
            vec![vec![
                AgentStreamEvent::ToolCall(ToolCallEventData {
                    call_id: "tc-001".into(),
                    name: "write_file".into(),
                    args: json!({ "path": "a.ts" }),
                    status: ToolCallStatus::Completed,
                    description: None,
                    input: None,
                    output: Some("ok".into()),
                    artifacts: Vec::new(),
                    retry: None,
                }),
                AgentStreamEvent::Error(ErrorEventData::legacy(
                    "rate limited",
                    Some(AgentErrorCode::UserLlmProviderRateLimited),
                )),
            ]],
        )
        .with_agent_type(AgentType::Nomi),
    );
    let runtime_registry = Arc::new(PersistentScriptedRuntimeRegistry::new(scripted));
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv_id,
        "failover-post-toolcall-no-switch",
        make_send_req(),
        &runtime_registry_dyn,
    )
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv_id).await;

    // No resend, no termination, no model change: the original error is surfaced as-is.
    assert_eq!(
        runtime_registry.sent_contents().len(),
        1,
        "a post-ToolCall fault must not resend (would re-run the tool side effect)"
    );
    assert_eq!(runtime_registry.termination_count(), 0);
    let row = repo.get(&conv_id).await.unwrap().unwrap();
    let model: ProviderWithModel = serde_json::from_str(row.model.as_deref().unwrap()).unwrap();
    assert_eq!(model.provider_id, PROVIDER_ID_1, "model must be unchanged on a post-ToolCall fault");
    // The original error is surfaced (not suppressed): an error `tips` row persists.
    let messages = repo.get_messages(&conv_id, 1, 50, SortOrder::Asc).await.unwrap().items;
    assert!(
        messages.iter().any(|message| message.r#type == "tips" && message.status.as_deref() == Some("error")),
        "a post-ToolCall fault must surface the original error as a tips row"
    );
}

#[tokio::test]
async fn failover_queue_exhausted_surfaces_original_error() {
    // The only queue entry is the model that just failed → picker returns None.
    let (svc, _broadcaster, repo, _provider_repo) = make_failover_service(vec![test_provider(PROVIDER_ID_1, &["m1"])]);
    let conv_id = seed_nomi_failover_conversation(
        &repo,
        pwm(PROVIDER_ID_1, "m1"),
        json!({ "enabled": true, "queue": [{"provider_id": PROVIDER_ID_1, "model": "m1"}] }),
    )
    .await;

    let scripted = Arc::new(
        ScriptedAgent::new(
            &conv_id,
            vec![vec![AgentStreamEvent::Error(ErrorEventData::legacy(
                "rate limited",
                Some(AgentErrorCode::UserLlmProviderRateLimited),
            ))]],
        )
        .with_agent_type(AgentType::Nomi),
    );
    let runtime_registry = Arc::new(PersistentScriptedRuntimeRegistry::new(scripted));
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv_id,
        "failover-queue-exhausted",
        make_send_req(),
        &runtime_registry_dyn,
    )
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv_id).await;

    assert_eq!(runtime_registry.sent_contents().len(), 1, "no resend when queue is exhausted");
    assert_eq!(runtime_registry.termination_count(), 0);
    let row = repo.get(&conv_id).await.unwrap().unwrap();
    let model: ProviderWithModel = serde_json::from_str(row.model.as_deref().unwrap()).unwrap();
    assert_eq!(model.provider_id, PROVIDER_ID_1);
}

#[tokio::test]
async fn failover_non_provider_error_does_not_switch() {
    let (svc, _broadcaster, repo, _provider_repo) = make_failover_service(vec![test_provider(PROVIDER_ID_2, &["m2"])]);
    let conv_id = seed_nomi_failover_conversation(
        &repo,
        pwm(PROVIDER_ID_1, "m1"),
        json!({ "enabled": true, "queue": [{"provider_id": PROVIDER_ID_2, "model": "m2"}] }),
    )
    .await;

    // A non-provider terminal error (e.g. conversation busy) must NOT fail over.
    let scripted = Arc::new(
        ScriptedAgent::new(
            &conv_id,
            vec![vec![AgentStreamEvent::Error(ErrorEventData::legacy(
                "busy",
                Some(AgentErrorCode::NomifunConversationBusy),
            ))]],
        )
        .with_agent_type(AgentType::Nomi),
    );
    let runtime_registry = Arc::new(PersistentScriptedRuntimeRegistry::new(scripted));
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv_id,
        "failover-non-provider-error",
        make_send_req(),
        &runtime_registry_dyn,
    )
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv_id).await;

    assert_eq!(runtime_registry.sent_contents().len(), 1);
    assert_eq!(runtime_registry.termination_count(), 0);
    let row = repo.get(&conv_id).await.unwrap().unwrap();
    let model: ProviderWithModel = serde_json::from_str(row.model.as_deref().unwrap()).unwrap();
    assert_eq!(model.provider_id, PROVIDER_ID_1);
}

#[tokio::test]
async fn failover_is_bounded_by_max_switches() {
    // Two backup candidates available, but max_switches=1 caps it at a single
    // switch: turn 1 fault → switch to p2 → turn 2 fault → bound reached →
    // surface the error (no second switch to p3).
    let (svc, _broadcaster, repo, _provider_repo) = make_failover_service(vec![
        test_provider(PROVIDER_ID_1, &["m1"]),
        test_provider(PROVIDER_ID_2, &["m2"]),
        test_provider(PROVIDER_ID_3, &["m3"]),
    ]);
    let conv_id = seed_nomi_failover_conversation(
        &repo,
        pwm(PROVIDER_ID_1, "m1"),
        json!({
            "enabled": true,
            "max_switches": 1,
            "queue": [
                {"provider_id": PROVIDER_ID_2, "model": "m2"},
                {"provider_id": PROVIDER_ID_3, "model": "m3"}
            ]
        }),
    )
    .await;

    // Every turn faults pre-response, so only the bound stops the switching.
    let scripted = Arc::new(
        ScriptedAgent::new(
            &conv_id,
            vec![
                vec![AgentStreamEvent::Error(ErrorEventData::legacy(
                    "rate limited",
                    Some(AgentErrorCode::UserLlmProviderRateLimited),
                ))],
                vec![AgentStreamEvent::Error(ErrorEventData::legacy(
                    "rate limited again",
                    Some(AgentErrorCode::UserLlmProviderRateLimited),
                ))],
                vec![AgentStreamEvent::Error(ErrorEventData::legacy(
                    "should never run",
                    Some(AgentErrorCode::UserLlmProviderRateLimited),
                ))],
            ],
        )
        .with_agent_type(AgentType::Nomi),
    );
    let runtime_registry = Arc::new(PersistentScriptedRuntimeRegistry::new(scripted));
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv_id,
        "failover-max-switch-bound",
        make_send_req(),
        &runtime_registry_dyn,
    )
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv_id).await;

    // Original send + exactly one resend; one kill_and_wait. Never reaches p3.
    assert_eq!(runtime_registry.sent_contents().len(), 2, "max_switches=1 caps at one resend");
    assert_eq!(runtime_registry.termination_count(), 1);
    let row = repo.get(&conv_id).await.unwrap().unwrap();
    let model: ProviderWithModel = serde_json::from_str(row.model.as_deref().unwrap()).unwrap();
    assert_eq!(model.provider_id, PROVIDER_ID_2, "stopped at the first switch, not p3");
}

// ── review #11: ACP exclusion (send-loop) + IDMM/perform direct on non-nomi ──

/// Seed an ACP conversation row with a model + a session-level `model_failover`
/// override (mirror of [`seed_nomi_failover_conversation`] but `type: "acp"`).
async fn seed_acp_failover_conversation(
    repo: &Arc<MockRepo>,
    model: ProviderWithModel,
    failover: serde_json::Value,
) -> String {
    let workspace = isolated_test_workspace("acp-failover");
    let row = ConversationRow {
        cron_job_id: None,
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        id: 0,
        conversation_id: ConversationId::new().into_string(),
        user_id: TEST_USER_1.into(),
        name: "acp-failover".into(),
        r#type: "acp".into(),
        extra: serde_json::to_string(&json!({
            "workspace": workspace,
            "model_failover": failover,
        }))
        .unwrap(),
        delegation_policy: "automatic".into(),
        execution_model_pool: None,
        decision_policy: "automatic".into(),
        execution_template_id: None,
        model: Some(serde_json::to_string(&model).unwrap()),
        status: Some("pending".into()),
        source: Some("nomifun".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 0,
        updated_at: 0,
    };
    repo.create(&row).await.unwrap()
}

#[tokio::test]
async fn failover_send_loop_excludes_acp_conversation() {
    // review #11(1) / plan D7: an ACP conversation that hits a pre-response
    // provider fault must NOT be failed over — ACP self-manages its model. With
    // failover deps wired + an enabled queue, the seam still stands down because
    // the conversation is ACP-typed: no resend (one send only), no model write,
    // and no unhealthy stamp. (The ACP terminal-error eviction path legitimately
    // terminates and recreates the runtime; that is unrelated to the failover seam, so we
    // assert the failover-specific facts rather than termination_count.)
    let (svc, _broadcaster, repo, provider_repo) =
        make_failover_service(vec![test_provider(PROVIDER_ID_1, &["m1"]), test_provider(PROVIDER_ID_2, &["m2"])]);
    let conv_id = seed_acp_failover_conversation(
        &repo,
        pwm(PROVIDER_ID_1, "m1"),
        json!({ "enabled": true, "queue": [{"provider_id": PROVIDER_ID_2, "model": "m2"}] }),
    )
    .await;

    // ACP-typed agent that faults pre-response on the first (only) turn.
    let scripted = Arc::new(ScriptedAgent::new(
        &conv_id,
        vec![vec![AgentStreamEvent::Error(ErrorEventData::legacy(
            "rate limited",
            Some(AgentErrorCode::UserLlmProviderRateLimited),
        ))]],
    )); // default agent_type = Acp
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    runtime_registry.insert_agent(&conv_id, AgentRuntimeHandle::Mock(scripted.clone()));
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    send_message_with_test_key(
        &svc,
        TEST_USER_1,
        &conv_id,
        "failover-acp-excluded",
        make_send_req(),
        &runtime_registry_dyn,
    )
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv_id).await;

    // No failover resend: the single send is the original turn only.
    assert_eq!(
        scripted.sent_contents().len(),
        1,
        "ACP conversation must not be failed over (no resend)"
    );
    // Model unchanged — the seam never wrote a new conversation.model.
    let row = repo.get(&conv_id).await.unwrap().unwrap();
    let model: ProviderWithModel = serde_json::from_str(row.model.as_deref().unwrap()).unwrap();
    assert_eq!(model.provider_id, PROVIDER_ID_1, "ACP model must be untouched by failover");
    // The failover unhealthy-stamp never ran.
    assert!(
        provider_repo.health_writes().is_empty(),
        "ACP exclusion: failover must not stamp any provider unhealthy"
    );
}

#[tokio::test]
async fn idmm_failover_conversation_returns_false_for_acp_conversation() {
    // IDMM is an observer, not the active turn owner. Even a fully-live
    // observation must be declined so only the send-loop can switch and
    // re-drive the exact current turn.
    let (svc, _broadcaster, repo, provider_repo) =
        make_failover_service(vec![test_provider(PROVIDER_ID_1, &["m1"]), test_provider(PROVIDER_ID_2, &["m2"])]);
    let conv_id = seed_acp_failover_conversation(
        &repo,
        pwm(PROVIDER_ID_1, "m1"),
        json!({ "enabled": true, "queue": [{"provider_id": PROVIDER_ID_2, "model": "m2"}] }),
    )
    .await;

    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    repo.update(
        &conv_id,
        &ConversationRowUpdate {
            status: Some("running".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let turn = svc
        .runtime_state()
        .try_acquire_turn_with_wire_context_at_epoch_and_owner(
            &conv_id,
            Some(MessageId::new().into_string()),
            crate::runtime_state::TurnWireContext::default(),
            None,
            Some(TEST_USER_1.to_owned()),
            true,
            None,
        )
        .unwrap();
    runtime_registry.insert_agent(
        &conv_id,
        AgentRuntimeHandle::Mock(Arc::new(SteerableAgent::new(
            &conv_id,
            Some(ConversationStatus::Running),
            true,
        ))),
    );
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    let switched = svc
        .idmm_failover_conversation(TEST_USER_1, &conv_id, &runtime_registry_dyn)
        .await
        .unwrap();
    assert!(!switched, "IDMM failover must report false for an ACP conversation");
    assert_eq!(runtime_registry.termination_count(), 0, "no termination on a rejected ACP failover");
    assert_eq!(
        runtime_registry.active_runtime_count(),
        1,
        "the IDMM observer must neither replace nor evict the owner runtime"
    );
    let row = repo.get(&conv_id).await.unwrap().unwrap();
    let model: ProviderWithModel = serde_json::from_str(row.model.as_deref().unwrap()).unwrap();
    assert_eq!(model.provider_id, PROVIDER_ID_1, "ACP model must be untouched");
    assert!(provider_repo.health_writes().is_empty());
    assert!(
        repo.get_messages(&conv_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .items
            .is_empty(),
        "declining an IDMM observation must not synthesize a continuation message"
    );
    drop(turn);
}

#[tokio::test]
async fn idmm_failover_on_finished_conversation_cannot_build_or_send() {
    let (svc, broadcaster, repo, provider_repo) =
        make_failover_service(vec![test_provider(PROVIDER_ID_1, &["m1"]), test_provider(PROVIDER_ID_2, &["m2"])]);
    let conv_id = seed_acp_failover_conversation(
        &repo,
        pwm(PROVIDER_ID_1, "m1"),
        json!({ "enabled": true, "queue": [{"provider_id": PROVIDER_ID_2, "model": "m2"}] }),
    )
    .await;
    repo.update(
        &conv_id,
        &ConversationRowUpdate {
            status: Some("finished".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    broadcaster.take_events();

    let error = svc
        .idmm_failover_conversation(TEST_USER_1, &conv_id, &runtime_registry_dyn)
        .await
        .expect_err("a stale Finished wake-up must fail closed");
    assert!(matches!(error, AppError::Conflict(_)));
    assert_eq!(runtime_registry.active_runtime_count(), 0);
    assert_eq!(runtime_registry.termination_count(), 0);
    assert_eq!(
        repo.get(&conv_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    assert!(
        repo.get_messages(&conv_id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(provider_repo.health_writes().is_empty());
    assert!(
        broadcaster
            .take_events()
            .iter()
            .all(|event| !matches!(
                event.name.as_str(),
                "message.userCreated" | "turn.started" | "turn.completed"
            ))
    );
}

#[tokio::test]
async fn perform_model_failover_returns_none_for_acp_conversation() {
    // review #11(2): calling the bottleneck directly on a non-nomi conversation
    // returns None (the review #9 ACP gate), with no termination and no model write.
    let (svc, _broadcaster, repo, provider_repo) =
        make_failover_service(vec![test_provider(PROVIDER_ID_1, &["m1"]), test_provider(PROVIDER_ID_2, &["m2"])]);
    let conv_id = seed_acp_failover_conversation(
        &repo,
        pwm(PROVIDER_ID_1, "m1"),
        json!({ "enabled": true, "queue": [{"provider_id": PROVIDER_ID_2, "model": "m2"}] }),
    )
    .await;

    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    let config = nomifun_api_types::ModelFailoverConfig {
        enabled: true,
        queue: vec![pwm(PROVIDER_ID_2, "m2")],
        ..Default::default()
    };
    let result = svc
        .perform_model_failover(&conv_id, &config, &[], &runtime_registry_dyn)
        .await;
    assert!(result.is_none(), "perform_model_failover must reject a non-nomi conversation");
    assert_eq!(runtime_registry.termination_count(), 0);
    let row = repo.get(&conv_id).await.unwrap().unwrap();
    let model: ProviderWithModel = serde_json::from_str(row.model.as_deref().unwrap()).unwrap();
    assert_eq!(model.provider_id, PROVIDER_ID_1);
    assert!(provider_repo.health_writes().is_empty());
}

// ── edit_and_resubmit tests ─────────────────────────────────────

/// 非 Nomi 会话调用 edit_and_resubmit → BadRequest（Nomi 门禁在取 agent/查消息之前）。
#[tokio::test]
async fn edit_and_resubmit_rejects_non_nomi() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());
    // make_create_req() 建的是 acp 会话
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let conv_id = conv.conversation_id.clone();

    let req: SendMessageRequest = serde_json::from_value(json!({ "content": "edited" })).unwrap();
    let err = svc
        .edit_and_resubmit(TEST_USER_1, &conv_id, MESSAGE_ID_1, req, &runtime_registry)
        .await
        .unwrap_err();

    assert!(matches!(err, AppError::BadRequest(_)));
    assert!(err.to_string().contains("Nomi"), "应为 Nomi 门禁错误，实际: {err}");
}

/// Nomi 会话但没有可编辑的用户消息 → BadRequest（消息查找守卫，在取 agent 之前）。
#[tokio::test]
async fn edit_and_resubmit_rejects_when_no_editable_message() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());
    let nomi_req: CreateConversationRequest =
        serde_json::from_value(json!({ "type": "nomi", "extra": { "workspace": "/project" } })).unwrap();
    let conv = svc.create(TEST_USER_1, nomi_req).await.unwrap();
    let conv_id = conv.conversation_id.clone();

    let req: SendMessageRequest = serde_json::from_value(json!({ "content": "edited" })).unwrap();
    let err = svc
        .edit_and_resubmit(TEST_USER_1, &conv_id, MESSAGE_ID_1, req, &runtime_registry)
        .await
        .unwrap_err();

    assert!(matches!(err, AppError::BadRequest(_)));
}
