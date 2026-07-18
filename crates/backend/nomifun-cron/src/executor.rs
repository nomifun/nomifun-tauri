use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nomifun_ai_agent::runtime_registry::AgentRuntimeRegistry;
use nomifun_ai_agent::types::AgentRuntimeBuildOptions;
#[cfg(test)]
use nomifun_ai_agent::types::SendMessageData;
use nomifun_ai_agent::{AgentRegistry, AgentStreamEvent};
use nomifun_api_types::{CreateConversationRequest, SendMessageRequest};
use nomifun_common::{
    AgentType, AppError, ConversationId, CronJobId, ExecutionAuthority, ProviderWithModel, UserId,
    now_ms, validate_prefixed_id, workspace_path_has_edge_whitespace_segment,
};
use nomifun_conversation::{ConversationService, runtime_state::RuntimeBuildLease};
use nomifun_db::models::MessageRow;
use nomifun_db::{ConversationRowUpdate, IConversationRepository};
use nomifun_realtime::UserEventSink;
use tokio::sync::broadcast;
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::artifacts::{build_cron_trigger_artifact, emit_artifact};
use crate::busy_guard::CronBusyGuard;
use crate::error::CronError;
use crate::prompt::{
    build_existing_conversation_prompt, build_new_conversation_prompt,
    build_new_conversation_prompt_with_skill_suggest, build_new_conversation_with_skill_prompt,
    build_skill_suggest_prompt,
};
use crate::skill_file::{
    cron_skill_name, read_skill_content, write_raw_skill_file, write_skill_file,
};
use crate::skill_suggest::SkillSuggestDetector;
use crate::types::{CronJob, ExecutionMode, cron_job_to_row};

pub const RETRY_INTERVAL_MS: u64 = 30_000;
const SKILL_SUGGEST_TURN_TIMEOUT: Duration = Duration::from_secs(120);
const TEMP_WORKSPACE_ID_EXTRA_KEY: &str = "temp_workspace_id";

fn parse_conversation_id(id: &str) -> Result<&str, AppError> {
    ConversationId::try_from(id)
        .map(|_| id)
        .map_err(|_| AppError::NotFound(format!("conversation {id}")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionResult {
    Success { conversation_id: String },
    Retrying { attempt: i64 },
    Skipped,
    Error { message: String },
}

#[derive(Debug)]
pub(crate) struct PreparedExecution {
    pub conversation_id: String,
    saved_skill: Option<SavedSkillContext>,
    build_lease: RuntimeBuildLease,
}

/// Inputs captured for the post-turn skill-suggest detection pipeline.
/// Grouped into a struct so the spawning function stays under the
/// clippy `too_many_arguments` threshold and so the agent/receiver
/// (which the spawner clones) remain distinct from these metadata
/// fields.
struct SkillSuggestContext {
    owner_id: String,
    conversation_id: String,
    job_id: String,
    job_name: String,
    workspace: String,
    needs_follow_up: bool,
    skill_names: Vec<String>,
}

pub struct JobExecutor {
    authoritative_user_id: Arc<str>,
    runtime_registry: Arc<dyn AgentRuntimeRegistry>,
    conversation_repo: Arc<dyn IConversationRepository>,
    conversation_service: Arc<ConversationService>,
    busy_guard: Arc<CronBusyGuard>,
    work_dir: PathBuf,
    data_dir: PathBuf,
    user_events: Arc<dyn UserEventSink>,
    agent_registry: Arc<AgentRegistry>,
    skill_suggest_detector: SkillSuggestDetector,
}

impl JobExecutor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        authoritative_user_id: Arc<str>,
        runtime_registry: Arc<dyn AgentRuntimeRegistry>,
        conversation_repo: Arc<dyn IConversationRepository>,
        conversation_service: Arc<ConversationService>,
        busy_guard: Arc<CronBusyGuard>,
        work_dir: PathBuf,
        data_dir: PathBuf,
        user_events: Arc<dyn UserEventSink>,
        agent_registry: Arc<AgentRegistry>,
    ) -> Self {
        let skill_suggest_detector = SkillSuggestDetector::new(
            Arc::clone(&user_events),
            conversation_repo.clone(),
            data_dir.clone(),
        );
        Self {
            authoritative_user_id,
            runtime_registry,
            conversation_repo,
            conversation_service,
            busy_guard,
            work_dir,
            data_dir,
            user_events,
            agent_registry,
            skill_suggest_detector,
        }
    }

    fn controls_host(&self, user_id: &str) -> bool {
        ExecutionAuthority::resolve(user_id, self.authoritative_user_id.as_ref())
            .controls_host()
    }

    async fn prepare_authorized_saved_skill(
        &self,
        job: &CronJob,
    ) -> Result<Option<SavedSkillContext>, CronError> {
        if self.controls_host(&job.user_id) {
            self.prepare_saved_skill(job).await
        } else {
            Ok(None)
        }
    }

    pub async fn execute(&self, job: &CronJob) -> ExecutionResult {
        if let Err(error) = cron_job_to_row(job) {
            return ExecutionResult::Error {
                message: error.to_string(),
            };
        }
        if job
            .conversation_id
            .as_deref()
            .is_some_and(|conversation_id| self.busy_guard.is_busy(conversation_id))
        {
            return self.handle_busy(job);
        }
        let mut build_lease = if matches!(job.execution_mode, ExecutionMode::Existing) {
            match job.conversation_id.as_deref() {
                Some(conversation_id) => match self
                    .conversation_service
                    .begin_public_runtime_build(conversation_id, &job.user_id)
                {
                    Ok(lease) => Some(lease),
                    Err(error) => {
                        return ExecutionResult::Error {
                            message: error.to_string(),
                        };
                    }
                },
                None => None,
            }
        } else {
            None
        };

        // Existing-mode cron is a public/background initiator. Fence retained
        // Attempt transcripts before skill-file preparation or any runtime
        // mutation; new-conversation mode receives a new ordinary Conversation
        // and is checked again after resolution below.
        if matches!(job.execution_mode, ExecutionMode::Existing)
            && let Some(conversation_id) = job.conversation_id.as_deref()
            && let Err(error) = self
                .ensure_public_conversation_mutable(job, conversation_id)
                .await
        {
            return ExecutionResult::Error {
                message: error.to_string(),
            };
        }

        let saved_skill = match self.prepare_authorized_saved_skill(job).await {
            Ok(skill) => skill,
            Err(e) => {
                error!(job_id = %job.id, error = %e, "Failed to prepare saved cron skill");
                return ExecutionResult::Error {
                    message: e.to_string(),
                };
            }
        };

        if let Err(e) = self.validate_runtime_job_workspace(job).await {
            error!(job_id = %job.id, error = %e, "Failed cron workspace validation");
            return ExecutionResult::Error {
                message: e.to_string(),
            };
        }

        let target_conversation_id =
            match self.resolve_conversation(job, saved_skill.as_ref()).await {
                Ok(id) => id,
                Err(e) => {
                    error!(job_id = %job.id, error = %e, "Failed to resolve conversation");
                    return ExecutionResult::Error {
                        message: e.to_string(),
                    };
                }
            };
        if build_lease.is_none() {
            build_lease = match self
                .conversation_service
                .begin_public_runtime_build(&target_conversation_id, &job.user_id)
            {
                Ok(lease) => Some(lease),
                Err(error) => {
                    return ExecutionResult::Error {
                        message: error.to_string(),
                    };
                }
            };
        }

        if let Err(error) = self
            .ensure_public_conversation_mutable(job, &target_conversation_id)
            .await
        {
            return ExecutionResult::Error {
                message: error.to_string(),
            };
        }

        self.busy_guard
            .set_processing(&target_conversation_id, true);

        let result = self
            .execute_inner_with_lease(
                job,
                &target_conversation_id,
                saved_skill.as_ref(),
                build_lease.expect("cron runtime lease resolved before execution"),
            )
            .await;

        self.busy_guard
            .set_processing(&target_conversation_id, false);

        result
    }

    pub(crate) async fn prepare_run_now(
        &self,
        job: &CronJob,
    ) -> Result<PreparedExecution, CronError> {
        cron_job_to_row(job)?;
        let mut build_lease = if matches!(job.execution_mode, ExecutionMode::Existing) {
            job.conversation_id
                .as_deref()
                .map(|conversation_id| {
                    self.conversation_service
                        .begin_public_runtime_build(conversation_id, &job.user_id)
                        .map_err(CronError::App)
                })
                .transpose()?
        } else {
            None
        };
        if matches!(job.execution_mode, ExecutionMode::Existing)
            && let Some(conversation_id) = job.conversation_id.as_deref()
        {
            self.ensure_public_conversation_mutable(job, conversation_id)
                .await?;
        }
        let saved_skill = match self.prepare_authorized_saved_skill(job).await {
            Ok(skill) => skill,
            Err(err) => {
                error!(
                    job_id = %job.id,
                    error = %err,
                    "Failed to prepare saved cron skill for run-now"
                );
                return Err(err);
            }
        };

        self.validate_runtime_job_workspace(job).await?;
        let conversation_id = self.resolve_conversation(job, saved_skill.as_ref()).await?;
        if build_lease.is_none() {
            build_lease = Some(
                self.conversation_service
                    .begin_public_runtime_build(&conversation_id, &job.user_id)
                    .map_err(CronError::App)?,
            );
        }
        self.ensure_public_conversation_mutable(job, &conversation_id)
            .await?;

        Ok(PreparedExecution {
            conversation_id,
            saved_skill,
            build_lease: build_lease.expect("cron runtime lease resolved during preparation"),
        })
    }

    pub(crate) async fn execute_prepared(
        &self,
        job: &CronJob,
        prepared: PreparedExecution,
    ) -> ExecutionResult {
        if let Err(error) = self
            .ensure_public_conversation_mutable(job, &prepared.conversation_id)
            .await
        {
            return ExecutionResult::Error {
                message: error.to_string(),
            };
        }
        let PreparedExecution {
            conversation_id,
            saved_skill,
            build_lease,
        } = prepared;
        self.busy_guard
            .set_processing(&conversation_id, true);

        let result = self
            .execute_inner_with_lease(
                job,
                &conversation_id,
                saved_skill.as_ref(),
                build_lease,
            )
            .await;

        self.busy_guard
            .set_processing(&conversation_id, false);

        result
    }

    pub fn busy_guard(&self) -> &CronBusyGuard {
        &self.busy_guard
    }

    async fn ensure_public_conversation_mutable(
        &self,
        job: &CronJob,
        conversation_id: &str,
    ) -> Result<(), CronError> {
        self.verify_target_conversation_owner(job, conversation_id)
            .await?;
        self.conversation_service
            .ensure_public_mutation_allowed(&job.user_id, conversation_id)
            .await
            .map_err(CronError::App)
    }

    pub async fn get_conversation_row(
        &self,
        conversation_id: &str,
    ) -> Result<Option<nomifun_db::models::ConversationRow>, CronError> {
        let key = parse_conversation_id(conversation_id)?;
        self.conversation_repo
            .get(key)
            .await
            .map_err(CronError::Database)
    }

    pub(crate) async fn resolve_job_workspace_raw(
        &self,
        job: &CronJob,
    ) -> Result<String, CronError> {
        self.resolve_execution_workspace_raw(job, job.conversation_id.as_deref())
            .await
    }

    pub(crate) async fn validate_runtime_job_workspace(
        &self,
        job: &CronJob,
    ) -> Result<(), CronError> {
        let workspace = self.resolve_job_workspace_raw(job).await?;
        if workspace.trim().is_empty() {
            return Ok(());
        }

        if workspace_path_has_edge_whitespace_segment(Path::new(&workspace)) {
            return Err(CronError::App(
                AppError::WorkspacePathEdgeWhitespaceRuntimeUnsupported(workspace),
            ));
        }

        Ok(())
    }

    pub async fn insert_tips_message(
        &self,
        owner_id: &str,
        conversation_id: &str,
        content: &str,
        tip_type: &str,
    ) -> Result<(), CronError> {
        UserId::try_from(owner_id)
            .map_err(|error| CronError::Scheduler(format!("invalid cron owner id: {error}")))?;
        let row = self
            .get_conversation_row(conversation_id)
            .await?
            .filter(|row| row.user_id == owner_id)
            .ok_or_else(|| {
                CronError::Scheduler(format!(
                    "conversation {conversation_id} is not owned by cron owner {owner_id}"
                ))
            })?;
        debug_assert_eq!(row.user_id, owner_id);
        // Reuse the conversation service's canonical message-ID minting
        // boundary so every source persists the same `msg_*` contract.
        let row = MessageRow {
            id: ConversationService::mint_msg_id(),
            conversation_id: parse_conversation_id(conversation_id)?.to_owned(),
            msg_id: None,
            r#type: "tips".into(),
            content: serde_json::json!({
                "content": content,
                "type": tip_type,
            })
            .to_string(),
            position: Some("center".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: nomifun_common::now_ms(),
        };

        self.conversation_repo
            .insert_message(&row)
            .await
            .map_err(CronError::Database)
    }

    /// Bind a conversation to its owning cron job by writing the
    /// `conversations.cron_job_id` FK column (was `extra.cronJobId`). This is
    /// the conversation-side half of the circular FK; the cron-side half
    /// (`cron_jobs.conversation_id`) is written by the service layer's
    /// `UpdateCronJobParams.conversation_id`.
    ///
    /// Idempotent: a no-op when the column already points at this job. The
    /// conversation row already exists (the executor only binds AFTER
    /// `ConversationService::create` returns) so the FK is always satisfiable.
    pub async fn bind_cron_job_to_conversation(
        &self,
        owner_id: &str,
        conversation_id: &str,
        cron_job_id: &str,
    ) -> Result<(), CronError> {
        UserId::try_from(owner_id)
            .map_err(|error| CronError::Scheduler(format!("invalid cron owner id: {error}")))?;
        CronJobId::try_from(cron_job_id)
            .map_err(|error| CronError::Scheduler(format!("invalid cron job id: {error}")))?;
        let Some(row) = self.get_conversation_row(conversation_id).await? else {
            return Err(CronError::Scheduler(format!(
                "conversation {conversation_id} not found while binding cron job {cron_job_id}"
            )));
        };
        if row.user_id != owner_id {
            return Err(CronError::Scheduler(format!(
                "conversation {conversation_id} owner does not match cron job {cron_job_id}"
            )));
        }

        if row.cron_job_id.as_deref() == Some(cron_job_id) {
            return Ok(());
        }

        let update = ConversationRowUpdate {
            cron_job_id: Some(Some(cron_job_id.to_owned())),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        self.conversation_repo
            .update(parse_conversation_id(conversation_id)?, &update)
            .await
            .map_err(CronError::Database)
    }

}

impl JobExecutor {
    fn handle_busy(&self, job: &CronJob) -> ExecutionResult {
        let max_retries = job.max_retries;
        let current_retry = job.retry_count;

        if current_retry >= max_retries {
            warn!(
                job_id = %job.id,
                retries = current_retry,
                "Max retries exceeded, skipping"
            );
            return ExecutionResult::Skipped;
        }

        let attempt = current_retry + 1;
        info!(
            job_id = %job.id,
            attempt,
            max_retries,
            "Conversation busy, scheduling retry"
        );
        ExecutionResult::Retrying { attempt }
    }

    async fn resolve_conversation(
        &self,
        job: &CronJob,
        saved_skill: Option<&SavedSkillContext>,
    ) -> Result<String, CronError> {
        match job.execution_mode {
            ExecutionMode::Existing => {
                // A job created without an anchor conversation (the frontend
                // creates "continue-this-conversation" jobs from the cron page
                // before any conversation exists) keeps `conversation_id`
                // absent until the first run. Treat that first run as a new
                // conversation; the service layer then persists the new id
                // back onto the job so subsequent runs reuse it.
                let Some(conversation_id) = job.conversation_id.as_deref() else {
                    return self.create_new_conversation(job, saved_skill).await;
                };
                self.verify_target_conversation_owner(job, conversation_id).await?;
                Ok(conversation_id.to_owned())
            }
            ExecutionMode::NewConversation => self.create_new_conversation(job, saved_skill).await,
        }
    }

    async fn create_new_conversation(
        &self,
        job: &CronJob,
        saved_skill: Option<&SavedSkillContext>,
    ) -> Result<String, CronError> {
        let agent_type = parse_agent_type(&self.agent_registry, &job.agent_type).await;
        let model = resolve_model(job);

        let extra = build_conversation_extra(&self.agent_registry, job, saved_skill).await;

        let req = CreateConversationRequest {
            r#type: agent_type,
            name: Some(job.name.clone()),
            model,
            source: None,
            channel_chat_id: None,
            preset_id: job.agent_config.as_ref().and_then(|config| config.preset_id.clone()),
            preset_overrides: None,
            delegation_policy: Default::default(),
            execution_model_pool: None,
            decision_policy: Default::default(),
            execution_template_id: None,
            extra,
        };

        let response = if let Some(snapshot) = job
            .agent_config
            .as_ref()
            .and_then(|config| config.preset_snapshot.clone())
        {
            self.conversation_service
                .create_from_preset_snapshot(&job.user_id, req, snapshot)
                .await
        } else {
            self.conversation_service.create(&job.user_id, req).await
        }
        .map_err(CronError::from_conversation_create)?;
        // Preserve the canonical conversation entity ID through all cron
        // workspace and persistence boundaries.
        let conversation_id = response.id;

        let response_workspace = response
            .extra
            .get("workspace")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .unwrap_or_default();

        if response_workspace.is_empty() {
            return Err(CronError::Scheduler(format!(
                "new conversation {conversation_id} did not persist a canonical managed workspace"
            )));
        }

        info!(
            job_id = %job.id,
            conversation_id = %conversation_id,
            "Created new conversation for cron job"
        );

        Ok(conversation_id)
    }

    async fn execute_inner(
        &self,
        job: &CronJob,
        conversation_id: &str,
        saved_skill: Option<&SavedSkillContext>,
    ) -> ExecutionResult {
        let build_lease = match self
            .conversation_service
            .begin_public_runtime_build(conversation_id, &job.user_id)
        {
            Ok(lease) => lease,
            Err(error) => {
                return ExecutionResult::Error {
                    message: error.to_string(),
                };
            }
        };
        self.execute_inner_with_lease(job, conversation_id, saved_skill, build_lease)
            .await
    }

    async fn execute_inner_with_lease(
        &self,
        job: &CronJob,
        conversation_id: &str,
        saved_skill: Option<&SavedSkillContext>,
        build_lease: RuntimeBuildLease,
    ) -> ExecutionResult {
        let build_token = build_lease.cancellation_token();
        let agent_type = parse_agent_type(&self.agent_registry, &job.agent_type).await;
        if let Err(error) = build_lease.ensure_active() {
            return ExecutionResult::Error {
                message: error.to_string(),
            };
        }
        // The interactive `send_message` path resolves the model by parsing
        // `conversation.model` via
        // `nomifun_conversation::runtime_options::provider_model_from_conversation_row`.
        // Cron routes through the same helper so that a Nomi job whose
        // cached `agent_config.backend` is a stale vendor label (`"nomi"`)
        // cannot reach the factory and raise `Provider 'nomi' not found`
        // (Sentry ELECTRON-1HM). `resolve_conversation` (called by
        // `execute`/`execute_prepared` before this method runs) guarantees the
        // row exists. Re-check both existence and owner here to close the
        // delete/rebind race before any runtime is obtained.
        let conversation_row = match self.get_conversation_row(conversation_id).await {
            Ok(Some(row)) if row.user_id == job.user_id => row,
            Ok(Some(_)) => {
                return ExecutionResult::Error {
                    message: format!(
                        "conversation {conversation_id} owner does not match cron job {}",
                        job.id
                    ),
                };
            }
            Ok(None) => {
                return ExecutionResult::Error {
                    message: format!("conversation {conversation_id} not found"),
                };
            }
            Err(e) => {
                error!(
                    job_id = %job.id,
                    conversation_id,
                    error = %e,
                    "Failed to load conversation row for cron runtime resolution"
                );
                return ExecutionResult::Error {
                    message: e.to_string(),
                };
            }
        };
        let model = match nomifun_conversation::runtime_options::provider_model_from_conversation_row(
            &conversation_row,
        ) {
            Ok(model) => model,
            Err(error) => {
                error!(job_id = %job.id, conversation_id, error = %error, "Failed to resolve canonical conversation model");
                return ExecutionResult::Error {
                    message: error.to_string(),
                };
            }
        };
        let delegation_policy = match nomifun_conversation::runtime_options::delegation_policy_from_conversation_row(&conversation_row) {
            Ok(policy) => policy,
            Err(error) => {
                error!(
                    job_id = %job.id,
                    conversation_id,
                    error = %error,
                    "Failed to resolve conversation delegation policy for cron runtime"
                );
                return ExecutionResult::Error {
                    message: error.to_string(),
                };
            }
        };
        let row_extra =
            match serde_json::from_str::<serde_json::Value>(&conversation_row.extra) {
                Ok(extra) => extra,
                Err(error) => {
                    return ExecutionResult::Error {
                        message: format!(
                            "conversation {conversation_id} has invalid extra JSON: {error}"
                        ),
                    };
                }
            };
        let managed_workspace = row_extra.get(TEMP_WORKSPACE_ID_EXTRA_KEY).is_some();
        let workspace = if managed_workspace {
            match default_temp_workspace_path(
                &self.work_dir,
                &agent_type,
                job,
                conversation_id,
                &row_extra,
            ) {
                Ok(workspace) => workspace.to_string_lossy().into_owned(),
                Err(error) => {
                    error!(
                        job_id = %job.id,
                        conversation_id,
                        error = %error,
                        "Failed to resolve managed cron workspace"
                    );
                    return ExecutionResult::Error {
                        message: error.to_string(),
                    };
                }
            }
        } else {
            match self.resolve_execution_workspace(job, conversation_id).await {
                Ok(workspace) if !workspace.trim().is_empty() => workspace,
                Ok(_) => {
                    return ExecutionResult::Error {
                        message: format!(
                            "conversation {conversation_id} has neither a custom workspace nor a canonical temp_workspace_id"
                        ),
                    };
                }
                Err(e) => {
                    error!(
                        job_id = %job.id,
                        conversation_id,
                        error = %e,
                        "Failed to resolve cron execution workspace"
                    );
                    return ExecutionResult::Error {
                        message: e.to_string(),
                    };
                }
            }
        };

        let skill_names = if self.controls_host(&job.user_id) {
            match self
                .resolve_task_skill_names(job, conversation_id, saved_skill)
                .await
            {
                Ok(names) => names,
                Err(e) => {
                    error!(job_id = %job.id, error = %e, "Failed to resolve task skills");
                    return ExecutionResult::Error {
                        message: e.to_string(),
                    };
                }
            }
        } else {
            Vec::new()
        };
        let mut build_extra =
            build_task_extra(&self.agent_registry, job, &skill_names).await;
        if managed_workspace {
            let temp_workspace_id = row_extra
                .get(TEMP_WORKSPACE_ID_EXTRA_KEY)
                .cloned()
                .expect("managed workspace marker checked above");
            build_extra[TEMP_WORKSPACE_ID_EXTRA_KEY] = temp_workspace_id;
            // The conversation row is the source of truth for the backend
            // label that was used when its managed workspace was minted.
            // Cron job configuration may be stale after restore/import.
            if let Some(backend) = row_extra.get("backend").and_then(serde_json::Value::as_str) {
                build_extra["backend"] = serde_json::Value::String(backend.to_owned());
            }
        }
        // Resolve this conversation instance's identity (row `created_at`) for
        // nomi session ownership validation; best-effort (None skips it).
        let conversation_created_at = Some(conversation_row.created_at);

        let options = AgentRuntimeBuildOptions {
            user_id: job.user_id.clone(),
            agent_type,
            workspace,
            model,
            conversation_id: conversation_id.to_owned(),
            delegation_policy,
            extra: build_extra,
            conversation_created_at,
        };

        if let Err(error) = build_lease.ensure_active() {
            return ExecutionResult::Error {
                message: error.to_string(),
            };
        }
        let agent = match self
            .runtime_registry
            .get_or_create_runtime_for_turn(
                conversation_id,
                build_lease.id(),
                build_token,
                options,
            )
            .await
        {
            Ok(handle) => handle,
            Err(e) => {
                error!(
                    job_id = %job.id,
                    error = %e,
                    "Failed to get or build Agent runtime"
                );
                return ExecutionResult::Error {
                    message: e.to_string(),
                };
            }
        };
        if build_lease.is_cancelled() {
            let _ = agent.kill(Some(nomifun_common::AgentKillReason::UserCancelled));
            return ExecutionResult::Error {
                message: format!("conversation {conversation_id} cron preparation was cancelled"),
            };
        }

        if let Err(e) = self.ensure_agent_session_mode(job, &agent).await {
            error!(
                job_id = %job.id,
                conversation_id,
                error = %e,
                "Failed to apply cron session mode"
            );
            return ExecutionResult::Error {
                message: e.to_string(),
            };
        }
        if build_lease.is_cancelled() {
            let _ = agent.kill(Some(nomifun_common::AgentKillReason::UserCancelled));
            return ExecutionResult::Error {
                message: format!("conversation {conversation_id} cron preparation was cancelled"),
            };
        }

        // Optionally clear the agent context before this run so a reused
        // conversation does not accumulate model context across ticks. Only
        // meaningful in `Existing` mode (a `NewConversation` run already starts
        // fresh). Visible message records are kept; a failure here is logged
        // but does not abort the run.
        let clear_each_run = job
            .agent_config
            .as_ref()
            .map(|c| c.clear_context_each_run)
            .unwrap_or(false);
        if clear_each_run && matches!(job.execution_mode, ExecutionMode::Existing) {
            match agent.clear_context().await {
                Ok(()) => {
                    info!(job_id = %job.id, conversation_id, "Cleared agent context before cron run")
                }
                Err(e) => warn!(
                    job_id = %job.id,
                    conversation_id,
                    error = %e,
                    "Failed to clear agent context before cron run; continuing with existing context"
                ),
            }
        }
        if build_lease.is_cancelled() {
            let _ = agent.kill(Some(nomifun_common::AgentKillReason::UserCancelled));
            return ExecutionResult::Error {
                message: format!("conversation {conversation_id} cron preparation was cancelled"),
            };
        }

        let prompt = build_prompt(job, saved_skill, self.controls_host(&job.user_id));
        let turn_rx = agent.subscribe();
        // msg_id is generated by ConversationService::send_message — we
        // intentionally do not set it here.
        let send_req = SendMessageRequest {
            content: prompt,
            files: vec![],
            inject_skills: skill_names.clone(),
            hidden: true,
            origin: Some("cron".into()),
            channel_platform: None,
        };

        match self
            .conversation_service
            .send_message_with_runtime_build_lease(
                &job.user_id,
                conversation_id,
                send_req,
                &self.runtime_registry,
                build_lease,
            )
            .await
        {
            Ok(_) => {
                if let Err(e) = self
                    .upsert_cron_trigger_artifact(conversation_id, job)
                    .await
                {
                    warn!(
                        job_id = %job.id,
                        conversation_id,
                        error = %e,
                        "Failed to persist/broadcast cron trigger artifact"
                    );
                }
                if self.controls_host(&job.user_id)
                    && saved_skill.is_none()
                    && matches!(job.execution_mode, ExecutionMode::NewConversation)
                {
                    self.spawn_skill_suggest_flow(
                        agent.clone(),
                        turn_rx,
                        SkillSuggestContext {
                            owner_id: job.user_id.clone(),
                            conversation_id: conversation_id.to_owned(),
                            job_id: job.id.clone(),
                            job_name: job.name.clone(),
                            workspace: agent.workspace().to_owned(),
                            needs_follow_up: false,
                            skill_names: skill_names.clone(),
                        },
                    );
                }
                info!(
                    job_id = %job.id,
                    conversation_id,
                    "Cron job message sent successfully"
                );
                ExecutionResult::Success {
                    conversation_id: conversation_id.to_owned(),
                }
            }
            Err(e) => {
                error!(
                    job_id = %job.id,
                    conversation_id,
                    error = %e,
                    "Failed to send cron job message"
                );
                ExecutionResult::Error {
                    message: e.to_string(),
                }
            }
        }
    }

    async fn verify_target_conversation_owner(
        &self,
        job: &CronJob,
        conversation_id: &str,
    ) -> Result<(), CronError> {
        let Some(row) = self.get_conversation_row(conversation_id).await? else {
            return Err(CronError::Scheduler(format!(
                "conversation {conversation_id} not found"
            )));
        };
        if row.user_id != job.user_id {
            return Err(CronError::Scheduler(format!(
                "conversation {conversation_id} owner does not match cron job {}",
                job.id
            )));
        }
        Ok(())
    }

    async fn upsert_cron_trigger_artifact(
        &self,
        conversation_id: &str,
        job: &CronJob,
    ) -> Result<(), CronError> {
        let created_at = now_ms();
        let row = build_cron_trigger_artifact(conversation_id, job, created_at)?;
        let row = self
            .conversation_repo
            .upsert_artifact(&row)
            .await
            .map_err(CronError::Database)?;
        emit_artifact(self.user_events.as_ref(), &job.user_id, &row)?;

        Ok(())
    }

    pub async fn mark_skill_suggest_artifacts_saved(
        &self,
        owner_id: &str,
        job_id: &str,
    ) -> Result<(), CronError> {
        UserId::try_from(owner_id)
            .map_err(|error| CronError::Scheduler(format!("invalid cron owner id: {error}")))?;
        CronJobId::try_from(job_id)
            .map_err(|error| CronError::Scheduler(format!("invalid cron job id: {error}")))?;
        let rows = self
            .conversation_repo
            .mark_skill_suggest_artifacts_saved(owner_id, job_id, now_ms())
            .await
            .map_err(CronError::Database)?;

        for row in rows {
            emit_artifact(self.user_events.as_ref(), owner_id, &row)?;
        }

        Ok(())
    }

    async fn resolve_execution_workspace_raw(
        &self,
        job: &CronJob,
        conversation_id: Option<&str>,
    ) -> Result<String, CronError> {
        if let Some(workspace) = job
            .agent_config
            .as_ref()
            .and_then(|config| config.workspace.as_deref())
        {
            return Ok(workspace.to_owned());
        }

        let Some(conversation_id) = conversation_id else {
            return Ok(String::new());
        };
        let Some(row) = self.get_conversation_row(conversation_id).await? else {
            return Err(CronError::Scheduler(format!(
                "conversation {conversation_id} not found while resolving cron workspace"
            )));
        };
        if row.user_id != job.user_id {
            return Err(CronError::Scheduler(format!(
                "conversation {conversation_id} owner does not match cron job {}",
                job.id
            )));
        }

        let extra = serde_json::from_str::<serde_json::Value>(&row.extra).unwrap_or_default();
        Ok(extra
            .get("workspace")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned())
    }

    async fn resolve_execution_workspace(
        &self,
        job: &CronJob,
        conversation_id: &str,
    ) -> Result<String, CronError> {
        Ok(self
            .resolve_execution_workspace_raw(job, Some(conversation_id))
            .await?
            .trim()
            .to_owned())
    }

    fn spawn_skill_suggest_flow(
        &self,
        agent: nomifun_ai_agent::AgentRuntimeHandle,
        main_rx: broadcast::Receiver<AgentStreamEvent>,
        ctx: SkillSuggestContext,
    ) {
        let detector = self.skill_suggest_detector.clone();
        let conversation_service = self.conversation_service.clone();
        let runtime_registry = self.runtime_registry.clone();
        let SkillSuggestContext {
            owner_id,
            conversation_id,
            job_id,
            job_name,
            workspace,
            needs_follow_up,
            skill_names,
        } = ctx;

        tokio::spawn(async move {
            if !wait_for_turn_completion(main_rx).await {
                warn!(
                    conversation_id,
                    job_id,
                    "Timed out waiting for cron turn completion before skill suggestion check"
                );
                return;
            }

            if needs_follow_up {
                let follow_up_rx = agent.subscribe();
                let follow_up = SendMessageRequest {
                    content: build_skill_suggest_prompt(&job_name),
                    files: vec![],
                    inject_skills: skill_names,
                    hidden: true,
                    origin: Some("cron".to_owned()),
                    channel_platform: None,
                };

                if let Err(err) = conversation_service
                    .send_message(
                        &owner_id,
                        &conversation_id,
                        follow_up,
                        &runtime_registry,
                    )
                    .await
                {
                    warn!(
                        conversation_id,
                        job_id,
                        error = %err,
                        "Failed to send cron skill suggestion follow-up prompt"
                    );
                    return;
                }

                if !wait_for_turn_completion(follow_up_rx).await {
                    warn!(
                        conversation_id,
                        job_id, "Timed out waiting for cron skill suggestion follow-up completion"
                    );
                    return;
                }
            }

            detector.schedule_check(owner_id, conversation_id, job_id, workspace);
        });
    }

    async fn prepare_saved_skill(
        &self,
        job: &CronJob,
    ) -> Result<Option<SavedSkillContext>, CronError> {
        if let Some(raw_content) = read_skill_content(&self.data_dir, &job.id).await?
            && !raw_content.trim().is_empty()
        {
            return Ok(Some(SavedSkillContext {
                name: cron_skill_name(&job.id)?,
                raw_content,
            }));
        }

        let legacy_content = job
            .skill_content
            .as_deref()
            .map(str::trim)
            .filter(|content| !content.is_empty());

        let Some(legacy_content) = legacy_content else {
            return Ok(None);
        };

        persist_legacy_skill_file(&self.data_dir, job, legacy_content).await?;
        let raw_content = read_skill_content(&self.data_dir, &job.id)
            .await?
            .unwrap_or_else(|| legacy_content.to_owned());

        Ok(Some(SavedSkillContext {
            name: cron_skill_name(&job.id)?,
            raw_content,
        }))
    }

    async fn resolve_task_skill_names(
        &self,
        job: &CronJob,
        conversation_id: &str,
        saved_skill: Option<&SavedSkillContext>,
    ) -> Result<Vec<String>, CronError> {
        let mut skills = match job.execution_mode {
            ExecutionMode::Existing => {
                self.load_conversation_skill_names(job, conversation_id).await?
            }
            ExecutionMode::NewConversation => Vec::new(),
        };

        if matches!(job.execution_mode, ExecutionMode::NewConversation)
            && let Some(saved_skill) = saved_skill
            && !skills.iter().any(|name| name == &saved_skill.name)
        {
            skills.push(saved_skill.name.clone());
        }

        Ok(skills)
    }

    async fn ensure_agent_session_mode(
        &self,
        job: &CronJob,
        agent: &nomifun_ai_agent::AgentRuntimeHandle,
    ) -> Result<(), CronError> {
        let Some(desired_mode) = job
            .agent_config
            .as_ref()
            .and_then(|config| config.mode.as_deref())
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
        else {
            return Ok(());
        };

        let current_mode = agent
            .get_mode()
            .await
            .map_err(|e| CronError::Scheduler(format!("get session mode: {e}")))?;

        if current_mode.mode == desired_mode {
            return Ok(());
        }

        agent.set_mode(desired_mode).await.map_err(|e| {
            CronError::Scheduler(format!("set session mode to {desired_mode}: {e}"))
        })?;

        info!(
            conversation_id = %agent.conversation_id(),
            from_mode = %current_mode.mode,
            to_mode = desired_mode,
            initialized = current_mode.initialized,
            "Applied cron session mode before execution"
        );

        Ok(())
    }

    async fn load_conversation_skill_names(
        &self,
        job: &CronJob,
        conversation_id: &str,
    ) -> Result<Vec<String>, CronError> {
        let Some(row) = self
            .conversation_repo
            .get(parse_conversation_id(conversation_id)?)
            .await
            .map_err(CronError::Database)?
        else {
            return Ok(Vec::new());
        };
        if row.user_id != job.user_id {
            return Err(CronError::Scheduler(format!(
                "conversation {conversation_id} owner does not match cron job {}",
                job.id
            )));
        }

        let Ok(extra) = serde_json::from_str::<serde_json::Value>(&row.extra) else {
            return Ok(Vec::new());
        };

        Ok(extra
            .get("skills")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }
}

async fn wait_for_turn_completion(mut rx: broadcast::Receiver<AgentStreamEvent>) -> bool {
    let fut = async move {
        loop {
            match rx.recv().await {
                Ok(AgentStreamEvent::Finish(_)) | Ok(AgentStreamEvent::Error(_)) => return true,
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Closed) => return true,
                // The skipped item may be the only Finish/Error. Report that
                // completion was not observed instead of waiting for another
                // terminal event while the long-lived sender stays open.
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "Cron turn completion stream lost integrity");
                    return false;
                }
            }
        }
    };

    timeout(SKILL_SUGGEST_TURN_TIMEOUT, fut)
        .await
        .unwrap_or(false)
}

/// Resolve a cron job's stored `agent_type` string into an [`AgentType`].
///
/// Cron persists this field as a free-form string because legacy rows
/// carry a vendor label (e.g. `"claude"`, `"gemini"`) instead of the
/// canonical `"acp"`. Resolution order:
/// 1. ACP vendor lookup via the registry — any builtin ACP row's
///    `backend` aliases to [`AgentType::Acp`]. Checked first so vendor
///    labels that also happen to match a legacy [`AgentType`] variant
///    (e.g. `"gemini"`) are routed to the modern ACP runtime rather
///    than the deprecated standalone adapter.
/// 2. Exact [`AgentType`] serde match.
/// 3. Fallback to [`AgentType::Acp`] to preserve the prior default.
async fn parse_agent_type(registry: &AgentRegistry, agent_type_str: &str) -> AgentType {
    if registry
        .find_builtin_by_backend(agent_type_str)
        .await
        .is_some()
    {
        return AgentType::Acp;
    }

    serde_json::from_value::<AgentType>(serde_json::Value::String(agent_type_str.to_owned()))
        .unwrap_or(AgentType::Acp)
}

/// Only nomi conversations carry meaningful model info in `conversations.model`;
/// ACP and other agent types ignore this field and resolve the model via their own
/// mechanisms (catalog defaults, CLI flags, etc.). Returning `None` lets the
/// `CreateConversationRequest.model` stay `None` for those types, which is the
/// correct semantic.
///
/// For nomi, `agent_config.backend` holds the provider_id (a DB hash, not a
/// vendor label). `CronService::add_job`/`update_job` already rejects nomi
/// jobs lacking a canonical provider ID, so the `None` return here is a
/// defensive check for invalid in-memory values.
fn resolve_model(job: &CronJob) -> Option<ProviderWithModel> {
    if job.agent_type != "nomi" {
        return None;
    }
    let config = job.agent_config.as_ref()?;
    if nomifun_common::ProviderId::try_from(config.backend.as_str()).is_err() {
        return None;
    }
    Some(ProviderWithModel {
        provider_id: config.backend.clone(),
        model: config
            .model_id
            .clone()
            .unwrap_or_else(|| "default".to_owned()),
        use_model: None,
    })
}

/// Fill `extra` with the agent identity the factory should use.
///
/// Preferred path: resolve a builtin ACP catalog row via the
/// registry and emit `agent_id` (exact factory lookup) alongside
/// `backend` (convenience for other consumers). Legacy path: when
/// `agent_config.backend` names something that isn't a builtin ACP
/// vendor (e.g. the bare string `"acp"` that old rows still carry),
/// pass it through unchanged so the factory's agent-type branch can
/// handle it. Same treatment for `agent_type` when there is no
/// `agent_config` but the stored type matches a vendor label.
async fn inject_agent_identity(
    extra: &mut serde_json::Map<String, serde_json::Value>,
    registry: &AgentRegistry,
    job: &CronJob,
) {
    let config_backend = job
        .agent_config
        .as_ref()
        .map(|c| c.backend.trim())
        .filter(|s| !s.is_empty());

    let lookup_label = config_backend.unwrap_or_else(|| job.agent_type.trim());
    if lookup_label.is_empty() {
        return;
    }

    if let Some(meta) = registry.find_builtin_by_backend(lookup_label).await {
        extra.insert(
            "agent_id".to_owned(),
            serde_json::Value::String(meta.id.clone()),
        );
        if let Some(backend) = meta.backend {
            extra.insert("backend".to_owned(), serde_json::Value::String(backend));
        }
        return;
    }

    // No catalog hit — fall through to the legacy raw-label emission
    // so existing rows keep working.
    if let Some(backend) = config_backend {
        extra.insert(
            "backend".to_owned(),
            serde_json::Value::String(backend.to_owned()),
        );
    }
}

/// Inject the cron-configured model into `extra` for ACP (non-nomi) agents.
///
/// ACP agents do **not** read `conversations.model` — `resolve_model`
/// deliberately returns `None` for them. They pick up their model from the
/// session `extra` carrying `current_model_id`, which the `AcpAgentManager`
/// seeds into its desired model and reconciles via `session/set_model` once
/// the session advertises its model catalog.
///
/// nomi is excluded: it resolves its model through the top-level
/// `CreateConversationRequest.model` provider path instead, so emitting
/// `current_model_id` here would be both redundant and off-channel.
fn inject_acp_current_model(extra: &mut serde_json::Map<String, serde_json::Value>, job: &CronJob) {
    if job.agent_type == "nomi" {
        return;
    }
    let Some(config) = &job.agent_config else {
        return;
    };
    let Some(model_id) = config
        .model_id
        .as_ref()
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
    else {
        return;
    };
    extra.insert(
        "current_model_id".to_owned(),
        serde_json::Value::String(model_id.to_owned()),
    );
}

async fn build_task_extra(
    registry: &AgentRegistry,
    job: &CronJob,
    skills: &[String],
) -> serde_json::Value {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "cron_job_id".to_owned(),
        serde_json::Value::String(job.id.clone()),
    );
    if !skills.is_empty() {
        extra.insert(
            "skills".to_owned(),
            serde_json::Value::Array(
                skills
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }

    inject_agent_identity(&mut extra, registry, job).await;
    inject_acp_current_model(&mut extra, job);

    if let Some(config) = &job.agent_config {
        if let Some(cli_path) = &config.cli_path {
            extra.insert(
                "cli_path".to_owned(),
                serde_json::Value::String(cli_path.clone()),
            );
        }
        if !config.name.is_empty() {
            extra.insert(
                "agent_name".to_owned(),
                serde_json::Value::String(config.name.clone()),
            );
        }
        if let Some(custom_agent_id) = &config.custom_agent_id {
            extra.insert(
                "custom_agent_id".to_owned(),
                serde_json::Value::String(custom_agent_id.clone()),
            );
        }
        if let Some(preset_id) = &config.preset_id {
            extra.insert("preset_id".to_owned(), serde_json::Value::String(preset_id.clone()));
        }
        if let Some(revision) = config.preset_revision {
            extra.insert("preset_revision".to_owned(), serde_json::Value::Number(revision.into()));
        }
        if let Some(snapshot) = &config.preset_snapshot {
            if let Ok(value) = serde_json::to_value(snapshot) {
                extra.insert("preset_snapshot".to_owned(), value);
            }
        }
        if let Some(mode) = &config.mode {
            extra.insert(
                "session_mode".to_owned(),
                serde_json::Value::String(mode.clone()),
            );
        }
    }

    serde_json::Value::Object(extra)
}

fn build_prompt(
    job: &CronJob,
    saved_skill: Option<&SavedSkillContext>,
    allow_skill_suggest: bool,
) -> String {
    let schedule_desc = schedule_description_text(&job.schedule);

    match job.execution_mode {
        ExecutionMode::Existing => {
            build_existing_conversation_prompt(&job.name, &schedule_desc, &job.message)
        }
        ExecutionMode::NewConversation => {
            if saved_skill.is_some() {
                build_new_conversation_with_skill_prompt(&job.name, &job.message)
            } else if allow_skill_suggest {
                build_new_conversation_prompt_with_skill_suggest(
                    &job.name,
                    &schedule_desc,
                    &job.message,
                )
            } else {
                build_new_conversation_prompt(&job.name, &schedule_desc, &job.message)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SavedSkillContext {
    name: String,
    raw_content: String,
}

async fn build_conversation_extra(
    registry: &AgentRegistry,
    job: &CronJob,
    saved_skill: Option<&SavedSkillContext>,
) -> serde_json::Value {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "cron_job_id".to_owned(),
        serde_json::Value::String(job.id.clone()),
    );
    extra.insert(
        "exclude_auto_inject_skills".to_owned(),
        serde_json::Value::Array(vec![serde_json::Value::String("cron".to_owned())]),
    );

    if let Some(saved_skill) = saved_skill {
        extra.insert(
            "preset_enabled_skills".to_owned(),
            serde_json::Value::Array(vec![serde_json::Value::String(saved_skill.name.clone())]),
        );
    }

    inject_agent_identity(&mut extra, registry, job).await;
    inject_acp_current_model(&mut extra, job);

    if let Some(config) = &job.agent_config {
        if let Some(cli_path) = &config.cli_path {
            extra.insert(
                "cli_path".to_owned(),
                serde_json::Value::String(cli_path.clone()),
            );
        }
        if !config.name.is_empty() {
            extra.insert(
                "agent_name".to_owned(),
                serde_json::Value::String(config.name.clone()),
            );
        }
        if let Some(custom_agent_id) = &config.custom_agent_id {
            extra.insert(
                "custom_agent_id".to_owned(),
                serde_json::Value::String(custom_agent_id.clone()),
            );
        }
        if let Some(preset_id) = &config.preset_id {
            extra.insert("preset_id".to_owned(), serde_json::Value::String(preset_id.clone()));
        }
        if let Some(revision) = config.preset_revision {
            extra.insert("preset_revision".to_owned(), serde_json::Value::Number(revision.into()));
        }
        if let Some(snapshot) = &config.preset_snapshot {
            if let Ok(value) = serde_json::to_value(snapshot) {
                extra.insert("preset_snapshot".to_owned(), value);
            }
        }
        if let Some(mode) = &config.mode {
            extra.insert(
                "session_mode".to_owned(),
                serde_json::Value::String(mode.clone()),
            );
        }
        if let Some(workspace) = &config.workspace
            && !workspace.trim().is_empty()
        {
            extra.insert(
                "workspace".to_owned(),
                serde_json::Value::String(workspace.clone()),
            );
        }
    }

    serde_json::Value::Object(extra)
}

fn schedule_description_text(schedule: &crate::types::CronSchedule) -> String {
    match schedule {
        crate::types::CronSchedule::At { at_ms, description } => {
            description.clone().unwrap_or_else(|| format!("At {at_ms}"))
        }
        crate::types::CronSchedule::Every {
            every_ms,
            description,
        } => description
            .clone()
            .unwrap_or_else(|| format!("Every {every_ms} ms")),
        crate::types::CronSchedule::Cron {
            expr,
            tz,
            description,
        } => description.clone().unwrap_or_else(|| match tz {
            Some(tz) => format!("{expr} ({tz})"),
            None => expr.clone(),
        }),
    }
}

fn default_temp_workspace_path(
    data_dir: &std::path::Path,
    agent_type: &AgentType,
    job: &CronJob,
    conversation_id: &str,
    extra: &serde_json::Value,
) -> Result<std::path::PathBuf, CronError> {
    let label = if *agent_type == AgentType::Acp {
        extra
            .get("backend")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|backend| !backend.is_empty())
            .or_else(|| {
                job.agent_config
                    .as_ref()
                    .map(|config| config.backend.trim())
                    .filter(|backend| !backend.is_empty())
            })
            .unwrap_or("acp")
            .to_owned()
    } else {
        agent_type.serde_name().to_owned()
    };

    let temp_workspace_id = extra
        .get(TEMP_WORKSPACE_ID_EXTRA_KEY)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CronError::Scheduler(format!(
                "conversation {conversation_id} has no canonical temp_workspace_id"
            ))
        })?;
    validate_prefixed_id(temp_workspace_id, "ws").map_err(|error| {
        CronError::Scheduler(format!(
            "conversation {conversation_id} has invalid temp_workspace_id '{temp_workspace_id}': {error}"
        ))
    })?;

    Ok(data_dir
        .join("conversations")
        .join(format!("{label}-temp-{temp_workspace_id}")))
}

fn schedule_description_ref(schedule: &crate::types::CronSchedule) -> Option<&str> {
    match schedule {
        crate::types::CronSchedule::At { description, .. }
        | crate::types::CronSchedule::Every { description, .. }
        | crate::types::CronSchedule::Cron { description, .. } => description.as_deref(),
    }
}

async fn persist_legacy_skill_file(
    data_dir: &Path,
    job: &CronJob,
    raw_content: &str,
) -> Result<(), CronError> {
    match write_raw_skill_file(data_dir, &job.id, raw_content).await {
        Ok(_) => Ok(()),
        Err(CronError::InvalidSkillContent(_)) => {
            let description = job
                .description
                .clone()
                .unwrap_or_else(|| format!("Saved cron skill for {}", job.name));
            write_skill_file(
                data_dir,
                &job.id,
                &job.name,
                &description,
                raw_content.trim(),
                schedule_description_ref(&job.schedule),
            )
            .await
            .map(|_| ())
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CreatedBy, CronAgentConfig, CronSchedule};
    use nomifun_ai_agent::runtime_handle::{AgentRuntimeHandle, AgentRuntimeControl, MockAgentRuntime};
    use nomifun_ai_agent::protocol::events::FinishEventData;
    use nomifun_api_types::{AgentModeResponse, WebSocketMessage};
    use nomifun_common::{AgentKillReason, ConversationStatus, PaginatedResult, TimestampMs};
    use nomifun_db::{
        ConversationArtifactRow, ConversationFilters, ConversationRowUpdate, MessageRowUpdate,
        MessageSearchRow, SortOrder,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::{RwLock, broadcast};

    const PROVIDER_ID: &str = "prov_0190f5fe-7c00-7a00-8000-000000000001";
    const JOB_ID: &str = "cron_0190f5fe-7c00-7a00-8000-000000000001";
    const USER_ID: &str = "user_0190f5fe-7c00-7a00-8000-000000000001";
    const JOB_SKILL_NAME: &str = "cron-cron_0190f5fe-7c00-7a00-8000-000000000001";

    fn sample_job() -> CronJob {
        CronJob {
            id: JOB_ID.into(),
            user_id: USER_ID.into(),
            name: "Test Job".into(),
            enabled: true,
            schedule: CronSchedule::Every {
                every_ms: 60000,
                description: None,
            },
            message: "do something".into(),
            execution_mode: ExecutionMode::Existing,
            agent_config: Some(CronAgentConfig {
                backend: "acp".into(),
                name: "Claude".into(),
                cli_path: Some("/usr/bin/claude".into()),
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model_id: Some("claude-sonnet-4".into()),
                config_options: None,
                workspace: Some("/home/user/project".into()),
                clear_context_each_run: false,
            }),
            conversation_id: Some("conv_0190f5fe-7c00-7a00-8abc-012345678901".into()),
            conversation_title: Some("Test Conv".into()),
            agent_type: "acp".into(),
            created_by: CreatedBy::User,
            skill_content: None,
            description: None,
            created_at: 1000,
            updated_at: 2000,
            next_run_at: Some(3000),
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
        }
    }

    async fn wait_for_agent_send(agent: &RecordingAgent, expected_calls: usize) {
        timeout(std::time::Duration::from_secs(1), async {
            loop {
                if agent.send_calls() >= expected_calls {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("agent send should complete");
    }

    #[test]
    fn default_temp_workspace_path_uses_backend_minted_token() {
        let job = sample_job();
        let path = default_temp_workspace_path(
            Path::new("/work"),
            &AgentType::Acp,
            &job,
            "conv_0190f5fe-7c00-7a00-8abc-012345678901",
            &serde_json::json!({
                "temp_workspace_id": "ws_0190f5fe-7c00-7a00-8abc-012345678901"
            }),
        )
        .unwrap();

        assert_eq!(
            path,
            Path::new("/work")
                .join("conversations")
                .join("acp-temp-ws_0190f5fe-7c00-7a00-8abc-012345678901")
        );
    }

    #[test]
    fn default_temp_workspace_path_missing_or_malformed_token_fails_closed() {
        let job = sample_job();
        for extra in [
            serde_json::json!({}),
            serde_json::json!({ "temp_workspace_id": "ws_abc" }),
            serde_json::json!({ "temp_workspace_id": 7 }),
        ] {
            let result = default_temp_workspace_path(
                Path::new("/work"),
                &AgentType::Acp,
                &job,
                "conv_0190f5fe-7c00-7a00-8abc-012345678901",
                &extra,
            );
            assert!(result.is_err());
        }
    }

    // -- handle_busy tests ---------------------------------------------------

    #[tokio::test]
    async fn handle_busy_returns_retrying_when_under_limit() {
        let guard = CronBusyGuard::new();
        let executor = make_executor_for_busy_tests(Arc::new(guard));

        let job = CronJob {
            retry_count: 1,
            max_retries: 3,
            ..sample_job()
        };
        let result = executor.handle_busy(&job);
        assert_eq!(result, ExecutionResult::Retrying { attempt: 2 });
    }

    #[tokio::test]
    async fn handle_busy_returns_skipped_when_at_limit() {
        let guard = CronBusyGuard::new();
        let executor = make_executor_for_busy_tests(Arc::new(guard));

        let job = CronJob {
            retry_count: 3,
            max_retries: 3,
            ..sample_job()
        };
        let result = executor.handle_busy(&job);
        assert_eq!(result, ExecutionResult::Skipped);
    }

    #[tokio::test]
    async fn handle_busy_returns_skipped_when_over_limit() {
        let guard = CronBusyGuard::new();
        let executor = make_executor_for_busy_tests(Arc::new(guard));

        let job = CronJob {
            retry_count: 5,
            max_retries: 3,
            ..sample_job()
        };
        let result = executor.handle_busy(&job);
        assert_eq!(result, ExecutionResult::Skipped);
    }

    #[tokio::test]
    async fn handle_busy_first_retry_returns_attempt_1() {
        let guard = CronBusyGuard::new();
        let executor = make_executor_for_busy_tests(Arc::new(guard));

        let job = CronJob {
            retry_count: 0,
            max_retries: 3,
            ..sample_job()
        };
        let result = executor.handle_busy(&job);
        assert_eq!(result, ExecutionResult::Retrying { attempt: 1 });
    }

    // -- build_prompt tests --------------------------------------------------

    #[test]
    fn build_prompt_existing_mode_no_skill() {
        let job = sample_job();
        let prompt = build_prompt(&job, None, true);
        assert!(prompt.contains("[Scheduled Task Execution]"));
        assert!(prompt.contains("Task instruction:\ndo something"));
    }

    #[test]
    fn build_prompt_existing_mode_with_skill_does_not_append_saved_skill() {
        let job = sample_job();
        let prompt = build_prompt(
            &job,
            Some(&SavedSkillContext {
                name: JOB_SKILL_NAME.into(),
                raw_content: "---\nname: test\ndescription: desc\n---\nDo X".into(),
            }),
            true,
        );
        assert!(prompt.contains("[Scheduled Task Execution]"));
        assert!(!prompt.contains("## Skill Instructions"));
        assert!(!prompt.contains("Do X"));
    }

    #[test]
    fn build_prompt_new_conv_with_skill() {
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        let prompt = build_prompt(
            &job,
            Some(&SavedSkillContext {
                name: JOB_SKILL_NAME.into(),
                raw_content: "---\nname: test\ndescription: desc\n---\nDo X".into(),
            }),
            true,
        );
        assert!(prompt.contains("A skill file with detailed instructions has been loaded"));
        assert!(prompt.contains("do something"));
    }

    #[test]
    fn build_prompt_new_conv_no_skill() {
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        let prompt = build_prompt(&job, None, true);
        assert!(prompt.contains("create a file named \"SKILL_SUGGEST.md\""));
    }

    #[test]
    fn build_prompt_new_conv_empty_skill() {
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        let prompt = build_prompt(&job, None, true);
        assert!(prompt.contains("SKILL_SUGGEST.md"));
    }

    #[test]
    fn build_prompt_model_only_new_conversation_never_requests_host_file() {
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        let prompt = build_prompt(&job, None, false);
        assert!(prompt.contains("[Scheduled Task Context]"));
        assert!(prompt.contains("do something"));
        assert!(!prompt.contains("SKILL_SUGGEST.md"));
        assert!(!prompt.contains("create a file"));
    }

    // -- registry helper ------------------------------------------------------

    /// Build a registry backed by an in-memory DB seeded from the
    /// production migrations, so backend-lookup tests exercise the
    /// same catalog rows the server would see at runtime.
    async fn hydrated_registry() -> Arc<AgentRegistry> {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let repo = Arc::new(nomifun_db::SqliteAgentMetadataRepository::new(
            db.pool().clone(),
        ));
        let registry = AgentRegistry::new(repo);
        registry.hydrate().await.unwrap();
        registry
    }

    // -- parse_agent_type tests -----------------------------------------------

    #[tokio::test]
    async fn parse_agent_type_known_types() {
        let registry = hydrated_registry().await;
        assert_eq!(parse_agent_type(&registry, "acp").await, AgentType::Acp);
        assert_eq!(
            parse_agent_type(&registry, "nanobot").await,
            AgentType::Nanobot
        );
    }

    #[tokio::test]
    async fn parse_agent_type_acp_backend_aliases_to_acp() {
        let registry = hydrated_registry().await;
        assert_eq!(parse_agent_type(&registry, "claude").await, AgentType::Acp);
        assert_eq!(parse_agent_type(&registry, "gemini").await, AgentType::Acp);
        assert_eq!(parse_agent_type(&registry, "qwen").await, AgentType::Acp);
        assert_eq!(parse_agent_type(&registry, "codex").await, AgentType::Acp);
    }

    #[tokio::test]
    async fn parse_agent_type_unknown_defaults_to_acp() {
        let registry = hydrated_registry().await;
        assert_eq!(
            parse_agent_type(&registry, "unknown_type").await,
            AgentType::Acp
        );
    }

    // -- resolve_model tests -------------------------------------------------

    #[test]
    fn resolve_model_returns_none_for_acp() {
        // Model info only applies to nomi; ACP ignores it.
        let job = sample_job();
        assert!(resolve_model(&job).is_none());
    }

    #[test]
    fn resolve_model_returns_none_for_acp_without_config() {
        let job = CronJob {
            agent_config: None,
            ..sample_job()
        };
        assert!(resolve_model(&job).is_none());
    }

    #[test]
    fn resolve_model_returns_none_for_non_nomi_type() {
        let job = CronJob {
            agent_type: "claude".into(),
            ..sample_job()
        };
        assert!(resolve_model(&job).is_none());
    }

    #[test]
    fn resolve_model_nomi_with_full_config() {
        let job = CronJob {
            agent_type: "nomi".into(),
            agent_config: Some(CronAgentConfig {
                backend: PROVIDER_ID.into(),
                name: "OpenAI".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model_id: Some("gpt-5".into()),
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };
        let model = resolve_model(&job).expect("nomi + full config returns Some");
        assert_eq!(model.provider_id, PROVIDER_ID);
        assert_eq!(model.model, "gpt-5");
    }

    #[test]
    fn resolve_model_nomi_without_model_id_defaults_to_default() {
        let job = CronJob {
            agent_type: "nomi".into(),
            agent_config: Some(CronAgentConfig {
                backend: PROVIDER_ID.into(),
                name: "OpenAI".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model_id: None,
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };
        let model = resolve_model(&job).expect("nomi without model_id still returns Some");
        assert_eq!(model.provider_id, PROVIDER_ID);
        assert_eq!(model.model, "default");
    }

    #[test]
    fn resolve_model_nomi_without_config_returns_none() {
        // Defensive: `add_job` rejects this shape, but resolve_model must not
        // fabricate a provider_id from the agent_type like the old code did.
        let job = CronJob {
            agent_type: "nomi".into(),
            agent_config: None,
            ..sample_job()
        };
        assert!(resolve_model(&job).is_none());
    }

    #[test]
    fn resolve_model_nomi_with_empty_backend_returns_none() {
        let job = CronJob {
            agent_type: "nomi".into(),
            agent_config: Some(CronAgentConfig {
                backend: "   ".into(),
                name: "Bogus".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model_id: Some("gpt-5".into()),
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };
        assert!(resolve_model(&job).is_none());
    }

    // -- build_task_extra tests -----------------------------------------------

    #[tokio::test]
    async fn build_task_extra_includes_cron_job_id() {
        let registry = hydrated_registry().await;
        let job = sample_job();
        let extra = build_task_extra(&registry, &job, &[]).await;
        assert_eq!(extra["cron_job_id"], JOB_ID);
    }

    #[tokio::test]
    async fn build_task_extra_with_config_fields() {
        let registry = hydrated_registry().await;
        let job = sample_job();
        let extra = build_task_extra(&registry, &job, &[JOB_SKILL_NAME.into()]).await;
        assert_eq!(extra["backend"], "acp");
        assert_eq!(extra["cli_path"], "/usr/bin/claude");
        assert_eq!(extra["agent_name"], "Claude");
        assert_eq!(extra["skills"], serde_json::json!([JOB_SKILL_NAME]));
    }

    #[tokio::test]
    async fn build_task_extra_without_config() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            agent_config: None,
            ..sample_job()
        };
        let extra = build_task_extra(&registry, &job, &[]).await;
        assert_eq!(extra["cron_job_id"], JOB_ID);
        assert!(extra.get("backend").is_none());
    }

    #[tokio::test]
    async fn build_task_extra_falls_back_to_agent_type_for_acp_backend() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            agent_type: "claude".into(),
            agent_config: None,
            ..sample_job()
        };
        let extra = build_task_extra(&registry, &job, &[]).await;
        assert_eq!(extra["backend"], "claude");
        // Vendor label must resolve to a catalog row so the factory can
        // skip the `find_builtin_by_backend` fallback.
        assert!(extra.get("agent_id").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn build_task_extra_injects_current_model_id_for_acp() {
        // ACP agents resolve their model via the session `extra` carrying
        // `current_model_id`, mirroring the Agent execution path. The
        // configured `agent_config.model_id` must reach the session.
        let registry = hydrated_registry().await;
        let job = CronJob {
            agent_type: "claude".into(),
            agent_config: Some(CronAgentConfig {
                backend: "claude".into(),
                name: "Claude".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model_id: Some("claude-sonnet-4-6".into()),
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };
        let extra = build_task_extra(&registry, &job, &[]).await;
        assert_eq!(extra["current_model_id"], "claude-sonnet-4-6");
    }

    #[tokio::test]
    async fn build_task_extra_omits_current_model_id_for_nomi() {
        // nomi resolves model via the top-level conversation.model provider
        // path, never `current_model_id`. The ACP injection must not bleed
        // into the nomi branch.
        let registry = hydrated_registry().await;
        let job = CronJob {
            agent_type: "nomi".into(),
            agent_config: Some(CronAgentConfig {
                backend: PROVIDER_ID.into(),
                name: "OpenAI".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model_id: Some("gpt-5".into()),
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };
        let extra = build_task_extra(&registry, &job, &[]).await;
        assert!(extra.get("current_model_id").is_none());
    }

    #[tokio::test]
    async fn build_conversation_extra_without_saved_skill_excludes_cron_auto_inject_only() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let extra = build_conversation_extra(&registry, &job, None).await;

        assert_eq!(extra["cron_job_id"], JOB_ID);
        assert_eq!(
            extra["exclude_auto_inject_skills"],
            serde_json::json!(["cron"])
        );
        assert!(extra.get("preset_enabled_skills").is_none());
    }

    #[tokio::test]
    async fn build_conversation_extra_with_saved_skill_enables_preset_skill() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        let saved_skill = SavedSkillContext {
            name: JOB_SKILL_NAME.into(),
            raw_content: "---\nname: test\ndescription: desc\n---\nDo X".into(),
        };

        let extra = build_conversation_extra(&registry, &job, Some(&saved_skill)).await;

        assert_eq!(
            extra["exclude_auto_inject_skills"],
            serde_json::json!(["cron"])
        );
        assert_eq!(
            extra["preset_enabled_skills"],
            serde_json::json!([JOB_SKILL_NAME])
        );
    }

    #[tokio::test]
    async fn build_conversation_extra_preserves_agent_workspace() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let extra = build_conversation_extra(&registry, &job, None).await;

        assert_eq!(extra["workspace"], "/home/user/project");
    }

    #[tokio::test]
    async fn build_conversation_extra_falls_back_to_agent_type_for_acp_backend() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            agent_type: "claude".into(),
            agent_config: None,
            ..sample_job()
        };

        let extra = build_conversation_extra(&registry, &job, None).await;

        assert_eq!(extra["backend"], "claude");
    }

    #[tokio::test]
    async fn build_conversation_extra_injects_current_model_id_for_acp() {
        // ACP agents pick up the configured model from the session `extra`
        // via `current_model_id` (the same channel interactive Agent sessions
        // use). Without this the cron-configured model is silently dropped.
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            agent_type: "claude".into(),
            agent_config: Some(CronAgentConfig {
                backend: "claude".into(),
                name: "Claude".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model_id: Some("claude-sonnet-4-6".into()),
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };

        let extra = build_conversation_extra(&registry, &job, None).await;

        assert_eq!(extra["current_model_id"], "claude-sonnet-4-6");
    }

    #[tokio::test]
    async fn build_conversation_extra_omits_current_model_id_for_nomi() {
        // nomi must keep resolving its model through the top-level
        // conversation.model provider path, never `current_model_id`.
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            agent_type: "nomi".into(),
            agent_config: Some(CronAgentConfig {
                backend: PROVIDER_ID.into(),
                name: "OpenAI".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model_id: Some("gpt-5".into()),
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };

        let extra = build_conversation_extra(&registry, &job, None).await;

        assert!(extra.get("current_model_id").is_none());
    }

    #[tokio::test]
    async fn build_conversation_extra_omits_current_model_id_when_model_unset() {
        // An ACP job without a configured model must not emit an empty
        // `current_model_id`; the agent falls back to its own default.
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            agent_type: "claude".into(),
            agent_config: Some(CronAgentConfig {
                backend: "claude".into(),
                name: "Claude".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model_id: None,
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };

        let extra = build_conversation_extra(&registry, &job, None).await;

        assert!(extra.get("current_model_id").is_none());
    }

    // -- execution_result display ---------------------------------------------

    #[test]
    fn execution_result_variants() {
        let success = ExecutionResult::Success {
            conversation_id: "conv_0190f5fe-7c00-7a00-8abc-012345678901".into(),
        };
        assert_eq!(
            success,
            ExecutionResult::Success {
                conversation_id: "conv_0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );

        let retrying = ExecutionResult::Retrying { attempt: 2 };
        assert_eq!(retrying, ExecutionResult::Retrying { attempt: 2 });

        assert_eq!(ExecutionResult::Skipped, ExecutionResult::Skipped);

        let error = ExecutionResult::Error {
            message: "oops".into(),
        };
        assert_eq!(
            error,
            ExecutionResult::Error {
                message: "oops".into()
            }
        );
    }

    #[tokio::test]
    async fn execute_inner_applies_desired_session_mode_before_sending() {
        let agent = Arc::new(RecordingAgent::new("conv_0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let executor = make_executor_with_agent(AgentRuntimeHandle::Mock(agent.clone()));
        let mut job = sample_job();
        job.agent_config.as_mut().unwrap().mode = Some("yolo".into());

        let result = executor.execute_inner(&job, "conv_0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        assert_eq!(agent.mode().await, "yolo");
        assert_eq!(agent.set_mode_calls(), 1);
        assert_eq!(agent.send_calls(), 1);
    }

    #[tokio::test]
    async fn execute_inner_applies_mode_even_for_uninitialized_agent() {
        let agent = Arc::new(RecordingAgent::new("conv_0190f5fe-7c00-7a00-8abc-012345678901", "default", false));
        let executor = make_executor_with_agent(AgentRuntimeHandle::Mock(agent.clone()));
        let mut job = sample_job();
        job.agent_config.as_mut().unwrap().mode = Some("yolo".into());

        let result = executor.execute_inner(&job, "conv_0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        assert_eq!(agent.mode().await, "yolo");
        assert_eq!(agent.set_mode_calls(), 1);
        assert_eq!(agent.send_calls(), 1);
    }

    #[tokio::test]
    async fn execute_inner_skips_mode_update_when_already_matching() {
        let agent = Arc::new(RecordingAgent::new("conv_0190f5fe-7c00-7a00-8abc-012345678901", "yolo", true));
        let executor = make_executor_with_agent(AgentRuntimeHandle::Mock(agent.clone()));
        let mut job = sample_job();
        job.agent_config.as_mut().unwrap().mode = Some("yolo".into());

        let result = executor.execute_inner(&job, "conv_0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        assert_eq!(agent.mode().await, "yolo");
        assert_eq!(agent.set_mode_calls(), 0);
        assert_eq!(agent.send_calls(), 1);
    }

    #[tokio::test]
    async fn execute_inner_new_conversation_without_saved_skill_requests_skill_suggest() {
        let agent = Arc::new(RecordingAgent::new("conv_0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(AgentRuntimeHandle::Mock(
            agent.clone(),
        )));
        let executor = make_executor_with_runtime_registry(runtime_registry.clone());
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let result = executor.execute_inner(&job, "conv_0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        let sent_messages = agent.sent_messages().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(
            sent_messages[0]
                .content
                .contains("create a file named \"SKILL_SUGGEST.md\"")
        );
        assert!(sent_messages[0].inject_skills.is_empty());

        let options = runtime_registry
            .last_options()
            .expect("runtime registry should capture build options");
        assert!(
            options
                .extra
                .get("skills")
                .and_then(|value| value.as_array())
                .is_none()
        );
    }

    #[tokio::test]
    async fn execute_inner_new_conversation_with_saved_skill_injects_saved_skill() {
        let agent = Arc::new(RecordingAgent::new("conv_0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(AgentRuntimeHandle::Mock(
            agent.clone(),
        )));
        let executor = make_executor_with_runtime_registry(runtime_registry.clone());
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        let saved_skill = SavedSkillContext {
            name: JOB_SKILL_NAME.into(),
            raw_content: "---\nname: test\ndescription: desc\n---\nDo X".into(),
        };

        let result = executor.execute_inner(&job, "conv_0190f5fe-7c00-7a00-8abc-012345678901", Some(&saved_skill)).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        let sent_messages = agent.sent_messages().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(
            sent_messages[0]
                .content
                .contains("A skill file with detailed instructions has been loaded")
        );
        assert!(!sent_messages[0].content.contains("SKILL_SUGGEST.md"));
        assert_eq!(
            sent_messages[0].inject_skills,
            vec![JOB_SKILL_NAME.to_owned()]
        );

        let options = runtime_registry
            .recorded_options()
            .into_iter()
            .next()
            .expect("runtime registry should capture build options");
        assert_eq!(
            options.extra["skills"],
            serde_json::json!([JOB_SKILL_NAME])
        );
    }

    #[tokio::test]
    async fn execute_inner_existing_with_saved_skill_keeps_saved_skill_out_of_prompt_and_turn() {
        let agent = Arc::new(RecordingAgent::new("conv_0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let executor = make_executor_with_agent(AgentRuntimeHandle::Mock(agent.clone()));
        let job = sample_job();
        let saved_skill = SavedSkillContext {
            name: JOB_SKILL_NAME.into(),
            raw_content: "---\nname: test\ndescription: desc\n---\nDo X".into(),
        };

        let result = executor.execute_inner(&job, "conv_0190f5fe-7c00-7a00-8abc-012345678901", Some(&saved_skill)).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        let sent_messages = agent.sent_messages().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(!sent_messages[0].content.contains("## Skill Instructions"));
        assert!(!sent_messages[0].content.contains("Do X"));
        assert!(sent_messages[0].inject_skills.is_empty());
    }

    #[tokio::test]
    async fn execute_inner_existing_without_saved_skill_does_not_send_skill_suggest_follow_up() {
        let agent = Arc::new(RecordingAgent::new("conv_0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let executor = make_executor_with_agent(AgentRuntimeHandle::Mock(agent.clone()));
        let job = sample_job();

        let result = executor.execute_inner(&job, "conv_0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;

        let _ = agent
            .event_tx
            .send(AgentStreamEvent::Finish(FinishEventData::default()));
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            agent.send_calls(),
            1,
            "existing-mode cron should not send a follow-up SKILL_SUGGEST prompt"
        );
    }

    #[tokio::test]
    async fn execute_inner_uses_conversation_workspace_when_job_workspace_missing() {
        let agent = Arc::new(RecordingAgent::new("conv_0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(AgentRuntimeHandle::Mock(
            agent.clone(),
        )));
        let executor = make_executor_with_runtime_registry(runtime_registry.clone());
        let mut job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        job.agent_config.as_mut().unwrap().workspace = None;

        let result = executor.execute_inner(&job, "conv_0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        let options = runtime_registry
            .last_options()
            .expect("runtime registry should capture build options");
        assert_eq!(options.workspace, "/tmp/existing-conversation-workspace");
    }

    #[tokio::test]
    async fn execute_inner_missing_workspace_identity_fails_closed() {
        let agent = Arc::new(RecordingAgent::new("conv_0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(AgentRuntimeHandle::Mock(
            agent.clone(),
        )));
        let repo = Arc::new(MissingWorkspaceConversationRepo::new(
            "conv_0190f5fe-7c00-7a00-8abc-012345678901",
            serde_json::json!({}),
        ));
        let executor = make_executor_with_runtime_registry_and_repo(runtime_registry.clone(), repo.clone());
        let mut job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        job.agent_config.as_mut().unwrap().workspace = None;

        let result = executor.execute_inner(&job, "conv_0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert!(matches!(
            result,
            ExecutionResult::Error { message }
                if message.contains("neither a custom workspace nor a canonical temp_workspace_id")
        ));
        assert_eq!(agent.send_calls(), 0);
        assert!(runtime_registry.last_options().is_none());
        assert!(repo.last_update_with_extra().is_none());
    }

    #[tokio::test]
    async fn execute_inner_rebases_managed_workspace_and_ignores_source_absolute_path() {
        const WORKSPACE_ID: &str =
            "ws_0190f5fe-7c00-7a00-8abc-012345678901";
        let agent = Arc::new(RecordingAgent::new(
            "conv_0190f5fe-7c00-7a00-8abc-012345678901",
            "default",
            true,
        ));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(
            AgentRuntimeHandle::Mock(agent.clone()),
        ));
        let repo = Arc::new(MissingWorkspaceConversationRepo::new(
            "conv_0190f5fe-7c00-7a00-8abc-012345678901",
            serde_json::json!({
                "backend": "claude",
                "temp_workspace_id": WORKSPACE_ID,
                "workspace": "/source-install/conversations/claude-temp-old"
            }),
        ));
        let executor = make_executor_with_runtime_registry_and_repo(
            runtime_registry.clone(),
            repo,
        );
        let mut job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        job.agent_config.as_mut().unwrap().workspace = None;

        let result = executor
            .execute_inner(
                &job,
                "conv_0190f5fe-7c00-7a00-8abc-012345678901",
                None,
            )
            .await;

        assert!(matches!(result, ExecutionResult::Success { .. }));
        wait_for_agent_send(&agent, 1).await;
        let options = runtime_registry
            .last_options()
            .expect("runtime registry should capture build options");
        let expected = std::env::temp_dir()
            .join("conversations")
            .join(format!("claude-temp-{WORKSPACE_ID}"))
            .to_string_lossy()
            .into_owned();
        assert_eq!(options.workspace, expected);
        assert_eq!(options.extra["temp_workspace_id"], WORKSPACE_ID);
        assert_ne!(
            options.workspace,
            "/source-install/conversations/claude-temp-old"
        );
    }

    #[tokio::test]
    async fn execute_inner_inserts_right_side_user_message_for_cron_prompt() {
        let agent = Arc::new(RecordingAgent::new("conv_0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(AgentRuntimeHandle::Mock(
            agent.clone(),
        )));
        let repo = Arc::new(MissingWorkspaceConversationRepo::new(
            "conv_0190f5fe-7c00-7a00-8abc-012345678901",
            serde_json::json!({ "workspace": "/tmp/existing-conversation-workspace" }),
        ));
        let executor = make_executor_with_runtime_registry_and_repo(runtime_registry, repo.clone());
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let result = executor.execute_inner(&job, "conv_0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;

        let messages = repo.inserted_messages();
        assert!(
            !messages.is_empty(),
            "cron execution should insert a user message"
        );
        let right_message = messages
            .iter()
            .find(|message| message.position.as_deref() == Some("right"))
            .expect("cron execution should insert a right-side prompt message");
        assert_eq!(right_message.r#type, "text");
        assert!(right_message.hidden);
        assert!(right_message.content.contains("SKILL_SUGGEST.md"));
    }

    #[tokio::test]
    async fn execute_inner_upserts_cron_trigger_artifact_and_broadcasts_event() {
        let agent = Arc::new(RecordingAgent::new("conv_0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(AgentRuntimeHandle::Mock(
            agent.clone(),
        )));
        let repo = Arc::new(MissingWorkspaceConversationRepo::new(
            "conv_0190f5fe-7c00-7a00-8abc-012345678901",
            serde_json::json!({ "workspace": "/tmp/existing-conversation-workspace" }),
        ));
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let executor = make_executor_with_runtime_registry_repo_and_broadcaster(
            runtime_registry,
            repo.clone(),
            broadcaster.clone(),
        );
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let result = executor.execute_inner(&job, "conv_0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "conv_0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;

        let messages = repo.inserted_messages();
        assert!(
            messages
                .iter()
                .all(|message| message.r#type != "cron_trigger"),
            "cron execution should no longer persist cron trigger as a message"
        );

        let events = broadcaster.events();
        let trigger_event = events
            .iter()
            .find(|event| {
                event["name"] == "conversation.artifact" && event["data"]["kind"] == "cron_trigger"
            })
            .expect("cron execution should broadcast cron trigger artifact");
        assert_eq!(
            trigger_event["data"]["conversation_id"],
            "conv_0190f5fe-7c00-7a00-8abc-012345678901"
        );
        assert_eq!(
            trigger_event["data"]["payload"]["cron_job_id"],
            JOB_ID
        );
        assert_eq!(
            trigger_event["data"]["payload"]["cron_job_name"],
            "Test Job"
        );
        assert!(
            trigger_event["data"]["payload"]["triggered_at"]
                .as_i64()
                .is_some()
        );
    }

    // -- helper ---------------------------------------------------------------

    fn make_executor_for_busy_tests(guard: Arc<CronBusyGuard>) -> JobExecutor {
        struct StubAgentRuntimeRegistry;
        #[async_trait::async_trait]
        impl AgentRuntimeRegistry for StubAgentRuntimeRegistry {
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
            fn collect_idle_runtimes(&self, _: nomifun_common::TimestampMs) -> Vec<String> {
                vec![]
            }
        }

        struct StubConvRepo;

        #[async_trait::async_trait]
        impl IConversationRepository for StubConvRepo {
            async fn get(
                &self,
                _id: &str,
            ) -> Result<Option<nomifun_db::models::ConversationRow>, nomifun_db::DbError>
            {
                Ok(None)
            }
            async fn create(
                &self,
                _row: &nomifun_db::models::ConversationRow,
            ) -> Result<String, nomifun_db::DbError> {
                Ok("conv_0190f5fe-7c00-7a00-8abc-012345678901".into())
            }
            async fn update(
                &self,
                _id: &str,
                _updates: &ConversationRowUpdate,
            ) -> Result<(), nomifun_db::DbError> {
                Ok(())
            }
            async fn delete(&self, _id: &str) -> Result<(), nomifun_db::DbError> {
                Ok(())
            }
            async fn list_paginated(
                &self,
                _user_id: &str,
                _filters: &ConversationFilters,
            ) -> Result<PaginatedResult<nomifun_db::models::ConversationRow>, nomifun_db::DbError>
            {
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
            ) -> Result<Option<nomifun_db::models::ConversationRow>, nomifun_db::DbError>
            {
                Ok(None)
            }
            async fn list_by_cron_job(
                &self,
                _user_id: &str,
                _cron_job_id: &str,
            ) -> Result<Vec<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
                Ok(vec![])
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
            ) -> Result<PaginatedResult<nomifun_db::models::MessageRow>, nomifun_db::DbError>
            {
                Ok(PaginatedResult {
                    items: vec![],
                    total: 0,
                    has_more: false,
                })
            }
            async fn insert_message(
                &self,
                _message: &nomifun_db::models::MessageRow,
            ) -> Result<(), nomifun_db::DbError> {
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
        }

        struct StubBroadcaster;
        impl nomifun_realtime::UserEventSink for StubBroadcaster {
            fn send_to_user(&self, _: &str, _: WebSocketMessage<serde_json::Value>) {}
        }

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

        let stub_broadcaster = Arc::new(StubBroadcaster);
        let stub_repo: Arc<dyn IConversationRepository> = Arc::new(StubConvRepo);
        let agent_metadata_repo: Arc<dyn nomifun_db::IAgentMetadataRepository> =
            Arc::new(StubAgentMetadataRepo);
        let acp_session_repo: Arc<dyn nomifun_db::IAcpSessionRepository> =
            Arc::new(StubAcpSessionRepo);
        let conv_service = Arc::new(ConversationService::new(
            Arc::<str>::from(USER_ID),
            std::env::temp_dir(),
            stub_broadcaster.clone(),
            Arc::new(StubSkillResolver),
            Arc::new(StubAgentRuntimeRegistry),
            Arc::clone(&stub_repo),
            Arc::clone(&agent_metadata_repo),
            acp_session_repo,
            Arc::new(nomifun_conversation::NoExecutionConversationBoundary),
        ));

        let agent_registry = AgentRegistry::new(agent_metadata_repo);

        JobExecutor::new(
            Arc::<str>::from(USER_ID),
            Arc::new(StubAgentRuntimeRegistry),
            stub_repo,
            conv_service,
            guard,
            std::env::temp_dir(),
            std::env::temp_dir(),
            stub_broadcaster,
            agent_registry,
        )
    }

    struct RecordingAgent {
        conversation_id: String,
        workspace: String,
        event_tx: broadcast::Sender<AgentStreamEvent>,
        mode: RwLock<String>,
        sent_messages: RwLock<Vec<SendMessageData>>,
        initialized: bool,
        set_mode_calls: AtomicUsize,
        send_calls: AtomicUsize,
    }

    impl RecordingAgent {
        fn new(conversation_id: &str, mode: &str, initialized: bool) -> Self {
            let (event_tx, _) = broadcast::channel(16);
            Self {
                conversation_id: conversation_id.to_owned(),
                workspace: "/tmp/cron-test".to_owned(),
                event_tx,
                mode: RwLock::new(mode.to_owned()),
                sent_messages: RwLock::new(Vec::new()),
                initialized,
                set_mode_calls: AtomicUsize::new(0),
                send_calls: AtomicUsize::new(0),
            }
        }

        async fn mode(&self) -> String {
            self.mode.read().await.clone()
        }

        fn set_mode_calls(&self) -> usize {
            self.set_mode_calls.load(Ordering::Relaxed)
        }

        fn send_calls(&self) -> usize {
            self.send_calls.load(Ordering::Relaxed)
        }

        async fn sent_messages(&self) -> Vec<SendMessageData> {
            self.sent_messages.read().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl AgentRuntimeControl for RecordingAgent {
        fn agent_type(&self) -> AgentType {
            AgentType::Acp
        }

        fn conversation_id(&self) -> &str {
            &self.conversation_id
        }

        fn workspace(&self) -> &str {
            &self.workspace
        }

        fn status(&self) -> Option<ConversationStatus> {
            Some(ConversationStatus::Pending)
        }

        fn last_activity_at(&self) -> TimestampMs {
            0
        }

        fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
            self.event_tx.subscribe()
        }

        async fn send_message(
            &self,
            data: SendMessageData,
        ) -> Result<(), nomifun_ai_agent::AgentSendError> {
            self.send_calls.fetch_add(1, Ordering::Relaxed);
            self.sent_messages.write().await.push(data);
            Ok(())
        }

        async fn cancel(&self) -> Result<(), nomifun_common::AppError> {
            Ok(())
        }

        fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), nomifun_common::AppError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl MockAgentRuntime for RecordingAgent {
        async fn mode(&self) -> Result<AgentModeResponse, nomifun_common::AppError> {
            Ok(AgentModeResponse {
                mode: self.mode().await,
                initialized: self.initialized,
            })
        }

        async fn set_mode(&self, mode: &str) -> Result<(), nomifun_common::AppError> {
            self.set_mode_calls.fetch_add(1, Ordering::Relaxed);
            let mut guard = self.mode.write().await;
            *guard = mode.to_owned();
            Ok(())
        }
    }

    struct FixedAgentRuntimeRegistry {
        agent: AgentRuntimeHandle,
    }

    #[async_trait::async_trait]
    impl AgentRuntimeRegistry for FixedAgentRuntimeRegistry {
        fn get_runtime(&self, _conversation_id: &str) -> Option<AgentRuntimeHandle> {
            Some(self.agent.clone())
        }

        async fn get_or_create_runtime(
            &self,
            _conversation_id: &str,
            _options: AgentRuntimeBuildOptions,
        ) -> Result<AgentRuntimeHandle, nomifun_common::AppError> {
            Ok(self.agent.clone())
        }

        fn terminate(
            &self,
            _conversation_id: &str,
            _reason: Option<AgentKillReason>,
        ) -> Result<(), nomifun_common::AppError> {
            Ok(())
        }

        fn terminate_and_wait(
            &self,
            _: &str,
            _: Option<AgentKillReason>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(std::future::ready(()))
        }

        fn terminate_all(&self) {}

        fn active_runtime_count(&self) -> usize {
            1
        }

        fn collect_idle_runtimes(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
            Vec::new()
        }
    }

    struct RecordingAgentRuntimeRegistry {
        agent: AgentRuntimeHandle,
        options: Mutex<Vec<AgentRuntimeBuildOptions>>,
    }

    impl RecordingAgentRuntimeRegistry {
        fn new(agent: AgentRuntimeHandle) -> Self {
            Self {
                agent,
                options: Mutex::new(Vec::new()),
            }
        }

        fn last_options(&self) -> Option<AgentRuntimeBuildOptions> {
            self.options
                .lock()
                .ok()
                .and_then(|items| items.last().cloned())
        }

        fn recorded_options(&self) -> Vec<AgentRuntimeBuildOptions> {
            self.options
                .lock()
                .map(|items| items.clone())
                .unwrap_or_default()
        }
    }

    #[async_trait::async_trait]
    impl AgentRuntimeRegistry for RecordingAgentRuntimeRegistry {
        fn get_runtime(&self, _conversation_id: &str) -> Option<AgentRuntimeHandle> {
            Some(self.agent.clone())
        }

        async fn get_or_create_runtime(
            &self,
            _conversation_id: &str,
            options: AgentRuntimeBuildOptions,
        ) -> Result<AgentRuntimeHandle, nomifun_common::AppError> {
            self.options.lock().unwrap().push(options);
            Ok(self.agent.clone())
        }

        fn terminate(
            &self,
            _conversation_id: &str,
            _reason: Option<AgentKillReason>,
        ) -> Result<(), nomifun_common::AppError> {
            Ok(())
        }

        fn terminate_and_wait(
            &self,
            _: &str,
            _: Option<AgentKillReason>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(std::future::ready(()))
        }

        fn terminate_all(&self) {}

        fn active_runtime_count(&self) -> usize {
            1
        }

        fn collect_idle_runtimes(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
            Vec::new()
        }
    }

    struct ExistingConversationRepo;

    #[async_trait::async_trait]
    impl IConversationRepository for ExistingConversationRepo {
        async fn get(
            &self,
            id: &str,
        ) -> Result<Option<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
            Ok(Some(nomifun_db::models::ConversationRow {
                id: id.to_owned(),
                user_id: USER_ID.into(),
                name: "Cron Conversation".into(),
                r#type: "acp".into(),
                extra: serde_json::json!({
                    "workspace": "/tmp/existing-conversation-workspace"
                })
                .to_string(),
                delegation_policy: "automatic".into(),
                execution_model_pool: None,
                decision_policy: "automatic".into(),
                execution_template_id: None,
                model: None,
                status: Some("finished".into()),
                source: None,
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                cron_job_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                created_at: 0,
                updated_at: 0,
            }))
        }

        async fn create(
            &self,
            _row: &nomifun_db::models::ConversationRow,
        ) -> Result<String, nomifun_db::DbError> {
            Ok("conv_0190f5fe-7c00-7a00-8abc-012345678901".into())
        }

        async fn update(
            &self,
            _id: &str,
            _updates: &ConversationRowUpdate,
        ) -> Result<(), nomifun_db::DbError> {
            Ok(())
        }

        async fn delete(&self, _id: &str) -> Result<(), nomifun_db::DbError> {
            Ok(())
        }

        async fn list_paginated(
            &self,
            _user_id: &str,
            _filters: &ConversationFilters,
        ) -> Result<PaginatedResult<nomifun_db::models::ConversationRow>, nomifun_db::DbError>
        {
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
            _cron_job_id: &str,
        ) -> Result<Vec<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
            Ok(vec![])
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
            _message: &nomifun_db::models::MessageRow,
        ) -> Result<(), nomifun_db::DbError> {
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
    }

    struct MissingWorkspaceConversationRepo {
        row: nomifun_db::models::ConversationRow,
        updates: Mutex<Vec<ConversationRowUpdate>>,
        inserted_messages: Mutex<Vec<nomifun_db::models::MessageRow>>,
        artifacts: Mutex<Vec<ConversationArtifactRow>>,
    }

    impl MissingWorkspaceConversationRepo {
        fn new(conversation_id: &str, extra: serde_json::Value) -> Self {
            Self {
                row: nomifun_db::models::ConversationRow {
                    id: conversation_id.to_owned(),
                    user_id: USER_ID.into(),
                    name: "Cron Conversation".into(),
                    r#type: "acp".into(),
                    extra: extra.to_string(),
                    delegation_policy: "automatic".into(),
                    execution_model_pool: None,
                    decision_policy: "automatic".into(),
                    execution_template_id: None,
                    model: None,
                    status: Some("finished".into()),
                    source: None,
                    channel_chat_id: None,
                    pinned: false,
                    pinned_at: None,
                    cron_job_id: None,
                    preset_id: None,
                    preset_revision: None,
                    preset_snapshot: None,
                    created_at: 0,
                    updated_at: 0,
                },
                updates: Mutex::new(Vec::new()),
                inserted_messages: Mutex::new(Vec::new()),
                artifacts: Mutex::new(Vec::new()),
            }
        }

        fn last_update_with_extra(&self) -> Option<ConversationRowUpdate> {
            self.updates.lock().ok().and_then(|items| {
                items
                    .iter()
                    .rev()
                    .find(|update| update.extra.is_some())
                    .cloned()
            })
        }

        fn inserted_messages(&self) -> Vec<nomifun_db::models::MessageRow> {
            self.inserted_messages
                .lock()
                .map(|items| items.clone())
                .unwrap_or_default()
        }
    }

    struct RecordingBroadcaster {
        events: Mutex<Vec<serde_json::Value>>,
    }

    impl RecordingBroadcaster {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<serde_json::Value> {
            self.events
                .lock()
                .map(|items| items.clone())
                .unwrap_or_default()
        }
    }

    impl nomifun_realtime::UserEventSink for RecordingBroadcaster {
        fn send_to_user(&self, _: &str, event: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(serde_json::json!({
                "name": event.name,
                "data": event.data,
            }));
        }
    }

    struct StubBroadcaster;

    impl nomifun_realtime::UserEventSink for StubBroadcaster {
        fn send_to_user(&self, _: &str, _: WebSocketMessage<serde_json::Value>) {}
    }

    #[async_trait::async_trait]
    impl IConversationRepository for MissingWorkspaceConversationRepo {
        async fn get(
            &self,
            _id: &str,
        ) -> Result<Option<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
            Ok(Some(self.row.clone()))
        }

        async fn create(
            &self,
            _row: &nomifun_db::models::ConversationRow,
        ) -> Result<String, nomifun_db::DbError> {
            Ok("conv_0190f5fe-7c00-7a00-8abc-012345678901".into())
        }

        async fn update(
            &self,
            _id: &str,
            updates: &ConversationRowUpdate,
        ) -> Result<(), nomifun_db::DbError> {
            self.updates.lock().unwrap().push(updates.clone());
            Ok(())
        }

        async fn delete(&self, _id: &str) -> Result<(), nomifun_db::DbError> {
            Ok(())
        }

        async fn list_paginated(
            &self,
            _user_id: &str,
            _filters: &ConversationFilters,
        ) -> Result<PaginatedResult<nomifun_db::models::ConversationRow>, nomifun_db::DbError>
        {
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
            _cron_job_id: &str,
        ) -> Result<Vec<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
            Ok(vec![])
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
            self.inserted_messages.lock().unwrap().push(message.clone());
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

        async fn upsert_artifact(
            &self,
            artifact: &ConversationArtifactRow,
        ) -> Result<ConversationArtifactRow, nomifun_db::DbError> {
            let mut artifacts = self.artifacts.lock().unwrap();
            if let Some(existing) = artifacts.iter_mut().find(|row| row.id == artifact.id) {
                *existing = artifact.clone();
                return Ok(existing.clone());
            }
            artifacts.push(artifact.clone());
            Ok(artifact.clone())
        }
    }

    fn make_executor_with_agent(agent: AgentRuntimeHandle) -> JobExecutor {
        make_executor_with_runtime_registry(Arc::new(FixedAgentRuntimeRegistry { agent }))
    }

    fn make_executor_with_runtime_registry(runtime_registry: Arc<dyn AgentRuntimeRegistry>) -> JobExecutor {
        make_executor_with_runtime_registry_and_repo(runtime_registry, Arc::new(ExistingConversationRepo))
    }

    fn make_executor_with_runtime_registry_and_repo(
        runtime_registry: Arc<dyn AgentRuntimeRegistry>,
        repo: Arc<dyn IConversationRepository>,
    ) -> JobExecutor {
        let broadcaster = Arc::new(StubBroadcaster);
        make_executor_with_runtime_registry_repo_and_broadcaster(runtime_registry, repo, broadcaster)
    }

    fn make_executor_with_runtime_registry_repo_and_broadcaster<B>(
        runtime_registry: Arc<dyn AgentRuntimeRegistry>,
        repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<B>,
    ) -> JobExecutor
    where
        B: nomifun_realtime::UserEventSink + 'static,
    {
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

        let agent_metadata_repo: Arc<dyn nomifun_db::IAgentMetadataRepository> =
            Arc::new(StubAgentMetadataRepo);
        let acp_session_repo: Arc<dyn nomifun_db::IAcpSessionRepository> =
            Arc::new(StubAcpSessionRepo);
        let conversation_service = Arc::new(ConversationService::new(
            Arc::<str>::from(USER_ID),
            std::env::temp_dir(),
            broadcaster.clone(),
            Arc::new(StubSkillResolver),
            Arc::clone(&runtime_registry),
            Arc::clone(&repo),
            Arc::clone(&agent_metadata_repo),
            acp_session_repo,
            Arc::new(nomifun_conversation::NoExecutionConversationBoundary),
        ));

        let agent_registry = AgentRegistry::new(agent_metadata_repo);

        JobExecutor::new(
            Arc::<str>::from(USER_ID),
            runtime_registry,
            repo,
            conversation_service,
            Arc::new(CronBusyGuard::new()),
            std::env::temp_dir(),
            std::env::temp_dir(),
            broadcaster,
            agent_registry,
        )
    }

    struct StubAcpSessionRepo;

    #[async_trait::async_trait]
    impl nomifun_db::IAcpSessionRepository for StubAcpSessionRepo {
        async fn get(
            &self,
            _conversation_id: &str,
        ) -> Result<Option<nomifun_db::models::AcpSessionRow>, nomifun_db::DbError> {
            Ok(None)
        }
        async fn create(
            &self,
            _params: &nomifun_db::CreateAcpSessionParams<'_>,
        ) -> Result<nomifun_db::models::AcpSessionRow, nomifun_db::DbError> {
            Err(nomifun_db::DbError::Init("stub".into()))
        }
        async fn update_session_id(
            &self,
            _conversation_id: &str,
            _session_id: &str,
        ) -> Result<bool, nomifun_db::DbError> {
            Ok(false)
        }
        async fn clear_session_id(
            &self,
            _conversation_id: &str,
        ) -> Result<bool, nomifun_db::DbError> {
            Ok(false)
        }
        async fn delete(&self, _conversation_id: &str) -> Result<bool, nomifun_db::DbError> {
            Ok(false)
        }
        async fn load_runtime_state(
            &self,
            _conversation_id: &str,
        ) -> Result<Option<nomifun_db::PersistedSessionState>, nomifun_db::DbError> {
            Ok(None)
        }
        async fn save_runtime_state(
            &self,
            _conversation_id: &str,
            _params: &nomifun_db::SaveRuntimeStateParams<'_>,
        ) -> Result<bool, nomifun_db::DbError> {
            Ok(false)
        }
    }

    struct StubAgentMetadataRepo;

    #[async_trait::async_trait]
    impl nomifun_db::IAgentMetadataRepository for StubAgentMetadataRepo {
        async fn list_all(
            &self,
        ) -> Result<Vec<nomifun_db::models::AgentMetadataRow>, nomifun_db::DbError> {
            Ok(Vec::new())
        }
        async fn get(
            &self,
            _id: &str,
        ) -> Result<Option<nomifun_db::models::AgentMetadataRow>, nomifun_db::DbError> {
            Ok(None)
        }
        async fn find_by_source_and_name(
            &self,
            _agent_source: &str,
            _name: &str,
        ) -> Result<Option<nomifun_db::models::AgentMetadataRow>, nomifun_db::DbError> {
            Ok(None)
        }
        async fn find_builtin_by_backend(
            &self,
            _backend: &str,
        ) -> Result<Option<nomifun_db::models::AgentMetadataRow>, nomifun_db::DbError> {
            Ok(None)
        }
        async fn upsert(
            &self,
            _params: &nomifun_db::models::UpsertAgentMetadataParams<'_>,
        ) -> Result<nomifun_db::models::AgentMetadataRow, nomifun_db::DbError> {
            Err(nomifun_db::DbError::Init("stub".into()))
        }
        async fn apply_handshake(
            &self,
            _id: &str,
            _params: &nomifun_db::models::UpdateAgentHandshakeParams<'_>,
        ) -> Result<Option<nomifun_db::models::AgentMetadataRow>, nomifun_db::DbError> {
            Ok(None)
        }
        async fn set_enabled(
            &self,
            _id: &str,
            _enabled: bool,
        ) -> Result<bool, nomifun_db::DbError> {
            Ok(false)
        }
        async fn delete(&self, _id: &str) -> Result<bool, nomifun_db::DbError> {
            Ok(false)
        }
    }
}
