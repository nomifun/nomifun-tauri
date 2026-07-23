//! Black-box integration tests for `CronService`.
//!
//! Uses real SQLite (in-memory), mock broadcaster, and stubs for
//! runtime registry / conversation service (since integration with AI agents
//! is out of scope for this service-layer test).
//!
//! Covers test-plan items: CJ-1..CJ-12, SK-1..SK-7, SC-1..SC-8,
//! OC-1, SR-1, ICronService trait integration.

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use nomifun_ai_agent::AgentRegistry;
use nomifun_ai_agent::runtime_handle::AgentRuntimeHandle;
use nomifun_ai_agent::types::AgentRuntimeBuildOptions;
use nomifun_api_types::{
    CreateCronJobRequest, CronAgentConfigDto, CronScheduleDto, ListCronJobsQuery,
    SaveCronSkillRequest, UpdateCronJobRequest, WebSocketMessage,
};
use nomifun_common::{PaginatedResult, TimestampMs, now_ms};
use nomifun_conversation::ConversationService;
use nomifun_conversation::response_middleware::{CronCreateParams, CronUpdateParams};
use nomifun_db::{
    ConversationFilters, ConversationRowUpdate, IAcpSessionRepository, IAgentMetadataRepository,
    IConversationRepository, ICronRepository, MessageRowUpdate, MessageSearchRow, SortOrder,
    SqliteAcpSessionRepository, SqliteAgentMetadataRepository, SqliteConversationRepository,
    SqliteCronRepository, models::MessageRow,
};
use nomifun_realtime::UserEventSink;

use nomifun_cron::busy_guard::CronBusyGuard;
use nomifun_cron::events::CronEventEmitter;
use nomifun_cron::executor::JobExecutor;
use nomifun_cron::scheduler::CronScheduler;
use nomifun_cron::service::CronService;
use nomifun_cron::skill_file::{has_skill_file, write_raw_skill_file};
use nomifun_cron::types::JobStatus;

const TEST_USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
const CONV_1: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
const CONV_2: &str = "0190f5fe-7c00-7a00-8abc-012345678902";
const CONV_3: &str = "0190f5fe-7c00-7a00-8abc-012345678903";
const CONV_4: &str = "0190f5fe-7c00-7a00-8abc-012345678904";
const CONV_5: &str = "0190f5fe-7c00-7a00-8abc-012345678905";
const CONV_6: &str = "0190f5fe-7c00-7a00-8abc-012345678906";
const CONV_7: &str = "0190f5fe-7c00-7a00-8abc-012345678907";
const CONV_8: &str = "0190f5fe-7c00-7a00-8abc-012345678908";
const CONV_MISSING: &str = "0190f5fe-7c00-7a00-8abc-012345678909";
const CONV_MODE: &str = "0190f5fe-7c00-7a00-8abc-012345678910";
const CONV_MODE_DEFAULT: &str = "0190f5fe-7c00-7a00-8abc-012345678911";
const CONV_MODE_CODEX: &str = "0190f5fe-7c00-7a00-8abc-012345678912";
const CONV_MODE_CLAUDE: &str = "0190f5fe-7c00-7a00-8abc-012345678913";
const CONV_MODE_NOMI: &str = "0190f5fe-7c00-7a00-8abc-012345678914";
const ARTIFACT_1: &str = "0190f5fe-7c00-7a00-8abc-012345678915";
const MISSING_JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-ffffffffffff";
const SECONDARY_USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000002";
const SAFE_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000002";
const FOREIGN_USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000003";
const OWNER_A_ID: &str = "0190f5fe-7c00-7a00-8000-000000000004";
const OWNER_B_ID: &str = "0190f5fe-7c00-7a00-8000-000000000005";
const GEMINI_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000003";
const CODEX_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000004";
const CLAUDE_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000005";
const NOMI_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000006";

async fn init_database_memory() -> Result<nomifun_db::Database, nomifun_db::DbError> {
    nomifun_db::init_database_memory_with_owner(
        nomifun_common::UserId::parse(TEST_USER_ID.to_owned()).expect("canonical fixture owner"),
    )
    .await
}

// ── Test infrastructure ────────────────────────────────────────────

struct MockBroadcaster {
    events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    deliveries: Mutex<Vec<(String, WebSocketMessage<serde_json::Value>)>>,
}

impl MockBroadcaster {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            deliveries: Mutex::new(Vec::new()),
        }
    }

    fn take_events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
        let mut guard = self.events.lock().unwrap();
        std::mem::take(&mut *guard)
    }

    fn take_deliveries(&self) -> Vec<(String, WebSocketMessage<serde_json::Value>)> {
        let mut guard = self.deliveries.lock().unwrap();
        std::mem::take(&mut *guard)
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

struct StubAgentRuntimeRegistry;

#[async_trait::async_trait]
impl nomifun_ai_agent::runtime_registry::AgentRuntimeRegistry for StubAgentRuntimeRegistry {
    fn get_runtime(&self, _: &str) -> Option<AgentRuntimeHandle> {
        None
    }
    async fn get_or_create_runtime(
        &self,
        _: &str,
        _: AgentRuntimeBuildOptions,
    ) -> Result<AgentRuntimeHandle, nomifun_common::AppError> {
        Err(nomifun_common::AppError::Internal("stub".into()))
    }
    fn terminate(
        &self,
        _: &str,
        _: Option<nomifun_common::AgentKillReason>,
    ) -> Result<(), nomifun_common::AppError> {
        Ok(())
    }
    fn terminate_and_wait(
        &self,
        _: &str,
        _: Option<nomifun_common::AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }
    fn terminate_all(&self) {}
    fn active_runtime_count(&self) -> usize {
        0
    }
    fn collect_idle_runtimes(&self, _: TimestampMs) -> Vec<String> {
        vec![]
    }
}

struct StubConvRepo {
    messages: Mutex<Vec<MessageRow>>,
    artifacts: Mutex<Vec<nomifun_db::ConversationArtifactRow>>,
    rows: Mutex<HashMap<String, nomifun_db::models::ConversationRow>>,
    fail_cron_binding: AtomicBool,
}

impl StubConvRepo {
    fn new() -> Self {
        Self {
            messages: Mutex::new(Vec::new()),
            artifacts: Mutex::new(Vec::new()),
            rows: Mutex::new(HashMap::new()),
            fail_cron_binding: AtomicBool::new(false),
        }
    }

    fn set_fail_cron_binding(&self, fail: bool) {
        self.fail_cron_binding.store(fail, Ordering::SeqCst);
    }

    fn take_messages(&self) -> Vec<MessageRow> {
        let mut guard = self.messages.lock().unwrap();
        std::mem::take(&mut *guard)
    }

    fn upsert_artifact_row(&self, artifact: nomifun_db::ConversationArtifactRow) {
        let mut guard = self.artifacts.lock().unwrap();
        if let Some(existing) = guard
            .iter_mut()
            .find(|row| row.conversation_artifact_id == artifact.conversation_artifact_id)
        {
            *existing = artifact;
        } else {
            guard.push(artifact);
        }
    }

    fn artifacts(&self) -> Vec<nomifun_db::ConversationArtifactRow> {
        self.artifacts.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl IConversationRepository for StubConvRepo {
    async fn get(
        &self,
        id: &str,
    ) -> Result<Option<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
        let mut rows = self.rows.lock().unwrap();

        if let Some(existing) = rows.get(id) {
            return Ok(Some(existing.clone()));
        }
        // Id 9 == "missing-conv-1": reported absent so orphan cleanup fires.
        if id == CONV_MISSING {
            return Ok(None);
        }

        let row = if id == CONV_MODE {
            // conv_mode
            nomifun_db::models::ConversationRow {
                id: 0,
                conversation_id: id.to_owned(),
                user_id: TEST_USER_ID.into(),
                name: "Gemini Chat".into(),
                r#type: "acp".into(),
                delegation_policy: "automatic".into(),
                execution_model_pool: None,
                decision_policy: "automatic".into(),
                execution_template_id: None,
                model: Some(
                    serde_json::json!({
                        "provider_id": GEMINI_PROVIDER_ID,
                        "model": "gemini-2.5-pro",
                        "use_model": "gemini-2.5-pro"
                    })
                    .to_string(),
                ),
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "backend": "gemini",
                    "agent_name": "Gemini",
                    "workspace": "/tmp/gemini-workspace",
                    "session_mode": "yolo",
                    "current_model_id": "gemini-2.5-pro"
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                cron_job_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else if id == CONV_MODE_DEFAULT {
            // conv_mode_default
            nomifun_db::models::ConversationRow {
                id: 0,
                conversation_id: id.to_owned(),
                user_id: TEST_USER_ID.into(),
                name: "Gemini Default Chat".into(),
                r#type: "acp".into(),
                delegation_policy: "automatic".into(),
                execution_model_pool: None,
                decision_policy: "automatic".into(),
                execution_template_id: None,
                model: Some(
                    serde_json::json!({
                        "provider_id": GEMINI_PROVIDER_ID,
                        "model": "gemini-2.5-pro",
                        "use_model": "gemini-2.5-pro"
                    })
                    .to_string(),
                ),
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "backend": "gemini",
                    "agent_name": "Gemini",
                    "workspace": "/tmp/gemini-default-workspace",
                    "session_mode": "default",
                    "current_model_id": "gemini-2.5-pro"
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                cron_job_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else if id == CONV_MODE_CODEX {
            // conv_mode_codex
            nomifun_db::models::ConversationRow {
                id: 0,
                conversation_id: id.to_owned(),
                user_id: TEST_USER_ID.into(),
                name: "Codex Chat".into(),
                r#type: "acp".into(),
                delegation_policy: "automatic".into(),
                execution_model_pool: None,
                decision_policy: "automatic".into(),
                execution_template_id: None,
                model: Some(
                    serde_json::json!({
                        "provider_id": CODEX_PROVIDER_ID,
                        "model": "gpt-5-codex",
                        "use_model": "gpt-5-codex"
                    })
                    .to_string(),
                ),
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "backend": "codex",
                    "agent_name": "Codex",
                    "workspace": "/tmp/codex-workspace",
                    "session_mode": "default",
                    "current_model_id": "gpt-5-codex"
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                cron_job_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else if id == CONV_MODE_CLAUDE {
            // conv_mode_claude
            nomifun_db::models::ConversationRow {
                id: 0,
                conversation_id: id.to_owned(),
                user_id: TEST_USER_ID.into(),
                name: "Claude Chat".into(),
                r#type: "acp".into(),
                delegation_policy: "automatic".into(),
                execution_model_pool: None,
                decision_policy: "automatic".into(),
                execution_template_id: None,
                model: Some(
                    serde_json::json!({
                        "provider_id": CLAUDE_PROVIDER_ID,
                        "model": "claude-sonnet-4-20250514",
                        "use_model": "claude-sonnet-4-20250514"
                    })
                    .to_string(),
                ),
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "backend": "claude",
                    "agent_name": "Claude",
                    "workspace": "/tmp/claude-workspace",
                    "session_mode": "default",
                    "current_model_id": "claude-sonnet-4-20250514"
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                cron_job_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else if id == CONV_MODE_NOMI {
            // conv_mode_nomi
            nomifun_db::models::ConversationRow {
                id: 0,
                conversation_id: id.to_owned(),
                user_id: TEST_USER_ID.into(),
                name: "Nomi Chat".into(),
                r#type: "nomi".into(),
                delegation_policy: "automatic".into(),
                execution_model_pool: None,
                decision_policy: "automatic".into(),
                execution_template_id: None,
                model: Some(
                    serde_json::json!({
                        "provider_id": NOMI_PROVIDER_ID,
                        "model": "claude-sonnet-4-20250514",
                        "use_model": "claude-sonnet-4-20250514"
                    })
                    .to_string(),
                ),
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "backend": "anthropic",
                    "agent_name": "Nomi",
                    "workspace": "/tmp/nomi-workspace",
                    "session_mode": "default",
                    "current_model_id": "claude-sonnet-4-20250514"
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                cron_job_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else {
            nomifun_db::models::ConversationRow {
                id: 0,
                conversation_id: id.to_owned(),
                user_id: TEST_USER_ID.into(),
                name: "stub".into(),
                r#type: "acp".into(),
                delegation_policy: "automatic".into(),
                execution_model_pool: None,
                decision_policy: "automatic".into(),
                execution_template_id: None,
                model: None,
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: r#"{"workspace":"/tmp/cron-test-workspace"}"#.into(),
                pinned: false,
                pinned_at: None,
                cron_job_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                created_at: 1000,
                updated_at: 1000,
            }
        };

        rows.insert(id.to_owned(), row.clone());
        Ok(Some(row))
    }
    async fn create(
        &self,
        row: &nomifun_db::models::ConversationRow,
    ) -> Result<String, nomifun_db::DbError> {
        self.rows
            .lock()
            .unwrap()
            .insert(row.conversation_id.clone(), row.clone());
        Ok(row.conversation_id.clone())
    }
    async fn update(
        &self,
        id: &str,
        updates: &ConversationRowUpdate,
    ) -> Result<(), nomifun_db::DbError> {
        if updates
            .cron_job_id
            .as_ref()
            .is_some_and(|cron_job_id| cron_job_id.is_some())
            && self.fail_cron_binding.load(Ordering::SeqCst)
        {
            return Err(nomifun_db::DbError::Conflict(
                "fixture cron binding failure".into(),
            ));
        }
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .entry(id.to_owned())
            .or_insert_with(|| nomifun_db::models::ConversationRow {
                id: 0,
                conversation_id: id.to_owned(),
                user_id: TEST_USER_ID.into(),
                name: "stub".into(),
                r#type: "acp".into(),
                delegation_policy: "automatic".into(),
                execution_model_pool: None,
                decision_policy: "automatic".into(),
                execution_template_id: None,
                model: None,
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: r#"{"workspace":"/tmp/cron-test-workspace"}"#.into(),
                pinned: false,
                pinned_at: None,
                cron_job_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                created_at: 1000,
                updated_at: 1000,
            });
        if let Some(extra) = &updates.extra {
            row.extra = extra.clone();
        }
        if let Some(cron_job_id) = &updates.cron_job_id {
            row.cron_job_id = cron_job_id.clone();
        }
        if let Some(updated_at) = updates.updated_at {
            row.updated_at = updated_at;
        }
        Ok(())
    }
    async fn delete(&self, _id: &str) -> Result<(), nomifun_db::DbError> {
        Ok(())
    }
    async fn list_paginated(
        &self,
        _user_id: &str,
        _filters: &ConversationFilters,
    ) -> Result<PaginatedResult<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
        Ok(PaginatedResult {
            items: vec![],
            total: 0,
            has_more: false,
        })
    }
    async fn find_by_source_and_chat(
        &self,
        _user_id: &str,
        _source: &str,
        _chat_id: &str,
        _agent_type: &str,
    ) -> Result<Option<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
        Ok(None)
    }
    async fn list_by_cron_job(
        &self,
        _user_id: &str,
        cron_job_id: &str,
    ) -> Result<Vec<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .values()
            .filter(|row| row.cron_job_id.as_deref() == Some(cron_job_id))
            .cloned()
            .collect())
    }
    async fn list_associated(
        &self,
        _user_id: &str,
        _conversation_id: &str,
    ) -> Result<Vec<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
        Ok(vec![])
    }
    async fn get_messages(
        &self,
        _conv_id: &str,
        _page: u32,
        _page_size: u32,
        _order: SortOrder,
    ) -> Result<PaginatedResult<nomifun_db::models::MessageRow>, nomifun_db::DbError> {
        Ok(PaginatedResult {
            items: vec![],
            total: 0,
            has_more: false,
        })
    }
    async fn insert_message(
        &self,
        message: &nomifun_db::models::MessageRow,
    ) -> Result<(), nomifun_db::DbError> {
        self.messages.lock().unwrap().push(message.clone());
        Ok(())
    }
    async fn update_message(
        &self,
        _id: &str,
        _updates: &MessageRowUpdate,
    ) -> Result<(), nomifun_db::DbError> {
        Ok(())
    }
    async fn delete_messages_by_conversation(
        &self,
        _conv_id: &str,
    ) -> Result<(), nomifun_db::DbError> {
        Ok(())
    }
    async fn get_message_by_msg_id(
        &self,
        _conv_id: &str,
        _msg_id: &str,
        _msg_type: &str,
    ) -> Result<Option<nomifun_db::models::MessageRow>, nomifun_db::DbError> {
        Ok(None)
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
    async fn list_artifacts(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<nomifun_db::ConversationArtifactRow>, nomifun_db::DbError> {
        Ok(self
            .artifacts
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row.conversation_id == conversation_id)
            .cloned()
            .collect())
    }
    async fn get_artifact(
        &self,
        conversation_id: &str,
        conversation_artifact_id: &str,
    ) -> Result<Option<nomifun_db::ConversationArtifactRow>, nomifun_db::DbError> {
        Ok(self
            .artifacts
            .lock()
            .unwrap()
            .iter()
            .find(|row| {
                row.conversation_id == conversation_id
                    && row.conversation_artifact_id == conversation_artifact_id
            })
            .cloned())
    }
    async fn upsert_artifact(
        &self,
        artifact: &nomifun_db::ConversationArtifactRow,
    ) -> Result<nomifun_db::ConversationArtifactRow, nomifun_db::DbError> {
        let mut guard = self.artifacts.lock().unwrap();
        // `skill_suggest` is upserted against the partial-unique
        // `(conversation_id, cron_job_id)`; `cron_trigger` is a fresh insert.
        if artifact.kind == "skill_suggest"
            && let Some(existing) = guard.iter_mut().find(|row| {
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
        guard.push(artifact.clone());
        Ok(artifact.clone())
    }
    async fn update_artifact_status(
        &self,
        conversation_id: &str,
        conversation_artifact_id: &str,
        status: &str,
        updated_at: TimestampMs,
    ) -> Result<Option<nomifun_db::ConversationArtifactRow>, nomifun_db::DbError> {
        let mut guard = self.artifacts.lock().unwrap();
        let Some(existing) = guard
            .iter_mut()
            .find(|row| {
                row.conversation_id == conversation_id
                    && row.conversation_artifact_id == conversation_artifact_id
            })
        else {
            return Ok(None);
        };
        existing.status = status.to_string();
        existing.updated_at = updated_at;
        Ok(Some(existing.clone()))
    }
    async fn mark_skill_suggest_artifacts_saved(
        &self,
        _user_id: &str,
        cron_job_id: &str,
        updated_at: TimestampMs,
    ) -> Result<Vec<nomifun_db::ConversationArtifactRow>, nomifun_db::DbError> {
        let mut guard = self.artifacts.lock().unwrap();
        let mut updated = Vec::new();
        for artifact in guard.iter_mut() {
            if artifact.kind == "skill_suggest"
                && artifact.cron_job_id.as_deref() == Some(cron_job_id)
            {
                artifact.status = "saved".into();
                artifact.updated_at = updated_at;
                updated.push(artifact.clone());
            }
        }
        Ok(updated)
    }
}

async fn setup() -> (CronService, Arc<dyn ICronRepository>, Arc<MockBroadcaster>) {
    let (svc, repo, bc, _, _, _) = setup_with_conv_repo().await;
    (svc, repo, bc)
}

async fn setup_with_conv_repo() -> (
    CronService,
    Arc<dyn ICronRepository>,
    Arc<MockBroadcaster>,
    Arc<StubConvRepo>,
    sqlx::SqlitePool,
    std::path::PathBuf,
) {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool().clone();
    let cron_repo: Arc<dyn ICronRepository> = Arc::new(SqliteCronRepository::new(pool.clone()));

    sqlx::query("UPDATE users SET password_hash='hash' WHERE user_id = ?")
        .bind(TEST_USER_ID)
        .execute(&pool)
        .await
        .unwrap();

    for (provider_id, name) in [
        (SAFE_PROVIDER_ID, "safe"),
        (GEMINI_PROVIDER_ID, "gemini"),
        (CODEX_PROVIDER_ID, "codex"),
        (CLAUDE_PROVIDER_ID, "claude"),
        (NOMI_PROVIDER_ID, "nomi"),
    ] {
        sqlx::query(
            "INSERT INTO providers (\
                provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
                capabilities, created_at, updated_at\
             ) VALUES (?, 'openai', ?, 'https://example.invalid', 'encrypted', \
                       '[\"model-safe\",\"gemini-2.5-pro\",\"claude-sonnet-4-20250514\"]', \
                       1, '[]', 1, 1)",
        )
        .bind(provider_id)
        .bind(name)
        .execute(&pool)
        .await
        .unwrap();
    }

    // Seed canonical conversation business IDs into the real DB so logical Cron relations
    // exercise the same UUIDv7 conversation contract as production.
    {
        let real_conv_repo = SqliteConversationRepository::new(pool.clone());
        for id in [
            CONV_1,
            CONV_2,
            CONV_3,
            CONV_4,
            CONV_5,
            CONV_6,
            CONV_7,
            CONV_8,
            CONV_MISSING,
            CONV_MODE,
            CONV_MODE_DEFAULT,
            CONV_MODE_CODEX,
            CONV_MODE_CLAUDE,
            CONV_MODE_NOMI,
        ] {
            real_conv_repo
                .create(&nomifun_db::models::ConversationRow {
                    id: 0,
                    conversation_id: id.to_owned(),
                    user_id: TEST_USER_ID.into(),
                    name: "Seed Conversation".into(),
                    r#type: "acp".into(),
                    extra: r#"{"workspace":"/tmp/cron-test-workspace"}"#.into(),
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
                    cron_job_id: None,
                    preset_id: None,
                    preset_revision: None,
                    preset_snapshot: None,
                    created_at: now_ms(),
                    updated_at: now_ms(),
                })
                .await
                .unwrap();
        }
    }

    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let acp_session_repo: Arc<dyn IAcpSessionRepository> =
        Arc::new(SqliteAcpSessionRepository::new(pool.clone()));
    let bc = Arc::new(MockBroadcaster::new());
    let data_dir = std::env::temp_dir().join(format!("nomifun-cron-test-{}", now_ms()));
    std::fs::create_dir_all(&data_dir).unwrap();

    struct StubSkillResolver;
    #[async_trait::async_trait]
    impl nomifun_conversation::skill_resolver::SkillResolver for StubSkillResolver {
        async fn auto_inject_names(&self) -> Vec<String> {
            Vec::new()
        }

        async fn resolve_skills(
            &self,
            _names: &[String],
        ) -> Vec<nomifun_conversation::skill_resolver::ResolvedAgentSkill> {
            Vec::new()
        }

        async fn link_workspace_skills(
            &self,
            _workspace: &std::path::Path,
            _rel_dirs: &[&str],
            _skills: &[nomifun_conversation::skill_resolver::ResolvedAgentSkill],
        ) -> usize {
            0
        }
    }

    let stub_conv_repo = Arc::new(StubConvRepo::new());
    let stub_conv_repo_trait: Arc<dyn IConversationRepository> = stub_conv_repo.clone();
    let runtime_registry: Arc<dyn nomifun_ai_agent::runtime_registry::AgentRuntimeRegistry> =
        Arc::new(StubAgentRuntimeRegistry);
    let conv_service = Arc::new(ConversationService::new(
        Arc::<str>::from(TEST_USER_ID),
        std::env::temp_dir(),
        bc.clone() as Arc<dyn UserEventSink>,
        Arc::new(StubSkillResolver),
        Arc::clone(&runtime_registry),
        Arc::clone(&stub_conv_repo_trait),
        Arc::clone(&agent_metadata_repo),
        acp_session_repo,
        Arc::new(nomifun_conversation::NoExecutionConversationBoundary),
    ));
    let agent_registry = AgentRegistry::new(agent_metadata_repo);
    agent_registry.hydrate().await.unwrap();
    let busy_guard = Arc::new(CronBusyGuard::new());
    let executor = Arc::new(JobExecutor::new(
        Arc::<str>::from(TEST_USER_ID),
        runtime_registry,
        stub_conv_repo_trait,
        conv_service,
        busy_guard,
        data_dir.clone(),
        data_dir.clone(),
        bc.clone() as Arc<dyn UserEventSink>,
        agent_registry,
    ));

    let scheduler = Arc::new(CronScheduler::new(Arc::new(|_, _| {})));

    let emitter = CronEventEmitter::new(bc.clone() as Arc<dyn UserEventSink>);
    let test_data_dir = data_dir.clone();
    let svc = CronService::new(
        Arc::<str>::from(TEST_USER_ID),
        cron_repo.clone(),
        scheduler,
        executor,
        emitter,
        data_dir,
    );

    std::mem::forget(db);
    (svc, cron_repo, bc, stub_conv_repo, pool, test_data_dir)
}

fn make_create_req(name: &str, schedule: CronScheduleDto) -> CreateCronJobRequest {
    CreateCronJobRequest {
        name: name.into(),
        description: Some("test description".into()),
        schedule,
        prompt: None,
        message: Some("test message".into()),
        conversation_id: None,
        conversation_title: None,
        agent_type: "acp".into(),
        created_by: "user".into(),
        execution_mode: None,
        agent_config: Some(CronAgentConfigDto {
            backend: Some("gemini".into()),
            name: "Gemini".into(),
            cli_path: None,
            custom_agent_id: None,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            mode: None,
            model: None,
            provider_id: None,
            config_options: None,
            workspace: None,
            clear_context_each_run: false,
        }),
    }
}

fn every_60s() -> CronScheduleDto {
    CronScheduleDto::Every {
        every_ms: 60000,
        description: Some("every minute".into()),
    }
}

fn at_future(offset_ms: i64) -> CronScheduleDto {
    CronScheduleDto::At {
        at_ms: now_ms() + offset_ms,
        description: Some("once".into()),
    }
}

fn cron_every_5min() -> CronScheduleDto {
    CronScheduleDto::Cron {
        expr: "0 */5 * * * *".into(),
        tz: None,
        description: Some("every 5 min".into()),
    }
}

#[tokio::test]
async fn secondary_cron_keeps_model_selection_but_cannot_gain_host_configuration() {
    let (svc, _repo, _events, _conversations, pool, data_dir) =
        setup_with_conv_repo().await;
    let secondary = SECONDARY_USER_ID;
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
         VALUES (?, 'secondary-cron-user', 'hash', 1, 1)",
    )
    .bind(secondary)
    .execute(&pool)
    .await
    .unwrap();

    let job = svc
        .add_job(
            secondary,
            CreateCronJobRequest {
                name: "Model only".into(),
                description: None,
                schedule: every_60s(),
                prompt: None,
                message: Some("summarize this".into()),
                conversation_id: None,
                conversation_title: None,
                agent_type: "nomi".into(),
                created_by: "user".into(),
                execution_mode: Some("new_conversation".into()),
                agent_config: Some(CronAgentConfigDto {
                    backend: None,
                    name: "Nomi".into(),
                    cli_path: Some("/bin/sh".into()),
                    custom_agent_id: Some("custom-host-agent".into()),
                    preset_id: Some("owner-preset".into()),
                    preset_revision: Some(7),
                    preset_snapshot: None,
                    mode: Some("yolo".into()),
                    model: Some("model-safe".into()),
                    provider_id: Some(SAFE_PROVIDER_ID.into()),
                    config_options: Some(HashMap::from([("host".into(), "true".into())])),
                    workspace: Some("/unsafe".into()),
                    clear_context_each_run: true,
                }),
            },
        )
        .await
        .unwrap();

    let config = job.agent_config.as_ref().unwrap();
    assert_eq!(config.backend, None);
    assert_eq!(config.provider_id.as_deref(), Some(SAFE_PROVIDER_ID));
    assert_eq!(config.model.as_deref(), Some("model-safe"));
    assert!(config.clear_context_each_run);
    assert!(config.cli_path.is_none());
    assert!(config.custom_agent_id.is_none());
    assert!(config.preset_id.is_none());
    assert!(config.preset_snapshot.is_none());
    assert!(config.mode.is_none());
    assert!(config.config_options.is_none());
    assert!(config.workspace.is_none());

    let persisted: String = sqlx::query_scalar(
        "SELECT agent_config FROM cron_jobs WHERE cron_job_id = ? AND user_id = ?",
    )
    .bind(&job.cron_job_id)
    .bind(secondary)
    .fetch_one(&pool)
    .await
    .unwrap();
    let persisted = serde_json::from_str::<serde_json::Value>(&persisted).unwrap();
    let keys = persisted.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(
        keys.into_iter().collect::<std::collections::BTreeSet<_>>(),
        ["clear_context_each_run", "model", "name", "provider_id"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );

    let skill_error = svc
        .save_skill(
            secondary,
            &job.cron_job_id,
            SaveCronSkillRequest {
                content: "---\nname: forbidden\ndescription: forbidden host skill\n---\nrun host steps".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(skill_error.to_string().contains("installation owner"));

    let mut forbidden = make_create_req("Host agent", every_60s());
    forbidden.conversation_id = None;
    let error = svc.add_job(secondary, forbidden).await.unwrap_err();
    assert!(error.to_string().contains("model-only"));

    sqlx::query("UPDATE cron_jobs SET enabled=0, agent_config=NULL WHERE cron_job_id=?")
        .bind(&job.cron_job_id)
        .execute(&pool)
        .await
        .unwrap();
    let reenable_error = svc
        .update_job(
            secondary,
            &job.cron_job_id,
            UpdateCronJobRequest {
                name: None,
                description: None,
                enabled: Some(true),
                schedule: None,
                message: None,
                agent_config: None,
                conversation_title: None,
                max_retries: None,
            },
        )
        .await
        .unwrap_err();
    assert!(reenable_error.to_string().contains("no model configured"));

    let owner_job = svc
        .add_job(TEST_USER_ID, make_create_req("Owner skill", every_60s()))
        .await
        .unwrap();
    let skill = "---\nname: retained-owner-skill\ndescription: owner-only scheduled instructions\n---\n\nPerform the scheduled task.";
    write_raw_skill_file(&data_dir, &job.cron_job_id, skill)
        .await
        .unwrap();
    svc.save_skill(
        TEST_USER_ID,
        &owner_job.cron_job_id,
        SaveCronSkillRequest {
            content: skill.into(),
        },
    )
        .await
        .unwrap();

    svc.init().await;

    assert!(
        !has_skill_file(&data_dir, &job.cron_job_id).await.unwrap(),
        "startup reconciliation must delete secondary-user skill directories outside the v3 ownership boundary"
    );
    assert!(
        has_skill_file(&data_dir, &owner_job.cron_job_id).await.unwrap(),
        "startup reconciliation must retain the installation owner's skill directory"
    );
}

// ── CJ-1: Create cron job ──────────────────────────────────────────

#[tokio::test]
async fn cj1_create_cron_job() {
    let (svc, _, bc) = setup().await;
    let req = make_create_req("Daily Report", every_60s());

    let job = svc.add_job(TEST_USER_ID, req).await.unwrap();

    let parsed_job_id =
        nomifun_common::CronJobId::parse(&job.cron_job_id).expect("created cron job has a canonical UUIDv7 id");
    assert_eq!(parsed_job_id.as_str(), job.cron_job_id);
    assert_eq!(job.cron_job_id.len(), 36);
    assert_eq!(job.cron_job_id, job.cron_job_id.to_ascii_lowercase());
    assert_eq!(job.name, "Daily Report");
    assert!(job.enabled);
    assert!(job.next_run_at.is_some());
    assert_eq!(job.run_count, 0);

    let events = bc.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "cron.job-created");
}

#[tokio::test]
async fn existing_conversation_binding_failure_compensates_inserted_cron_job() {
    let (svc, _repo, _events, conversations, pool, _data_dir) =
        setup_with_conv_repo().await;
    conversations.set_fail_cron_binding(true);

    let mut request = make_create_req("binding failure", every_60s());
    request.conversation_id = Some(CONV_1.to_owned());
    request.execution_mode = Some("existing".into());

    let error = svc.add_job(TEST_USER_ID, request).await.unwrap_err();
    assert!(error.to_string().contains("fixture cron binding failure"));
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cron_jobs WHERE user_id = ? AND name = 'binding failure'",
    )
    .bind(TEST_USER_ID)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn existing_conversation_binding_failure_compensates_updated_cron_job() {
    let (svc, _repo, _events, conversations, pool, _data_dir) =
        setup_with_conv_repo().await;
    let mut request = make_create_req("before binding failure", every_60s());
    request.conversation_id = Some(CONV_1.to_owned());
    let job = svc.add_job(TEST_USER_ID, request).await.unwrap();
    conversations
        .update(
            CONV_1,
            &ConversationRowUpdate {
                cron_job_id: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    conversations.set_fail_cron_binding(true);

    let error = svc
        .update_job(
            TEST_USER_ID,
            &job.cron_job_id,
            UpdateCronJobRequest {
                name: Some("after binding failure".into()),
                description: None,
                enabled: None,
                schedule: None,
                message: None,
                agent_config: None,
                conversation_title: None,
                max_retries: None,
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("fixture cron binding failure"));

    let row = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT name, conversation_id FROM cron_jobs WHERE cron_job_id = ?",
    )
    .bind(&job.cron_job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "before binding failure");
    assert_eq!(row.1.as_deref(), Some(CONV_1));
}

#[tokio::test]
async fn cron_crud_run_history_and_skill_boundaries_are_owner_scoped() {
    let (svc, _, _) = setup().await;
    // This boundary test does not exercise a Conversation binding. Keep the
    // aggregate unbound so its result depends only on Cron ownership and host
    // capability ordering, not on the StubConvRepo's fixed `u1` fixture.
    let mut owner_request = make_create_req("Owner Boundary", every_60s());
    owner_request.conversation_id = None;
    owner_request.execution_mode = Some("new_conversation".into());
    let job = svc
        .add_job(TEST_USER_ID, owner_request)
        .await
        .unwrap();
    let foreign = FOREIGN_USER_ID;

    assert!(matches!(
        svc.get_job(foreign, &job.cron_job_id).await,
        Err(nomifun_cron::error::CronError::JobNotFound(_))
    ));
    assert!(
        svc.list_jobs(foreign, &ListCronJobsQuery::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        svc.update_job(
            foreign,
            &job.cron_job_id,
            UpdateCronJobRequest {
                name: Some("forged".into()),
                description: None,
                enabled: None,
                schedule: None,
                message: None,
                agent_config: None,
                conversation_title: None,
                max_retries: None,
            },
        )
        .await,
        Err(nomifun_cron::error::CronError::JobNotFound(_))
    ));
    assert!(matches!(
        svc.run_now(foreign, &job.cron_job_id).await,
        Err(nomifun_cron::error::CronError::JobNotFound(_))
    ));
    assert!(matches!(
        svc.list_runs(foreign, &job.cron_job_id).await,
        Err(nomifun_cron::error::CronError::JobNotFound(_))
    ));
    assert!(matches!(
        svc.has_skill(foreign, &job.cron_job_id).await,
        Err(nomifun_cron::error::CronError::JobNotFound(_))
    ));
    assert!(matches!(
        svc.save_skill(
            foreign,
            &job.cron_job_id,
            SaveCronSkillRequest {
                content: "---\nname: forged\n---\nForeign content".into(),
            },
        )
        .await,
        Err(nomifun_cron::error::CronError::JobNotFound(_))
    ));
    assert!(matches!(
        svc.delete_skill(foreign, &job.cron_job_id).await,
        Err(nomifun_cron::error::CronError::JobNotFound(_))
    ));

    // Scheduler callbacks capture the owner at timer installation.  A stale
    // callback (for example after delete/recreate) must not execute the row
    // merely because the opaque job id still exists.
    svc.tick(foreign, &job.cron_job_id).await;
    assert!(matches!(
        svc.remove_job(foreign, &job.cron_job_id).await,
        Err(nomifun_cron::error::CronError::JobNotFound(_))
    ));

    let unchanged = svc.get_job(TEST_USER_ID, &job.cron_job_id).await.unwrap();
    assert_eq!(unchanged.name, "Owner Boundary");
    assert_eq!(unchanged.run_count, 0);

    let mut foreign_request = make_create_req("Cross Owner", every_60s());
    // Keep the foreign principal within its model-only authority so this
    // assertion reaches the owner-scoped Conversation lookup rather than being
    // rejected earlier for requesting an ACP host runtime.
    foreign_request.agent_type = "nomi".into();
    foreign_request.agent_config = None;
    assert!(matches!(
        svc.add_job(foreign, foreign_request).await,
        Err(nomifun_cron::error::CronError::JobNotFound(_))
    ));
}

#[tokio::test]
async fn cj1_private_job_events_are_scoped_to_each_conversation_owner() {
    let (svc, _repo, user_events, conversation_repo, pool, _) =
        setup_with_conv_repo().await;

    for owner in [OWNER_A_ID, OWNER_B_ID] {
        sqlx::query(
            "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
             VALUES (?, ?, 'hash', 0, 0)",
        )
        .bind(owner)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO providers (\
            provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
            capabilities, created_at, updated_at\
         ) VALUES ('0190f5fe-7c00-7a00-8000-000000000008', 'openai', 'multiuser', \
                   'https://example.invalid', 'encrypted', \
                   '[\"model-multiuser\"]', 1, '[]', 1, 1)",
    )
        .execute(&pool)
        .await
        .unwrap();
    // Conversation ownership is immutable in the v3 contract. Replace the
    // setup's installation-owner seed rows with legal model-only rows instead
    // of rewriting `user_id` in place.
    sqlx::query("DELETE FROM conversations WHERE conversation_id IN (?, ?)")
        .bind(CONV_1)
        .bind(CONV_2)
        .execute(&pool)
        .await
        .unwrap();
    for (id, owner) in [(CONV_1, OWNER_A_ID), (CONV_2, OWNER_B_ID)] {
        sqlx::query(
            "INSERT INTO conversations (\
                conversation_id, user_id, name, type, model, status, delegation_policy, \
                decision_policy, extra, created_at, updated_at\
             ) VALUES (?, ?, 'Private model-only cron conversation', 'nomi', \
                       '{\"provider_id\":\"0190f5fe-7c00-7a00-8000-000000000008\",\"model\":\"model-multiuser\"}', \
                       'finished', 'disabled', 'automatic', '{}', 1, 1)",
        )
        .bind(id)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    }

    let mut owner_a_conversation = conversation_repo.get(CONV_1).await.unwrap().unwrap();
    owner_a_conversation.user_id = OWNER_A_ID.into();
    owner_a_conversation.r#type = "nomi".into();
    owner_a_conversation.delegation_policy = "disabled".into();
    owner_a_conversation.model = Some(
        serde_json::json!({
            "provider_id": "0190f5fe-7c00-7a00-8000-000000000008",
            "model": "model-multiuser"
        })
        .to_string(),
    );
    owner_a_conversation.extra = r#"{"workspace":"/tmp/owner-a-workspace"}"#.into();
    conversation_repo
        .create(&owner_a_conversation)
        .await
        .unwrap();
    let mut owner_b_conversation = conversation_repo.get(CONV_2).await.unwrap().unwrap();
    owner_b_conversation.user_id = OWNER_B_ID.into();
    owner_b_conversation.r#type = "nomi".into();
    owner_b_conversation.delegation_policy = "disabled".into();
    owner_b_conversation.model = owner_a_conversation.model.clone();
    owner_b_conversation.extra = r#"{"workspace":"/tmp/owner-b-workspace"}"#.into();
    conversation_repo
        .create(&owner_b_conversation)
        .await
        .unwrap();

    let mut owner_a_job = make_create_req("Owner A Job", every_60s());
    owner_a_job.conversation_id = Some(CONV_1.to_owned());
    owner_a_job.agent_type = "nomi".into();
    owner_a_job.agent_config = None;
    svc.add_job(OWNER_A_ID, owner_a_job).await.unwrap();

    let mut owner_b_job = make_create_req("Owner B Job", every_60s());
    owner_b_job.conversation_id = Some(CONV_2.to_owned());
    owner_b_job.agent_type = "nomi".into();
    owner_b_job.agent_config = None;
    svc.add_job(OWNER_B_ID, owner_b_job).await.unwrap();

    let deliveries = user_events.take_deliveries();
    assert_eq!(deliveries.len(), 2);
    assert_eq!(deliveries[0].0, OWNER_A_ID);
    assert_eq!(deliveries[0].1.name, "cron.job-created");
    assert_eq!(deliveries[1].0, OWNER_B_ID);
    assert_eq!(deliveries[1].1.name, "cron.job-created");
}

// ── CJ-2: Create three schedule types ──────────────────────────────

#[tokio::test]
async fn cj2_create_three_schedule_types() {
    let (svc, _, _) = setup().await;
    let now = now_ms();

    let at_job = svc
        .add_job(TEST_USER_ID, make_create_req("At Job", at_future(3600000)))
        .await
        .unwrap();
    assert!(at_job.next_run_at.unwrap() > now);

    let every_job = svc
        .add_job(TEST_USER_ID, make_create_req("Every Job", every_60s()))
        .await
        .unwrap();
    let next = every_job.next_run_at.unwrap();
    assert!((next - now - 60000).abs() < 2000);

    let cron_job = svc
        .add_job(TEST_USER_ID, make_create_req("Cron Job", cron_every_5min()))
        .await
        .unwrap();
    assert!(cron_job.next_run_at.unwrap() > now);
}

// ── CJ-4: Get single job ──────────────────────────────────────────

#[tokio::test]
async fn cj4_get_single_job() {
    let (svc, _, _) = setup().await;
    let created = svc
        .add_job(TEST_USER_ID, make_create_req("Get Test", every_60s()))
        .await
        .unwrap();

    let fetched = svc.get_job(TEST_USER_ID, &created.cron_job_id).await.unwrap();
    assert_eq!(fetched.cron_job_id, created.cron_job_id);
    assert_eq!(fetched.name, "Get Test");
}

// ── CJ-5: Get nonexistent job ─────────────────────────────────────

#[tokio::test]
async fn cj5_get_nonexistent_job() {
    let (svc, _, _) = setup().await;
    let err = svc.get_job(TEST_USER_ID, MISSING_JOB_ID).await.unwrap_err();
    assert!(matches!(
        err,
        nomifun_cron::error::CronError::JobNotFound(_)
    ));
}

// ── CJ-6: List all jobs ───────────────────────────────────────────

#[tokio::test]
async fn cj6_list_all_jobs() {
    let (svc, _, _) = setup().await;
    for (i, conversation_id) in [CONV_1, CONV_2, CONV_3].into_iter().enumerate() {
        let mut request = make_create_req(&format!("Job {i}"), every_60s());
        request.conversation_id = Some(conversation_id.to_owned());
        svc.add_job(TEST_USER_ID, request).await.unwrap();
    }

    let jobs = svc.list_jobs(TEST_USER_ID, &ListCronJobsQuery::default()).await.unwrap();
    assert!(jobs.len() >= 3);
}

// ── CJ-7: List by conversation ────────────────────────────────────

#[tokio::test]
async fn cj7_list_by_conversation() {
    let (svc, _, _) = setup().await;

    let mut req1 = make_create_req("Job A", every_60s());
    req1.conversation_id = Some(CONV_2.to_owned());
    svc.add_job(TEST_USER_ID, req1).await.unwrap();

    let mut req2 = make_create_req("Job B", every_60s());
    req2.conversation_id = Some(CONV_3.to_owned());
    svc.add_job(TEST_USER_ID, req2).await.unwrap();

    let mut req3 = make_create_req("Job C", every_60s());
    req3.conversation_id = Some(CONV_1.to_owned());
    svc.add_job(TEST_USER_ID, req3).await.unwrap();

    let query = ListCronJobsQuery {
        conversation_id: Some(CONV_2.to_owned()),
    };
    let jobs = svc.list_jobs(TEST_USER_ID, &query).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].name, "Job A");
}

#[tokio::test]
async fn cj7b_add_job_binds_existing_conversation_to_job() {
    let (svc, _, _, conv_repo, _, _) = setup_with_conv_repo().await;

    let mut req = make_create_req("Bound Existing Conversation", every_60s());
    req.conversation_id = Some(CONV_4.to_owned());

    let job = svc.add_job(TEST_USER_ID, req).await.unwrap();

    let bound = conv_repo.get(CONV_4).await.unwrap().unwrap();
    assert_eq!(bound.cron_job_id.as_deref(), Some(job.cron_job_id.as_str()));

    let linked = conv_repo
        .list_by_cron_job(TEST_USER_ID, &job.cron_job_id)
        .await
        .unwrap();
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].conversation_id, CONV_4);
}

#[tokio::test]
async fn cj7c_existing_conversation_binding_is_one_to_one_and_fail_closed() {
    let (svc, _, _, conv_repo, _, _) = setup_with_conv_repo().await;

    let mut first_request = make_create_req("First binding", every_60s());
    first_request.conversation_id = Some(CONV_4.to_owned());
    let first = svc.add_job(TEST_USER_ID, first_request).await.unwrap();

    let mut second_request = make_create_req("Second binding", every_60s());
    second_request.conversation_id = Some(CONV_4.to_owned());
    let error = svc
        .add_job(TEST_USER_ID, second_request)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        nomifun_cron::error::CronError::App(nomifun_common::AppError::Conflict(_))
    ));
    let bound = conv_repo.get(CONV_4).await.unwrap().unwrap();
    assert_eq!(bound.cron_job_id.as_deref(), Some(first.cron_job_id.as_str()));
    assert!(svc.list_jobs(TEST_USER_ID, &ListCronJobsQuery::default()).await.unwrap().iter().all(
        |job| job.name != "Second binding"
    ));
}

// ── CJ-8: Update job ──────────────────────────────────────────────

#[tokio::test]
async fn cj8_update_job() {
    let (svc, _, bc) = setup().await;
    let created = svc
        .add_job(TEST_USER_ID, make_create_req("Original", every_60s()))
        .await
        .unwrap();
    bc.take_events();

    let req = UpdateCronJobRequest {
        name: Some("Updated Name".into()),
        description: Some("Updated description".into()),
        enabled: Some(false),
        schedule: None,
        message: None,
        agent_config: None,
        conversation_title: None,
        max_retries: None,
    };

    let updated = svc.update_job(TEST_USER_ID, &created.cron_job_id, req).await.unwrap();
    assert_eq!(updated.name, "Updated Name");
    assert_eq!(updated.description.as_deref(), Some("Updated description"));
    assert!(!updated.enabled);
    assert!(updated.updated_at >= created.created_at);

    let events = bc.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "cron.job-updated");
}

// ── CJ-9: Update schedule type ────────────────────────────────────

#[tokio::test]
async fn cj9_update_schedule_type() {
    let (svc, _, _) = setup().await;
    let created = svc
        .add_job(TEST_USER_ID, make_create_req("Schedule Change", every_60s()))
        .await
        .unwrap();

    let req = UpdateCronJobRequest {
        name: None,
        description: None,
        enabled: None,
        schedule: Some(cron_every_5min()),
        message: None,
        agent_config: None,
        conversation_title: None,
        max_retries: None,
    };

    let updated = svc.update_job(TEST_USER_ID, &created.cron_job_id, req).await.unwrap();
    assert!(matches!(
        updated.schedule,
        nomifun_cron::types::CronSchedule::Cron { .. }
    ));
    assert!(updated.next_run_at.is_some());
}

// ── CJ-10: Update nonexistent job ─────────────────────────────────

#[tokio::test]
async fn cj10_update_nonexistent() {
    let (svc, _, _) = setup().await;
    let req = UpdateCronJobRequest {
        name: Some("x".into()),
        description: None,
        enabled: None,
        schedule: None,
        message: None,
        agent_config: None,
        conversation_title: None,
        max_retries: None,
    };
    let err = svc.update_job(TEST_USER_ID, MISSING_JOB_ID, req).await.unwrap_err();
    assert!(matches!(
        err,
        nomifun_cron::error::CronError::JobNotFound(_)
    ));
}

// ── CJ-11: Delete job ─────────────────────────────────────────────

#[tokio::test]
async fn cj11_delete_job() {
    let (svc, _, bc) = setup().await;
    let created = svc
        .add_job(TEST_USER_ID, make_create_req("To Delete", every_60s()))
        .await
        .unwrap();
    bc.take_events();

    svc.remove_job(TEST_USER_ID, &created.cron_job_id).await.unwrap();

    let err = svc.get_job(TEST_USER_ID, &created.cron_job_id).await.unwrap_err();
    assert!(matches!(
        err,
        nomifun_cron::error::CronError::JobNotFound(_)
    ));

    let events = bc.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "cron.job-removed");
}

// ── CJ-12: Delete nonexistent ─────────────────────────────────────

#[tokio::test]
async fn cj12_delete_nonexistent() {
    let (svc, _, _) = setup().await;
    let err = svc.remove_job(TEST_USER_ID, MISSING_JOB_ID).await.unwrap_err();
    assert!(matches!(
        err,
        nomifun_cron::error::CronError::JobNotFound(_)
    ));
}

// ── SK-1: Save skill ──────────────────────────────────────────────

#[tokio::test]
async fn sk1_save_skill() {
    let (svc, _, _) = setup().await;
    let job = svc
        .add_job(TEST_USER_ID, make_create_req("Skill Job", every_60s()))
        .await
        .unwrap();

    let req = SaveCronSkillRequest {
        content: "---\nname: test\ndescription: test skill\n---\nDo something".into(),
    };
    svc.save_skill(TEST_USER_ID, &job.cron_job_id, req).await.unwrap();
}

#[tokio::test]
async fn sk1_1_save_skill_marks_related_skill_suggest_artifacts_saved() {
    let (svc, _, bc, conv_repo, _, _) = setup_with_conv_repo().await;
    let job = svc
        .add_job(TEST_USER_ID, make_create_req("Skill Artifact Job", every_60s()))
        .await
        .unwrap();

    conv_repo.upsert_artifact_row(nomifun_db::ConversationArtifactRow {
        conversation_artifact_id: ARTIFACT_1.to_owned(),
        conversation_id: CONV_1.to_owned(),
        cron_job_id: Some(job.cron_job_id.clone()),
        kind: "skill_suggest".into(),
        status: "active".into(),
        payload: serde_json::json!({
            "cron_job_id": &job.cron_job_id,
            "name": "daily-report",
            "description": "Daily report",
            "skillContent": "---\nname: daily-report\n---\nUse it."
        })
        .to_string(),
        created_at: 1000,
        updated_at: 1000,
    });

    svc.save_skill(
        TEST_USER_ID,
        &job.cron_job_id,
        SaveCronSkillRequest {
            content: "---\nname: daily-report\ndescription: Daily report\n---\nUse it.".into(),
        },
    )
    .await
    .unwrap();

    let artifacts = conv_repo.artifacts();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].status, "saved");

    let events = bc.take_events();
    let saved_event = events
        .iter()
        .find(|event| {
            event.name == "conversation.artifact"
                && event.data["conversation_artifact_id"].as_str()
                    == Some(artifacts[0].conversation_artifact_id.as_str())
                && event.data["status"] == "saved"
        })
        .expect("save_skill should broadcast saved artifact upsert");
    assert_eq!(saved_event.data["conversation_id"], CONV_1);
}

// ── SK-2: Has skill (true) ────────────────────────────────────────

#[tokio::test]
async fn sk2_has_skill_true() {
    let (svc, _, _) = setup().await;
    let job = svc
        .add_job(TEST_USER_ID, make_create_req("Skill Check", every_60s()))
        .await
        .unwrap();

    svc.save_skill(
        TEST_USER_ID,
        &job.cron_job_id,
        SaveCronSkillRequest {
            content: "---\nname: x\n---\nContent".into(),
        },
    )
    .await
    .unwrap();

    let resp = svc.has_skill(TEST_USER_ID, &job.cron_job_id).await.unwrap();
    assert!(resp.has_skill);
}

// ── SK-3: Has skill (false) ───────────────────────────────────────

#[tokio::test]
async fn sk3_has_skill_false() {
    let (svc, _, _) = setup().await;
    let job = svc
        .add_job(TEST_USER_ID, make_create_req("No Skill", every_60s()))
        .await
        .unwrap();

    let resp = svc.has_skill(TEST_USER_ID, &job.cron_job_id).await.unwrap();
    assert!(!resp.has_skill);
}

// ── SK-4: Save empty skill ────────────────────────────────────────

#[tokio::test]
async fn sk4_save_empty_skill() {
    let (svc, _, _) = setup().await;
    let job = svc
        .add_job(TEST_USER_ID, make_create_req("Empty Skill", every_60s()))
        .await
        .unwrap();

    let err = svc
        .save_skill(TEST_USER_ID, &job.cron_job_id, SaveCronSkillRequest { content: "".into() })
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        nomifun_cron::error::CronError::InvalidSkillContent(_)
    ));
}

// ── SK-5: Save placeholder skill ──────────────────────────────────

#[tokio::test]
async fn sk5_save_placeholder_skill() {
    let (svc, _, _) = setup().await;
    let job = svc
        .add_job(TEST_USER_ID, make_create_req("Placeholder Skill", every_60s()))
        .await
        .unwrap();

    let err = svc
        .save_skill(
            TEST_USER_ID,
            &job.cron_job_id,
            SaveCronSkillRequest {
                content: "TODO: fill in later".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        nomifun_cron::error::CronError::InvalidSkillContent(_)
    ));
}

// ── SK-6: Save skill for nonexistent job ──────────────────────────

#[tokio::test]
async fn sk6_save_skill_nonexistent() {
    let (svc, _, _) = setup().await;
    let err = svc
        .save_skill(
            TEST_USER_ID,
            MISSING_JOB_ID,
            SaveCronSkillRequest {
                content: "---\nname: x\n---\nOk".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        nomifun_cron::error::CronError::JobNotFound(_)
    ));
}

// ── SK-7: Delete skill on job removal ─────────────────────────────

#[tokio::test]
async fn sk7_delete_cleans_skill() {
    let (svc, _, _) = setup().await;
    let job = svc
        .add_job(TEST_USER_ID, make_create_req("Skill Cleanup", every_60s()))
        .await
        .unwrap();
    svc.save_skill(
        TEST_USER_ID,
        &job.cron_job_id,
        SaveCronSkillRequest {
            content: "---\nname: x\n---\nContent".into(),
        },
    )
    .await
    .unwrap();

    svc.remove_job(TEST_USER_ID, &job.cron_job_id).await.unwrap();

    let err = svc.has_skill(TEST_USER_ID, &job.cron_job_id).await.unwrap_err();
    assert!(matches!(
        err,
        nomifun_cron::error::CronError::JobNotFound(_)
    ));
}

// ── SC-3: Every type next_run ─────────────────────────────────────

#[tokio::test]
async fn sc3_every_type_next_run() {
    let (svc, _, _) = setup().await;
    let now = now_ms();
    let job = svc
        .add_job(TEST_USER_ID, make_create_req("Every 60s", every_60s()))
        .await
        .unwrap();

    let next = job.next_run_at.unwrap();
    let diff = (next - now - 60000).abs();
    assert!(diff < 2000, "expected next_run ≈ now+60000, diff={diff}");
}

// ── SC-5: Invalid cron expression ─────────────────────────────────

#[tokio::test]
async fn sc5_invalid_cron_expression() {
    let (svc, _, _) = setup().await;
    let req = make_create_req(
        "Invalid Cron",
        CronScheduleDto::Cron {
            expr: "invalid cron".into(),
            tz: None,
            description: None,
        },
    );
    let err = svc.add_job(TEST_USER_ID, req).await.unwrap_err();
    assert!(matches!(
        err,
        nomifun_cron::error::CronError::InvalidCronExpression(_)
    ));
}

// ── SC-6: Cron with timezone ──────────────────────────────────────

#[tokio::test]
async fn sc6_cron_with_timezone() {
    let (svc, _, _) = setup().await;
    let now = now_ms();
    let req = make_create_req(
        "Shanghai Job",
        CronScheduleDto::Cron {
            expr: "0 0 9 * * *".into(),
            tz: Some("Asia/Shanghai".into()),
            description: None,
        },
    );
    let job = svc.add_job(TEST_USER_ID, req).await.unwrap();
    assert!(job.next_run_at.unwrap() > now);
}

// ── SC-7: Every zero interval ─────────────────────────────────────

#[tokio::test]
async fn sc7_every_zero_interval() {
    let (svc, _, _) = setup().await;
    let req = make_create_req(
        "Zero Interval",
        CronScheduleDto::Every {
            every_ms: 0,
            description: None,
        },
    );
    let err = svc.add_job(TEST_USER_ID, req).await.unwrap_err();
    assert!(matches!(
        err,
        nomifun_cron::error::CronError::InvalidSchedule(_)
    ));
}

// ── SC-8: Every negative interval ─────────────────────────────────

#[tokio::test]
async fn sc8_every_negative_interval() {
    let (svc, _, _) = setup().await;
    let req = make_create_req(
        "Negative Interval",
        CronScheduleDto::Every {
            every_ms: -1000,
            description: None,
        },
    );
    let err = svc.add_job(TEST_USER_ID, req).await.unwrap_err();
    assert!(matches!(
        err,
        nomifun_cron::error::CronError::InvalidSchedule(_)
    ));
}

// ── OC-1: Init preserves lazy-bind "existing" jobs with empty conversation_id ─────

#[tokio::test]
async fn oc1_init_preserves_lazy_existing_jobs() {
    // "existing + empty conversation_id" is a legitimate lazy-binding job:
    // the frontend creates a cron from the standalone cron page before any
    // conversation exists, and the first execution materializes it. Those
    // jobs must survive init, not be cleaned up as orphans.
    let (svc, _repo, _) = setup().await;

    let mut req = make_create_req("Lazy Existing", every_60s());
    req.conversation_id = None;
    req.execution_mode = Some("existing".into());
    let lazy = svc.add_job(TEST_USER_ID, req).await.unwrap();

    let normal_req = make_create_req("Normal", every_60s());
    let normal = svc.add_job(TEST_USER_ID, normal_req).await.unwrap();

    svc.init().await;

    let found_lazy = svc.get_job(TEST_USER_ID, &lazy.cron_job_id).await;
    assert!(
        found_lazy.is_ok(),
        "lazy-bind existing job should survive init"
    );

    let found = svc.get_job(TEST_USER_ID, &normal.cron_job_id).await;
    assert!(found.is_ok());
}

// NewConversation jobs do not accept a pre-existing conversation relation.
#[tokio::test]
async fn oc1b_new_conversation_rejects_conversation_id_without_persisting() {
    let (svc, _, _) = setup().await;

    let mut empty_req = make_create_req("New-conv empty", every_60s());
    empty_req.conversation_id = None;
    empty_req.execution_mode = Some("new_conversation".into());
    let empty = svc.add_job(TEST_USER_ID, empty_req).await.unwrap();

    let mut stale_req = make_create_req("New-conv with stale id", every_60s());
    stale_req.conversation_id = Some(CONV_8.to_owned());
    stale_req.execution_mode = Some("new_conversation".into());
    let error = svc.add_job(TEST_USER_ID, stale_req).await.unwrap_err();

    assert!(matches!(
        error,
        nomifun_cron::error::CronError::App(nomifun_common::AppError::BadRequest(_))
    ));

    assert!(
        svc.get_job(TEST_USER_ID, &empty.cron_job_id).await.is_ok(),
        "valid unbound new_conversation job must persist"
    );
    let jobs = svc.list_jobs(TEST_USER_ID, &ListCronJobsQuery::default()).await.unwrap();
    assert_eq!(jobs.len(), 1);
}

#[tokio::test]
async fn oc2_init_cleans_jobs_with_missing_conversation() {
    let (svc, repo, _) = setup().await;

    let mut missing_req = make_create_req("Missing Conversation", every_60s());
    // Create through a valid owner-scoped boundary, then mutate the persisted
    // fixture to emulate an orphaned logical reference discovered during boot.
    // New API writes reject a missing Conversation before persistence.
    missing_req.conversation_id = Some(CONV_7.to_owned());
    let missing = svc.add_job(TEST_USER_ID, missing_req).await.unwrap();
    repo.update(
        TEST_USER_ID,
        &missing.cron_job_id,
        &nomifun_db::UpdateCronJobParams {
            conversation_id: Some(Some(CONV_MISSING.to_owned())),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let mut normal_req = make_create_req("Existing Conversation", every_60s());
    normal_req.conversation_id = Some(CONV_7.to_owned());
    let normal = svc.add_job(TEST_USER_ID, normal_req).await.unwrap();

    svc.init().await;

    let err = svc.get_job(TEST_USER_ID, &missing.cron_job_id).await;
    assert!(err.is_err());

    let found = svc.get_job(TEST_USER_ID, &normal.cron_job_id).await;
    assert!(found.is_ok());
}

// ── Delete skill explicitly ───────────────────────────────────────

#[tokio::test]
async fn delete_skill_clears_content() {
    let (svc, _, _) = setup().await;
    let job = svc
        .add_job(TEST_USER_ID, make_create_req("Del Skill", every_60s()))
        .await
        .unwrap();

    svc.save_skill(
        TEST_USER_ID,
        &job.cron_job_id,
        SaveCronSkillRequest {
            content: "---\nname: x\n---\nOk".into(),
        },
    )
    .await
    .unwrap();
    assert!(svc.has_skill(TEST_USER_ID, &job.cron_job_id).await.unwrap().has_skill);

    svc.delete_skill(TEST_USER_ID, &job.cron_job_id).await.unwrap();
    assert!(!svc.has_skill(TEST_USER_ID, &job.cron_job_id).await.unwrap().has_skill);
}

// ── ICronService trait: create ─────────────────────────────────────

#[tokio::test]
async fn icron_service_create_job() {
    let (svc, _, _, conv_repo, _, _) = setup_with_conv_repo().await;

    use nomifun_conversation::response_middleware::ICronService;

    let params = CronCreateParams {
        name: "Agent Job".into(),
        schedule: "0 */10 * * * *".into(),
        schedule_description: "every 10 min".into(),
        message: "do agent work".into(),
    };

    let result = ICronService::create_job(&svc, TEST_USER_ID, CONV_1, &params).await;
    assert!(result.success);
    assert!(result.message.contains("Agent Job"));

    let bound = conv_repo.get(CONV_1).await.unwrap().unwrap();
    assert!(bound.cron_job_id.is_some());
}

#[tokio::test]
async fn icron_service_create_job_inherits_conversation_mode_and_backend() {
    let (svc, _, _) = setup().await;

    use nomifun_conversation::response_middleware::ICronService;

    let params = CronCreateParams {
        name: "Agent Job".into(),
        schedule: "0 */10 * * * *".into(),
        schedule_description: "every 10 min".into(),
        message: "do agent work".into(),
    };

    let result = ICronService::create_job(&svc, TEST_USER_ID, CONV_MODE, &params).await;
    assert!(result.success);

    let jobs = svc
        .list_jobs(TEST_USER_ID, &ListCronJobsQuery {
            conversation_id: Some(CONV_MODE.to_owned()),
        })
        .await
        .unwrap();
    assert_eq!(jobs.len(), 1);

    let job = &jobs[0];
    let config = job
        .agent_config
        .as_ref()
        .expect("agent config should be copied");
    assert_eq!(job.agent_type, "acp");
    assert_eq!(job.conversation_title.as_deref(), Some("Gemini Chat"));
    assert_eq!(config.backend.as_deref(), Some("gemini"));
    assert_eq!(config.name, "Gemini");
    assert_eq!(config.mode.as_deref(), Some("yolo"));
    assert_eq!(config.model.as_deref(), Some("gemini-2.5-pro"));
    assert_eq!(config.workspace.as_deref(), Some("/tmp/gemini-workspace"));
}

#[tokio::test]
async fn icron_service_create_job_forces_full_auto_mode_for_generated_crons() {
    let (svc, _, _) = setup().await;

    use nomifun_conversation::response_middleware::ICronService;

    let params = CronCreateParams {
        name: "Generated Agent Job".into(),
        schedule: "0 */10 * * * *".into(),
        schedule_description: "every 10 min".into(),
        message: "do agent work".into(),
    };

    let gemini = ICronService::create_job(&svc, TEST_USER_ID, CONV_MODE_DEFAULT, &params).await;
    assert!(gemini.success);

    let codex = ICronService::create_job(&svc, TEST_USER_ID, CONV_MODE_CODEX, &params).await;
    assert!(codex.success);

    let claude = ICronService::create_job(&svc, TEST_USER_ID, CONV_MODE_CLAUDE, &params).await;
    assert!(claude.success);

    let nomi = ICronService::create_job(&svc, TEST_USER_ID, CONV_MODE_NOMI, &params).await;
    assert!(nomi.success);

    let gemini_jobs = svc
        .list_jobs(TEST_USER_ID, &ListCronJobsQuery {
            conversation_id: Some(CONV_MODE_DEFAULT.to_owned()),
        })
        .await
        .unwrap();
    assert_eq!(
        gemini_jobs[0]
            .agent_config
            .as_ref()
            .and_then(|config| config.mode.as_deref()),
        Some("yolo")
    );

    let codex_jobs = svc
        .list_jobs(TEST_USER_ID, &ListCronJobsQuery {
            conversation_id: Some(CONV_MODE_CODEX.to_owned()),
        })
        .await
        .unwrap();
    assert_eq!(
        codex_jobs[0]
            .agent_config
            .as_ref()
            .and_then(|config| config.mode.as_deref()),
        Some("full-access")
    );

    let claude_jobs = svc
        .list_jobs(TEST_USER_ID, &ListCronJobsQuery {
            conversation_id: Some(CONV_MODE_CLAUDE.to_owned()),
        })
        .await
        .unwrap();
    assert_eq!(
        claude_jobs[0]
            .agent_config
            .as_ref()
            .and_then(|config| config.mode.as_deref()),
        Some("bypassPermissions")
    );

    let nomi_jobs = svc
        .list_jobs(TEST_USER_ID, &ListCronJobsQuery {
            conversation_id: Some(CONV_MODE_NOMI.to_owned()),
        })
        .await
        .unwrap();
    assert_eq!(
        nomi_jobs[0]
            .agent_config
            .as_ref()
            .and_then(|config| config.mode.as_deref()),
        Some("yolo")
    );
}

// ── ICronService trait: list ───────────────────────────────────────

#[tokio::test]
async fn icron_service_list_jobs() {
    let (svc, _, _) = setup().await;

    use nomifun_conversation::response_middleware::ICronService;

    let result = ICronService::list_jobs(&svc, TEST_USER_ID, CONV_1).await;
    assert!(result.success);
    assert!(
        result
            .message
            .contains(&format!("No cron jobs found for conversation '{}'", CONV_1))
    );

    let mut req = make_create_req("Listed Job", every_60s());
    req.conversation_id = Some(CONV_1.to_owned());
    svc.add_job(TEST_USER_ID, req).await.unwrap();

    let result = ICronService::list_jobs(&svc, TEST_USER_ID, CONV_1).await;
    assert!(result.success);
    assert!(
        result
            .message
            .contains(&format!("Found 1 cron job(s) for conversation '{}'", CONV_1))
    );
    assert!(result.message.contains("Listed Job"));
}

// ── ICronService trait: update ─────────────────────────────────────

#[tokio::test]
async fn icron_service_update_job() {
    let (svc, _, _, conv_repo, _, _) = setup_with_conv_repo().await;

    use nomifun_conversation::response_middleware::ICronService;

    let mut request = make_create_req("Update Via Trait", every_60s());
    request.conversation_id = Some(CONV_1.to_owned());
    let job = svc.add_job(TEST_USER_ID, request).await.unwrap();

    let params = CronUpdateParams {
        job_id: job.cron_job_id.clone(),
        name: "Updated Via Trait".into(),
        schedule: "0 */10 * * * *".into(),
        schedule_description: "every 10 min".into(),
        message: "do updated work".into(),
    };

    let result = ICronService::update_job(&svc, TEST_USER_ID, CONV_1, &params).await;
    assert!(result.success);
    assert!(result.message.contains("Updated Via Trait"));

    let bound = conv_repo.get(CONV_1).await.unwrap().unwrap();
    assert_eq!(bound.cron_job_id.as_deref(), Some(job.cron_job_id.as_str()));

    let linked = conv_repo
        .list_by_cron_job(TEST_USER_ID, &job.cron_job_id)
        .await
        .unwrap();
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].conversation_id, CONV_1);
}

#[tokio::test]
async fn icron_service_update_job_rejects_cross_conversation_scope() {
    let (svc, _, _, _, _, _) = setup_with_conv_repo().await;

    use nomifun_conversation::response_middleware::ICronService;

    let job = svc
        .add_job(TEST_USER_ID, make_create_req("Scoped Update", every_60s()))
        .await
        .unwrap();
    let params = CronUpdateParams {
        job_id: job.cron_job_id.clone(),
        name: "Must Not Change".into(),
        schedule: "0 */10 * * * *".into(),
        schedule_description: "every 10 min".into(),
        message: "must not change".into(),
    };

    let result = ICronService::update_job(&svc, TEST_USER_ID, CONV_2, &params).await;
    assert!(!result.success);
    assert!(result.message.contains("is not bound to conversation"));

    let persisted = svc
        .get_job(TEST_USER_ID, &job.cron_job_id)
        .await
        .unwrap();
    assert_eq!(persisted.name, "Scoped Update");
    assert_eq!(persisted.message, "test message");
}

// ── ICronService trait: delete ─────────────────────────────────────

#[tokio::test]
async fn icron_service_delete_job() {
    let (svc, _, _) = setup().await;

    use nomifun_conversation::response_middleware::ICronService;

    let job = svc
        .add_job(TEST_USER_ID, make_create_req("Delete Via Trait", every_60s()))
        .await
        .unwrap();

    let result = ICronService::delete_job(&svc, TEST_USER_ID, &job.cron_job_id).await;
    assert!(result.success);

    let result = ICronService::delete_job(&svc, TEST_USER_ID, MISSING_JOB_ID).await;
    assert!(!result.success);
}

// ── Update with max_retries ───────────────────────────────────────

#[tokio::test]
async fn update_max_retries() {
    let (svc, repo, _) = setup().await;
    let job = svc
        .add_job(TEST_USER_ID, make_create_req("Retries", every_60s()))
        .await
        .unwrap();
    assert_eq!(job.max_retries, 3);

    let req = UpdateCronJobRequest {
        name: None,
        description: None,
        enabled: None,
        schedule: None,
        message: None,
        agent_config: None,
        conversation_title: None,
        max_retries: Some(5),
    };
    let updated = svc.update_job(TEST_USER_ID, &job.cron_job_id, req).await.unwrap();
    assert_eq!(updated.max_retries, 5);
    let persisted = repo
        .get_by_cron_job_id(TEST_USER_ID, &job.cron_job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.max_retries, 5);
}

#[tokio::test]
async fn update_rejects_negative_max_retries_without_partial_write() {
    let (svc, repo, _) = setup().await;
    let job = svc
        .add_job(TEST_USER_ID, make_create_req("Retries", every_60s()))
        .await
        .unwrap();

    let error = svc
        .update_job(
            TEST_USER_ID,
            &job.cron_job_id,
            UpdateCronJobRequest {
                name: Some("Must Not Change".into()),
                description: None,
                enabled: None,
                schedule: None,
                message: None,
                agent_config: None,
                conversation_title: None,
                max_retries: Some(-1),
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("must be non-negative"));

    let persisted = repo
        .get_by_cron_job_id(TEST_USER_ID, &job.cron_job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.name, "Retries");
    assert_eq!(persisted.max_retries, 3);
}

// ── SC-1: At type — future timestamp, nextRunAtMs == atMs ────────

#[tokio::test]
async fn sc1_at_type_future_timestamp() {
    let (svc, _, _) = setup().await;
    let target_ms = now_ms() + 3_600_000;
    let req = make_create_req(
        "At Future",
        CronScheduleDto::At {
            at_ms: target_ms,
            description: Some("once in 1h".into()),
        },
    );
    let job = svc.add_job(TEST_USER_ID, req).await.unwrap();
    assert_eq!(job.next_run_at, Some(target_ms));
}

// ── SC-2: At type — past timestamp, nextRunAtMs == atMs ──────────

#[tokio::test]
async fn sc2_at_type_past_timestamp() {
    let (svc, _, _) = setup().await;
    let target_ms = now_ms() - 3_600_000;
    let req = make_create_req(
        "At Past",
        CronScheduleDto::At {
            at_ms: target_ms,
            description: Some("once in the past".into()),
        },
    );
    let job = svc.add_job(TEST_USER_ID, req).await.unwrap();
    assert_eq!(job.next_run_at, Some(target_ms));
}

// ── SR-1: System resume detects missed jobs ──────────────────────

#[tokio::test]
async fn sr1_system_resume_missed_job() {
    let (svc, repo, bc, conv_repo, _, _) = setup_with_conv_repo().await;

    let mut req = make_create_req("Resume Job", every_60s());
    req.conversation_id = Some(CONV_1.to_owned());
    let job = svc.add_job(TEST_USER_ID, req).await.unwrap();
    bc.take_events();

    let past_ms = now_ms() - 10_000;
    let params = nomifun_db::UpdateCronJobParams {
        next_run_at: Some(Some(past_ms)),
        ..Default::default()
    };
    repo.update(TEST_USER_ID, &job.cron_job_id, &params).await.unwrap();

    svc.handle_system_resume().await;

    let updated = svc.get_job(TEST_USER_ID, &job.cron_job_id).await.unwrap();
    assert!(
        updated.last_run_at.is_none(),
        "missed job should not be auto-executed on resume"
    );
    assert_eq!(updated.last_status, Some(JobStatus::Missed));
    assert!(
        updated.next_run_at.is_some(),
        "job should be rescheduled after being marked missed"
    );
    assert!(
        updated.next_run_at.unwrap() > now_ms() - 2000,
        "next_run_at should be in the future"
    );

    let messages = conv_repo.take_messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].r#type, "tips");
    assert!(messages[0].content.contains("Resume Job"));
    assert!(messages[0].content.contains("not run automatically"));

    let events = bc.take_events();
    let updated_event = events
        .iter()
        .find(|event| event.name == "cron.job-updated")
        .expect("resume should broadcast the fully persisted job before the execution event");
    assert_eq!(updated_event.data["metadata"]["conversation_id"], CONV_1);
    assert_eq!(updated_event.data["state"]["last_status"], "missed");
    assert!(updated_event.data["state"]["next_run_at_ms"].as_i64().is_some());
    let updated_index = events
        .iter()
        .position(|event| event.name == "cron.job-updated")
        .unwrap();
    let executed_index = events
        .iter()
        .position(|event| event.name == "cron.job-executed")
        .unwrap();
    assert!(updated_index < executed_index, "job-updated must precede job-executed");
    assert!(
        events
            .iter()
            .any(|event| { event.name == "cron.job-executed" && event.data["status"] == "missed" }),
        "resume should emit a missed execution event"
    );
    assert!(
        events.iter().any(|event| {
            event.name == "message.stream"
                && event.data["type"] == "tips"
                && event.data["conversation_id"] == CONV_1
        }),
        "resume should emit a tips websocket message"
    );
}

// ── CD-1: Cascade delete cron jobs when conversation is deleted ──

#[tokio::test]
async fn cd1_cascade_delete_by_conversation() {
    let (svc, _repo, bc) = setup().await;

    let mut req_a = make_create_req("Cascade A", every_60s());
    req_a.conversation_id = Some(CONV_5.to_owned());
    let job_a = svc.add_job(TEST_USER_ID, req_a).await.unwrap();

    let mut req_b = make_create_req("Cascade B", every_60s());
    req_b.conversation_id = Some(CONV_6.to_owned());
    let job_b = svc.add_job(TEST_USER_ID, req_b).await.unwrap();

    let mut req_c = make_create_req("Unrelated", every_60s());
    req_c.conversation_id = Some(CONV_3.to_owned());
    let _job_c = svc.add_job(TEST_USER_ID, req_c).await.unwrap();

    bc.take_events();

    svc.delete_jobs_by_conversation(TEST_USER_ID, CONV_5)
        .await;

    assert!(svc.get_job(TEST_USER_ID, &job_a.cron_job_id).await.is_err());
    assert!(svc.get_job(TEST_USER_ID, &job_b.cron_job_id).await.is_ok());

    let remaining = svc.list_jobs(TEST_USER_ID, &ListCronJobsQuery::default()).await.unwrap();
    assert_eq!(remaining.len(), 2, "unrelated jobs should remain");

    let events = bc.take_events();
    let removed_events: Vec<_> = events
        .iter()
        .filter(|e| e.name == "cron.job-removed")
        .collect();
    assert_eq!(removed_events.len(), 1, "should emit 1 removed event");
}

// ── CD-2: Cascade delete on empty conversation (no-op) ──────────

#[tokio::test]
async fn cd2_cascade_delete_no_matching_jobs() {
    let (svc, _repo, bc) = setup().await;

    svc.add_job(TEST_USER_ID, make_create_req("Existing", every_60s()))
        .await
        .unwrap();
    bc.take_events();

    svc.delete_jobs_by_conversation(TEST_USER_ID, "0190f5fe-7c00-7a00-8abc-012345679999")
        .await;

    let events = bc.take_events();
    assert!(
        events.is_empty(),
        "no events should be emitted when no jobs match"
    );

    let all = svc.list_jobs(TEST_USER_ID, &ListCronJobsQuery::default()).await.unwrap();
    assert_eq!(all.len(), 1, "existing job should remain untouched");
}

// ── CD-3: OnConversationDelete trait dispatches cascade ──────────

#[tokio::test]
async fn cd3_on_conversation_delete_trait() {
    use nomifun_common::OnConversationDelete;

    let (svc, _repo, bc) = setup().await;

    let mut req = make_create_req("Trait Cascade", every_60s());
    req.conversation_id = Some(CONV_6.to_owned());
    let job = svc.add_job(TEST_USER_ID, req).await.unwrap();
    bc.take_events();

    svc.on_conversation_deleted(TEST_USER_ID, CONV_6).await;

    assert!(svc.get_job(TEST_USER_ID, &job.cron_job_id).await.is_err());

    let events = bc.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "cron.job-removed");
}

#[tokio::test]
async fn cd4_conversation_transaction_hands_captured_job_ids_to_post_commit_cleanup() {
    let (cron_service, _repo, bc, _stub_conversations, pool, data_dir) =
        setup_with_conv_repo().await;
    let cron_service = Arc::new(cron_service);

    let mut req = make_create_req("Transactional Cascade", every_60s());
    req.conversation_id = Some(CONV_MODE.to_owned());
    let job = cron_service.add_job(TEST_USER_ID, req).await.unwrap();
    write_raw_skill_file(
        &data_dir,
        &job.cron_job_id,
        "---\nname: transactional-cascade\ndescription: cascade fixture\n---\n\nRun it.",
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO cron_job_runs \
            (cron_job_run_id, cron_job_id, executed_at_ms, status, created_at_ms) \
         VALUES (?, ?, ?, 'ok', ?)",
    )
    .bind(nomifun_common::CronJobRunId::new().into_string())
    .bind(&job.cron_job_id)
    .bind(now_ms())
    .bind(now_ms())
    .execute(&pool)
    .await
    .unwrap();
    bc.take_events();

    struct EmptySkillResolver;
    #[async_trait::async_trait]
    impl nomifun_conversation::skill_resolver::SkillResolver for EmptySkillResolver {
        async fn auto_inject_names(&self) -> Vec<String> {
            Vec::new()
        }

        async fn resolve_skills(
            &self,
            _names: &[String],
        ) -> Vec<nomifun_conversation::skill_resolver::ResolvedAgentSkill> {
            Vec::new()
        }

        async fn link_workspace_skills(
            &self,
            _workspace: &std::path::Path,
            _rel_dirs: &[&str],
            _skills: &[nomifun_conversation::skill_resolver::ResolvedAgentSkill],
        ) -> usize {
            0
        }
    }

    let conversation_service = ConversationService::new(
        Arc::<str>::from(TEST_USER_ID),
        std::env::temp_dir(),
        bc.clone() as Arc<dyn UserEventSink>,
        Arc::new(EmptySkillResolver),
        Arc::new(StubAgentRuntimeRegistry),
        Arc::new(SqliteConversationRepository::new(pool.clone())),
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone())),
        Arc::new(SqliteAcpSessionRepository::new(pool.clone())),
        Arc::new(nomifun_conversation::NoExecutionConversationBoundary),
    );
    conversation_service.with_delete_hook(cron_service.clone());
    conversation_service
        .delete(TEST_USER_ID, CONV_MODE)
        .await
        .unwrap();

    assert!(
        cron_service.get_job(TEST_USER_ID, &job.cron_job_id).await.is_err(),
        "the Conversation transaction must delete the Cron row"
    );
    let run_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM cron_job_runs WHERE cron_job_id = ?")
            .bind(&job.cron_job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(run_count, 0, "Cron run history must cascade in the same transaction");

    for _ in 0..50 {
        if !has_skill_file(&data_dir, &job.cron_job_id).await.unwrap() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        !has_skill_file(&data_dir, &job.cron_job_id).await.unwrap(),
        "the post-commit hook must remove the generated skill using captured IDs"
    );
    assert!(
        bc.take_events()
            .iter()
            .any(|event| event.name == "cron.job-removed" && event.data["cron_job_id"] == job.cron_job_id),
        "the post-commit hook must emit the Cron removal event"
    );
}
