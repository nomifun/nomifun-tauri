use std::collections::VecDeque;
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
use nomifun_ai_agent::{AgentSendError, AgentRuntimeRegistry};

use crate::response_middleware::{CronCommandResult, CronCreateParams, CronUpdateParams, ICronService};
use nomifun_api_types::AgentErrorCode;
use nomifun_api_types::{
    CloneConversationRequest, CreateConversationRequest, ListConversationsQuery, SearchMessagesQuery,
    SendMessageRequest, UpdateConversationRequest, WebSocketMessage,
};
use nomifun_common::{
    AdaptationPolicy, AgentExecutionEventKind, AgentExecutionStatus, AgentKillReason,
    AgentStepMode, AgentToolPolicy, AgentType, AppError, Confirmation, ConversationSource,
    ConversationArtifactId, ConversationId, ConversationStatus,
    DecisionPolicy, DelegationPolicy, ExecutionAttemptStatus, ExecutionStepKind,
    ExecutionStepStatus, MessageId, PaginatedResult, ParticipantAssignmentSource, PlanGate,
    StepFailurePolicy, TimestampMs, now_ms,
};
use nomifun_db::models::{
    AcpSessionRow, AgentMetadataRow, ConversationArtifactRow, ConversationRow, MessageRow, UpdateAgentHandshakeParams,
    UpsertAgentMetadataParams,
};
use nomifun_db::{
    AttemptConversationEffectParams, ConversationFilters, ConversationRowUpdate,
    CreateAcpSessionParams,
    CreateAgentExecutionAttemptParams, CreateAgentExecutionParams, DbError,
    IAcpSessionRepository, IAgentExecutionRepository, IAgentMetadataRepository,
    IConversationRepository, MessageRowUpdate, MessageSearchRow, NewAgentExecutionEvent,
    NewAgentExecutionParticipant, NewAgentExecutionStep, PersistedSessionState,
    ReconcileAgentExecutionPlanParams, SaveRuntimeStateParams,
    SettleAgentExecutionAttemptParams, SortOrder, SqliteAgentExecutionRepository,
    SqliteConversationRepository,
};
use nomifun_realtime::{EventBroadcaster, UserEventSink};
use serde_json::json;
use tokio::sync::{Notify, broadcast};

use crate::service::ConversationService;
use crate::RepositoryExecutionConversationBoundary;
use crate::skill_resolver::{FixedSkillResolver, ResolvedAgentSkill, SkillResolver};
use nomifun_knowledge::{KnowledgeBinding, KnowledgeCompleter, KnowledgeEventEmitter, KnowledgeService};

#[path = "service_test/acp_error_recovery_test.rs"]
mod acp_error_recovery_test;

const SQLITE_TEST_OWNER: &str = "user_0190f5fe-7c00-7a00-8000-000000000001";
const TEST_USER_1: &str = "user_0190f5fe-7c00-7a00-8000-000000000011";
const TEST_USER_2: &str = "user_0190f5fe-7c00-7a00-8000-000000000012";
const PROVIDER_ID_1: &str = "prov_0190f5fe-7c00-7a00-8000-000000000001";
const EXECUTION_PARTICIPANT_ID_1: &str =
    "execpart_0190f5fe-7c00-7a00-8000-000000000001";
const EXECUTION_STEP_ID_1: &str = "execstep_0190f5fe-7c00-7a00-8000-000000000001";
const MESSAGE_ID_1: &str = "msg_0190f5fe-7c00-7a00-8000-000000000001";
const PROVIDER_ID_2: &str = "prov_0190f5fe-7c00-7a00-8000-000000000002";
const PROVIDER_ID_3: &str = "prov_0190f5fe-7c00-7a00-8000-000000000003";

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
}

impl MockRepo {
    fn new() -> Self {
        Self {
            rows: Mutex::new(vec![]),
            messages: Mutex::new(vec![]),
            artifacts: Mutex::new(vec![]),
        }
    }
}

#[async_trait::async_trait]
impl IConversationRepository for MockRepo {
    async fn get(&self, id: &str) -> Result<Option<ConversationRow>, nomifun_db::DbError> {
        let rows = self.rows.lock().unwrap();
        Ok(rows.iter().find(|r| r.id == id).cloned())
    }

    async fn create(&self, row: &ConversationRow) -> Result<String, nomifun_db::DbError> {
        let mut rows = self.rows.lock().unwrap();
        if rows.iter().any(|existing| existing.id == row.id) {
            return Err(nomifun_db::DbError::Conflict(format!(
                "Conversation {}",
                row.id
            )));
        }
        rows.push(row.clone());
        Ok(row.id.clone())
    }

    async fn update(&self, id: &str, updates: &ConversationRowUpdate) -> Result<(), nomifun_db::DbError> {
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|r| r.id == id)
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

    async fn delete(&self, id: &str) -> Result<(), nomifun_db::DbError> {
        let mut rows = self.rows.lock().unwrap();
        let len_before = rows.len();
        rows.retain(|r| r.id != id);
        if rows.len() == len_before {
            return Err(nomifun_db::DbError::NotFound(format!("Conversation {id}")));
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

    async fn get_message(&self, conv_id: &str, message_id: &str) -> Result<Option<MessageRow>, nomifun_db::DbError> {
        let messages = self.messages.lock().unwrap();
        Ok(messages
            .iter()
            .find(|message| message.conversation_id == conv_id && message.id == message_id)
            .cloned())
    }

    async fn insert_message(&self, message: &MessageRow) -> Result<(), nomifun_db::DbError> {
        self.messages.lock().unwrap().push(message.clone());
        Ok(())
    }

    async fn update_message(&self, id: &str, updates: &MessageRowUpdate) -> Result<(), nomifun_db::DbError> {
        let mut messages = self.messages.lock().unwrap();
        let message = messages
            .iter_mut()
            .find(|message| message.id == id)
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
        artifact_id: &str,
    ) -> Result<Option<ConversationArtifactRow>, nomifun_db::DbError> {
        Ok(self
            .artifacts
            .lock()
            .unwrap()
            .iter()
            .find(|artifact| artifact.conversation_id == conversation_id && artifact.id == artifact_id)
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
            let id = existing.id.clone();
            *existing = artifact.clone();
            existing.id = id;
            return Ok(existing.clone());
        }
        if artifacts.iter().any(|existing| existing.id == artifact.id) {
            return Err(nomifun_db::DbError::Conflict(format!(
                "Conversation artifact {}",
                artifact.id
            )));
        }
        artifacts.push(artifact.clone());
        Ok(artifact.clone())
    }

    async fn update_artifact_status(
        &self,
        conversation_id: &str,
        artifact_id: &str,
        status: &str,
        updated_at: TimestampMs,
    ) -> Result<Option<ConversationArtifactRow>, nomifun_db::DbError> {
        let mut artifacts = self.artifacts.lock().unwrap();
        let Some(existing) = artifacts
            .iter_mut()
            .find(|artifact| artifact.conversation_id == conversation_id && artifact.id == artifact_id)
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

    async fn list_legacy_cron_trigger_messages(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<MessageRow>, nomifun_db::DbError> {
        Ok(self
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message.conversation_id == conversation_id && message.r#type == "cron_trigger")
            .cloned()
            .collect())
    }
}

// ── Helpers ────────────────────────────────────────────────────────

/// Stub repository for tests — every lookup returns `None` so the
/// service falls back to `AgentType::native_skills_dirs()` paths.
struct StubAgentMetadataRepo;

#[async_trait::async_trait]
impl IAgentMetadataRepository for StubAgentMetadataRepo {
    async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
        Ok(Vec::new())
    }
    async fn get(&self, _id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
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
}

impl StubAcpSessionRepo {
    fn runtime_state_saves(&self) -> Vec<RuntimeStateSaveCall> {
        self.runtime_state_saves.lock().unwrap().clone()
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
            conversation_id: params.conversation_id.to_owned(),
            agent_backend: "stub".into(),
            agent_source: "stub".into(),
            agent_id: "stub".into(),
            session_id: None,
            session_status: "idle".into(),
            session_config: "{}".into(),
            last_active_at: None,
            suspended_at: None,
        })
    }
    async fn update_session_id(&self, _conversation_id: &str, _session_id: &str) -> Result<bool, DbError> {
        Ok(false)
    }
    async fn clear_session_id(&self, _conversation_id: &str) -> Result<bool, DbError> {
        Ok(false)
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
    serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": "/project" }
    }))
    .unwrap()
}

// ── Create tests ───────────────────────────────────────────────────

#[tokio::test]
async fn create_returns_conversation_with_defaults() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();

    let resp = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    assert!(ConversationId::try_from(resp.id.as_str()).is_ok());
    assert_eq!(resp.r#type, AgentType::Acp);
    assert_eq!(resp.status, ConversationStatus::Pending);
    assert_eq!(resp.source, Some(ConversationSource::Nomifun));
    assert!(!resp.pinned);
    assert!(resp.pinned_at.is_none());
    assert_eq!(resp.extra["workspace"], "/project");
    assert!(resp.created_at > 0);
    assert_eq!(resp.created_at, resp.modified_at);

    // Should have broadcast a listChanged(created) event
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "conversation.listChanged");
    assert_eq!(events[0].data["action"], "created");
    assert_eq!(events[0].data["conversation_id"], resp.id);
    assert_eq!(events[0].data["source"], "nomifun");
}

#[tokio::test]
async fn create_rejects_numeric_session_mcp_ids() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
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
        "extra": { "workspace": workspace_with_trailing_space }
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
        "extra": { "workspace": workspace.to_string_lossy() }
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
        "extra": {}
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
async fn get_existing_conversation() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let created = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let fetched = svc.get(TEST_USER_1, &created.id).await.unwrap();
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.name, created.name);
    assert!(fetched.runtime.is_some());
}

#[tokio::test]
async fn get_reports_idle_runtime_when_only_persisted_status_is_running() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let created = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.update(
        &created.id,
        &ConversationRowUpdate {
            status: Some("running".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let fetched = svc.get(TEST_USER_1, &created.id).await.unwrap();
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
        "extra": {}
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
    svc.update(TEST_USER_1, &conv.id, update_req, &runtime_registry).await.unwrap();

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
    let updated = svc.update(TEST_USER_1, &conv.id, req, &runtime_registry).await.unwrap();

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
    let updated = svc.update(TEST_USER_1, &conv.id, req, &runtime_registry).await.unwrap();
    assert!(updated.pinned);
    assert!(updated.pinned_at.is_some());
}

#[tokio::test]
async fn update_unpin_clears_pinned_at() {
    let (svc, _broadcaster, _repo, runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    // Pin first
    let pin_req: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": true })).unwrap();
    let pinned = svc.update(TEST_USER_1, &conv.id, pin_req, &runtime_registry).await.unwrap();
    assert!(pinned.pinned);
    assert!(pinned.pinned_at.is_some());

    // Unpin
    let unpin_req: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": false })).unwrap();
    let unpinned = svc.update(TEST_USER_1, &conv.id, unpin_req, &runtime_registry).await.unwrap();
    assert!(!unpinned.pinned);
    assert!(unpinned.pinned_at.is_none());
}

#[tokio::test]
async fn update_extra_merge() {
    let (svc, _broadcaster, _repo, runtime_registry) = make_service();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": "/old", "contextFileName": "ctx.md" }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, req).await.unwrap();

    // Update only workspace — contextFileName should be preserved
    let update_req: UpdateConversationRequest =
        serde_json::from_value(json!({ "extra": { "workspace": "/new" } })).unwrap();
    let updated = svc.update(TEST_USER_1, &conv.id, update_req, &runtime_registry).await.unwrap();

    assert_eq!(updated.extra["workspace"], "/new");
    assert_eq!(updated.extra["contextFileName"], "ctx.md");
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
    let updated = svc.update(TEST_USER_1, &conv.id, req, &runtime_registry).await.unwrap();

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
    let updated = svc.update(TEST_USER_1, &conv.id, repoint, &mgr).await.unwrap();
    assert_eq!(updated.extra["workspace"], "/other/project");
    assert_eq!(mock.termination_count(), 1, "workspace change must recycle the agent");

    // Re-applying the SAME workspace is a no-op → no further recycle.
    let same: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "workspace": "/other/project" }
    }))
    .unwrap();
    svc.update(TEST_USER_1, &conv.id, same, &mgr).await.unwrap();
    assert_eq!(mock.termination_count(), 1, "no-op workspace update must not recycle");

    // A non-workspace extra change must also not recycle.
    let other: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "some_flag": true }
    }))
    .unwrap();
    svc.update(TEST_USER_1, &conv.id, other, &mgr).await.unwrap();
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

    svc.delete(TEST_USER_1, &conv.id).await.unwrap();

    // Should be gone
    let err = svc.get(TEST_USER_1, &conv.id).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));

    // Should broadcast deleted
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["action"], "deleted");
    assert_eq!(events[0].data["conversation_id"], conv.id);
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
    svc.delete(TEST_USER_1, &conv.id).await.unwrap();

    let calls = hook.0.lock().unwrap();
    assert_eq!(calls.as_slice(), &[(TEST_USER_1.to_owned(), conv.id)]);
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
            &conversation.id,
            operation_id,
            "final synthesis",
            "agent_execution_report",
        )
        .await
        .unwrap();
    let replay = service
        .project_assistant_message_idempotent(
            PROJECTION_OWNER,
            &conversation.id,
            operation_id,
            "final synthesis",
            "agent_execution_report",
        )
        .await
        .unwrap();
    assert_eq!(first, replay);

    let messages = repository
        .get_messages(&conversation.id, 1, 20, SortOrder::Asc)
        .await
        .unwrap();
    assert_eq!(messages.items.len(), 1);
    assert_eq!(messages.items[0].id, first);
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
            &conversation.id,
            operation_id,
            "different content",
            "agent_execution_report",
        )
        .await
        .is_err());
    assert!(service
        .project_assistant_message_idempotent(
            "other-owner",
            &conversation.id,
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
                    "companion_id": "companion_0190f5fe-7c00-7a00-8abc-012345678941",
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
            &conversation.id,
            "exec-lead-report:exec_2:event:9",
            "companion synthesis",
            "agent_execution_report",
        )
        .await
        .unwrap();
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["companion"], true);
    assert_eq!(events[0].data["companion_id"], "companion_0190f5fe-7c00-7a00-8abc-012345678941");
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
        .id;
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
        owner_b_conversation.id
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
            &owner_a_conversation.id,
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
        owner_a_conversation.id
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
            id, platform, name, base_url, api_key_encrypted, models, enabled, \
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
        serde_json::from_value(json!({
            "type": "nomi",
            "name": name,
            "model": {
                "provider_id": PROVIDER_ID_1,
                "model": "model_test",
                "use_model": "model_test"
            },
            "extra": { "workspace": "/project" }
        }))
        .unwrap()
    };
    let lead = svc.create(USER_ID, request("lead")).await.unwrap();
    let attempt_conversation = svc.create(USER_ID, request("attempt")).await.unwrap();
    let event = |event_type: AgentExecutionEventKind| NewAgentExecutionEvent {
        event_type,
        step_id: None,
        attempt_id: None,
        actor: nomifun_common::AgentExecutionActor::system(),
        payload: "{}".to_owned(),
    };
    let participant = NewAgentExecutionParticipant {
        id: EXECUTION_PARTICIPANT_ID_1.to_owned(),
        source_agent_id: "agent_nomi".to_owned(),
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
                lead_conversation_id: Some(lead.id.clone()),
                initial_plan_input: r#"{"mode":"automatic"}"#.to_owned(),
            },
            &[participant],
            &event(AgentExecutionEventKind::Created),
        )
        .await
        .unwrap();
    execution_repo
        .reconcile_plan(
            USER_ID,
            &execution.id,
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
                    id: EXECUTION_STEP_ID_1.to_owned(),
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
                    assigned_participant_id: Some(EXECUTION_PARTICIPANT_ID_1.to_owned()),
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
    let queued = execution_repo
        .create_attempt(
            USER_ID,
            &execution.id,
            EXECUTION_STEP_ID_1,
            0,
            None,
            &CreateAgentExecutionAttemptParams {
                participant_id: Some(EXECUTION_PARTICIPANT_ID_1.to_owned()),
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
    let attempt_id = queued.current_attempt.unwrap().attempt.id;
    execution_repo
        .start_attempt(
            USER_ID,
            &execution.id,
            EXECUTION_STEP_ID_1,
            1,
            &attempt_id,
            0,
            &attempt_conversation.id,
            None,
            &event(AgentExecutionEventKind::AttemptChanged),
        )
        .await
        .unwrap();

    let ordinary_send = svc
        .send_message(
            USER_ID,
            &attempt_conversation.id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap_err();
    assert!(matches!(ordinary_send, AppError::Conflict(_)));
    let ordinary_update = svc
        .update(
            USER_ID,
            &attempt_conversation.id,
            serde_json::from_value(json!({ "name": "must not mutate" })).unwrap(),
            &runtime_registry,
        )
        .await
        .unwrap_err();
    assert!(matches!(ordinary_update, AppError::Conflict(_)));
    let ordinary_cancel = svc
        .cancel(
            USER_ID,
            &attempt_conversation.id,
            &runtime_registry,
        )
        .await
        .unwrap_err();
    assert!(matches!(ordinary_cancel, AppError::Conflict(_)));
    assert!(!svc.user_cancelled_since(&attempt_conversation.id, 0));
    let ordinary_warmup = svc
        .warmup(
            USER_ID,
            &attempt_conversation.id,
            &runtime_registry,
        )
        .await
        .unwrap_err();
    assert!(matches!(ordinary_warmup, AppError::Conflict(_)));
    assert_eq!(runtime_registry_impl.active_runtime_count(), 0);
    assert_eq!(runtime_registry_impl.termination_count(), 0);
    assert!(
        conversation_repo
            .get_messages(&attempt_conversation.id, 1, 20, SortOrder::Asc)
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
            &attempt_conversation.id,
            "execution:test:initial",
            make_send_req(),
        )
        .await
        .unwrap();
    assert!(MessageId::try_from(delivery.message_id.as_str()).is_ok());
    wait_for_turn_released(&svc, &attempt_conversation.id).await;
    assert_eq!(
        conversation_repo
            .get_messages(&attempt_conversation.id, 1, 20, SortOrder::Asc)
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
            &attempt_conversation.id,
            &runtime_registry,
        )
        .await
        .unwrap()
        .is_empty(),
        "read-only confirmation inspection remains available"
    );
    let projected_lead = svc.get(USER_ID, &lead.id).await.unwrap();
    assert_eq!(projected_lead.linked_execution_id.as_deref(), Some(execution.id.as_str()));
    assert!(projected_lead.execution_step_id.is_none());
    assert!(projected_lead.execution_attempt_id.is_none());

    let projected_attempt = svc
        .get(USER_ID, &attempt_conversation.id)
        .await
        .unwrap();
    assert_eq!(projected_attempt.linked_execution_id.as_deref(), Some(execution.id.as_str()));
    assert_eq!(projected_attempt.execution_step_id.as_deref(), Some(EXECUTION_STEP_ID_1));
    assert_eq!(
        projected_attempt.execution_attempt_id.as_deref(),
        Some(attempt_id.as_str())
    );
    execution_repo
        .settle_attempt(
            USER_ID,
            &execution.id,
            EXECUTION_STEP_ID_1,
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
            &attempt_conversation.id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap_err(),
        AppError::Conflict(_)
    ));
    let continuation = execution_port
        .deliver_turn(
            USER_ID,
            &attempt_conversation.id,
            "execution:test:continuation",
            serde_json::from_value(json!({ "content": "continue" })).unwrap(),
        )
        .await
        .unwrap();
    assert!(MessageId::try_from(continuation.message_id.as_str()).is_ok());
    wait_for_turn_released(&svc, &attempt_conversation.id).await;
    execution_repo
        .resume_waiting_attempt(
            USER_ID,
            &execution.id,
            4,
            EXECUTION_STEP_ID_1,
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
    execution_repo
        .settle_attempt(
            USER_ID,
            &execution.id,
            EXECUTION_STEP_ID_1,
            4,
            &attempt_id,
            3,
            None,
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
        .get(USER_ID, &attempt_conversation.id)
        .await
        .unwrap();
    assert_eq!(
        settled_attempt.linked_execution_id.as_deref(),
        Some(execution.id.as_str())
    );
    assert_eq!(settled_attempt.execution_step_id.as_deref(), Some(EXECUTION_STEP_ID_1));
    assert_eq!(
        settled_attempt.execution_attempt_id.as_deref(),
        Some(attempt_id.as_str())
    );
    assert!(matches!(
        svc.send_message(
            USER_ID,
            &attempt_conversation.id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap_err(),
        AppError::Conflict(_)
    ));
    svc.update_extra(
        &attempt_conversation.id,
        json!({ "execution_cleanup_state": "retained" }),
    )
    .await
    .unwrap();
    svc.cancel_for_execution(
        USER_ID,
        &attempt_conversation.id,
        &runtime_registry,
    )
    .await
    .unwrap();

    let cleanup = execution_repo
        .list_pending_conversation_cleanups(Some(&execution.id), 10)
        .await
        .unwrap();
    assert_eq!(cleanup.len(), 1);
    assert!(
        execution_repo
            .mark_conversation_cleanup_completed(&cleanup[0].link_id, now_ms())
            .await
            .unwrap()
    );
    let cleaned_attempt = svc
        .get(USER_ID, &attempt_conversation.id)
        .await
        .unwrap();
    assert_eq!(cleaned_attempt.execution_step_id.as_deref(), Some(EXECUTION_STEP_ID_1));
    assert_eq!(
        cleaned_attempt.execution_attempt_id.as_deref(),
        Some(attempt_id.as_str())
    );
    assert!(
        execution_repo
            .delete_execution(
                USER_ID,
                &execution.id,
                6,
                &event(AgentExecutionEventKind::Deleted),
            )
            .await
            .unwrap()
    );

    let tombstoned_attempt = svc
        .get(USER_ID, &attempt_conversation.id)
        .await
        .unwrap();
    assert_eq!(
        tombstoned_attempt.linked_execution_id, None,
        "a retained transcript must not expose a soft-deleted execution route"
    );
    assert_eq!(
        tombstoned_attempt.execution_step_id.as_deref(),
        Some(EXECUTION_STEP_ID_1)
    );
    assert_eq!(
        tombstoned_attempt.execution_attempt_id.as_deref(),
        Some(attempt_id.as_str())
    );
    assert!(matches!(
        svc.send_message(
            USER_ID,
            &attempt_conversation.id,
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
            &attempt_conversation.id,
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
            &attempt_conversation.id,
            &runtime_registry,
        )
        .await
        .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert!(matches!(
        svc.warmup(
            USER_ID,
            &attempt_conversation.id,
            &runtime_registry,
        )
        .await
        .unwrap_err(),
        AppError::Conflict(_)
    ));
    assert_eq!(
        conversation_repo
            .get_messages(&attempt_conversation.id, 1, 20, SortOrder::Asc)
            .await
            .unwrap()
            .items
            .len(),
        2
    );

    let error = svc
        .delete(USER_ID, &attempt_conversation.id)
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::Conflict(_)));
    assert!(
        conversation_repo
            .get(&attempt_conversation.id)
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
        "extra": {}
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, req).await.unwrap();
    broadcaster.take_events();

    svc.delete(TEST_USER_1, &conv.id).await.unwrap();
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
    svc.update(TEST_USER_1, &conv.id, req, &runtime_registry).await.unwrap();
    let events = broadcaster.take_events();
    assert_eq!(events[0].data["action"], "updated");

    // Delete
    svc.delete(TEST_USER_1, &conv.id).await.unwrap();
    let events = broadcaster.take_events();
    assert_eq!(events[0].data["action"], "deleted");
}

// ── Ownership tests (M-3) ─────────────────────────────────────────

#[tokio::test]
async fn get_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let err = svc.get(TEST_USER_2, &conv.id).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn update_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let req: UpdateConversationRequest = serde_json::from_value(json!({ "name": "hacked" })).unwrap();
    let err = svc.update(TEST_USER_2, &conv.id, req, &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));

    // Original should be unchanged
    let original = svc.get(TEST_USER_1, &conv.id).await.unwrap();
    assert_ne!(original.name, "hacked");
}

#[tokio::test]
async fn delete_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let err = svc.delete(TEST_USER_2, &conv.id).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));

    // Should still exist
    let still_exists = svc.get(TEST_USER_1, &conv.id).await.unwrap();
    assert_eq!(still_exists.id, conv.id);
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

#[tokio::test]
async fn reset_sets_status_to_pending() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    svc.reset(TEST_USER_1, &conv.id).await.unwrap();

    let fetched = svc.get(TEST_USER_1, &conv.id).await.unwrap();
    assert_eq!(fetched.status, ConversationStatus::Pending);
}

#[tokio::test]
async fn reset_clears_conversation_artifacts() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.upsert_artifact(&ConversationArtifactRow {
        id: ConversationArtifactId::new().into_string(),
        conversation_id: conv.id.clone(),
        cron_job_id: Some("cron_1".into()),
        kind: "skill_suggest".into(),
        status: "pending".into(),
        payload: json!({ "cron_job_id": "cron_1", "name": "daily-report" }).to_string(),
        created_at: 1000,
        updated_at: 1000,
    })
    .await
    .unwrap();

    svc.reset(TEST_USER_1, &conv.id).await.unwrap();

    let artifacts = repo.list_artifacts(&conv.id).await.unwrap();
    assert!(artifacts.is_empty());
}

#[tokio::test]
async fn list_artifacts_does_not_project_retired_cron_trigger_messages() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    repo.insert_message(&MessageRow {
        id: "legacy-msg-1".into(),
        conversation_id: conv.id.clone(),
        msg_id: Some("legacy-trigger-1".into()),
        r#type: "cron_trigger".into(),
        content: json!({
            "cron_job_id": "cron_1",
            "cron_job_name": "Daily Report",
            "triggered_at": 1234
        })
        .to_string(),
        position: Some("center".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: 1234,
    })
    .await
    .unwrap();

    let artifacts = svc.list_artifacts(TEST_USER_1, &conv.id).await.unwrap();

    assert!(artifacts.is_empty());
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

    let err = svc.reset(TEST_USER_2, &conv.id).await.unwrap_err();
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
    /// Optional workspace override; falls back to "/tmp/test" when `None`.
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
        self.workspace_override.as_deref().unwrap_or("/tmp/test")
    }
    fn status(&self) -> Option<ConversationStatus> {
        None
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
    termination_records: Mutex<Vec<(String, Option<AgentKillReason>)>>,
    termination_count: AtomicUsize,
    termination_wait_count: AtomicUsize,
}

impl MockAgentRuntimeRegistry {
    fn new() -> Self {
        Self {
            agents: Mutex::new(std::collections::HashMap::new()),
            termination_records: Mutex::new(Vec::new()),
            termination_count: AtomicUsize::new(0),
            termination_wait_count: AtomicUsize::new(0),
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
        _options: AgentRuntimeBuildOptions,
    ) -> Result<AgentRuntimeHandle, AppError> {
        let mut agents = self.agents.lock().unwrap();
        if let Some(existing) = agents.get(conversation_id) {
            return Ok(existing.clone());
        }
        let instance = AgentRuntimeHandle::Mock(Arc::new(MockAgent::new(conversation_id)));
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
        _options: AgentRuntimeBuildOptions,
    ) -> Result<AgentRuntimeHandle, AppError> {
        self.build_calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.built.store(true, Ordering::SeqCst);
        Ok(AgentRuntimeHandle::Mock(Arc::new(MockAgent::new(conversation_id))))
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
            event_tx,
            scripts: Mutex::new(VecDeque::from(scripts)),
            sent_contents: Mutex::new(vec![]),
        }
    }

    fn with_agent_type(mut self, agent_type: AgentType) -> Self {
        self.agent_type = agent_type;
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
        "/tmp/test"
    }

    fn status(&self) -> Option<ConversationStatus> {
        Some(ConversationStatus::Finished)
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
        "/tmp/test"
    }
    fn status(&self) -> Option<ConversationStatus> {
        self.status
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
    fn steer(&self, text: String) -> Result<bool, AppError> {
        self.steered.lock().unwrap().push(text);
        if self.steer_err {
            return Err(AppError::BadRequest("Steering is not supported for this agent type".into()));
        }
        Ok(self.steer_result)
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
}

impl RecordingKnowledgeCompleter {
    fn new(response: String) -> Self {
        Self {
            response,
            prompts: Mutex::new(Vec::new()),
        }
    }

    fn prompts(&self) -> Vec<(String, String)> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl KnowledgeCompleter for RecordingKnowledgeCompleter {
    async fn complete(&self, system: &str, user: &str) -> Result<String, AppError> {
        self.prompts.lock().unwrap().push((system.to_owned(), user.to_owned()));
        Ok(self.response.clone())
    }
}

struct BlockingFirstKnowledgeCompleter {
    response: String,
    prompts: Mutex<Vec<(String, String)>>,
    calls: AtomicUsize,
    started: Notify,
    release: Notify,
}

impl BlockingFirstKnowledgeCompleter {
    fn new(response: String) -> Self {
        Self {
            response,
            prompts: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            started: Notify::new(),
            release: Notify::new(),
        }
    }

    async fn wait_started(&self) {
        self.started.notified().await;
    }

    fn release(&self) {
        self.release.notify_waiters();
    }
}

#[async_trait::async_trait]
impl KnowledgeCompleter for BlockingFirstKnowledgeCompleter {
    async fn complete(&self, system: &str, user: &str) -> Result<String, AppError> {
        self.prompts.lock().unwrap().push((system.to_owned(), user.to_owned()));
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.started.notify_waiters();
            self.release.notified().await;
        }
        Ok(self.response.clone())
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
    let msg_id = svc
        .send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry)
        .await
        .unwrap();

    assert!(!msg_id.is_empty(), "msg_id must be non-empty");
    assert!(msg_id.starts_with("msg_"), "msg_id should be a msg_-prefixed entity ID");
}

#[tokio::test]
async fn send_message_rejects_pathological_workspace_with_runtime_error_code() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let legacy_workspace = "/tmp/my project ".to_owned();
    repo.update(
        &conv.id,
        &ConversationRowUpdate {
            extra: Some(json!({ "workspace": legacy_workspace }).to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let err = svc
        .send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        AppError::WorkspacePathEdgeWhitespaceRuntimeUnsupported(message) if message == "/tmp/my project "
    ));

    let messages = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let messages = repo.get_messages(&conv.id, 1, 20, SortOrder::Asc).await.unwrap().items;
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

    let msg_id = svc
        .send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry)
        .await
        .unwrap();

    let events = broadcaster.take_events();
    let user_created = events
        .iter()
        .find(|e| e.name == "message.userCreated")
        .expect("should broadcast message.userCreated event");

    assert_eq!(user_created.data["conversation_id"], conv.id);
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

    svc.send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry)
        .await
        .unwrap();

    let events = broadcaster.take_events();
    let turn_started = events
        .iter()
        .find(|e| e.name == "turn.started")
        .expect("should broadcast turn.started event as soon as a turn is acquired");

    assert_eq!(turn_started.data["conversation_id"], conv.id);
    assert_eq!(turn_started.data["conversation_id"], conv.id);
    assert_eq!(turn_started.data["status"], "running");
    assert_eq!(turn_started.data["runtime"]["state"], "starting");
    assert_eq!(turn_started.data["runtime"]["is_processing"], true);
    assert_eq!(turn_started.data["runtime"]["can_send_message"], false);
    assert!(
        turn_started.data["runtime"]["processing_started_at"].is_number(),
        "turn.started runtime should expose a stable processing start timestamp"
    );
}

#[tokio::test]
async fn send_message_broadcasts_companion_markers_for_companion_conversation() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let create_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": "/project", "companion_session": true, "companion_id": "companion_0190f5fe-7c00-7a00-8abc-012345678942" }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, create_req).await.unwrap();
    broadcaster.take_events();

    svc.send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    let events = broadcaster.take_events();
    let user_created = events
        .iter()
        .find(|e| e.name == "message.userCreated")
        .expect("should broadcast message.userCreated event");
    assert_eq!(user_created.data["companion"], true);
    assert_eq!(user_created.data["companion_id"], "companion_0190f5fe-7c00-7a00-8abc-012345678942");

    let turn_completed = events
        .iter()
        .find(|e| e.name == "turn.completed")
        .expect("should broadcast turn.completed event");
    assert_eq!(turn_completed.data["companion"], true);
    assert_eq!(turn_completed.data["companion_id"], "companion_0190f5fe-7c00-7a00-8abc-012345678942");
}

#[tokio::test]
async fn send_message_stamps_channel_platform_for_channel_agent_conversation() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    // A Channel Agent conversation (see nomifun-channel's
    // apply_master_agent_extra): companion_session + companion_id + channel_platform.
    let create_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": "/project", "companion_session": true, "companion_id": "companion_0190f5fe-7c00-7a00-8abc-012345678942", "channel_platform": "telegram" }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, create_req).await.unwrap();
    broadcaster.take_events();

    svc.send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    let events = broadcaster.take_events();
    let user_created = events
        .iter()
        .find(|e| e.name == "message.userCreated")
        .expect("should broadcast message.userCreated event");
    assert_eq!(user_created.data["channel_platform"], "telegram");
    assert_eq!(user_created.data["companion"], true);
    assert_eq!(user_created.data["companion_id"], "companion_0190f5fe-7c00-7a00-8abc-012345678942");

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
    svc.send_message(TEST_USER_1, &conv.id, req, &runtime_registry).await.unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

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

    svc.send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

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
        svc.send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry),
    )
    .await
    .expect("send_message should return before cold agent build finishes")
    .unwrap();

    assert!(!msg_id.is_empty(), "msg_id must be non-empty");
    assert!(
        !slow_runtime_registry.was_built(),
        "cold agent build should continue in the background after send_message returns"
    );

    let updated = repo.get(&conv.id).await.unwrap().unwrap();
    assert_ne!(updated.status.as_deref(), Some("running"));
    assert!(
        svc.runtime_state().has_active_turn(&conv.id),
        "turn handle must cover the cold Agent build window"
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
    let request: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": "/project" }
    }))
    .unwrap();
    let conversation = svc.create(USER_ID, request).await.unwrap();
    let operation_id = "execution:decision:stable";

    let first = svc
        .send_message_idempotent(
            USER_ID,
            &conversation.id,
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
            &conversation.id,
            operation_id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert_eq!(replay_while_pending.message_id, first.message_id);
    assert!(!replay_while_pending.completed);

    wait_for_turn_released(&svc, &conversation.id).await;
    assert_eq!(
        slow_registry.build_calls(),
        1,
        "a replay while the stable user transcript is processing must not start another model turn"
    );
    let completed_replay = svc
        .send_message_idempotent(
            USER_ID,
            &conversation.id,
            operation_id,
            make_send_req(),
            &runtime_registry,
        )
        .await
        .unwrap();
    assert!(completed_replay.completed);
    assert_eq!(slow_registry.build_calls(), 1);

    let user_messages = repo
        .get_messages(&conversation.id, 1, 20, SortOrder::Asc)
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|message| message.position.as_deref() == Some("right"))
        .collect::<Vec<_>>();
    assert_eq!(user_messages.len(), 1);
    nomifun_common::MessageId::parse(&user_messages[0].id)
        .expect("idempotent user transcript row has a canonical message ID");
    assert_eq!(user_messages[0].id, first.message_id);
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

    svc.send_message(TEST_USER_1, &conv.id, req, &runtime_registry).await.unwrap();

    let messages = repo.get_messages(&conv.id, 1, 20, SortOrder::Asc).await.unwrap().items;
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

    let msg_id = svc
        .send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry)
        .await
        .unwrap();

    assert!(!msg_id.is_empty(), "msg_id must be non-empty");

    let messages = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let messages = repo.get_messages(&conv.id, 1, 20, SortOrder::Asc).await.unwrap().items;
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

    let updated = repo.get(&conv.id).await.unwrap().unwrap();
    assert_eq!(updated.status.as_deref(), Some("finished"));
    assert!(
        !svc.runtime_state().has_active_turn(&conv.id),
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

    let err = svc.send_message(TEST_USER_1, &conv.id, req, &runtime_registry).await.unwrap_err();
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

    let err = svc.send_message(TEST_USER_1, &conv.id, req, &runtime_registry).await.unwrap_err();
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
        .send_message(TEST_USER_2, &conv.id, make_send_req(), &runtime_registry)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn send_message_allows_stale_db_running_without_active_turn() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    // Manually set status to running
    let update = ConversationRowUpdate {
        status: Some("running".into()),
        ..Default::default()
    };
    repo.update(&conv.id, &update).await.unwrap();

    let result = svc.send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry).await;
    assert!(result.is_ok(), "stale DB running must not block sending");
}

#[tokio::test]
async fn send_message_rejects_active_turn() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let _turn_handle = svc
        .runtime_state()
        .try_acquire_turn(&conv.id)
        .expect("test turn handle should be acquired");

    let err = svc
        .send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry)
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
        "extra": {}
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, req).await.unwrap();

    let invalid_extra = ConversationRowUpdate {
        extra: Some(r#"{"workspace":""}"#.to_owned()),
        ..Default::default()
    };
    repo.update(&conv.id, &invalid_extra).await.unwrap();

    let error = svc
        .send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AppError::Internal(message)
            if message.contains("neither a custom workspace nor a canonical temp_workspace_id")
    ));
}

#[tokio::test]
async fn build_runtime_options_rebases_managed_workspace_after_restore() {
    let destination_root =
        std::env::temp_dir().join(format!("nomifun-rebase-{}", nomifun_common::generate_id()));
    let (svc, _broadcaster, repo, _runtime_registry) =
        make_service_with_workspace_root(destination_root.clone());
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "backend": "claude" }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, req).await.unwrap();
    let temp_workspace_id = conv.extra["temp_workspace_id"]
        .as_str()
        .expect("create must stamp temp_workspace_id")
        .to_owned();

    let restored_extra = json!({
        "backend": "claude",
        "temp_workspace_id": temp_workspace_id,
        "workspace": "/source-install/conversations/claude-temp-stale",
        "skills": []
    });
    repo.update(
        &conv.id,
        &ConversationRowUpdate {
            extra: Some(restored_extra.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let response = svc.get(TEST_USER_1, &conv.id).await.unwrap();
    let expected = destination_root
        .join("conversations")
        .join(format!("claude-temp-{temp_workspace_id}"));
    assert_eq!(
        PathBuf::from(response.extra["workspace"].as_str().unwrap()),
        expected
    );
    let row = repo.get(&conv.id).await.unwrap().unwrap();

    let options = svc.build_runtime_options(&row).unwrap();
    assert_eq!(PathBuf::from(options.workspace), expected);
    assert_ne!(
        options.extra["workspace"],
        "/source-install/conversations/claude-temp-stale"
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
            "backend": "claude",
            "workspace": custom_workspace.to_string_lossy()
        }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, req).await.unwrap();
    let row = repo.get(&conv.id).await.unwrap().unwrap();

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
        &conv.id,
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
    runtime_registry.insert_agent(&conv.id, AgentRuntimeHandle::Mock(scripted_agent.clone()));
    svc.with_cron_service(Some(Arc::new(MockCronContinuationService)));

    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    let req: SendMessageRequest = serde_json::from_value(json!({
        "content": "Create the task now"
    }))
    .unwrap();

    svc.send_message(TEST_USER_1, &conv.id, req, &runtime_registry_dyn).await.unwrap();

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

    wait_for_turn_released(&svc, &conv.id).await;

    let finished = svc.get(TEST_USER_1, &conv.id).await.unwrap();
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
        "type": "acp",
        "extra": { "workspace": workspace }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, create_req).await.unwrap();
    let kb = knowledge.create_base("turn-final", "", None, None).await.unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at) \
         VALUES (?, ?, 'turn-final', 'acp', 'pending', 1, 1)",
    )
    .bind(&conv.id)
    .bind(&knowledge_owner)
    .execute(knowledge_db.pool())
    .await
    .unwrap();
    knowledge
        .set_binding(
            "conversation",
            &conv.id,
            KnowledgeBinding {
                enabled: true,
                writeback: true,
                writeback_mode: "staged".into(),
                writeback_eagerness: "aggressive".into(),
                kb_ids: vec![kb.id.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let candidate = format!(
        r##"{{"candidates":[{{"kb_id":"{}","rel_path":"patterns/cron-final.md","content":"# Final scheduling lesson\n\nThe final answer after cron continuation is durable."}}]}}"##,
        kb.id
    );
    let completer = Arc::new(RecordingKnowledgeCompleter::new(candidate));
    knowledge.set_completer(completer.clone());

    let scripted_agent = Arc::new(ScriptedAgent::new(
        &conv.id,
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
    ));
    runtime_registry.insert_agent(&conv.id, AgentRuntimeHandle::Mock(scripted_agent.clone()));
    svc.with_cron_service(Some(Arc::new(MockCronContinuationService)));
    broadcaster.take_events();

    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    let req: SendMessageRequest = serde_json::from_value(json!({
        "content": "Create the task now"
    }))
    .unwrap();

    svc.send_message(TEST_USER_1, &conv.id, req, &runtime_registry_dyn).await.unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

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
        turn_idx < first_writeback_idx,
        "turn completion must be visible only after the turn handle has been released, before writeback post-processing"
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
        .get_message(&conv.id, msg_id)
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
        .filter(|message| message.conversation_id == conv.id && message.r#type == "thinking")
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
    assert!(rel_path.starts_with(&format!("_inbox/{}/", conv.id)));
    assert!(rel_path.ends_with("/patterns/cron-final.md"));
    let staged = knowledge.read_file(&kb.id, rel_path).await.unwrap();
    assert!(staged.content.contains("final answer after cron continuation"));

    let prompts = completer.prompts();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].1.contains("Create the task now"));
    assert!(prompts[0].1.contains("Done. The task is scheduled."));
    assert!(
        !prompts[0].1.contains("[System: No scheduled tasks]"),
        "hidden system continuation text must not replace the human turn input"
    );

    let _ = tokio::fs::remove_dir_all(&workspace).await;
    let _ = tokio::fs::remove_dir_all(&data_dir).await;
}

#[tokio::test]
async fn send_message_releases_turn_before_slow_turn_writeback_finishes() {
    let (svc, _broadcaster, _repo, _default_runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
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
            Arc::from(TEST_USER_1),
        ),
    ));
    svc.with_knowledge_service(knowledge.clone());

    let create_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace }
    }))
    .unwrap();
    let conv = svc.create(TEST_USER_1, create_req).await.unwrap();
    let kb = knowledge.create_base("turn-final", "", None, None).await.unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at) \
         VALUES (?, ?, 'turn-final', 'acp', 'pending', 1, 1)",
    )
    .bind(&conv.id)
    .bind(&knowledge_owner)
    .execute(knowledge_db.pool())
    .await
    .unwrap();
    knowledge
        .set_binding(
            "conversation",
            &conv.id,
            KnowledgeBinding {
                enabled: true,
                writeback: true,
                writeback_mode: "staged".into(),
                writeback_eagerness: "aggressive".into(),
                kb_ids: vec![kb.id.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let completer = Arc::new(BlockingFirstKnowledgeCompleter::new(r#"{"candidates":[]}"#.into()));
    knowledge.set_completer(completer.clone());

    let scripted_agent = Arc::new(ScriptedAgent::new(
        &conv.id,
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
    ));
    runtime_registry.insert_agent(&conv.id, AgentRuntimeHandle::Mock(scripted_agent));
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    let first_req: SendMessageRequest = serde_json::from_value(json!({ "content": "first" })).unwrap();
    svc.send_message(TEST_USER_1, &conv.id, first_req, &runtime_registry_dyn)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), completer.wait_started())
        .await
        .expect("turn-final writeback should start");

    let second_req: SendMessageRequest = serde_json::from_value(json!({ "content": "second" })).unwrap();
    let second = svc
        .send_message(TEST_USER_1, &conv.id, second_req, &runtime_registry_dyn)
        .await;
    completer.release();
    wait_for_turn_released(&svc, &conv.id).await;

    assert!(
        second.is_ok(),
        "slow turn-final writeback must not keep the conversation running: {second:?}"
    );

    let _ = tokio::fs::remove_dir_all(&workspace).await;
    let _ = tokio::fs::remove_dir_all(&data_dir).await;
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
    let conv_id = conv.id.clone();

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
    assert!(msg_id.starts_with("msg_"), "msg_id should be a msg_-prefixed entity ID");

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
    assert_eq!(stored[0].id, msg_id);
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
    let conv_id = conv.id.clone();

    // No agent registered → get_runtime() returns None → fall back to send_message.
    let req: SendMessageRequest = serde_json::from_value(json!({ "content": "start working" })).unwrap();
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    let msg_id = svc
        .steer_message(TEST_USER_1, &conv_id, req, &runtime_registry_dyn)
        .await
        .unwrap();
    assert!(msg_id.starts_with("msg_"), "fallback must return a real msg_ id");

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
        stored.iter().any(|m| m.id == msg_id && m.position.as_deref() == Some("right")),
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
    let conv_id = conv.id.clone();

    // Live agent but status is Finished (turn already over) → no steering.
    let agent = Arc::new(SteerableAgent::new(&conv_id, Some(ConversationStatus::Finished), true));
    runtime_registry.insert_agent(&conv_id, AgentRuntimeHandle::Mock(agent.clone()));

    let req: SendMessageRequest = serde_json::from_value(json!({ "content": "anything" })).unwrap();
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    svc.steer_message(TEST_USER_1, &conv_id, req, &runtime_registry_dyn)
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
    let conv_id = conv.id.clone();

    // Running at the status check, but steer() reports the turn already ended.
    let agent = Arc::new(SteerableAgent::new(&conv_id, Some(ConversationStatus::Running), false));
    runtime_registry.insert_agent(&conv_id, AgentRuntimeHandle::Mock(agent.clone()));

    let req: SendMessageRequest =
        serde_json::from_value(json!({ "content": "race-tail interjection" })).unwrap();
    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();

    let msg_id = svc
        .steer_message(TEST_USER_1, &conv_id, req, &runtime_registry_dyn)
        .await
        .unwrap();
    assert!(msg_id.starts_with("msg_"), "fallback must return a real msg_ id");

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
    let conv_id = conv.id.clone();

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
    let conv_id = conv.id.clone();
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
        &conv.id,
        vec![vec![AgentStreamEvent::Finish(FinishEventData::default())]],
    ));
    runtime_registry.insert_agent(&conv.id, AgentRuntimeHandle::Mock(scripted_agent));

    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    svc.send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

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
            &conv.id,
            vec![vec![AgentStreamEvent::Error(ErrorEventData::legacy(
                "nomi terminal error",
                Some(AgentErrorCode::UnknownUpstreamError),
            ))]],
        )
        .with_agent_type(AgentType::Nomi),
    );
    runtime_registry.insert_agent(&conv.id, AgentRuntimeHandle::Mock(scripted_agent));

    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    svc.send_message(TEST_USER_1, &conv.id, make_send_req(), &runtime_registry_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

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
    svc.send_message(
        TEST_USER_1,
        &conv.id,
        make_send_req(),
        &(runtime_registry.clone() as Arc<dyn AgentRuntimeRegistry>),
    )
    .await
    .unwrap();

    // Stop should succeed since agent exists
    let result = svc
        .cancel(TEST_USER_1, &conv.id, &(runtime_registry as Arc<dyn AgentRuntimeRegistry>))
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
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let result = svc.cancel(TEST_USER_1, &conv.id, &runtime_registry).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn execution_cleanup_does_not_record_a_user_cancel_stamp() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let since = nomifun_common::now_ms();

    svc.cancel_for_execution(
        TEST_USER_1,
        &conv.id,
        &runtime_registry,
    )
    .await
    .unwrap();

    assert!(!svc.user_cancelled_since(&conv.id, since));
}

#[tokio::test]
async fn stop_stream_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let err = svc.cancel(TEST_USER_2, &conv.id, &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

// ── warmup tests ────────────────────────────────────────────────

#[tokio::test]
async fn warmup_creates_agent_runtime() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let result = svc
        .warmup(TEST_USER_1, &conv.id, &(runtime_registry.clone() as Arc<dyn AgentRuntimeRegistry>))
        .await;
    assert!(result.is_ok());

    // Agent should now exist
    assert!(runtime_registry.get_runtime(&conv.id).is_some());
}

#[tokio::test]
async fn warmup_conversation_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let err = svc.warmup(TEST_USER_1, "no-such-id", &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn warmup_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let err = svc.warmup(TEST_USER_2, &conv.id, &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn warmup_rejects_pathological_workspace_with_runtime_error_code() {
    let (svc, _broadcaster, repo, _runtime_registry) = make_service();
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let legacy_workspace = "/tmp/my project ".to_owned();
    repo.update(
        &conv.id,
        &ConversationRowUpdate {
            extra: Some(json!({ "workspace": legacy_workspace }).to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let err = svc.warmup(TEST_USER_1, &conv.id, &runtime_registry).await.unwrap_err();
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
    let result = svc.list_confirmations(TEST_USER_1, &conv.id, &runtime_registry).await.unwrap();
    assert!(result.is_empty());
}

#[tokio::test]
async fn list_confirmations_returns_items() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let agent = AgentRuntimeHandle::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.id,
        make_test_confirmations(),
    )));
    runtime_registry.insert_agent(&conv.id, agent);

    let result = svc
        .list_confirmations(TEST_USER_1, &conv.id, &(runtime_registry as Arc<dyn AgentRuntimeRegistry>))
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
    let err = svc.list_confirmations(TEST_USER_2, &conv.id, &runtime_registry).await.unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn confirm_removes_confirmation_and_broadcasts() {
    let (svc, broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    broadcaster.take_events(); // clear create event

    let agent = AgentRuntimeHandle::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.id,
        make_test_confirmations(),
    )));
    runtime_registry.insert_agent(&conv.id, agent);

    let req = nomifun_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: false,
    };
    svc.confirm(
        TEST_USER_1,
        &conv.id,
        "call-1",
        req,
        &(runtime_registry.clone() as Arc<dyn AgentRuntimeRegistry>),
    )
    .await
    .unwrap();

    // Confirmation should be removed from the agent
    let remaining = runtime_registry.get_runtime(&conv.id).unwrap().get_confirmations();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].call_id, "call-2");

    // Should broadcast confirmation.remove event
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "confirmation.remove");
    assert_eq!(events[0].data["conversation_id"], conv.id);
    assert_eq!(events[0].data["id"], "c1");
}

#[tokio::test]
async fn confirm_with_always_allow_stores_approval() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let agent = AgentRuntimeHandle::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.id,
        make_test_confirmations(),
    )));
    runtime_registry.insert_agent(&conv.id, agent);

    let req = nomifun_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: true,
    };
    let runtime_registry_arc: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    svc.confirm(TEST_USER_1, &conv.id, "call-1", req, &runtime_registry_arc)
        .await
        .unwrap();

    // check_approval should now return true for edit_file:bash
    let agent = runtime_registry.get_runtime(&conv.id).unwrap();
    assert!(agent.check_approval("edit_file", Some("bash")));
    assert!(!agent.check_approval("delete_file", None));
}

#[tokio::test]
async fn confirm_nonexistent_call_id_returns_not_found() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let agent = AgentRuntimeHandle::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.id,
        make_test_confirmations(),
    )));
    runtime_registry.insert_agent(&conv.id, agent);

    let req = nomifun_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: false,
    };
    let err = svc
        .confirm(
            TEST_USER_1,
            &conv.id,
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

    let agent = AgentRuntimeHandle::Mock(Arc::new(MockAgent::with_direct_confirm(&conv.id)));
    runtime_registry.insert_agent(&conv.id, agent);

    let req = nomifun_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!("allow_once"),
        always_allow: false,
    };
    svc.confirm(
        TEST_USER_1,
        &conv.id,
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
        .confirm(TEST_USER_1, &conv.id, "call-1", req, &runtime_registry)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)));
}

#[tokio::test]
async fn check_approval_returns_false_when_not_set() {
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());

    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let agent = AgentRuntimeHandle::Mock(Arc::new(MockAgent::new(&conv.id)));
    runtime_registry.insert_agent(&conv.id, agent);

    let result = svc
        .check_approval(
            TEST_USER_1,
            &conv.id,
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
        &conv.id,
        make_test_confirmations(),
    )));
    runtime_registry.insert_agent(&conv.id, agent);

    // Confirm with always_allow
    let req = nomifun_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: true,
    };
    let runtime_registry_arc: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    svc.confirm(TEST_USER_1, &conv.id, "call-1", req, &runtime_registry_arc)
        .await
        .unwrap();

    // Now check_approval should return true
    let result = svc
        .check_approval(TEST_USER_1, &conv.id, "edit_file", Some("bash"), &runtime_registry_arc)
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
        .check_approval(TEST_USER_1, &conv.id, "edit_file", None, &runtime_registry)
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
}

#[tokio::test]
async fn create_writes_empty_skills_when_no_auto_inject_and_no_preset() {
    let resolver = Arc::new(FixedSkillResolver { names: vec![] });
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service_with_resolver(resolver);

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": "/project", "backend": "claude" },
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
    svc.warmup(TEST_USER_1, &resp.id, &runtime_registry).await.unwrap();

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
        "extra": { "workspace": "/project", "backend": "claude" },
    }))
    .unwrap();
    let resp = svc.create(TEST_USER_1, req).await.unwrap();

    let update_req: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "skills": ["cron"] },
    }))
    .unwrap();
    let err = svc
        .update(TEST_USER_1, &resp.id, update_req, &runtime_registry)
        .await
        .unwrap_err();

    match err {
        AppError::BadRequest(msg) => assert!(msg.contains("skills"), "msg = {msg:?}"),
        other => panic!("expected BadRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn update_allows_other_extra_fields() {
    let (svc, _broadcaster, _repo, runtime_registry) = make_service();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": "/project", "backend": "claude" },
    }))
    .unwrap();
    let resp = svc.create(TEST_USER_1, req).await.unwrap();

    let update_req: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "current_model_id": "claude-3-5-sonnet" },
    }))
    .unwrap();
    let updated = svc
        .update(TEST_USER_1, &resp.id, update_req, &runtime_registry)
        .await
        .unwrap();

    assert_eq!(updated.extra["current_model_id"], "claude-3-5-sonnet");
}

#[tokio::test]
async fn get_backfills_legacy_row_and_persists() {
    let resolver = Arc::new(FixedSkillResolver {
        names: vec!["cron".into(), "todo-tracker".into()],
    });
    let (svc, _broadcaster, repo, _runtime_registry) = make_service_with_resolver(resolver);

    // Seed a legacy row directly via the repo — simulates a pre-migration
    // conversation that the service has never touched.
    let legacy_row = ConversationRow {
        cron_job_id: None,
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        id: ConversationId::new().into_string(),
        user_id: TEST_USER_1.into(),
        name: "legacy".into(),
        r#type: "acp".into(),
        extra: serde_json::to_string(&json!({
            "workspace": "/tmp/x",
            "enabled_skills": ["pdf"],
            "exclude_builtin_skills": ["todo-tracker"],
            "loaded_skills": [{"name": "cron", "description": "stale"}],
        }))
        .unwrap(),
        delegation_policy: "automatic".into(),
        execution_model_pool: None,
        decision_policy: "automatic".into(),
        execution_template_id: None,
        model: None,
        status: Some("finished".into()),
        source: Some("nomifun".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 0,
        updated_at: 0,
    };
    let legacy_id = repo.create(&legacy_row).await.unwrap();

    let resp = svc.get(TEST_USER_1, &legacy_id).await.unwrap();
    assert_eq!(resp.extra["skills"], json!(["cron", "pdf"]));
    assert!(resp.extra.get("enabled_skills").is_none());
    assert!(resp.extra.get("exclude_builtin_skills").is_none());
    assert!(resp.extra.get("loaded_skills").is_none());

    // Second read returns the same result.
    let resp2 = svc.get(TEST_USER_1, &legacy_id).await.unwrap();
    assert_eq!(resp2.extra["skills"], json!(["cron", "pdf"]));

    // Verify the row on disk was persisted with the new shape.
    let persisted = repo.get(&legacy_id).await.unwrap().unwrap();
    let persisted_extra: serde_json::Value = serde_json::from_str(&persisted.extra).unwrap();
    assert_eq!(persisted_extra["skills"], json!(["cron", "pdf"]));
    assert!(persisted_extra.get("enabled_skills").is_none());
    assert!(persisted_extra.get("exclude_builtin_skills").is_none());
    assert!(persisted_extra.get("loaded_skills").is_none());
}

#[tokio::test]
async fn list_backfills_mixed_rows() {
    let resolver = Arc::new(FixedSkillResolver {
        names: vec!["cron".into()],
    });
    let (svc, _broadcaster, repo, _runtime_registry) = make_service_with_resolver(resolver);

    // Row 1: legacy (needs backfill).
    let legacy = ConversationRow {
        cron_job_id: None,
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        id: ConversationId::new().into_string(),
        user_id: "u".into(),
        name: "a".into(),
        r#type: "acp".into(),
        extra: serde_json::to_string(&json!({
            "workspace": "/tmp/a",
            "enabled_skills": ["pdf"],
        }))
        .unwrap(),
        delegation_policy: "automatic".into(),
        execution_model_pool: None,
        decision_policy: "automatic".into(),
        execution_template_id: None,
        model: None,
        status: None,
        source: None,
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 1,
        updated_at: 1,
    };
    // Row 2: already migrated.
    let modern = ConversationRow {
        cron_job_id: None,
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        id: ConversationId::new().into_string(),
        user_id: "u".into(),
        name: "b".into(),
        r#type: "acp".into(),
        extra: serde_json::to_string(&json!({
            "workspace": "/tmp/b",
            "skills": ["cron", "pdf"],
        }))
        .unwrap(),
        delegation_policy: "automatic".into(),
        execution_model_pool: None,
        decision_policy: "automatic".into(),
        execution_template_id: None,
        model: None,
        status: None,
        source: None,
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 2,
        updated_at: 2,
    };
    repo.create(&legacy).await.unwrap();
    repo.create(&modern).await.unwrap();

    let resp = svc.list("u", ListConversationsQuery::default(), false).await.unwrap();
    let extras: Vec<_> = resp.items.iter().map(|c| c.extra.clone()).collect();
    assert!(extras.iter().any(|e| e["skills"] == json!(["cron", "pdf"])));
}

#[tokio::test]
async fn create_honors_legacy_alias_fields_from_clone_merge() {
    let resolver = Arc::new(FixedSkillResolver {
        names: vec!["cron".into()],
    });
    let (svc, _broadcaster, _repo, _runtime_registry) = make_service_with_resolver(resolver);

    // Legacy-shaped extra — what clone_create might merge in from an
    // unmigrated source conversation.
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "workspace": "/project",
            "backend": "claude",
            "enabled_skills": ["pdf"],
            "exclude_builtin_skills": ["cron"],
            "loaded_skills": [{"name": "cron", "description": "stale"}],
        },
    }))
    .unwrap();
    let resp = svc.create(TEST_USER_1, req).await.unwrap();

    // Legacy enabled_skills ["pdf"] surfaces as preset; legacy exclude drops
    // cron; snapshot = {} ∪ ["pdf"] = ["pdf"].
    assert_eq!(resp.extra["skills"], json!(["pdf"]));
    assert!(resp.extra.get("enabled_skills").is_none());
    assert!(resp.extra.get("exclude_builtin_skills").is_none());
    assert!(resp.extra.get("loaded_skills").is_none());
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
        id: id.into(),
        platform: "openai".into(),
        name: id.into(),
        base_url: "https://example.com".into(),
        api_key_encrypted: "x".into(),
        models: serde_json::to_string(models).unwrap(),
        enabled: true,
        capabilities: "[]".into(),
        context_limit: None,
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
        Ok(self.providers.iter().find(|p| p.id == id).cloned())
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
            .find(|p| p.id == id)
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
    let temp_workspace_id = nomifun_common::generate_prefixed_id("ws");
    let row = ConversationRow {
        cron_job_id: None,
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        id: ConversationId::new().into_string(),
        user_id: TEST_USER_1.into(),
        name: "failover".into(),
        r#type: "nomi".into(),
        extra: serde_json::to_string(&json!({
            "temp_workspace_id": temp_workspace_id,
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

    svc.send_message(TEST_USER_1, &conv_id, make_send_req(), &runtime_registry_dyn)
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

    svc.send_message(TEST_USER_1, &conv_id, make_send_req(), &runtime_registry_dyn)
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

    svc.send_message(TEST_USER_1, &conv_id, make_send_req(), &runtime_registry_dyn)
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

    svc.send_message(TEST_USER_1, &conv_id, make_send_req(), &runtime_registry_dyn)
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

    svc.send_message(TEST_USER_1, &conv_id, make_send_req(), &runtime_registry_dyn)
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

    svc.send_message(TEST_USER_1, &conv_id, make_send_req(), &runtime_registry_dyn)
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

    svc.send_message(TEST_USER_1, &conv_id, make_send_req(), &runtime_registry_dyn)
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
    let row = ConversationRow {
        cron_job_id: None,
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        id: ConversationId::new().into_string(),
        user_id: TEST_USER_1.into(),
        name: "acp-failover".into(),
        r#type: "acp".into(),
        extra: serde_json::to_string(&json!({
            "workspace": "/project",
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

    svc.send_message(TEST_USER_1, &conv_id, make_send_req(), &runtime_registry_dyn)
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
    // review #11(2): the SHARED bottleneck `perform_model_failover` (review #9)
    // gates on AgentType::Nomi AFTER loading the row, so the IDMM path
    // (`idmm_failover_conversation`) reports `Ok(false)` for an ACP conversation
    // even with deps wired + an enabled queue — and performs NO termination / model write.
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

    let switched = svc
        .idmm_failover_conversation(TEST_USER_1, &conv_id, &runtime_registry_dyn)
        .await
        .unwrap();
    assert!(!switched, "IDMM failover must report false for an ACP conversation");
    assert_eq!(runtime_registry.termination_count(), 0, "no termination on a rejected ACP failover");
    let row = repo.get(&conv_id).await.unwrap().unwrap();
    let model: ProviderWithModel = serde_json::from_str(row.model.as_deref().unwrap()).unwrap();
    assert_eq!(model.provider_id, PROVIDER_ID_1, "ACP model must be untouched");
    assert!(provider_repo.health_writes().is_empty());
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
    let conv_id = conv.id.clone();

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
    let conv_id = conv.id.clone();

    let req: SendMessageRequest = serde_json::from_value(json!({ "content": "edited" })).unwrap();
    let err = svc
        .edit_and_resubmit(TEST_USER_1, &conv_id, MESSAGE_ID_1, req, &runtime_registry)
        .await
        .unwrap_err();

    assert!(matches!(err, AppError::BadRequest(_)));
}
