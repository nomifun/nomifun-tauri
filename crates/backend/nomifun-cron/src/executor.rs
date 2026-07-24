use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nomifun_ai_agent::runtime_registry::AgentRuntimeRegistry;
use nomifun_ai_agent::types::AgentRuntimeBuildOptions;
#[cfg(test)]
use nomifun_ai_agent::types::SendMessageData;
use nomifun_ai_agent::{AgentRegistry, AgentStreamEvent};
use nomifun_api_types::{AgentSource, CreateConversationRequest, SendMessageRequest};
use nomifun_common::{
    AgentId, AgentType, AppError, ConversationId, ExecutionAuthority, ProviderWithModel, UserId,
    now_ms, validate_uuidv7, workspace_path_has_edge_whitespace_segment,
};
use nomifun_conversation::{
    ConversationService, IdempotentMessageDelivery,
    service::{
        BackgroundTurnReconciliationDisposition, BackgroundTurnRuntimePreparation,
        ObservedIdempotentMessageDelivery, PublicTurnDeliveryState,
    },
};
use nomifun_db::models::MessageRow;
use nomifun_db::{ConversationRowUpdate, IConversationRepository};
use nomifun_realtime::UserEventSink;
use tokio::sync::broadcast;
#[cfg(test)]
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::artifacts::{build_cron_trigger_artifact, emit_artifact};
use crate::busy_guard::CronBusyGuard;
use crate::error::CronError;
use crate::prompt::{
    build_existing_conversation_prompt, build_new_conversation_prompt,
    build_new_conversation_prompt_with_skill_suggest, build_new_conversation_with_skill_prompt,
};
use crate::skill_file::{
    cron_skill_name, validate_skill_content, write_raw_skill_file,
};
use crate::skill_suggest::SkillSuggestDetector;
use crate::types::{CronAgentConfig, CronJob, ExecutionMode, cron_job_to_row};

pub const RETRY_INTERVAL_MS: u64 = 30_000;
const DURABLE_RECEIPT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TEMP_WORKSPACE_ID_EXTRA_KEY: &str = "temp_workspace_id";

fn parse_conversation_id(id: &str) -> Result<&str, AppError> {
    ConversationId::try_from(id)
        .map(|_| id)
        .map_err(|_| AppError::NotFound(format!("conversation {id}")))
}

fn cron_turn_key(run_id: &str) -> String {
    format!("cron:{run_id}:turn")
}

fn background_reconciliation_error_is_retryable(error: &AppError) -> bool {
    matches!(
        error,
        AppError::Internal(_)
            | AppError::BadGateway(_)
            | AppError::Timeout(_)
            | AppError::RateLimited
            | AppError::ProviderUnavailable(_)
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionResult {
    Success { conversation_id: String },
    Retrying { attempt: i64 },
    Skipped,
    Error { message: String },
    /// Exact Conversation authority exists but cannot safely be terminalized.
    ///
    /// The Cron reservation must remain `reserved`: service handlers may log
    /// this result, but must not update the job, advance its timer, or settle
    /// the run independently of the accepted Conversation receipt.
    Quarantined { message: String },
}

#[derive(Debug)]
pub(crate) struct PreparedExecution {
    pub conversation_id: String,
    run_id: String,
    saved_skill: Option<SavedSkillContext>,
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

    pub(crate) async fn canonicalize_new_conversation_agent(
        &self,
        job: &mut CronJob,
    ) -> Result<(), CronError> {
        let agent_type = resolve_new_conversation_agent_type(&self.agent_registry, job).await?;
        if agent_type == AgentType::Acp {
            let meta = require_configured_acp_agent(&self.agent_registry, job).await?;
            let config = job.agent_config.get_or_insert_with(|| CronAgentConfig {
                backend: Some(meta.backend.clone().unwrap_or_else(|| "acp".to_owned())),
                name: meta.name.clone(),
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
            });
            config.custom_agent_id = Some(meta.agent_id);
            if let Some(backend) = meta.backend {
                config.backend = Some(backend);
            }
            config.provider_id = None;
        }
        job.agent_type = agent_type.serde_name().to_owned();
        Ok(())
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

    pub async fn execute(&self, job: &CronJob, run_id: &str) -> ExecutionResult {
        if nomifun_common::CronJobRunId::parse(run_id).is_err() {
            return ExecutionResult::Error {
                message: format!("invalid durable cron run id: {run_id}"),
            };
        }
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

        let saved_skill = match self.prepare_authorized_saved_skill(job).await {
            Ok(skill) => skill,
            Err(e) => {
                error!(job_id = %job.cron_job_id, error = %e, "Failed to prepare saved cron skill");
                return ExecutionResult::Error {
                    message: e.to_string(),
                };
            }
        };

        if let Err(e) = self.validate_runtime_job_workspace(job).await {
            error!(job_id = %job.cron_job_id, error = %e, "Failed cron workspace validation");
            return ExecutionResult::Error {
                message: e.to_string(),
            };
        }

        let target_conversation_id =
            match self
                .resolve_conversation(job, saved_skill.as_ref(), run_id)
                .await
            {
                Ok(id) => id,
                Err(e) => {
                    error!(job_id = %job.cron_job_id, error = %e, "Failed to resolve conversation");
                    return ExecutionResult::Error {
                        message: e.to_string(),
                    };
                }
            };

        self.busy_guard
            .set_processing(&target_conversation_id, true);

        let result = self
            .execute_inner_with_run_id(
                job,
                run_id,
                &target_conversation_id,
                saved_skill.as_ref(),
            )
            .await;

        self.busy_guard
            .set_processing(&target_conversation_id, false);

        result
    }

    pub(crate) async fn prepare_run_now(
        &self,
        job: &CronJob,
        run_id: &str,
    ) -> Result<PreparedExecution, CronError> {
        cron_job_to_row(job)?;
        nomifun_common::CronJobRunId::parse(run_id).map_err(|error| {
            CronError::Scheduler(format!("invalid durable cron run id: {error}"))
        })?;
        let saved_skill = match self.prepare_authorized_saved_skill(job).await {
            Ok(skill) => skill,
            Err(err) => {
                error!(
                    job_id = %job.cron_job_id,
                    error = %err,
                    "Failed to prepare saved cron skill for run-now"
                );
                return Err(err);
            }
        };

        self.validate_runtime_job_workspace(job).await?;
        let conversation_id = self
            .resolve_conversation(job, saved_skill.as_ref(), run_id)
            .await?;

        Ok(PreparedExecution {
            conversation_id,
            run_id: run_id.to_owned(),
            saved_skill,
        })
    }

    pub(crate) async fn execute_prepared(
        &self,
        job: &CronJob,
        prepared: PreparedExecution,
    ) -> ExecutionResult {
        let PreparedExecution {
            conversation_id,
            run_id,
            saved_skill,
        } = prepared;
        if self.busy_guard.is_busy(&conversation_id) {
            return self.handle_busy(job);
        }
        self.busy_guard
            .set_processing(&conversation_id, true);

        let result = self
            .execute_inner_with_run_id(
                job,
                &run_id,
                &conversation_id,
                saved_skill.as_ref(),
            )
            .await;

        self.busy_guard
            .set_processing(&conversation_id, false);

        result
    }

    pub fn busy_guard(&self) -> &CronBusyGuard {
        &self.busy_guard
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
        // Reuse the conversation service's canonical bare UUIDv7 message-ID
        // minting boundary.
        let row = MessageRow {
            id: 0,
            message_id: ConversationService::mint_msg_id(),
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

    /// Bind a conversation to its owning cron job through the logical
    /// `conversations.cron_job_id` relation. The reverse logical relation
    /// (`cron_jobs.conversation_id`) is maintained by the service layer.
    ///
    /// Idempotent: a no-op when the column already points at this job. The
    /// conversation row already exists because the executor binds only after
    /// `ConversationService::create` returns.
    pub async fn bind_cron_job_to_conversation(
        &self,
        owner_id: &str,
        conversation_id: &str,
        cron_job_id: &str,
    ) -> Result<(), CronError> {
        UserId::try_from(owner_id)
            .map_err(|error| CronError::Scheduler(format!("invalid cron owner id: {error}")))?;
        nomifun_common::CronJobId::parse(cron_job_id)
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
        if let Some(existing_cron_job_id) = row.cron_job_id.as_deref() {
            return Err(CronError::App(AppError::Conflict(format!(
                "conversation {conversation_id} is already bound to cron job {existing_cron_job_id}"
            ))));
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

    /// Read the exact Conversation receipt for one durably admitted Cron run.
    ///
    /// The Conversation service owns the private receipt namespace; Cron owns
    /// only its public run key and cannot reconstruct repository coordinates.
    pub(crate) async fn public_turn_delivery_state(
        &self,
        user_id: &str,
        conversation_id: &str,
        run_id: &str,
    ) -> Result<PublicTurnDeliveryState, AppError> {
        self.conversation_service
            .public_turn_delivery_state(
                user_id,
                conversation_id,
                &cron_turn_key(run_id),
            )
            .await
    }

    /// Reconcile an accepted exact Cron turn without ever granting resend
    /// authority. Used by startup before it projects the durable receipt.
    pub(crate) async fn reconcile_accepted_turn_on_boot(
        &self,
        user_id: &str,
        conversation_id: &str,
        run_id: &str,
    ) -> Result<BackgroundTurnReconciliationDisposition, AppError> {
        self.conversation_service
            .reconcile_quiescent_running_turn_for_background(
                user_id,
                conversation_id,
                &cron_turn_key(run_id),
                &self.runtime_registry,
            )
            .await
    }

}

impl JobExecutor {
    fn handle_busy(&self, job: &CronJob) -> ExecutionResult {
        let max_retries = job.max_retries;
        let current_retry = job.retry_count;

        if current_retry >= max_retries {
            warn!(
                job_id = %job.cron_job_id,
                retries = current_retry,
                "Max retries exceeded, skipping"
            );
            return ExecutionResult::Skipped;
        }

        let attempt = current_retry + 1;
        info!(
            job_id = %job.cron_job_id,
            attempt,
            max_retries,
            "Conversation busy before cron side effects"
        );
        ExecutionResult::Retrying { attempt }
    }

    async fn resolve_conversation(
        &self,
        job: &CronJob,
        saved_skill: Option<&SavedSkillContext>,
        run_id: &str,
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
                    return self
                        .create_new_conversation(job, saved_skill, run_id)
                        .await;
                };
                self.verify_target_conversation_owner(job, conversation_id).await?;
                Ok(conversation_id.to_owned())
            }
            ExecutionMode::NewConversation => {
                self.create_new_conversation(job, saved_skill, run_id).await
            }
        }
    }

    async fn create_new_conversation(
        &self,
        job: &CronJob,
        saved_skill: Option<&SavedSkillContext>,
        run_id: &str,
    ) -> Result<String, CronError> {
        let agent_type =
            resolve_new_conversation_agent_type(&self.agent_registry, job).await?;
        let model = resolve_model(job);

        let extra =
            build_conversation_extra(&self.agent_registry, job, saved_skill, agent_type)
                .await?;

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

        let creation_key = format!("cron:{run_id}:conversation");
        let response = if let Some(snapshot) = job
            .agent_config
            .as_ref()
            .and_then(|config| config.preset_snapshot.clone())
        {
            self.conversation_service
                .create_from_preset_snapshot_idempotent(
                    &job.user_id,
                    req,
                    snapshot,
                    &creation_key,
                )
                .await
        } else {
            self.conversation_service
                .create_idempotent(&job.user_id, req, &creation_key)
                .await
        }
        .map_err(CronError::from_conversation_create)?;
        // Preserve the canonical conversation entity ID through all cron
        // workspace and persistence boundaries.
        let conversation_id = response.conversation_id;

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
            job_id = %job.cron_job_id,
            conversation_id = %conversation_id,
            "Created new conversation for cron job"
        );

        Ok(conversation_id)
    }

    #[cfg(test)]
    async fn execute_inner(
        &self,
        job: &CronJob,
        conversation_id: &str,
        saved_skill: Option<&SavedSkillContext>,
    ) -> ExecutionResult {
        let run_id = nomifun_common::CronJobRunId::new().into_string();
        self.execute_inner_with_run_id(job, &run_id, conversation_id, saved_skill)
            .await
    }

    async fn execute_inner_with_run_id(
        &self,
        job: &CronJob,
        run_id: &str,
        conversation_id: &str,
        saved_skill: Option<&SavedSkillContext>,
    ) -> ExecutionResult {
        // The interactive `send_message` path resolves the model by parsing
        // `conversation.model` via
        // `nomifun_conversation::runtime_options::provider_model_from_conversation_row`.
        // Cron routes through the same helper so that a Nomi job whose
        // cached `agent_config.provider_id` is invalid or missing
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
                        job.cron_job_id
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
                    job_id = %job.cron_job_id,
                    conversation_id,
                    error = %e,
                    "Failed to load conversation row for cron runtime resolution"
                );
                return ExecutionResult::Error {
                    message: e.to_string(),
                };
            }
        };
        let agent_type = match serde_json::from_value::<AgentType>(
            serde_json::Value::String(conversation_row.r#type.clone()),
        ) {
            Ok(agent_type) => agent_type,
            Err(_) => {
                return ExecutionResult::Error {
                    message: format!(
                        "conversation {conversation_id} has unknown agent type '{}'",
                        conversation_row.r#type
                    ),
                };
            }
        };
        let model = match nomifun_conversation::runtime_options::provider_model_from_conversation_row(
            &conversation_row,
        ) {
            Ok(model) => model,
            Err(error) => {
                error!(job_id = %job.cron_job_id, conversation_id, error = %error, "Failed to resolve canonical conversation model");
                return ExecutionResult::Error {
                    message: error.to_string(),
                };
            }
        };
        let delegation_policy = match nomifun_conversation::runtime_options::delegation_policy_from_conversation_row(&conversation_row) {
            Ok(policy) => policy,
            Err(error) => {
                error!(
                    job_id = %job.cron_job_id,
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
                conversation_id,
                &row_extra,
            ) {
                Ok(workspace) => workspace.to_string_lossy().into_owned(),
                Err(error) => {
                    error!(
                        job_id = %job.cron_job_id,
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
                        job_id = %job.cron_job_id,
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
                    error!(job_id = %job.cron_job_id, error = %e, "Failed to resolve task skills");
                    return ExecutionResult::Error {
                        message: e.to_string(),
                    };
                }
            }
        } else {
            Vec::new()
        };
        let prompt = build_prompt(job, saved_skill, self.controls_host(&job.user_id));
        // Materialize the full request before runtime/knowledge/session
        // mutation. An accepted or completed durable receipt is absorbing and
        // must return without rebuilding or clearing the Conversation runtime.
        let receipt_req = build_cron_send_request(&prompt, &skill_names);
        let turn_key = cron_turn_key(run_id);
        let preflight = match self
            .probe_durable_turn_delivery_until_known(
                job,
                run_id,
                conversation_id,
                &turn_key,
                &receipt_req,
                "before runtime preparation",
            )
            .await
        {
            Ok(delivery) => delivery,
            Err(quarantined) => return quarantined,
        };
        if let Some(delivery) = preflight {
                info!(
                    job_id = %job.cron_job_id,
                    run_id,
                    conversation_id,
                    "Cron turn replay absorbed before runtime preparation"
                );
                let delivery = if delivery.completed {
                    delivery
                } else {
                    if let Err(quarantined) = self
                        .reconcile_accepted_turn_before_wait(
                            job,
                            run_id,
                            conversation_id,
                            &turn_key,
                        )
                        .await
                    {
                        return quarantined;
                    }
                    match self
                        .await_durable_turn_completion(
                            job,
                            run_id,
                            conversation_id,
                            &turn_key,
                            &receipt_req,
                            None,
                        )
                        .await
                    {
                        Ok(delivery) => delivery,
                        Err(error) => {
                            return ExecutionResult::Quarantined {
                                message: error.to_string(),
                            };
                        }
                    }
                };
                return replayed_delivery_result(run_id, conversation_id, delivery);
        }

        let mut build_extra = build_task_extra(job, &skill_names);
        if agent_type == AgentType::Acp {
            for key in ["agent_id", "backend", "agent_source"] {
                let Some(value) = row_extra.get(key).cloned() else {
                    return ExecutionResult::Error {
                        message: format!(
                            "conversation {conversation_id} is missing canonical ACP extra.{key}"
                        ),
                    };
                };
                build_extra[key] = value;
            }
        }
        if managed_workspace {
            let temp_workspace_id = row_extra
                .get(TEMP_WORKSPACE_ID_EXTRA_KEY)
                .cloned()
                .expect("managed workspace marker checked above");
            build_extra[TEMP_WORKSPACE_ID_EXTRA_KEY] = temp_workspace_id;
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
            workspace_binding_lease: None,
        };
        let skill_suggest_workspace = options.workspace.clone();
        let desired_mode = job
            .agent_config
            .as_ref()
            .and_then(|config| config.mode.clone());
        let clear_context = matches!(job.execution_mode, ExecutionMode::Existing)
            && job
                .agent_config
                .as_ref()
                .is_some_and(|config| config.clear_context_each_run);
        let runtime_preparation = BackgroundTurnRuntimePreparation {
            runtime_options: options,
            desired_mode,
            clear_context,
            pre_send_hook: None,
        };
        // This lease fences stop/reset while Conversation atomically claims the
        // durable turn. Cron never uses it to build or mutate a runtime itself.
        let build_lease = match self
            .conversation_service
            .begin_public_runtime_preparation(conversation_id, &job.user_id)
        {
            Ok(lease) => lease,
            Err(error) => {
                return ExecutionResult::Error {
                    message: error.to_string(),
                };
            }
        };
        let observed = match self
            .conversation_service
            .send_observed_background_message_with_idempotency_key(
                &job.user_id,
                conversation_id,
                &turn_key,
                build_cron_send_request(&prompt, &skill_names),
                &self.runtime_registry,
                build_lease,
                runtime_preparation,
            )
            .await
        {
            Ok(observed) => observed,
            Err(e) => {
                error!(
                    job_id = %job.cron_job_id,
                    conversation_id,
                    error = %e,
                    "Failed to send cron job message"
                );
                // The receiver may have durably accepted the exact turn
                // before a later preparation/send error escaped. Re-read the
                // receipt before deciding whether Cron may become terminal.
                let delivery = match self
                    .probe_durable_turn_delivery_until_known(
                        job,
                        run_id,
                        conversation_id,
                        &turn_key,
                        &receipt_req,
                        "after keyed send returned an error",
                    )
                    .await
                {
                    Ok(Some(delivery)) => delivery,
                    Ok(None) => {
                        return ExecutionResult::Error {
                            message: e.to_string(),
                        };
                    }
                    Err(quarantined) => return quarantined,
                };
                let delivery = if delivery.completed {
                    delivery
                } else {
                    if let Err(quarantined) = self
                        .reconcile_accepted_turn_before_wait(
                            job,
                            run_id,
                            conversation_id,
                            &turn_key,
                        )
                        .await
                    {
                        return quarantined;
                    }
                    match self
                        .await_durable_turn_completion(
                            job,
                            run_id,
                            conversation_id,
                            &turn_key,
                            &receipt_req,
                            None,
                        )
                        .await
                    {
                        Ok(delivery) => delivery,
                        Err(error) => {
                            return ExecutionResult::Quarantined {
                                message: error.to_string(),
                            };
                        }
                    }
                };
                return replayed_delivery_result(run_id, conversation_id, delivery);
            }
        };
        let ObservedIdempotentMessageDelivery {
            delivery,
            runtime: _runtime,
            events,
        } = observed;
        if delivery.replayed {
            info!(
                job_id = %job.cron_job_id,
                run_id,
                conversation_id,
                "Cron turn replay absorbed by durable delivery receipt"
            );
            let delivery = if delivery.completed {
                delivery
            } else {
                if let Err(quarantined) = self
                    .reconcile_accepted_turn_before_wait(
                        job,
                        run_id,
                        conversation_id,
                        &turn_key,
                    )
                    .await
                {
                    return quarantined;
                }
                match self
                    .await_durable_turn_completion(
                        job,
                        run_id,
                        conversation_id,
                        &turn_key,
                        &receipt_req,
                        events,
                    )
                    .await
                {
                    Ok(delivery) => delivery,
                    Err(error) => {
                        return ExecutionResult::Quarantined {
                            message: error.to_string(),
                        };
                    }
                }
            };
            return replayed_delivery_result(run_id, conversation_id, delivery);
        }

        let delivery = if delivery.completed {
            delivery
        } else {
            match self
                .await_durable_turn_completion(
                    job,
                    run_id,
                    conversation_id,
                    &turn_key,
                    &receipt_req,
                    events,
                )
                .await
            {
                Ok(delivery) => delivery,
                Err(error) => {
                    return ExecutionResult::Quarantined {
                        message: error.to_string(),
                    };
                }
            }
        };
        let terminal_result = replayed_delivery_result(run_id, conversation_id, delivery);
        if !matches!(terminal_result, ExecutionResult::Success { .. }) {
            return terminal_result;
        }

        if let Err(e) = self
            .upsert_cron_trigger_artifact(conversation_id, job)
            .await
        {
            warn!(
                job_id = %job.cron_job_id,
                conversation_id,
                error = %e,
                "Failed to persist/broadcast cron trigger artifact"
            );
        }
        if self.controls_host(&job.user_id)
            && saved_skill.is_none()
            && matches!(job.execution_mode, ExecutionMode::NewConversation)
        {
            self.skill_suggest_detector.schedule_check(
                job.user_id.clone(),
                conversation_id.to_owned(),
                job.cron_job_id.clone(),
                skill_suggest_workspace,
            );
        }
        info!(
            job_id = %job.cron_job_id,
            conversation_id,
            "Cron job turn completed successfully"
        );
        terminal_result
    }

    #[allow(clippy::too_many_arguments)]
    async fn probe_durable_turn_delivery_until_known(
        &self,
        job: &CronJob,
        run_id: &str,
        conversation_id: &str,
        turn_key: &str,
        request: &SendMessageRequest,
        phase: &'static str,
    ) -> Result<Option<IdempotentMessageDelivery>, ExecutionResult> {
        let mut retry_delay = Duration::from_millis(25);
        loop {
            match self
                .conversation_service
                .idempotent_delivery_result_with_idempotency_key(
                    &job.user_id,
                    conversation_id,
                    turn_key,
                    request,
                )
                .await
            {
                Ok(delivery) => return Ok(delivery),
                Err(error) if background_reconciliation_error_is_retryable(&error) => {
                    warn!(
                        job_id = %job.cron_job_id,
                        run_id,
                        conversation_id,
                        phase,
                        error = %error,
                        "Cron cannot yet prove its exact durable turn receipt state; retaining the run as non-terminal"
                    );
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
                }
                Err(error) => {
                    return Err(ExecutionResult::Quarantined {
                        message: format!(
                            "cron run {run_id} exact turn receipt is quarantined {phase}: {error}"
                        ),
                    });
                }
            }
        }
    }

    async fn reconcile_accepted_turn_before_wait(
        &self,
        job: &CronJob,
        run_id: &str,
        conversation_id: &str,
        turn_key: &str,
    ) -> Result<(), ExecutionResult> {
        let mut retry_delay = Duration::from_millis(25);
        loop {
            match self
                .conversation_service
                .reconcile_quiescent_running_turn_for_background(
                    &job.user_id,
                    conversation_id,
                    turn_key,
                    &self.runtime_registry,
                )
                .await
            {
                Ok(
                    BackgroundTurnReconciliationDisposition::LiveExactOwnerWait
                    | BackgroundTurnReconciliationDisposition::ReconciledOrTerminalReRead,
                ) => return Ok(()),
                Ok(
                    BackgroundTurnReconciliationDisposition::ExternalProofRequiredFailClosed,
                ) => {
                    return Err(ExecutionResult::Quarantined {
                        message: format!(
                            "cron run {run_id} has an accepted external Conversation turn whose terminal state is not proven"
                        ),
                    });
                }
                Ok(BackgroundTurnReconciliationDisposition::StaleConflict) => {
                    return Err(ExecutionResult::Quarantined {
                        message: format!(
                            "cron run {run_id} has an accepted Conversation receipt that no longer matches the exact active turn generation"
                        ),
                    });
                }
                Err(error) if background_reconciliation_error_is_retryable(&error) => {
                    warn!(
                        job_id = %job.cron_job_id,
                        run_id,
                        conversation_id,
                        error = %error,
                        "Accepted Cron turn reconciliation failed transiently; retaining the Cron run as non-terminal"
                    );
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
                }
                Err(error) => {
                    return Err(ExecutionResult::Quarantined {
                        message: format!(
                            "cron run {run_id} accepted turn reconciliation was quarantined: {error}"
                        ),
                    });
                }
            }
        }
    }

    async fn await_durable_turn_completion(
        &self,
        job: &CronJob,
        run_id: &str,
        conversation_id: &str,
        turn_key: &str,
        request: &SendMessageRequest,
        mut events: Option<broadcast::Receiver<AgentStreamEvent>>,
    ) -> Result<IdempotentMessageDelivery, AppError> {
        let mut consecutive_probe_failures = 0_u64;
        loop {
            match self
                .conversation_service
                .idempotent_delivery_result_with_idempotency_key(
                    &job.user_id,
                    conversation_id,
                    turn_key,
                    request,
                )
                .await
            {
                Ok(Some(delivery)) => {
                    consecutive_probe_failures = 0;
                    if delivery.completed {
                        return Ok(delivery);
                    }
                }
                Ok(None) => {
                    consecutive_probe_failures =
                        consecutive_probe_failures.saturating_add(1);
                    if consecutive_probe_failures == 1
                        || consecutive_probe_failures.is_multiple_of(100)
                    {
                        warn!(
                            job_id = %job.cron_job_id,
                            run_id,
                            conversation_id,
                            consecutive_probe_failures,
                            "Accepted Cron turn receipt is temporarily unavailable; retaining the Cron run as non-terminal"
                        );
                    }
                }
                Err(error) => {
                    consecutive_probe_failures =
                        consecutive_probe_failures.saturating_add(1);
                    if consecutive_probe_failures == 1
                        || consecutive_probe_failures.is_multiple_of(100)
                    {
                        warn!(
                            job_id = %job.cron_job_id,
                            run_id,
                            conversation_id,
                            consecutive_probe_failures,
                            error = %error,
                            "Failed to re-read accepted Cron turn receipt; retaining the Cron run as non-terminal"
                        );
                    }
                }
            }

            if let Some(receiver) = events.as_mut() {
                tokio::select! {
                    event = receiver.recv() => {
                        match event {
                            Ok(AgentStreamEvent::Finish(_))
                            | Ok(AgentStreamEvent::Error(_))
                            | Err(broadcast::error::RecvError::Closed) => {
                                // The receipt is authoritative. A terminal stream
                                // event only prompts an immediate re-read because
                                // atomic DB finalization may complete just after it.
                                events = None;
                            }
                            Ok(_) => {}
                            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                warn!(
                                    job_id = %job.cron_job_id,
                                    run_id,
                                    conversation_id,
                                    skipped,
                                    "Cron turn event stream lagged; continuing from durable receipt"
                                );
                                events = None;
                            }
                        }
                    }
                    _ = tokio::time::sleep(DURABLE_RECEIPT_POLL_INTERVAL) => {}
                }
            } else {
                tokio::time::sleep(DURABLE_RECEIPT_POLL_INTERVAL).await;
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
                job.cron_job_id
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
        nomifun_common::CronJobId::parse(job_id)
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
                job.cron_job_id
            )));
        }

        let extra = serde_json::from_str::<serde_json::Value>(&row.extra).map_err(|error| {
            CronError::Scheduler(format!(
                "conversation {conversation_id} has invalid extra JSON: {error}"
            ))
        })?;
        if !extra.is_object() {
            return Err(CronError::Scheduler(format!(
                "conversation {conversation_id} extra must be a JSON object"
            )));
        }
        Ok(extra
            .get("workspace")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                CronError::Scheduler(format!(
                    "conversation {conversation_id} has no canonical workspace"
                ))
            })?
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

    async fn prepare_saved_skill(
        &self,
        job: &CronJob,
    ) -> Result<Option<SavedSkillContext>, CronError> {
        let Some(raw_content) = job
            .skill_content
            .as_deref()
            .filter(|content| !content.is_empty())
        else {
            return Ok(None);
        };
        validate_skill_content(raw_content).map_err(|error| {
            CronError::Scheduler(format!(
                "cron job {} has invalid persisted skill_content: {error}",
                job.cron_job_id
            ))
        })?;
        // SQLite is authoritative. SKILL.md is a generated artifact refreshed
        // from the canonical field before each execution; it is never read as
        // a fallback source.
        write_raw_skill_file(&self.data_dir, &job.cron_job_id, raw_content).await?;

        Ok(Some(SavedSkillContext {
            name: cron_skill_name(&job.cron_job_id)?,
            raw_content: raw_content.to_owned(),
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
                job.cron_job_id
            )));
        }

        let extra = serde_json::from_str::<serde_json::Value>(&row.extra).map_err(|error| {
            CronError::Scheduler(format!(
                "conversation {conversation_id} has invalid extra JSON: {error}"
            ))
        })?;
        if !extra.is_object() {
            return Err(CronError::Scheduler(format!(
                "conversation {conversation_id} extra must be a JSON object"
            )));
        }

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

fn configured_agent_id(job: &CronJob) -> Option<&str> {
    job.agent_config
        .as_ref()
        .and_then(|config| config.custom_agent_id.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn require_configured_agent_id(job: &CronJob) -> Result<&str, CronError> {
    let agent_id = configured_agent_id(job).ok_or_else(|| {
        CronError::InvalidAgentConfig(format!(
            "cron job {} requires an explicit UUIDv7 agent_config.custom_agent_id",
            job.cron_job_id
        ))
    })?;
    AgentId::parse(agent_id.to_owned()).map_err(|error| {
        CronError::InvalidAgentConfig(format!(
            "cron job {} has invalid agent_config.custom_agent_id '{agent_id}': {error}",
            job.cron_job_id
        ))
    })?;
    Ok(agent_id)
}

async fn resolve_configured_agent(
    registry: &AgentRegistry,
    job: &CronJob,
) -> Result<Option<nomifun_api_types::AgentMetadata>, CronError> {
    if job.agent_type == AgentType::Nomi.serde_name() {
        return Ok(None);
    }
    let agent_id = require_configured_agent_id(job)?;

    Ok(registry.get(agent_id).await)
}

async fn resolve_new_conversation_agent_type(
    registry: &AgentRegistry,
    job: &CronJob,
) -> Result<AgentType, CronError> {
    if let Some(agent) = resolve_configured_agent(registry, job).await? {
        return Ok(agent.agent_type);
    }

    let raw = job.agent_type.trim();
    let agent_type =
        serde_json::from_value::<AgentType>(serde_json::Value::String(raw.to_owned()))
            .map_err(|_| {
                CronError::InvalidAgentConfig(format!(
                    "cron job {} has unknown agent selector '{raw}'",
                    job.cron_job_id
                ))
            })?;
    if agent_type == AgentType::Acp {
        return Err(CronError::InvalidAgentConfig(format!(
            "cron job {} requires an explicit UUIDv7 agent_config.custom_agent_id",
            job.cron_job_id
        )));
    }
    Ok(agent_type)
}

fn validate_acp_agent_metadata(
    job_id: &str,
    meta: &nomifun_api_types::AgentMetadata,
) -> Result<(), CronError> {
    if meta.agent_type != AgentType::Acp {
        return Err(CronError::InvalidAgentConfig(format!(
            "cron job {job_id} agent '{}' is type '{}', not ACP",
            meta.agent_id,
            meta.agent_type.serde_name()
        )));
    }
    if !meta.enabled {
        return Err(CronError::InvalidAgentConfig(format!(
            "cron job {job_id} agent '{}' is disabled",
            meta.agent_id
        )));
    }
    if meta.agent_source == AgentSource::Internal {
        return Err(CronError::InvalidAgentConfig(format!(
            "cron job {job_id} agent '{}' has unsupported internal source",
            meta.agent_id
        )));
    }
    Ok(())
}

async fn require_configured_acp_agent(
    registry: &AgentRegistry,
    job: &CronJob,
) -> Result<nomifun_api_types::AgentMetadata, CronError> {
    let agent_id = require_configured_agent_id(job)?;
    let meta = resolve_configured_agent(registry, job)
        .await?
        .ok_or_else(|| {
            CronError::InvalidAgentConfig(format!(
                "cron job {} references missing ACP agent '{agent_id}'",
                job.cron_job_id
            ))
        })?;
    validate_acp_agent_metadata(&job.cron_job_id, &meta)?;
    Ok(meta)
}

/// Only nomi conversations carry meaningful model info in `conversations.model`;
/// ACP and other agent types ignore this field and resolve the model via their own
/// mechanisms (catalog defaults, CLI flags, etc.). Returning `None` lets the
/// `CreateConversationRequest.model` stay `None` for those types, which is the
/// correct semantic.
///
/// For Nomi, `agent_config.provider_id` holds the Provider UUIDv7 while
/// `agent_config.backend` is reserved for ACP/agent runtime selection.
/// `CronService::add_job`/`update_job` already rejects Nomi
/// jobs lacking a canonical provider ID, so the `None` return here is a
/// defensive check for invalid in-memory values.
fn resolve_model(job: &CronJob) -> Option<ProviderWithModel> {
    if job.agent_type != "nomi" {
        return None;
    }
    let config = job.agent_config.as_ref()?;
    if config.backend.is_some() {
        return None;
    }
    let provider_id = config.provider_id.as_deref()?;
    if nomifun_common::ProviderId::try_from(provider_id).is_err() {
        return None;
    }
    Some(ProviderWithModel {
        provider_id: provider_id.to_owned(),
        model: config
            .model
            .clone()
            .filter(|model| !model.is_empty() && model.trim() == model)?,
        use_model: None,
    })
}

async fn inject_agent_identity(
    extra: &mut serde_json::Map<String, serde_json::Value>,
    registry: &AgentRegistry,
    job: &CronJob,
    agent_type: AgentType,
) -> Result<(), CronError> {
    if agent_type != AgentType::Acp {
        return Ok(());
    }

    let meta = require_configured_acp_agent(registry, job).await?;
    let agent_source = match meta.agent_source {
        AgentSource::Builtin => "builtin",
        AgentSource::Extension => "extension",
        AgentSource::Custom => "custom",
        AgentSource::Internal => unreachable!("validated above"),
    };
    extra.insert("agent_id".to_owned(), serde_json::Value::String(meta.agent_id));
    extra.insert(
        "agent_source".to_owned(),
        serde_json::Value::String(agent_source.to_owned()),
    );
    if let Some(backend) = meta.backend {
        extra.insert("backend".to_owned(), serde_json::Value::String(backend));
    }
    Ok(())
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
    let Some(model) = config
        .model
        .as_ref()
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
    else {
        return;
    };
    extra.insert(
        "current_model_id".to_owned(),
        serde_json::Value::String(model.to_owned()),
    );
}

fn build_task_extra(job: &CronJob, skills: &[String]) -> serde_json::Value {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "cron_job_id".to_owned(),
        serde_json::Value::String(job.cron_job_id.clone()),
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

fn build_cron_send_request(prompt: &str, skill_names: &[String]) -> SendMessageRequest {
    SendMessageRequest {
        content: prompt.to_owned(),
        files: Vec::new(),
        inject_skills: skill_names.to_vec(),
        hidden: true,
        origin: Some("cron".to_owned()),
        channel_platform: None,
    }
}

fn replayed_delivery_result(
    run_id: &str,
    conversation_id: &str,
    delivery: IdempotentMessageDelivery,
) -> ExecutionResult {
    if !delivery.completed {
        return ExecutionResult::Quarantined {
            message: format!(
                "cron run {run_id} remains accepted without an exact durable terminal outcome"
            ),
        };
    }
    if delivery.result_ok == Some(false) {
        return ExecutionResult::Error {
            message: delivery
                .result_error
                .or(delivery.result_text)
                .unwrap_or_else(|| format!("cron run {run_id} completed with an error")),
        };
    }
    ExecutionResult::Success {
        conversation_id: conversation_id.to_owned(),
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
    agent_type: AgentType,
) -> Result<serde_json::Value, CronError> {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "cron_job_id".to_owned(),
        serde_json::Value::String(job.cron_job_id.clone()),
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

    inject_agent_identity(&mut extra, registry, job, agent_type).await?;
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

    Ok(serde_json::Value::Object(extra))
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
    conversation_id: &str,
    extra: &serde_json::Value,
) -> Result<std::path::PathBuf, CronError> {
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
    validate_uuidv7(temp_workspace_id).map_err(|error| {
        CronError::Scheduler(format!(
            "conversation {conversation_id} has invalid temp_workspace_id '{temp_workspace_id}': {error}"
        ))
    })?;

    Ok(data_dir.join("conversations").join(temp_workspace_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CreatedBy, CronAgentConfig, CronSchedule};
    use nomifun_ai_agent::runtime_handle::{AgentRuntimeHandle, AgentRuntimeControl, MockAgentRuntime};
    use nomifun_ai_agent::protocol::events::{FinishEventData, TextEventData};
    use nomifun_api_types::{AgentModeResponse, WebSocketMessage};
    use nomifun_common::{AgentKillReason, ConversationStatus, PaginatedResult, TimestampMs};
    use nomifun_db::{
        ConversationArtifactRow, ConversationDeliveryReceiptClaim, ConversationFilters,
        ConversationRowUpdate, ConversationTurnAdmissionState, MessageRowUpdate,
        MessageSearchRow, SortOrder, TurnLifecycleTransition, TurnReceiptCompletion,
    };
    use nomifun_db::models::ConversationDeliveryReceiptRow;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::{Barrier, RwLock, broadcast};

    const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
    const USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const JOB_SKILL_NAME: &str = JOB_ID;
    const TEST_ACP_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000101";

    fn sample_job() -> CronJob {
        CronJob {
            cron_job_id: JOB_ID.into(),
            user_id: USER_ID.into(),
            name: "Test Job".into(),
            enabled: true,
            schedule_revision: 1,
            schedule: CronSchedule::Every {
                every_ms: 60000,
                description: None,
            },
            message: "do something".into(),
            execution_mode: ExecutionMode::Existing,
            agent_config: Some(CronAgentConfig {
                backend: Some("claude".into()),
                name: "Claude".into(),
                cli_path: Some("/usr/bin/claude".into()),
                custom_agent_id: Some(TEST_ACP_AGENT_ID.into()),
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model: Some("claude-sonnet-4".into()),
                provider_id: None,
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            conversation_id: Some("0190f5fe-7c00-7a00-8abc-012345678901".into()),
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

    fn claimed_test_delivery_receipt(
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        kind: &str,
        request_payload: &str,
        now: i64,
    ) -> ConversationDeliveryReceiptClaim {
        let message_id = nomifun_common::generate_id();
        ConversationDeliveryReceiptClaim {
            receipt: ConversationDeliveryReceiptRow {
                id: 1,
                operation_id: operation_id.to_owned(),
                message_id: message_id.clone(),
                conversation_id: conversation_id.to_owned(),
                user_id: user_id.to_owned(),
                kind: kind.to_owned(),
                request_payload: request_payload.to_owned(),
                status: "accepted".into(),
                result_ok: None,
                result_text: None,
                result_error: None,
                created_at: now,
                updated_at: now,
                completed_at: None,
                projected_conversation_id: Some(conversation_id.to_owned()),
                projected_message_id: Some(message_id),
            },
            claimed_new: true,
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
        let path = default_temp_workspace_path(
            Path::new("/work"),
            "0190f5fe-7c00-7a00-8abc-012345678901",
            &serde_json::json!({
                "temp_workspace_id": "0190f5fe-7c00-7a00-8abc-012345678901"
            }),
        )
        .unwrap();

        assert_eq!(
            path,
            Path::new("/work")
                .join("conversations")
                .join("0190f5fe-7c00-7a00-8abc-012345678901")
        );
    }

    #[test]
    fn default_temp_workspace_path_missing_or_malformed_token_fails_closed() {
        for extra in [
            serde_json::json!({}),
            serde_json::json!({ "temp_workspace_id": "ws_abc" }),
            serde_json::json!({ "temp_workspace_id": 7 }),
        ] {
            let result = default_temp_workspace_path(
                Path::new("/work"),
                "0190f5fe-7c00-7a00-8abc-012345678901",
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

    /// Build a registry backed by an in-memory DB seeded from the v3 baseline,
    /// so agent-id lookup tests exercise the same catalog rows the server sees.
    async fn hydrated_registry() -> Arc<AgentRegistry> {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let repo = Arc::new(nomifun_db::SqliteAgentMetadataRepository::new(
            db.pool().clone(),
        ));
        let registry = AgentRegistry::new(repo);
        registry.hydrate().await.unwrap();
        registry
    }

    // -- canonical Agent identity tests --------------------------------------

    #[tokio::test]
    async fn new_conversation_agent_type_resolves_canonical_agent_id() {
        let registry = hydrated_registry().await;
        let job = sample_job();
        assert_eq!(
            resolve_new_conversation_agent_type(&registry, &job)
                .await
                .unwrap(),
            AgentType::Acp
        );
    }

    #[tokio::test]
    async fn bare_acp_type_is_rejected_without_agent_id() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            agent_type: "acp".into(),
            agent_config: None,
            ..sample_job()
        };
        let error = resolve_new_conversation_agent_type(&registry, &job)
            .await
            .unwrap_err();
        assert!(matches!(error, CronError::InvalidAgentConfig(_)));
    }

    #[tokio::test]
    async fn unknown_agent_selector_is_rejected() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            agent_type: "unknown_type".into(),
            agent_config: None,
            ..sample_job()
        };
        let error = resolve_new_conversation_agent_type(&registry, &job)
            .await
            .unwrap_err();
        assert!(matches!(error, CronError::InvalidAgentConfig(_)));
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
                backend: None,
                name: "OpenAI".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model: Some("gpt-5".into()),
                provider_id: Some(PROVIDER_ID.into()),
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
    fn resolve_model_nomi_without_model_id_returns_none() {
        let job = CronJob {
            agent_type: "nomi".into(),
            agent_config: Some(CronAgentConfig {
                backend: None,
                name: "OpenAI".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model: None,
                provider_id: Some(PROVIDER_ID.into()),
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };
        assert!(resolve_model(&job).is_none());
    }

    #[test]
    fn resolve_model_nomi_without_config_returns_none() {
        // Defensive: `add_job` rejects this shape, and `resolve_model` must not
        // infer a provider ID from `agent_type`.
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
                backend: Some("   ".into()),
                name: "Bogus".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model: Some("gpt-5".into()),
                provider_id: None,
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };
        assert!(resolve_model(&job).is_none());
    }

    // -- build_task_extra tests -----------------------------------------------

    #[test]
    fn build_task_extra_includes_cron_job_id() {
        let job = sample_job();
        let extra = build_task_extra(&job, &[]);
        assert_eq!(extra["cron_job_id"], JOB_ID);
    }

    #[test]
    fn build_task_extra_with_config_fields() {
        let job = sample_job();
        let extra = build_task_extra(&job, &[JOB_SKILL_NAME.into()]);
        assert!(extra.get("agent_id").is_none());
        assert!(extra.get("backend").is_none());
        assert_eq!(extra["cli_path"], "/usr/bin/claude");
        assert_eq!(extra["agent_name"], "Claude");
        assert_eq!(extra["custom_agent_id"], TEST_ACP_AGENT_ID);
        assert_eq!(extra["skills"], serde_json::json!([JOB_SKILL_NAME]));
    }

    #[test]
    fn build_task_extra_without_config() {
        let job = CronJob {
            agent_config: None,
            ..sample_job()
        };
        let extra = build_task_extra(&job, &[]);
        assert_eq!(extra["cron_job_id"], JOB_ID);
        assert!(extra.get("backend").is_none());
    }

    #[test]
    fn build_task_extra_does_not_rederive_agent_identity_from_job_selector() {
        let job = CronJob {
            agent_type: "claude".into(),
            agent_config: None,
            ..sample_job()
        };
        let extra = build_task_extra(&job, &[]);
        assert!(extra.get("agent_id").is_none());
        assert!(extra.get("backend").is_none());
        assert!(extra.get("agent_source").is_none());
    }

    #[test]
    fn build_task_extra_injects_current_model_id_for_acp() {
        // ACP agents resolve their model via the session `extra` carrying
        // `current_model_id`, mirroring the Agent execution path. The
        // configured `agent_config.model` must reach the session.
        let job = CronJob {
            agent_type: "claude".into(),
            agent_config: Some(CronAgentConfig {
                backend: Some("claude".into()),
                name: "Claude".into(),
                cli_path: None,
                custom_agent_id: Some(TEST_ACP_AGENT_ID.into()),
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model: Some("claude-sonnet-4-6".into()),
                provider_id: None,
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };
        let extra = build_task_extra(&job, &[]);
        assert_eq!(extra["current_model_id"], "claude-sonnet-4-6");
    }

    #[test]
    fn build_task_extra_omits_current_model_id_for_nomi() {
        // nomi resolves model via the top-level conversation.model provider
        // path, never `current_model_id`. The ACP injection must not bleed
        // into the nomi branch.
        let job = CronJob {
            agent_type: "nomi".into(),
            agent_config: Some(CronAgentConfig {
                backend: None,
                name: "OpenAI".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model: Some("gpt-5".into()),
                provider_id: Some(PROVIDER_ID.into()),
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };
        let extra = build_task_extra(&job, &[]);
        assert!(extra.get("current_model_id").is_none());
    }

    #[tokio::test]
    async fn build_conversation_extra_without_saved_skill_excludes_cron_auto_inject_only() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let agent_type = resolve_new_conversation_agent_type(&registry, &job).await.unwrap();
        let extra = build_conversation_extra(&registry, &job, None, agent_type)
            .await
            .unwrap();

        assert_eq!(extra["cron_job_id"], JOB_ID);
        assert_eq!(extra["agent_id"], TEST_ACP_AGENT_ID);
        assert_eq!(extra["backend"], "claude");
        assert_eq!(extra["agent_source"], "builtin");
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

        let agent_type = resolve_new_conversation_agent_type(&registry, &job).await.unwrap();
        let extra = build_conversation_extra(&registry, &job, Some(&saved_skill), agent_type)
            .await
            .unwrap();

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
        let mut job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        job.agent_config
            .as_mut()
            .expect("sample agent config")
            .workspace = Some("/home/user/project".into());

        let agent_type = resolve_new_conversation_agent_type(&registry, &job).await.unwrap();
        let extra = build_conversation_extra(&registry, &job, None, agent_type)
            .await
            .unwrap();

        assert_eq!(extra["workspace"], "/home/user/project");
    }

    #[tokio::test]
    async fn build_conversation_extra_rejects_backend_without_agent_id() {
        let registry = hydrated_registry().await;
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            agent_type: "claude".into(),
            agent_config: None,
            ..sample_job()
        };

        let error = resolve_new_conversation_agent_type(&registry, &job)
            .await
            .unwrap_err();
        assert!(matches!(error, CronError::InvalidAgentConfig(_)));
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
                backend: Some("claude".into()),
                name: "Claude".into(),
                cli_path: None,
                custom_agent_id: Some(TEST_ACP_AGENT_ID.into()),
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model: Some("claude-sonnet-4-6".into()),
                provider_id: None,
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };

        let agent_type = resolve_new_conversation_agent_type(&registry, &job).await.unwrap();
        let extra = build_conversation_extra(&registry, &job, None, agent_type)
            .await
            .unwrap();

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
                backend: None,
                name: "OpenAI".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model: Some("gpt-5".into()),
                provider_id: Some(PROVIDER_ID.into()),
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            ..sample_job()
        };

        let agent_type = resolve_new_conversation_agent_type(&registry, &job).await.unwrap();
        let extra = build_conversation_extra(&registry, &job, None, agent_type)
            .await
            .unwrap();

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
                backend: Some("claude".into()),
                name: "Claude".into(),
                cli_path: None,
                custom_agent_id: Some(TEST_ACP_AGENT_ID.into()),
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
            ..sample_job()
        };

        let agent_type = resolve_new_conversation_agent_type(&registry, &job).await.unwrap();
        let extra = build_conversation_extra(&registry, &job, None, agent_type)
            .await
            .unwrap();

        assert!(extra.get("current_model_id").is_none());
    }

    // -- execution_result display ---------------------------------------------

    #[test]
    fn execution_result_variants() {
        let success = ExecutionResult::Success {
            conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into(),
        };
        assert_eq!(
            success,
            ExecutionResult::Success {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into()
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

        let quarantined = ExecutionResult::Quarantined {
            message: "unproven external owner".into(),
        };
        assert_eq!(
            quarantined,
            ExecutionResult::Quarantined {
                message: "unproven external owner".into()
            }
        );
    }

    #[tokio::test]
    async fn execute_inner_applies_desired_session_mode_before_sending() {
        let agent = Arc::new(RecordingAgent::new("0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let executor = make_executor_with_agent(AgentRuntimeHandle::Mock(agent.clone()));
        let mut job = sample_job();
        job.agent_config.as_mut().unwrap().mode = Some("yolo".into());

        let result = executor.execute_inner(&job, "0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        assert_eq!(agent.mode().await, "yolo");
        assert_eq!(agent.set_mode_calls(), 1);
        assert_eq!(agent.send_calls(), 1);
    }

    #[tokio::test]
    async fn execute_inner_completed_replay_is_absorbing_before_runtime_preparation() {
        const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
        const COMPLETED_RUN_ID: &str = "0190f5fe-7c00-7a00-8000-000000000021";
        let agent = Arc::new(RecordingAgent::new(CONVERSATION_ID, "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(
            AgentRuntimeHandle::Mock(agent.clone()),
        ));
        let mut job = sample_job();
        let config = job.agent_config.as_mut().expect("sample agent config");
        config.mode = Some("yolo".into());
        config.clear_context_each_run = true;
        let request_payload = serde_json::json!({
            "content": build_prompt(&job, None, true),
            "files": Vec::<String>::new(),
            "inject_skills": Vec::<String>::new(),
            "hidden": true,
            "origin": Some("cron"),
            "channel_platform": Option::<String>::None,
        })
        .to_string();
        let receipt = ConversationDeliveryReceiptRow {
            id: 1,
            operation_id: format!(
                "public-turn:v1:{USER_ID}:{CONVERSATION_ID}:cron:{COMPLETED_RUN_ID}:turn"
            ),
            message_id: "0190f5fe-7c00-7a00-8000-000000000022".into(),
            conversation_id: CONVERSATION_ID.into(),
            user_id: USER_ID.into(),
            kind: "turn".into(),
            request_payload,
            status: "completed".into(),
            result_ok: Some(true),
            result_text: None,
            result_error: None,
            created_at: 1,
            updated_at: 2,
            completed_at: Some(2),
            projected_conversation_id: Some(CONVERSATION_ID.into()),
            projected_message_id: Some(
                "0190f5fe-7c00-7a00-8000-000000000022".into(),
            ),
        };
        let workspace_dir = tempfile::tempdir().expect("replay-receipt workspace fixture");
        let workspace_path = workspace_dir.path().to_string_lossy().into_owned();
        let repo = Arc::new(
            MissingWorkspaceConversationRepo::new(
                CONVERSATION_ID,
                serde_json::json!({ "workspace": workspace_path }),
            )
            .with_delivery_receipt(receipt),
        );
        let executor =
            make_executor_with_runtime_registry_and_repo(runtime_registry.clone(), repo.clone());

        let result = executor
            .execute_inner_with_run_id(&job, COMPLETED_RUN_ID, CONVERSATION_ID, None)
            .await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: CONVERSATION_ID.into()
            }
        );
        assert!(
            runtime_registry.recorded_options().is_empty(),
            "completed replay must not build a runtime or mount prepared knowledge"
        );
        assert_eq!(agent.set_mode_calls(), 0);
        assert_eq!(agent.send_calls(), 0, "completed replay must not redeliver");
        assert!(repo.inserted_messages().is_empty());
        assert!(repo.artifacts().is_empty());
    }

    #[tokio::test]
    async fn accepted_local_restart_orphan_is_quarantined_without_redelivery() {
        const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
        const RUN_ID: &str = "0190f5fe-7c00-7a00-8000-000000000020";
        let agent = Arc::new(RecordingAgent::new(CONVERSATION_ID, "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(
            AgentRuntimeHandle::Mock(agent.clone()),
        ));
        let job = sample_job();
        let request_payload = serde_json::json!({
            "content": build_prompt(&job, None, true),
            "files": Vec::<String>::new(),
            "inject_skills": Vec::<String>::new(),
            "hidden": true,
            "origin": Some("cron"),
            "channel_platform": Option::<String>::None,
        })
        .to_string();
        let receipt = ConversationDeliveryReceiptRow {
            id: 1,
            operation_id: format!(
                "public-turn:v1:{USER_ID}:{CONVERSATION_ID}:cron:{RUN_ID}:turn"
            ),
            message_id: "0190f5fe-7c00-7a00-8000-000000000023".into(),
            conversation_id: CONVERSATION_ID.into(),
            user_id: USER_ID.into(),
            kind: "turn".into(),
            request_payload,
            status: "accepted".into(),
            result_ok: None,
            result_text: None,
            result_error: None,
            created_at: 1,
            updated_at: 1,
            completed_at: None,
            projected_conversation_id: Some(CONVERSATION_ID.into()),
            projected_message_id: Some(
                "0190f5fe-7c00-7a00-8000-000000000023".into(),
            ),
        };
        let workspace_dir = tempfile::tempdir().expect("orphan-replay workspace fixture");
        let workspace_path = workspace_dir.path().to_string_lossy().into_owned();
        let repo = Arc::new(
            MissingWorkspaceConversationRepo::new(
                CONVERSATION_ID,
                serde_json::json!({ "workspace": workspace_path }),
            )
            .with_delivery_receipt(receipt),
        );
        let executor =
            make_executor_with_runtime_registry_and_repo(runtime_registry.clone(), repo.clone());

        let result = executor
            .execute_inner_with_run_id(&job, RUN_ID, CONVERSATION_ID, None)
            .await;

        assert!(
            matches!(
                result,
                ExecutionResult::Quarantined { message }
                    if message.contains("external Conversation turn")
            ),
            "restart cannot prove local process-tree quiescence, so the exact receipt must stay quarantined"
        );
        assert!(runtime_registry.recorded_options().is_empty());
        assert_eq!(agent.set_mode_calls(), 0);
        assert_eq!(agent.send_calls(), 0, "orphan recovery must never redeliver");
        assert!(repo.inserted_messages().is_empty());
        let settled = repo
            .turn
            .lock()
            .expect("orphan receipt state")
            .delivery_receipt
            .clone()
            .expect("orphan receipt");
        assert_eq!(
            settled.status, "accepted",
            "Cron must not manufacture a terminal outcome from process absence after restart"
        );
        assert_eq!(settled.result_ok, None);
    }

    #[tokio::test]
    async fn accepted_external_turn_is_quarantined_without_cron_terminalization() {
        const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
        const RUN_ID: &str = "0190f5fe-7c00-7a00-8000-000000000024";
        let agent = Arc::new(RecordingAgent::new(CONVERSATION_ID, "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(
            AgentRuntimeHandle::Mock(agent.clone()),
        ));
        let job = sample_job();
        let request_payload = serde_json::json!({
            "content": build_prompt(&job, None, true),
            "files": Vec::<String>::new(),
            "inject_skills": Vec::<String>::new(),
            "hidden": true,
            "origin": Some("cron"),
            "channel_platform": Option::<String>::None,
        })
        .to_string();
        let receipt = ConversationDeliveryReceiptRow {
            id: 1,
            operation_id: format!(
                "public-turn:v1:{USER_ID}:{CONVERSATION_ID}:cron:{RUN_ID}:turn"
            ),
            message_id: "0190f5fe-7c00-7a00-8000-000000000025".into(),
            conversation_id: CONVERSATION_ID.into(),
            user_id: USER_ID.into(),
            kind: "turn".into(),
            request_payload,
            status: "accepted".into(),
            result_ok: None,
            result_text: None,
            result_error: None,
            created_at: 1,
            updated_at: 1,
            completed_at: None,
            projected_conversation_id: Some(CONVERSATION_ID.into()),
            projected_message_id: Some(
                "0190f5fe-7c00-7a00-8000-000000000025".into(),
            ),
        };
        let workspace_dir = tempfile::tempdir().expect("external-replay workspace fixture");
        let workspace_path = workspace_dir.path().to_string_lossy().into_owned();
        let repo = Arc::new(
            MissingWorkspaceConversationRepo::new(
                CONVERSATION_ID,
                serde_json::json!({ "workspace": workspace_path }),
            )
            .with_agent_type("remote")
            .with_delivery_receipt(receipt),
        );
        let executor =
            make_executor_with_runtime_registry_and_repo(runtime_registry.clone(), repo.clone());

        let result = executor
            .execute_inner_with_run_id(&job, RUN_ID, CONVERSATION_ID, None)
            .await;

        assert!(
            matches!(
                result,
                ExecutionResult::Quarantined { message }
                    if message.contains("external Conversation turn")
            ),
            "an unproven external owner must remain quarantined"
        );
        assert!(runtime_registry.recorded_options().is_empty());
        assert_eq!(agent.send_calls(), 0);
        let receipt = repo
            .turn
            .lock()
            .expect("external receipt state")
            .delivery_receipt
            .clone()
            .expect("external receipt");
        assert_eq!(
            receipt.status, "accepted",
            "Cron must not settle the Conversation receipt unilaterally"
        );
    }

    #[tokio::test]
    async fn accepted_receipt_probe_errors_never_terminalize_the_cron_run_early() {
        const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
        const RUN_ID: &str = "0190f5fe-7c00-7a00-8000-000000000029";
        let job = sample_job();
        let request = build_cron_send_request(&build_prompt(&job, None, true), &[]);
        let request_payload = serde_json::json!({
            "content": request.content,
            "files": request.files,
            "inject_skills": request.inject_skills,
            "hidden": request.hidden,
            "origin": request.origin,
            "channel_platform": request.channel_platform,
        })
        .to_string();
        let turn_key = format!("cron:{RUN_ID}:turn");
        let receipt = ConversationDeliveryReceiptRow {
            id: 1,
            operation_id: format!(
                "public-turn:v1:{USER_ID}:{CONVERSATION_ID}:{turn_key}"
            ),
            message_id: "0190f5fe-7c00-7a00-8000-000000000028".into(),
            conversation_id: CONVERSATION_ID.into(),
            user_id: USER_ID.into(),
            kind: "turn".into(),
            request_payload,
            status: "accepted".into(),
            result_ok: None,
            result_text: None,
            result_error: None,
            created_at: 1,
            updated_at: 1,
            completed_at: None,
            projected_conversation_id: Some(CONVERSATION_ID.into()),
            projected_message_id: Some(
                "0190f5fe-7c00-7a00-8000-000000000028".into(),
            ),
        };
        let repo = Arc::new(
            MissingWorkspaceConversationRepo::new(
                CONVERSATION_ID,
                serde_json::json!({}),
            )
            .with_delivery_receipt(receipt),
        );
        repo.fail_next_receipt_probes(3);
        let agent = Arc::new(RecordingAgent::new(
            CONVERSATION_ID,
            "default",
            true,
        ));
        let executor = make_executor_with_runtime_registry_and_repo(
            Arc::new(RecordingAgentRuntimeRegistry::new(
                AgentRuntimeHandle::Mock(agent),
            )),
            repo.clone(),
        );

        let completion_repo = repo.clone();
        tokio::spawn(async move {
            while completion_repo.remaining_receipt_probe_failures() != 0 {
                tokio::task::yield_now().await;
            }
            completion_repo.complete_delivery_receipt_ok();
        });

        let delivery = timeout(
            Duration::from_secs(2),
            executor.await_durable_turn_completion(
                &job,
                RUN_ID,
                CONVERSATION_ID,
                &turn_key,
                &request,
                None,
            ),
        )
        .await
        .expect("Cron waiter should survive transient receipt probe failures")
        .expect("completed durable receipt");
        assert!(delivery.completed);
        assert_eq!(delivery.result_ok, Some(true));
    }

    #[tokio::test]
    async fn concurrent_duplicate_cron_run_waits_for_one_durable_turn() {
        const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
        const RUN_ID: &str = "0190f5fe-7c00-7a00-8000-000000000030";
        let agent = Arc::new(RecordingAgent::without_auto_finish(
            CONVERSATION_ID,
            "default",
            true,
        ));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(
            AgentRuntimeHandle::Mock(agent.clone()),
        ));
        let executor = Arc::new(make_executor_with_runtime_registry(
            runtime_registry.clone(),
        ));
        let job = Arc::new(sample_job());

        let first_executor = executor.clone();
        let first_job = job.clone();
        let first = tokio::spawn(async move {
            first_executor
                .execute_inner_with_run_id(&first_job, RUN_ID, CONVERSATION_ID, None)
                .await
        });
        wait_for_agent_send(&agent, 1).await;

        let second_executor = executor.clone();
        let second_job = job.clone();
        let second = tokio::spawn(async move {
            second_executor
                .execute_inner_with_run_id(&second_job, RUN_ID, CONVERSATION_ID, None)
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(
            agent.send_calls(),
            1,
            "same durable Cron run must never redeliver while its receipt is accepted"
        );
        assert!(
            !second.is_finished(),
            "duplicate caller must wait for the authoritative receipt terminal state"
        );

        agent.finish_successfully();
        let first_result = timeout(Duration::from_secs(2), first)
            .await
            .expect("first Cron owner should settle")
            .expect("first Cron task");
        let second_result = timeout(Duration::from_secs(2), second)
            .await
            .expect("duplicate Cron waiter should observe settlement")
            .expect("duplicate Cron task");
        let expected = ExecutionResult::Success {
            conversation_id: CONVERSATION_ID.to_owned(),
        };
        assert_eq!(first_result, expected);
        assert_eq!(second_result, expected);
        assert_eq!(agent.send_calls(), 1);
        assert_eq!(
            runtime_registry.recorded_options().len(),
            1,
            "accepted replay must not build or mutate a second runtime"
        );
    }

    #[tokio::test]
    async fn cron_and_interactive_turn_race_have_one_execution_owner() {
        const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
        const RUN_ID: &str = "0190f5fe-7c00-7a00-8000-000000000031";
        let agent = Arc::new(RecordingAgent::without_auto_finish(
            CONVERSATION_ID,
            "default",
            true,
        ));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(
            AgentRuntimeHandle::Mock(agent.clone()),
        ));
        let executor = Arc::new(make_executor_with_runtime_registry(
            runtime_registry.clone(),
        ));
        let conversation_service = executor.conversation_service.clone();
        let interactive_registry: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
        let barrier = Arc::new(Barrier::new(2));
        let job = Arc::new(sample_job());

        let cron_barrier = barrier.clone();
        let cron_executor = executor.clone();
        let cron_job = job.clone();
        let cron = tokio::spawn(async move {
            cron_barrier.wait().await;
            cron_executor
                .execute_inner_with_run_id(&cron_job, RUN_ID, CONVERSATION_ID, None)
                .await
        });
        let interactive_barrier = barrier.clone();
        let interactive = tokio::spawn(async move {
            interactive_barrier.wait().await;
            conversation_service
                .send_message_with_idempotency_key(
                    USER_ID,
                    CONVERSATION_ID,
                    "interactive-race",
                    SendMessageRequest {
                        content: "interactive message".to_owned(),
                        files: Vec::new(),
                        inject_skills: Vec::new(),
                        hidden: false,
                        origin: None,
                        channel_platform: None,
                    },
                    &interactive_registry,
                )
                .await
        });

        wait_for_agent_send(&agent, 1).await;
        assert_eq!(
            agent.send_calls(),
            1,
            "Cron and interactive admission may have only one model execution owner"
        );
        agent.finish_successfully();

        let cron_result = timeout(Duration::from_secs(2), cron)
            .await
            .expect("Cron race participant should terminate")
            .expect("Cron race task");
        let interactive_result = timeout(Duration::from_secs(2), interactive)
            .await
            .expect("interactive race participant should terminate")
            .expect("interactive race task");
        let cron_won = matches!(cron_result, ExecutionResult::Success { .. });
        let interactive_won = interactive_result.is_ok();
        assert_ne!(
            cron_won, interactive_won,
            "exact durable admission must select one and only one execution owner"
        );
        assert_eq!(agent.send_calls(), 1);
    }

    #[tokio::test]
    async fn execute_inner_applies_mode_even_for_uninitialized_agent() {
        let agent = Arc::new(RecordingAgent::new("0190f5fe-7c00-7a00-8abc-012345678901", "default", false));
        let executor = make_executor_with_agent(AgentRuntimeHandle::Mock(agent.clone()));
        let mut job = sample_job();
        job.agent_config.as_mut().unwrap().mode = Some("yolo".into());

        let result = executor.execute_inner(&job, "0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        assert_eq!(agent.mode().await, "yolo");
        assert_eq!(agent.set_mode_calls(), 1);
        assert_eq!(agent.send_calls(), 1);
    }

    #[tokio::test]
    async fn execute_inner_skips_mode_update_when_already_matching() {
        let agent = Arc::new(RecordingAgent::new("0190f5fe-7c00-7a00-8abc-012345678901", "yolo", true));
        let executor = make_executor_with_agent(AgentRuntimeHandle::Mock(agent.clone()));
        let mut job = sample_job();
        job.agent_config.as_mut().unwrap().mode = Some("yolo".into());

        let result = executor.execute_inner(&job, "0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        assert_eq!(agent.mode().await, "yolo");
        assert_eq!(agent.set_mode_calls(), 0);
        assert_eq!(agent.send_calls(), 1);
    }

    #[tokio::test]
    async fn execute_inner_new_conversation_without_saved_skill_requests_skill_suggest() {
        let agent = Arc::new(RecordingAgent::new("0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(AgentRuntimeHandle::Mock(
            agent.clone(),
        )));
        let executor = make_executor_with_runtime_registry(runtime_registry.clone());
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let result = executor.execute_inner(&job, "0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into()
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
        let agent = Arc::new(RecordingAgent::new("0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
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

        let result = executor.execute_inner(&job, "0190f5fe-7c00-7a00-8abc-012345678901", Some(&saved_skill)).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into()
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
        let agent = Arc::new(RecordingAgent::new("0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let executor = make_executor_with_agent(AgentRuntimeHandle::Mock(agent.clone()));
        let job = sample_job();
        let saved_skill = SavedSkillContext {
            name: JOB_SKILL_NAME.into(),
            raw_content: "---\nname: test\ndescription: desc\n---\nDo X".into(),
        };

        let result = executor.execute_inner(&job, "0190f5fe-7c00-7a00-8abc-012345678901", Some(&saved_skill)).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into()
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
        let agent = Arc::new(RecordingAgent::new("0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let executor = make_executor_with_agent(AgentRuntimeHandle::Mock(agent.clone()));
        let job = sample_job();

        let result = executor.execute_inner(&job, "0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into()
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
        let agent = Arc::new(RecordingAgent::new("0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(AgentRuntimeHandle::Mock(
            agent.clone(),
        )));
        let repo = Arc::new(ExistingConversationRepo::new());
        let expected_workspace = repo.workspace_path();
        let executor =
            make_executor_with_runtime_registry_and_repo(runtime_registry.clone(), repo);
        let mut job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        job.agent_config.as_mut().unwrap().workspace = None;

        let result = executor.execute_inner(&job, "0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into()
            }
        );
        wait_for_agent_send(&agent, 1).await;
        let options = runtime_registry
            .last_options()
            .expect("runtime registry should capture build options");
        assert_eq!(options.workspace, expected_workspace);
    }

    #[tokio::test]
    async fn execute_inner_missing_workspace_identity_fails_closed() {
        let agent = Arc::new(RecordingAgent::new("0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(AgentRuntimeHandle::Mock(
            agent.clone(),
        )));
        let repo = Arc::new(MissingWorkspaceConversationRepo::new(
            "0190f5fe-7c00-7a00-8abc-012345678901",
            serde_json::json!({}),
        ));
        let executor = make_executor_with_runtime_registry_and_repo(runtime_registry.clone(), repo.clone());
        let mut job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        job.agent_config.as_mut().unwrap().workspace = None;

        let result = executor.execute_inner(&job, "0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert!(matches!(
            result,
            ExecutionResult::Error { message }
                if message.contains("has no canonical workspace")
        ));
        assert_eq!(agent.send_calls(), 0);
        assert!(runtime_registry.last_options().is_none());
        assert!(repo.last_update_with_extra().is_none());
    }

    #[tokio::test]
    async fn execute_inner_rebases_managed_workspace_and_ignores_source_absolute_path() {
        const WORKSPACE_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
        let work_dir =
            tempfile::tempdir().expect("managed conversation workspace root");
        let expected_path = work_dir.path().join("conversations").join(WORKSPACE_ID);
        std::fs::create_dir_all(&expected_path)
            .expect("create managed conversation workspace fixture");
        let agent = Arc::new(RecordingAgent::new(
            "0190f5fe-7c00-7a00-8abc-012345678901",
            "default",
            true,
        ));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(
            AgentRuntimeHandle::Mock(agent.clone()),
        ));
        let repo = Arc::new(MissingWorkspaceConversationRepo::new(
            "0190f5fe-7c00-7a00-8abc-012345678901",
            serde_json::json!({
                "backend": "claude",
                "temp_workspace_id": WORKSPACE_ID,
                "workspace": "/source-install/conversations/0190f5fe-7c00-7a00-8abc-000000000000"
            }),
        ));
        let executor = make_executor_with_runtime_registry_and_repo_with_work_dir(
            runtime_registry.clone(),
            repo,
            work_dir.path().to_path_buf(),
        );
        let mut job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        job.agent_config.as_mut().unwrap().workspace = None;

        let result = executor
            .execute_inner(
                &job,
                "0190f5fe-7c00-7a00-8abc-012345678901",
                None,
            )
            .await;

        assert!(matches!(result, ExecutionResult::Success { .. }));
        wait_for_agent_send(&agent, 1).await;
        let options = runtime_registry
            .last_options()
            .expect("runtime registry should capture build options");
        let expected = expected_path.to_string_lossy().into_owned();
        assert_eq!(options.workspace, expected);
        assert_eq!(options.extra["temp_workspace_id"], WORKSPACE_ID);
        assert_ne!(
            options.workspace,
            "/source-install/conversations/0190f5fe-7c00-7a00-8abc-000000000000"
        );
    }

    #[tokio::test]
    async fn execute_inner_inserts_right_side_user_message_for_cron_prompt() {
        let agent = Arc::new(RecordingAgent::new("0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(AgentRuntimeHandle::Mock(
            agent.clone(),
        )));
        let workspace_dir =
            tempfile::tempdir().expect("right-side prompt workspace fixture");
        let workspace_path = workspace_dir.path().to_string_lossy().into_owned();
        let repo = Arc::new(MissingWorkspaceConversationRepo::new(
            "0190f5fe-7c00-7a00-8abc-012345678901",
            serde_json::json!({ "workspace": workspace_path }),
        ));
        let executor = make_executor_with_runtime_registry_and_repo(runtime_registry, repo.clone());
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };

        let result = executor.execute_inner(&job, "0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into()
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
        let agent = Arc::new(RecordingAgent::new("0190f5fe-7c00-7a00-8abc-012345678901", "default", true));
        let runtime_registry = Arc::new(RecordingAgentRuntimeRegistry::new(AgentRuntimeHandle::Mock(
            agent.clone(),
        )));
        let workspace_dir =
            tempfile::tempdir().expect("cron-trigger artifact workspace fixture");
        let workspace_path = workspace_dir.path().to_string_lossy().into_owned();
        let repo = Arc::new(MissingWorkspaceConversationRepo::new(
            "0190f5fe-7c00-7a00-8abc-012345678901",
            serde_json::json!({ "workspace": workspace_path }),
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

        let result = executor.execute_inner(&job, "0190f5fe-7c00-7a00-8abc-012345678901", None).await;

        assert_eq!(
            result,
            ExecutionResult::Success {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into()
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
            "0190f5fe-7c00-7a00-8abc-012345678901"
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
                Ok("0190f5fe-7c00-7a00-8abc-012345678901".into())
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
        auto_finish: bool,
        set_mode_calls: AtomicUsize,
        send_calls: AtomicUsize,
    }

    impl RecordingAgent {
        fn new(conversation_id: &str, mode: &str, initialized: bool) -> Self {
            Self::with_auto_finish(conversation_id, mode, initialized, true)
        }

        fn without_auto_finish(
            conversation_id: &str,
            mode: &str,
            initialized: bool,
        ) -> Self {
            Self::with_auto_finish(conversation_id, mode, initialized, false)
        }

        fn with_auto_finish(
            conversation_id: &str,
            mode: &str,
            initialized: bool,
            auto_finish: bool,
        ) -> Self {
            let (event_tx, _) = broadcast::channel(16);
            Self {
                conversation_id: conversation_id.to_owned(),
                workspace: "/tmp/cron-test".to_owned(),
                event_tx,
                mode: RwLock::new(mode.to_owned()),
                sent_messages: RwLock::new(Vec::new()),
                initialized,
                auto_finish,
                set_mode_calls: AtomicUsize::new(0),
                send_calls: AtomicUsize::new(0),
            }
        }

        fn finish_successfully(&self) {
            let _ = self.event_tx.send(AgentStreamEvent::Text(TextEventData {
                content: "cron test completed".to_owned(),
            }));
            let _ = self
                .event_tx
                .send(AgentStreamEvent::Finish(FinishEventData::default()));
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

        fn is_transport_healthy(&self) -> bool {
            true
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
            if self.auto_finish {
                self.finish_successfully();
            }
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

        fn terminate_and_wait_result(
            &self,
            _: &str,
            _: Option<AgentKillReason>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), nomifun_common::AppError>>
                    + Send,
            >,
        > {
            Box::pin(std::future::ready(Ok(())))
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

        fn terminate_and_wait_result(
            &self,
            _: &str,
            _: Option<AgentKillReason>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), nomifun_common::AppError>>
                    + Send,
            >,
        > {
            Box::pin(std::future::ready(Ok(())))
        }

        fn terminate_all(&self) {}

        fn active_runtime_count(&self) -> usize {
            1
        }

        fn collect_idle_runtimes(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
            Vec::new()
        }
    }

    struct TestTurnState {
        status: Option<String>,
        epoch: i64,
        active_operation_id: Option<String>,
        delivery_receipt: Option<ConversationDeliveryReceiptRow>,
    }

    impl TestTurnState {
        fn terminal() -> Self {
            Self {
                status: Some("finished".to_owned()),
                epoch: 0,
                active_operation_id: None,
                delivery_receipt: None,
            }
        }
    }

    fn test_turn_admission_state(
        state: &Mutex<TestTurnState>,
    ) -> ConversationTurnAdmissionState {
        let state = state.lock().expect("test turn state");
        ConversationTurnAdmissionState {
            epoch: state.epoch,
            active_operation_id: state.active_operation_id.clone(),
        }
    }

    fn test_get_delivery_receipt(
        state: &Mutex<TestTurnState>,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
    ) -> Option<ConversationDeliveryReceiptRow> {
        state
            .lock()
            .expect("test turn state")
            .delivery_receipt
            .clone()
            .filter(|receipt| {
                receipt.user_id == user_id
                    && receipt.conversation_id == conversation_id
                    && receipt.operation_id == operation_id
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn test_claim_turn(
        state: &Mutex<TestTurnState>,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        candidate_message_id: &str,
        request_payload: &str,
        expected_admission_epoch: i64,
        now: i64,
    ) -> Result<ConversationDeliveryReceiptClaim, nomifun_db::DbError> {
        let mut state = state.lock().expect("test turn state");
        if let Some(receipt) = state.delivery_receipt.as_ref() {
            if receipt.user_id != user_id
                || receipt.conversation_id != conversation_id
                || receipt.operation_id != operation_id
                || receipt.kind != "turn"
                || receipt.request_payload != request_payload
            {
                return Err(nomifun_db::DbError::Conflict(
                    "test turn operation identity was reused".to_owned(),
                ));
            }
            return Ok(ConversationDeliveryReceiptClaim {
                receipt: receipt.clone(),
                claimed_new: false,
            });
        }
        if state.epoch != expected_admission_epoch
            || state.active_operation_id.is_some()
            || state.status.as_deref() == Some("running")
        {
            return Err(nomifun_db::DbError::Conflict(
                "test Conversation admission authority is stale".to_owned(),
            ));
        }

        state.epoch += 1;
        state.status = Some("running".to_owned());
        state.active_operation_id = Some(operation_id.to_owned());
        let receipt = ConversationDeliveryReceiptRow {
            id: 1,
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
            projected_message_id: Some(candidate_message_id.to_owned()),
        };
        state.delivery_receipt = Some(receipt.clone());
        Ok(ConversationDeliveryReceiptClaim {
            receipt,
            claimed_new: true,
        })
    }

    fn test_finalize_turn(
        state: &Mutex<TestTurnState>,
        user_id: &str,
        conversation_id: &str,
        completion: &TurnReceiptCompletion,
        completed_at: i64,
    ) -> Result<TurnLifecycleTransition, nomifun_db::DbError> {
        let mut state = state.lock().expect("test turn state");
        let Some(receipt) = state.delivery_receipt.as_ref() else {
            return Ok(TurnLifecycleTransition::Stale);
        };
        if receipt.user_id != user_id
            || receipt.conversation_id != conversation_id
            || receipt.operation_id != completion.operation_id
            || receipt.kind != completion.kind
            || receipt.request_payload != completion.request_payload
        {
            return Ok(TurnLifecycleTransition::Stale);
        }
        if receipt.status == "completed" {
            return Ok(TurnLifecycleTransition::AlreadyApplied);
        }
        if state.active_operation_id.as_deref() != Some(completion.operation_id.as_str())
            || state.status.as_deref() != Some("running")
        {
            return Ok(TurnLifecycleTransition::Stale);
        }
        let receipt = state
            .delivery_receipt
            .as_mut()
            .expect("receipt checked above");
        receipt.status = "completed".to_owned();
        receipt.result_ok = Some(completion.result_ok);
        receipt.result_text = completion.result_text.clone();
        receipt.result_error = completion.result_error.clone();
        receipt.updated_at = completed_at;
        receipt.completed_at = Some(completed_at);
        state.status = Some("finished".to_owned());
        state.active_operation_id = None;
        Ok(TurnLifecycleTransition::Committed)
    }

    #[allow(clippy::too_many_arguments)]
    fn test_finalize_cancelled_turn(
        state: &Mutex<TestTurnState>,
        user_id: &str,
        conversation_id: &str,
        expected_admission_epoch: i64,
        expected_active_operation_id: Option<&str>,
        reason: &str,
        completed_at: i64,
    ) -> Result<TurnLifecycleTransition, nomifun_db::DbError> {
        let mut state = state.lock().expect("test turn state");
        if state.epoch != expected_admission_epoch
            || state.active_operation_id.as_deref() != expected_active_operation_id
        {
            return Ok(TurnLifecycleTransition::Stale);
        }
        let Some(receipt) = state.delivery_receipt.as_mut() else {
            return Ok(TurnLifecycleTransition::Stale);
        };
        if receipt.user_id != user_id
            || receipt.conversation_id != conversation_id
            || Some(receipt.operation_id.as_str()) != expected_active_operation_id
        {
            return Ok(TurnLifecycleTransition::Stale);
        }
        if receipt.status == "completed" {
            return Ok(TurnLifecycleTransition::AlreadyApplied);
        }
        receipt.status = "completed".to_owned();
        receipt.result_ok = Some(false);
        receipt.result_text = None;
        receipt.result_error = Some(reason.to_owned());
        receipt.updated_at = completed_at;
        receipt.completed_at = Some(completed_at);
        state.status = Some("finished".to_owned());
        state.active_operation_id = None;
        Ok(TurnLifecycleTransition::Committed)
    }

    struct ExistingConversationRepo {
        workspace: tempfile::TempDir,
        turn: Mutex<TestTurnState>,
    }

    impl ExistingConversationRepo {
        fn new() -> Self {
            Self {
                workspace: tempfile::tempdir()
                    .expect("existing-conversation workspace fixture"),
                turn: Mutex::new(TestTurnState::terminal()),
            }
        }

        fn workspace_path(&self) -> String {
            self.workspace.path().to_string_lossy().into_owned()
        }
    }

    #[async_trait::async_trait]
    impl IConversationRepository for ExistingConversationRepo {
        async fn get(
            &self,
            id: &str,
        ) -> Result<Option<nomifun_db::models::ConversationRow>, nomifun_db::DbError> {
            let workspace = self.workspace_path();
            let status = self
                .turn
                .lock()
                .expect("existing-conversation turn state")
                .status
                .clone();
            Ok(Some(nomifun_db::models::ConversationRow {
                id: 0,
                conversation_id: id.to_owned(),
                user_id: USER_ID.into(),
                name: "Cron Conversation".into(),
                r#type: "acp".into(),
                extra: serde_json::json!({
                    "workspace": workspace,
                    "agent_id": TEST_ACP_AGENT_ID,
                    "backend": "claude",
                    "agent_source": "builtin"
                })
                .to_string(),
                delegation_policy: "automatic".into(),
                execution_model_pool: None,
                decision_policy: "automatic".into(),
                execution_template_id: None,
                model: None,
                status,
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
            Ok("0190f5fe-7c00-7a00-8abc-012345678901".into())
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

        async fn has_accepted_delivery_receipt_operation_prefix(
            &self,
            user_id: &str,
            conversation_id: &str,
            operation_id_prefix: &str,
        ) -> Result<bool, nomifun_db::DbError> {
            Ok(self
                .turn
                .lock()
                .expect("existing-conversation turn state")
                .delivery_receipt
                .as_ref()
                .is_some_and(|receipt| {
                    receipt.user_id == user_id
                        && receipt.conversation_id == conversation_id
                        && receipt.operation_id.starts_with(operation_id_prefix)
                        && receipt.status == "accepted"
                }))
        }

        async fn claim_delivery_receipt_once(
            &self,
            user_id: &str,
            conversation_id: &str,
            operation_id: &str,
            kind: &str,
            request_payload: &str,
            now: i64,
        ) -> Result<ConversationDeliveryReceiptClaim, nomifun_db::DbError> {
            Ok(claimed_test_delivery_receipt(
                user_id,
                conversation_id,
                operation_id,
                kind,
                request_payload,
                now,
            ))
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
        ) -> Result<ConversationDeliveryReceiptClaim, nomifun_db::DbError> {
            test_claim_turn(
                &self.turn,
                user_id,
                conversation_id,
                operation_id,
                candidate_message_id,
                request_payload,
                expected_admission_epoch,
                now,
            )
        }

        async fn get_delivery_receipt(
            &self,
            user_id: &str,
            conversation_id: &str,
            operation_id: &str,
        ) -> Result<Option<ConversationDeliveryReceiptRow>, nomifun_db::DbError> {
            Ok(test_get_delivery_receipt(
                &self.turn,
                user_id,
                conversation_id,
                operation_id,
            ))
        }

        async fn get_turn_admission_state(
            &self,
            _user_id: &str,
            _conversation_id: &str,
        ) -> Result<ConversationTurnAdmissionState, nomifun_db::DbError> {
            Ok(test_turn_admission_state(&self.turn))
        }

        async fn validate_active_turn_operation(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            operation_id: &str,
        ) -> Result<bool, nomifun_db::DbError> {
            Ok(self
                .turn
                .lock()
                .expect("existing-conversation turn state")
                .active_operation_id
                .as_deref()
                == Some(operation_id))
        }

        async fn finalize_exact_turn_operation(
            &self,
            user_id: &str,
            conversation_id: &str,
            completion: &TurnReceiptCompletion,
            completed_at: TimestampMs,
        ) -> Result<TurnLifecycleTransition, nomifun_db::DbError> {
            test_finalize_turn(
                &self.turn,
                user_id,
                conversation_id,
                completion,
                completed_at,
            )
        }
    }

    struct MissingWorkspaceConversationRepo {
        row: nomifun_db::models::ConversationRow,
        updates: Mutex<Vec<ConversationRowUpdate>>,
        inserted_messages: Mutex<Vec<nomifun_db::models::MessageRow>>,
        artifacts: Mutex<Vec<ConversationArtifactRow>>,
        turn: Mutex<TestTurnState>,
        receipt_probe_failures: AtomicUsize,
    }

    impl MissingWorkspaceConversationRepo {
        fn new(conversation_id: &str, extra: serde_json::Value) -> Self {
            let mut extra = extra;
            let extra = extra
                .as_object_mut()
                .expect("conversation fixture extra must be a JSON object");
            extra
                .entry("agent_id")
                .or_insert_with(|| serde_json::Value::String(TEST_ACP_AGENT_ID.to_owned()));
            extra
                .entry("backend")
                .or_insert_with(|| serde_json::Value::String("claude".to_owned()));
            extra
                .entry("agent_source")
                .or_insert_with(|| serde_json::Value::String("builtin".to_owned()));
            Self {
                row: nomifun_db::models::ConversationRow {
                    id: 0,
                    conversation_id: conversation_id.to_owned(),
                    user_id: USER_ID.into(),
                    name: "Cron Conversation".into(),
                    r#type: "acp".into(),
                    extra: serde_json::Value::Object(extra.clone()).to_string(),
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
                turn: Mutex::new(TestTurnState::terminal()),
                receipt_probe_failures: AtomicUsize::new(0),
            }
        }

        fn with_delivery_receipt(self, receipt: ConversationDeliveryReceiptRow) -> Self {
            let mut turn = self.turn.lock().expect("missing-workspace turn state");
            if receipt.status == "accepted" {
                turn.status = Some("running".to_owned());
                turn.epoch = 1;
                turn.active_operation_id = Some(receipt.operation_id.clone());
            }
            turn.delivery_receipt = Some(receipt);
            drop(turn);
            self
        }

        fn with_agent_type(mut self, agent_type: &str) -> Self {
            self.row.r#type = agent_type.to_owned();
            self
        }

        fn complete_delivery_receipt_ok(&self) {
            let mut turn = self.turn.lock().expect("missing-workspace turn state");
            let receipt = turn
                .delivery_receipt
                .as_mut()
                .expect("delivery receipt fixture");
            receipt.status = "completed".to_owned();
            receipt.result_ok = Some(true);
            receipt.result_text = None;
            receipt.result_error = None;
            receipt.updated_at += 1;
            receipt.completed_at = Some(receipt.updated_at);
            turn.status = Some("finished".to_owned());
            turn.active_operation_id = None;
        }

        fn fail_next_receipt_probes(&self, count: usize) {
            self.receipt_probe_failures.store(count, Ordering::SeqCst);
        }

        fn remaining_receipt_probe_failures(&self) -> usize {
            self.receipt_probe_failures.load(Ordering::SeqCst)
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

        fn artifacts(&self) -> Vec<ConversationArtifactRow> {
            self.artifacts
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
            let mut row = self.row.clone();
            row.status = self
                .turn
                .lock()
                .expect("missing-workspace turn state")
                .status
                .clone();
            Ok(Some(row))
        }

        async fn create(
            &self,
            _row: &nomifun_db::models::ConversationRow,
        ) -> Result<String, nomifun_db::DbError> {
            Ok("0190f5fe-7c00-7a00-8abc-012345678901".into())
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

        async fn has_accepted_delivery_receipt_operation_prefix(
            &self,
            user_id: &str,
            conversation_id: &str,
            operation_id_prefix: &str,
        ) -> Result<bool, nomifun_db::DbError> {
            Ok(self
                .turn
                .lock()
                .expect("missing-workspace turn state")
                .delivery_receipt
                .as_ref()
                .is_some_and(|receipt| {
                    receipt.user_id == user_id
                        && receipt.conversation_id == conversation_id
                        && receipt.operation_id.starts_with(operation_id_prefix)
                        && receipt.status == "accepted"
                }))
        }

        async fn claim_delivery_receipt_once(
            &self,
            user_id: &str,
            conversation_id: &str,
            operation_id: &str,
            kind: &str,
            request_payload: &str,
            now: i64,
        ) -> Result<ConversationDeliveryReceiptClaim, nomifun_db::DbError> {
            Ok(claimed_test_delivery_receipt(
                user_id,
                conversation_id,
                operation_id,
                kind,
                request_payload,
                now,
            ))
        }

        async fn get_delivery_receipt(
            &self,
            user_id: &str,
            conversation_id: &str,
            operation_id: &str,
        ) -> Result<Option<ConversationDeliveryReceiptRow>, nomifun_db::DbError> {
            if self
                .receipt_probe_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    if remaining > 0 {
                        Some(remaining - 1)
                    } else {
                        None
                    }
                })
                .is_ok()
            {
                return Err(nomifun_db::DbError::Init(
                    "injected durable receipt probe failure".to_owned(),
                ));
            }
            Ok(test_get_delivery_receipt(
                &self.turn,
                user_id,
                conversation_id,
                operation_id,
            ))
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
        ) -> Result<ConversationDeliveryReceiptClaim, nomifun_db::DbError> {
            test_claim_turn(
                &self.turn,
                user_id,
                conversation_id,
                operation_id,
                candidate_message_id,
                request_payload,
                expected_admission_epoch,
                now,
            )
        }

        async fn get_turn_admission_state(
            &self,
            _user_id: &str,
            _conversation_id: &str,
        ) -> Result<ConversationTurnAdmissionState, nomifun_db::DbError> {
            Ok(test_turn_admission_state(&self.turn))
        }

        async fn validate_active_turn_operation(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            operation_id: &str,
        ) -> Result<bool, nomifun_db::DbError> {
            Ok(self
                .turn
                .lock()
                .expect("missing-workspace turn state")
                .active_operation_id
                .as_deref()
                == Some(operation_id))
        }

        async fn finalize_exact_turn_operation(
            &self,
            user_id: &str,
            conversation_id: &str,
            completion: &TurnReceiptCompletion,
            completed_at: TimestampMs,
        ) -> Result<TurnLifecycleTransition, nomifun_db::DbError> {
            test_finalize_turn(
                &self.turn,
                user_id,
                conversation_id,
                completion,
                completed_at,
            )
        }

        async fn finalize_exact_cancelled_turn_generation(
            &self,
            user_id: &str,
            conversation_id: &str,
            expected_admission_epoch: i64,
            expected_active_operation_id: Option<&str>,
            reason: &str,
            completed_at: TimestampMs,
        ) -> Result<TurnLifecycleTransition, nomifun_db::DbError> {
            test_finalize_cancelled_turn(
                &self.turn,
                user_id,
                conversation_id,
                expected_admission_epoch,
                expected_active_operation_id,
                reason,
                completed_at,
            )
        }

        async fn upsert_artifact(
            &self,
            artifact: &ConversationArtifactRow,
        ) -> Result<ConversationArtifactRow, nomifun_db::DbError> {
            let mut artifacts = self.artifacts.lock().unwrap();
            if let Some(existing) = artifacts.iter_mut().find(|row| {
                row.conversation_artifact_id == artifact.conversation_artifact_id
                    || (artifact.kind == "skill_suggest"
                        && row.kind == "skill_suggest"
                        && row.conversation_id == artifact.conversation_id
                        && row.cron_job_id == artifact.cron_job_id)
            }) {
                let conversation_artifact_id = existing.conversation_artifact_id.clone();
                *existing = artifact.clone();
                existing.conversation_artifact_id = conversation_artifact_id;
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
        make_executor_with_runtime_registry_and_repo(
            runtime_registry,
            Arc::new(ExistingConversationRepo::new()),
        )
    }

    fn make_executor_with_runtime_registry_and_repo(
        runtime_registry: Arc<dyn AgentRuntimeRegistry>,
        repo: Arc<dyn IConversationRepository>,
    ) -> JobExecutor {
        let broadcaster = Arc::new(StubBroadcaster);
        make_executor_with_runtime_registry_repo_and_broadcaster(runtime_registry, repo, broadcaster)
    }

    fn make_executor_with_runtime_registry_and_repo_with_work_dir(
        runtime_registry: Arc<dyn AgentRuntimeRegistry>,
        repo: Arc<dyn IConversationRepository>,
        work_dir: PathBuf,
    ) -> JobExecutor {
        let broadcaster = Arc::new(StubBroadcaster);
        make_executor_with_runtime_registry_repo_broadcaster_and_work_dir(
            runtime_registry,
            repo,
            broadcaster,
            work_dir,
        )
    }

    fn make_executor_with_runtime_registry_repo_and_broadcaster<B>(
        runtime_registry: Arc<dyn AgentRuntimeRegistry>,
        repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<B>,
    ) -> JobExecutor
    where
        B: nomifun_realtime::UserEventSink + 'static,
    {
        make_executor_with_runtime_registry_repo_broadcaster_and_work_dir(
            runtime_registry,
            repo,
            broadcaster,
            std::env::temp_dir(),
        )
    }

    fn make_executor_with_runtime_registry_repo_broadcaster_and_work_dir<B>(
        runtime_registry: Arc<dyn AgentRuntimeRegistry>,
        repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<B>,
        work_dir: PathBuf,
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
            work_dir.clone(),
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
            work_dir.clone(),
            work_dir,
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
