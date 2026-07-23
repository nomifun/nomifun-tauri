use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::RwLock;

use nomifun_api_types::{
    CreateCronJobRequest, CronJobResponse, CronJobRunResponse, CronScheduleDto, HasSkillResponse,
    ListCronJobsQuery, RunNowResponse, SaveCronSkillRequest, UpdateCronJobRequest,
};
use nomifun_common::{
    AgentType, AppError, ConversationId, CronJobId, CronJobRunId, ExecutionAuthority, ProviderId,
    UserId, now_ms,
    workspace_path_has_edge_whitespace_segment,
};
use nomifun_db::{
    CRON_RUN_HISTORY_LIMIT, CronJobRunRow, ICronRepository, UpdateCronJobParams,
    models::CronJobRow,
};
use tracing::{error, info, warn};

use crate::events::CronEventEmitter;

use crate::error::CronError;
use crate::executor::{ExecutionResult, JobExecutor, RETRY_INTERVAL_MS};
use crate::scheduler::{CronScheduler, compute_next_run, validate_schedule};
use crate::skill_file::{
    CRON_SKILL_DIR_PREFIX, CRON_SKILLS_REL_DIR, build_skill_content, delete_skill_file,
    validate_skill_content, write_raw_skill_file,
};
use crate::types::{
    CreatedBy, CronAgentConfig, CronJob, CronSchedule, ExecutionMode, cron_job_from_row,
    cron_job_to_response, cron_job_to_row, schedule_from_dto,
};

const PLACEHOLDER_PATTERNS: &[&str] = &[
    "todo:",
    "todo ",
    "fill in",
    "placeholder",
    "replace this",
    "your ",
    "insert ",
    "add your",
    "write your",
    "put your",
];

fn validate_cron_job_id(job_id: &str) -> Result<String, CronError> {
    CronJobId::parse(job_id.trim())
        .map(CronJobId::into_string)
        .map_err(|error| {
            CronError::App(AppError::BadRequest(format!(
                "invalid cron job id: {error}"
            )))
        })
}

fn validate_cron_user_id(user_id: &str) -> Result<&str, CronError> {
    UserId::try_from(user_id)
        .map(|_| user_id)
        .map_err(|error| CronError::App(AppError::Forbidden(format!("invalid cron caller: {error}"))))
}

fn validate_conversation_id(conversation_id: &str) -> Result<&str, CronError> {
    ConversationId::try_from(conversation_id)
        .map(|_| conversation_id)
        .map_err(|error| {
            CronError::App(AppError::BadRequest(format!(
                "invalid conversation id: {error}"
            )))
        })
}

#[derive(Clone)]
pub struct CronService {
    authoritative_user_id: Arc<str>,
    repo: Arc<dyn ICronRepository>,
    scheduler: Arc<CronScheduler>,
    executor: Arc<JobExecutor>,
    emitter: CronEventEmitter,
    data_dir: PathBuf,
    preset_service: Arc<RwLock<Option<Arc<nomifun_preset::PresetService>>>>,
}

#[derive(Debug)]
struct SuccessFinalizationFailure {
    message: String,
    execution_already_counted: bool,
    clear_conversation_id: bool,
}

impl CronService {
    pub fn new(
        authoritative_user_id: Arc<str>,
        repo: Arc<dyn ICronRepository>,
        scheduler: Arc<CronScheduler>,
        executor: Arc<JobExecutor>,
        emitter: CronEventEmitter,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            authoritative_user_id,
            repo,
            scheduler,
            executor,
            emitter,
            data_dir,
            preset_service: Arc::new(RwLock::new(None)),
        }
    }

    fn execution_authority(&self, user_id: &str) -> ExecutionAuthority {
        ExecutionAuthority::resolve(user_id, self.authoritative_user_id.as_ref())
    }

    fn require_host_control(&self, user_id: &str) -> Result<(), CronError> {
        if self.execution_authority(user_id).controls_host() {
            Ok(())
        } else {
            Err(CronError::App(AppError::Forbidden(
                "Cron skill management requires the installation owner".into(),
            )))
        }
    }

    pub fn with_preset_service(&self, service: Arc<nomifun_preset::PresetService>) {
        if let Ok(mut guard) = self.preset_service.write() {
            *guard = Some(service);
        }
    }

    async fn emit_job_created_for(&self, job: &CronJob) {
        self.emitter
            .emit_job_created(&job.user_id, &cron_job_to_response(job));
    }

    async fn emit_job_updated_for(&self, job: &CronJob) {
        self.emitter
            .emit_job_updated(&job.user_id, &cron_job_to_response(job));
    }

    /// Execution updates are persisted in several repository writes (status,
    /// lazy conversation binding, and next run). Reload before broadcasting so
    /// clients never receive a stale pre-execution aggregate.
    async fn emit_persisted_job_updated_for(&self, job: &CronJob) {
        let row = match self.repo.get_by_cron_job_id(&job.user_id, &job.cron_job_id).await {
            Ok(Some(row)) => row,
            Ok(None) => return,
            Err(error) => {
                warn!(
                    job_id = %job.cron_job_id,
                    error = %error,
                    "Failed to reload cron job for update event"
                );
                return;
            }
        };
        match cron_job_from_row(row) {
            Ok(persisted) => self.emit_job_updated_for(&persisted).await,
            Err(error) => warn!(
                job_id = %job.cron_job_id,
                error = %error,
                "Refusing to emit an invalid persisted cron job"
            ),
        }
    }

    async fn emit_job_removed_for(&self, job: &CronJob) {
        self.emitter.emit_job_removed(&job.user_id, &job.cron_job_id);
    }

    async fn emit_job_executed_for(&self, job: &CronJob, status: &str, error: Option<&str>) {
        self.emitter
            .emit_job_executed(&job.user_id, &job.cron_job_id, status, error);
    }

    async fn resolve_preset_config(
        &self,
        config: &mut nomifun_api_types::CronAgentConfigDto,
    ) -> Result<(), CronError> {
        let Some(preset_id) = config.preset_id.clone() else { return Ok(()) };
        let service = self
            .preset_service
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
            .ok_or_else(|| CronError::Scheduler("preset service is not wired".into()))?;
        let snapshot = service
            .resolve(
                &preset_id,
                nomifun_api_types::PresetTarget::Cron,
                None,
                nomifun_api_types::PresetOverrides::default(),
            )
            .await?;
        config.name = snapshot.preset_name.clone();
        config.custom_agent_id = snapshot.resolved_agent_id.clone();
        let is_nomi = snapshot
            .resolved_agent_type
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("nomi"));
        if is_nomi {
            config.backend = None;
        } else if let Some(backend) = snapshot
                .resolved_agent_backend
                .clone()
                .or(snapshot.resolved_agent_type.clone())
        {
            config.backend = Some(backend);
            config.provider_id = None;
        }
        if let Some(model) = snapshot.resolved_model.as_ref() {
            if is_nomi && let Some(provider_id) = model.provider_id.as_ref() {
                config.provider_id = Some(provider_id.clone());
            };
            config.model = Some(model.model.clone());
        }
        config.preset_revision = Some(snapshot.preset_revision);
        config.preset_snapshot = Some(snapshot);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // CRUD
    // -----------------------------------------------------------------------

    pub async fn add_job(
        &self,
        user_id: &str,
        mut req: CreateCronJobRequest,
    ) -> Result<CronJob, CronError> {
        let user_id = validate_cron_user_id(user_id)?;
        let execution_mode = parse_execution_mode(req.execution_mode.as_deref())?;
        if matches!(execution_mode, ExecutionMode::NewConversation) && req.conversation_id.is_some() {
            return Err(CronError::App(AppError::BadRequest(
                "new_conversation cron jobs must not specify conversation_id".into(),
            )));
        }
        let controls_host = self.execution_authority(user_id).controls_host();
        if !controls_host {
            if req.agent_type != AgentType::Nomi.serde_name() {
                return Err(CronError::App(AppError::Forbidden(
                    "Non-owner scheduled tasks are Nomi model-only".into(),
                )));
            }
            if let Some(config) = req.agent_config.as_mut() {
                clamp_model_only_cron_config(config);
            }
        }
        if controls_host && let Some(config) = req.agent_config.as_mut() {
            // Only `preset_id` is trusted from the client; always replace an
            // incoming snapshot with a fresh server-side resolution.
            config.preset_snapshot = None;
            config.preset_revision = None;
            self.resolve_preset_config(config).await?;
        }
        validate_agent_config_shape(&req.agent_type, req.agent_config.as_ref())?;
        let schedule = schedule_from_dto(&req.schedule);
        validate_schedule(&schedule)?;

        let conversation_id = req.conversation_id;
        if let Some(conversation_id) = conversation_id.as_deref() {
            validate_conversation_id(conversation_id)?;
        }

        if let Some(conversation_id) = conversation_id.as_deref() {
            let row = self
                .executor
                .get_conversation_row(conversation_id)
                .await?
                .filter(|row| row.user_id == user_id)
                .ok_or_else(|| {
                    CronError::JobNotFound(format!(
                        "conversation {conversation_id} is not owned by the caller"
                    ))
                })?;
            debug_assert_eq!(row.user_id, user_id);
        }

        // The model source depends on execution mode: an Existing job bound to
        // a conversation takes its model from that conversation at run time, so
        // `agent_config` may legitimately be absent (the desktop "指定会话"
        // flow omits it). Only NewConversation / lazy-bind jobs require
        // `agent_config.provider_id` and `agent_config.model`.
        self.validate_nomi_job_model(
            &req.agent_type,
            execution_mode,
            conversation_id.as_deref(),
            req.agent_config
                .as_ref()
                .and_then(|config| config.backend.as_deref()),
            req.agent_config
                .as_ref()
                .and_then(|config| config.provider_id.as_deref()),
            req.agent_config
                .as_ref()
                .and_then(|config| config.model.as_deref()),
        )
        .await?;

        let created_by = CreatedBy::from_str(&req.created_by)?;
        let message = req.message.or(req.prompt).unwrap_or_default();

        let agent_config = req.agent_config.map(|c| CronAgentConfig {
            backend: c.backend,
            name: c.name,
            cli_path: c.cli_path,
            custom_agent_id: c.custom_agent_id,
            preset_id: c.preset_id,
            preset_revision: c.preset_revision,
            preset_snapshot: c.preset_snapshot,
            mode: c.mode,
            model: c.model,
            provider_id: c.provider_id,
            config_options: c.config_options,
            workspace: c.workspace,
            clear_context_each_run: c.clear_context_each_run,
        });

        let now = now_ms();
        let next_run_at = compute_next_run(&schedule, now);

        let mut job = CronJob {
            cron_job_id: CronJobId::new().into_string(),
            user_id: user_id.to_owned(),
            name: req.name,
            enabled: true,
            schedule,
            message,
            execution_mode,
            agent_config,
            conversation_id,
            conversation_title: req.conversation_title,
            agent_type: req.agent_type,
            created_by,
            skill_content: None,
            description: req.description,
            created_at: now,
            updated_at: now,
            next_run_at,
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
        };

        if controls_host && job.conversation_id.is_none() {
            self.executor
                .canonicalize_new_conversation_agent(&mut job)
                .await?;
        }
        self.validate_job_workspace(&job).await?;

        let row = cron_job_to_row(&job)?;
        self.repo.insert(&row).await?;
        if let Err(bind_error) = self.bind_existing_conversation_if_needed(&job).await {
            if let Err(compensation_error) =
                self.repo.delete(user_id, &job.cron_job_id).await
            {
                return Err(CronError::Scheduler(format!(
                    "failed to bind existing conversation for cron job {}: {bind_error}; \
                     failed to compensate inserted cron job: {compensation_error}",
                    job.cron_job_id
                )));
            }
            return Err(bind_error);
        }
        self.scheduler.schedule_job(&job);
        self.emit_job_created_for(&job).await;

        info!(job_id = %job.cron_job_id, name = %job.name, "Cron job created");
        Ok(job)
    }

    pub async fn update_job(
        &self,
        user_id: &str,
        job_id: &str,
        mut req: UpdateCronJobRequest,
    ) -> Result<CronJob, CronError> {
        let user_id = validate_cron_user_id(user_id)?;
        let job_id = validate_cron_job_id(job_id)?;
        let existing_row = self
            .repo
            .get_by_cron_job_id(user_id, &job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_string()))?;
        let previous_row = existing_row.clone();
        let mut job = cron_job_from_row(existing_row)?;
        let controls_host = self.execution_authority(user_id).controls_host();
        if !controls_host {
            if job.agent_type != AgentType::Nomi.serde_name() {
                return Err(CronError::App(AppError::Forbidden(
                    "Non-owner scheduled tasks are Nomi model-only".into(),
                )));
            }
            if let Some(config) = req.agent_config.as_mut() {
                clamp_model_only_cron_config(config);
            }
        }
        if controls_host && let Some(config) = req.agent_config.as_mut() {
            config.preset_snapshot = None;
            config.preset_revision = None;
            self.resolve_preset_config(config).await?;
        }
        if let Some(config) = req.agent_config.as_ref() {
            validate_agent_config_shape(&job.agent_type, Some(config))?;
        }

        if let Some(name) = &req.name {
            job.name = name.clone();
        }
        if let Some(description) = &req.description {
            job.description = Some(description.clone());
        }
        if let Some(enabled) = req.enabled {
            job.enabled = enabled;
        }
        if let Some(schedule_dto) = &req.schedule {
            let schedule = schedule_from_dto_with_existing_timezone(schedule_dto, &job.schedule);
            validate_schedule(&schedule)?;
            job.schedule = schedule;
        }
        if let Some(message) = &req.message {
            job.message = message.clone();
        }
        if let Some(config_dto) = &req.agent_config {
            job.agent_config = Some(CronAgentConfig {
                backend: config_dto.backend.clone(),
                name: config_dto.name.clone(),
                cli_path: config_dto.cli_path.clone(),
                custom_agent_id: config_dto.custom_agent_id.clone(),
                preset_id: config_dto.preset_id.clone(),
                preset_revision: config_dto.preset_revision,
                preset_snapshot: config_dto.preset_snapshot.clone(),
                mode: config_dto.mode.clone(),
                model: config_dto.model.clone(),
                provider_id: config_dto.provider_id.clone(),
                config_options: config_dto.config_options.clone(),
                workspace: config_dto.workspace.clone(),
                clear_context_each_run: config_dto.clear_context_each_run,
            });
        }
        if let Some(title) = &req.conversation_title {
            job.conversation_title = Some(title.clone());
        }
        if let Some(max_retries) = req.max_retries {
            if max_retries < 0 {
                return Err(CronError::App(AppError::BadRequest(
                    "max_retries must be non-negative".into(),
                )));
            }
            job.max_retries = max_retries;
        }
        if req.agent_config.is_some() || req.enabled == Some(true) {
            // Validate the resulting aggregate, not just a supplied config. A
            // disabled row cannot be re-enabled without first selecting a
            // usable Nomi model.
            self.validate_nomi_job_model(
                &job.agent_type,
                job.execution_mode,
                job.conversation_id.as_deref(),
                job.agent_config
                    .as_ref()
                    .and_then(|config| config.backend.as_deref()),
                job.agent_config
                    .as_ref()
                    .and_then(|config| config.provider_id.as_deref()),
                job.agent_config
                    .as_ref()
                    .and_then(|config| config.model.as_deref()),
            )
            .await?;
        }

        if req.schedule.is_some() || req.enabled.is_some() {
            job.next_run_at = compute_next_run(&job.schedule, now_ms());
        }

        job.updated_at = now_ms();
        if controls_host
            && job.conversation_id.is_none()
            && (req.agent_config.is_some() || req.enabled == Some(true))
        {
            self.executor
                .canonicalize_new_conversation_agent(&mut job)
                .await?;
        }
        self.validate_job_workspace(&job).await?;

        let params = build_update_params(&job, &req)?;
        self.repo.update(user_id, &job_id, &params).await?;

        if let Err(bind_error) = self.bind_existing_conversation_if_needed(&job).await {
            let compensation = restore_update_params(&previous_row);
            if let Err(compensation_error) =
                self.repo.update(user_id, &job_id, &compensation).await
            {
                return Err(CronError::Scheduler(format!(
                    "failed to bind existing conversation for cron job {job_id}: {bind_error}; \
                     failed to restore the previous cron job state: {compensation_error}"
                )));
            }
            return Err(bind_error);
        }
        self.scheduler.reschedule_job(&job);
        self.emit_job_updated_for(&job).await;

        info!(job_id = %job.cron_job_id, "Cron job updated");
        Ok(job)
    }

    /// Validate that a nomi agent job has a usable model source before it is
    /// created or updated.
    ///
    /// The model source depends on the execution mode (see [`nomi_model_check`]):
    ///
    /// * An [`ExecutionMode::Existing`] job bound to a real conversation takes
    ///   its model from that conversation row at run time (`executor`'s
    ///   `execute_inner` → `provider_model_from_conversation_row`), *not* from
    ///   `agent_config.provider_id`. The desktop "指定会话" flow deliberately omits
    ///   `agent_config` (passing it would clobber the conversation's own
    ///   workspace), so demanding `agent_config.provider_id` here wrongly rejected
    ///   every nomi specified-conversation job. Validate the bound conversation
    ///   actually carries a model instead — only then is the "no model
    ///   configured" message accurate.
    /// * An [`ExecutionMode::NewConversation`] job — or an `Existing` job with
    ///   no bound conversation yet, whose first run lazily creates one — uses
    ///   `agent_config.provider_id` as the model source (`executor::resolve_model`),
    ///   so the original static check applies.
    async fn validate_nomi_job_model(
        &self,
        agent_type: &str,
        execution_mode: ExecutionMode,
        conversation_id: Option<&str>,
        agent_backend: Option<&str>,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), CronError> {
        match nomi_model_check(agent_type, execution_mode, conversation_id) {
            NomiModelCheck::Skip => Ok(()),
            NomiModelCheck::AgentConfig => {
                validate_nomi_agent_selection(agent_type, agent_backend, provider_id, model)
            }
            NomiModelCheck::BoundConversation => {
                let conversation_id = conversation_id.expect("bound conversation check requires an ID");
                match self.executor.get_conversation_row(conversation_id).await {
                    Ok(Some(row)) => {
                        match nomifun_conversation::runtime_options::provider_model_from_conversation_row(&row) {
                            Ok(Some(_)) => Ok(()),
                            Ok(None) => Err(CronError::InvalidAgentConfig(
                                "the bound nomi conversation has no model configured; \
                             open the conversation and choose a model first, then create the job"
                                    .into(),
                            )),
                            Err(error) => Err(CronError::InvalidAgentConfig(format!(
                                "the bound nomi conversation has an invalid model: {error}"
                            ))),
                        }
                    }
                    Ok(None) => Err(CronError::InvalidAgentConfig(format!(
                        "bound conversation {conversation_id} does not exist"
                    ))),
                    Err(err) => Err(CronError::InvalidAgentConfig(format!(
                        "failed to validate bound conversation {conversation_id}: {err}"
                    ))),
                }
            }
        }
    }

    pub async fn remove_job(&self, user_id: &str, job_id: &str) -> Result<(), CronError> {
        let user_id = validate_cron_user_id(user_id)?;
        let job_id = validate_cron_job_id(job_id)?;
        let job = self.get_job(user_id, &job_id).await?;
        self.scheduler.cancel_job(&job_id);
        self.repo.delete(user_id, &job_id).await?;
        if let Err(err) = delete_skill_file(&self.data_dir, &job_id).await {
            warn!(
                job_id,
                error = %err,
                "Cron job deleted; generated skill directory is orphaned and will be retried by startup reconciliation"
            );
        }
        self.emit_job_removed_for(&job).await;
        info!(job_id, "Cron job removed");
        Ok(())
    }

    pub async fn get_job(&self, user_id: &str, job_id: &str) -> Result<CronJob, CronError> {
        let user_id = validate_cron_user_id(user_id)?;
        let job_id = validate_cron_job_id(job_id)?;
        let row = self
            .repo
            .get_by_cron_job_id(user_id, &job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_string()))?;
        cron_job_from_row(row)
    }

    pub async fn list_jobs(
        &self,
        user_id: &str,
        query: &ListCronJobsQuery,
    ) -> Result<Vec<CronJob>, CronError> {
        let user_id = validate_cron_user_id(user_id)?;
        if let Some(conversation_id) = query.conversation_id.as_deref() {
            validate_conversation_id(conversation_id)?;
        }
        let rows = if let Some(conv_id) = &query.conversation_id {
            self.repo.list_by_conversation(user_id, conv_id).await?
        } else {
            self.repo.list_all(user_id).await?
        };

        rows.into_iter().map(cron_job_from_row).collect()
    }

    pub async fn list_runs(
        &self,
        user_id: &str,
        job_id: &str,
    ) -> Result<Vec<CronJobRunResponse>, CronError> {
        let user_id = validate_cron_user_id(user_id)?;
        let job_id = validate_cron_job_id(job_id)?;
        self.repo
            .get_by_cron_job_id(user_id, &job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_string()))?;

        let rows = self
            .repo
            .list_runs_by_job(user_id, &job_id, CRON_RUN_HISTORY_LIMIT)
            .await?;

        rows.into_iter().map(cron_run_to_response).collect()
    }

    // -----------------------------------------------------------------------
    // Init / Tick / Resume / RunNow
    // -----------------------------------------------------------------------

    pub async fn init(&self) {
        self.reconcile_skill_files().await;

        let rows = match self.repo.list_enabled_for_scheduler().await {
            Ok(rows) => rows,
            Err(e) => {
                error!(error = %e, "Failed to load enabled cron jobs");
                return;
            }
        };

        let mut scheduled = 0u32;
        let mut orphans = 0u32;
        for row in rows {
            let db_id = row.id;
            let raw_cron_job_id = row.cron_job_id.clone();
            let job = match cron_job_from_row(row) {
                Ok(j) => j,
                Err(e) => {
                    error!(
                        db_id,
                        cron_job_id = %raw_cron_job_id,
                        error = %e,
                        "Failed to parse cron job row"
                    );
                    continue;
                }
            };

            if self.is_orphan(&job).await {
                warn!(
                    job_id = %job.cron_job_id,
                    job_name = %job.name,
                    conversation_id = ?job.conversation_id,
                    execution_mode = job.execution_mode.as_str(),
                    "Deleting orphan cron job whose bound conversation no longer exists"
                );
                if let Err(e) = self.repo.delete(&job.user_id, &job.cron_job_id).await {
                    error!(job_id = %job.cron_job_id, error = %e, "Failed to delete orphan cron job");
                } else if let Err(e) = delete_skill_file(&self.data_dir, &job.cron_job_id).await {
                    warn!(job_id = %job.cron_job_id, error = %e, "Failed to delete orphan cron skill directory");
                }
                orphans += 1;
                continue;
            }

            self.scheduler.schedule_job(&job);
            scheduled += 1;
        }

        info!(scheduled, orphans, "Cron service initialized");
    }

    /// Reconcile the filesystem side of the cron aggregate before scheduling.
    ///
    /// Keep a skill directory only when its live cron aggregate still belongs
    /// to the installation owner. Remove secondary-user and orphan directories
    /// so filesystem state cannot outlive the v3 ownership boundary.
    async fn reconcile_skill_files(&self) {
        match self
            .repo
            .list_all(self.authoritative_user_id.as_ref())
            .await
        {
            Ok(rows) => {
                for row in rows {
                    let Some(content) = row
                        .skill_content
                        .as_deref()
                        .filter(|content| !content.trim().is_empty())
                    else {
                        continue;
                    };
                    if let Err(error) =
                        write_raw_skill_file(&self.data_dir, &row.cron_job_id, content).await
                    {
                        warn!(
                            job_id = %row.cron_job_id,
                            error = %error,
                            "Failed to rebuild generated cron SKILL.md from canonical database content"
                        );
                    }
                }
            }
            Err(error) => {
                warn!(
                    error = %error,
                    "Could not rebuild generated cron skill files from database"
                );
            }
        }

        let root = self.data_dir.join(CRON_SKILLS_REL_DIR);
        let mut entries = match tokio::fs::read_dir(&root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                warn!(path = %root.display(), error = %error, "Failed to scan cron skill directory");
                return;
            }
        };

        let mut removed = 0u32;
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) => {
                    warn!(path = %root.display(), error = %error, "Failed while scanning cron skill directory");
                    break;
                }
            };
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(job_id) = name.strip_prefix(CRON_SKILL_DIR_PREFIX) else {
                continue;
            };
            let Ok(job_id) = CronJobId::parse(job_id) else {
                match tokio::fs::remove_dir_all(entry.path()).await {
                    Ok(()) => removed += 1,
                    Err(error) => {
                        warn!(path = %entry.path().display(), error = %error, "Failed to remove invalid cron skill directory");
                    }
                }
                continue;
            };

            let retain = match self
                .repo
                .get_by_cron_job_id_for_scheduler(job_id.as_str())
                .await
            {
                Ok(Some(row)) => {
                    self.execution_authority(&row.user_id).controls_host()
                        && row
                            .skill_content
                            .as_deref()
                            .is_some_and(|content| !content.trim().is_empty())
                }
                Ok(None) => false,
                Err(error) => {
                    warn!(job_id = %job_id, error = %error, "Could not verify cron skill ownership; retaining directory");
                    continue;
                }
            };
            if retain {
                continue;
            }
            match delete_skill_file(&self.data_dir, job_id.as_str()).await {
                Ok(()) => removed += 1,
                Err(error) => {
                    warn!(job_id = %job_id, error = %error, "Failed to remove unauthorized cron skill directory");
                }
            }
        }

        if removed > 0 {
            info!(removed, "Removed unauthorized or orphan cron skill directories");
        }
    }

    pub async fn tick(&self, expected_user_id: &str, job_id: &str) {
        let expected_user_id = match validate_cron_user_id(expected_user_id) {
            Ok(user_id) => user_id,
            Err(error) => {
                error!(job_id, error = %error, "Tick: invalid timer owner");
                return;
            }
        };
        let job_id = match validate_cron_job_id(job_id) {
            Ok(job_id) => job_id,
            Err(error) => {
                error!(job_id, error = %error, "Tick: invalid cron job id");
                return;
            }
        };
        let row = match self.repo.get_by_cron_job_id_for_scheduler(&job_id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                warn!(job_id, "Tick: job not found, cancelling timer");
                self.scheduler
                    .cancel_job_for_owner(&job_id, expected_user_id);
                return;
            }
            Err(e) => {
                error!(job_id, error = %e, "Tick: failed to load job");
                return;
            }
        };

        if row.user_id != expected_user_id {
            warn!(
                job_id,
                expected_user_id,
                actual_user_id = %row.user_id,
                "Tick: timer owner no longer matches persisted job; cancelling stale timer"
            );
            self.scheduler
                .cancel_job_for_owner(&job_id, expected_user_id);
            return;
        }

        let job = match cron_job_from_row(row) {
            Ok(j) => j,
            Err(e) => {
                error!(job_id, error = %e, "Tick: failed to parse job");
                self.scheduler
                    .cancel_job_for_owner(&job_id, expected_user_id);
                return;
            }
        };

        if !job.enabled {
            info!(job_id, "Tick: job disabled, skipping");
            self.scheduler
                .cancel_job_for_owner(&job_id, expected_user_id);
            return;
        }

        let result = self.executor.execute(&job).await;
        self.handle_execution_result(job, result).await;
    }

    pub async fn handle_system_resume(&self) {
        let rows = match self.repo.list_enabled_for_scheduler().await {
            Ok(r) => r,
            Err(e) => {
                error!(error = %e, "Resume: failed to load enabled jobs");
                return;
            }
        };

        let now = now_ms();

        for row in rows {
            let db_id = row.id;
            let raw_cron_job_id = row.cron_job_id.clone();
            let job = match cron_job_from_row(row) {
                Ok(j) => j,
                Err(e) => {
                    error!(
                        db_id,
                        cron_job_id = %raw_cron_job_id,
                        error = %e,
                        "Resume: failed to parse job"
                    );
                    continue;
                }
            };

            if let Some(next_run) = job.next_run_at
                && next_run < now
            {
                info!(
                    job_id = %job.cron_job_id,
                    conversation_id = ?job.conversation_id,
                    "Resume: missed job detected, marking missed without auto-execution"
                );
                self.record_missed_execution(&job).await;
                self.insert_missed_job_tips(&job).await;
                self.reschedule_after_missed(&job).await;
                self.emit_persisted_job_updated_for(&job).await;
                self.emit_job_executed_for(&job, "missed", None).await;
                continue;
            }

            self.scheduler.reschedule_job(&job);
        }

        info!("System resume: all cron timers rescheduled");
    }

    pub async fn run_now(&self, user_id: &str, job_id: &str) -> Result<RunNowResponse, CronError> {
        let user_id = validate_cron_user_id(user_id)?;
        let job_id = validate_cron_job_id(job_id)?;
        let row = self
            .repo
            .get_by_cron_job_id(user_id, &job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_string()))?;
        let job = cron_job_from_row(row)?;

        let prepared = self.executor.prepare_run_now(&job).await?;
        let conversation_id = prepared.conversation_id.clone();
        validate_conversation_id(&conversation_id)?;
        let service = self.clone();

        tokio::spawn(async move {
            let result = service.executor.execute_prepared(&job, prepared).await;
            service.handle_run_now_result(&job, result).await;
        });

        // The executor returns the canonical conversation entity ID unchanged.
        Ok(RunNowResponse { conversation_id })
    }

    // -----------------------------------------------------------------------
    // Skill management
    // -----------------------------------------------------------------------

    pub async fn save_skill(
        &self,
        user_id: &str,
        job_id: &str,
        req: SaveCronSkillRequest,
    ) -> Result<(), CronError> {
        let user_id = validate_cron_user_id(user_id)?;
        let job_id = validate_cron_job_id(job_id)?;
        let row = self
            .repo
            .get_by_cron_job_id(user_id, &job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_string()))?;
        // Resolve the aggregate through its owner-scoped repository before the
        // host-capability gate. A caller must not be able to distinguish
        // another user's cron id from a missing id; only an owned model-only
        // job reaches the explicit installation-owner denial below.
        self.require_host_control(user_id)?;

        validate_skill_body_content(&req.content)?;
        let job = cron_job_from_row(row)?;
        let canonical_content = canonical_skill_content(&job, &req.content)?;

        let params = UpdateCronJobParams {
            skill_content: Some(Some(canonical_content.clone())),
            ..Default::default()
        };
        self.repo.update(user_id, &job_id, &params).await?;
        if let Err(err) = write_raw_skill_file(&self.data_dir, &job_id, &canonical_content).await {
            warn!(
                job_id,
                error = %err,
                "Cron skill metadata saved; generated SKILL.md is stale and will be rebuilt before execution"
            );
            return Err(CronError::InvalidSkillContent(format!(
                "skill metadata saved, but generated SKILL.md could not be written: {err}"
            )));
        }
        self.executor
            .mark_skill_suggest_artifacts_saved(user_id, &job_id)
            .await?;

        info!(job_id, "Skill content saved");
        Ok(())
    }

    pub async fn has_skill(
        &self,
        user_id: &str,
        job_id: &str,
    ) -> Result<HasSkillResponse, CronError> {
        let user_id = validate_cron_user_id(user_id)?;
        let job_id = validate_cron_job_id(job_id)?;
        let row = self
            .repo
            .get_by_cron_job_id(user_id, &job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_string()))?;
        self.require_host_control(user_id)?;

        let has_skill = row
            .skill_content
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty());

        Ok(HasSkillResponse { has_skill })
    }

    pub async fn delete_skill(&self, user_id: &str, job_id: &str) -> Result<(), CronError> {
        let user_id = validate_cron_user_id(user_id)?;
        let job_id = validate_cron_job_id(job_id)?;
        self.repo
            .get_by_cron_job_id(user_id, &job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_string()))?;
        self.require_host_control(user_id)?;

        let params = UpdateCronJobParams {
            skill_content: Some(None),
            ..Default::default()
        };
        self.repo.update(user_id, &job_id, &params).await?;
        if let Err(err) = delete_skill_file(&self.data_dir, &job_id).await {
            warn!(
                job_id,
                error = %err,
                "Cron skill metadata deleted; generated skill directory is orphaned and requires cleanup"
            );
            return Err(CronError::InvalidSkillContent(format!(
                "skill metadata deleted, but generated SKILL.md could not be removed: {err}"
            )));
        }

        info!(job_id, "Skill content deleted");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    pub fn to_response(job: &CronJob) -> CronJobResponse {
        cron_job_to_response(job)
    }

    async fn bind_existing_conversation_if_needed(&self, job: &CronJob) -> Result<(), CronError> {
        if !matches!(job.execution_mode, ExecutionMode::Existing) {
            return Ok(());
        }
        let Some(conversation_id) = job.conversation_id.as_deref() else {
            return Ok(());
        };

        self.executor
            .bind_cron_job_to_conversation(&job.user_id, conversation_id, &job.cron_job_id)
            .await
    }

    async fn is_orphan(&self, job: &CronJob) -> bool {
        // NewConversation jobs never depend on an existing conversation —
        // every run materializes a fresh one. They must not be cleaned up
        // based on conversation state.
        if matches!(job.execution_mode, ExecutionMode::NewConversation) {
            return false;
        }

        // Existing-mode jobs can be unbound until their first lazy-binding run.
        let Some(conversation_id) = job.conversation_id.as_deref() else { return false };

        if self.executor.busy_guard().is_busy(conversation_id) {
            return false;
        }

        // Only true orphan case: Existing + bound conversation_id, but that
        // conversation has been deleted.
        match self.executor.get_conversation_row(conversation_id).await {
            Ok(Some(row)) => row.user_id != job.user_id,
            Ok(None) => true,
            Err(err) => {
                warn!(
                    job_id = %job.cron_job_id,
                    conversation_id,
                    error = %err,
                    "Failed to verify cron conversation during orphan cleanup"
                );
                false
            }
        }
    }

    async fn validate_job_workspace(&self, job: &CronJob) -> Result<(), CronError> {
        // The guard rejects pathological directory names (leading/trailing
        // whitespace — they break Win32 path round-tripping).
        let workspace = self.executor.resolve_job_workspace_raw(job).await?;
        if workspace.trim().is_empty() {
            return Ok(());
        }

        if workspace_path_has_edge_whitespace_segment(Path::new(&workspace)) {
            return Err(CronError::App(AppError::WorkspacePathEdgeWhitespace(
                workspace,
            )));
        }

        Ok(())
    }

    async fn handle_execution_result(&self, job: CronJob, result: ExecutionResult) {
        let job_id = job.cron_job_id.clone();

        match result {
            ExecutionResult::Success { conversation_id } => {
                match self
                    .update_job_after_success(&job, &conversation_id)
                    .await
                {
                    Ok(()) => {
                        self.record_execution_run(&job, "ok").await;
                        self.reschedule_after_execution(&job).await;
                        self.emit_persisted_job_updated_for(&job).await;
                        self.emit_job_executed_for(&job, "ok", None).await;
                    }
                    Err(failure) => {
                        self.handle_success_finalization_failure(&job, failure, true)
                            .await;
                    }
                }
            }
            ExecutionResult::Retrying { attempt } => {
                let retry_at = now_ms() + RETRY_INTERVAL_MS as i64;
                let params = UpdateCronJobParams {
                    retry_count: Some(attempt),
                    next_run_at: Some(Some(retry_at)),
                    ..Default::default()
                };
                if let Err(e) = self.repo.update(&job.user_id, &job_id, &params).await {
                    error!(job_id, error = %e, "Failed to update retry count");
                }
                self.schedule_retry(&job, retry_at);
                self.emit_persisted_job_updated_for(&job).await;
            }
            ExecutionResult::Skipped => {
                let params = UpdateCronJobParams {
                    last_status: Some(Some("skipped".into())),
                    retry_count: Some(0),
                    ..Default::default()
                };
                if let Err(e) = self.repo.update(&job.user_id, &job_id, &params).await {
                    error!(job_id, error = %e, "Failed to update skipped status");
                }
                self.record_execution_run(&job, "skipped").await;
                self.reschedule_after_execution(&job).await;
                self.emit_persisted_job_updated_for(&job).await;
                self.emit_job_executed_for(&job, "skipped", None).await;
            }
            ExecutionResult::Error { message } => {
                self.update_job_after_error(&job, &message).await;
                self.record_execution_run(&job, "error").await;
                self.reschedule_after_execution(&job).await;
                self.emit_persisted_job_updated_for(&job).await;
                self.emit_job_executed_for(&job, "error", Some(&message))
                    .await;
            }
        }
    }

    async fn handle_run_now_result(&self, job: &CronJob, result: ExecutionResult) {
        let job_id = job.cron_job_id.clone();
        match result {
            ExecutionResult::Success { conversation_id } => {
                match self.update_job_after_success(job, &conversation_id).await {
                    Ok(()) => {
                        self.record_execution_run(job, "ok").await;
                        self.emit_persisted_job_updated_for(job).await;
                        self.emit_job_executed_for(job, "ok", None).await;
                    }
                    Err(failure) => {
                        self.handle_success_finalization_failure(job, failure, false)
                            .await;
                    }
                }
            }
            ExecutionResult::Error { message } => {
                self.update_job_after_error(job, &message).await;
                self.record_execution_run(job, "error").await;
                self.emit_persisted_job_updated_for(job).await;
                self.emit_job_executed_for(job, "error", Some(&message))
                    .await;
            }
            ExecutionResult::Retrying { attempt } => {
                let params = UpdateCronJobParams {
                    retry_count: Some(attempt),
                    ..Default::default()
                };
                if let Err(err) = self.repo.update(&job.user_id, &job_id, &params).await {
                    error!(
                        job_id,
                        error = %err,
                        "Failed to update run-now retry count"
                    );
                }
                self.emit_persisted_job_updated_for(job).await;
            }
            ExecutionResult::Skipped => {
                let params = UpdateCronJobParams {
                    last_status: Some(Some("skipped".into())),
                    retry_count: Some(0),
                    ..Default::default()
                };
                if let Err(err) = self.repo.update(&job.user_id, &job_id, &params).await {
                    error!(
                        job_id,
                        error = %err,
                        "Failed to update run-now skipped status"
                    );
                }
                self.record_execution_run(job, "skipped").await;
                self.emit_persisted_job_updated_for(job).await;
                self.emit_job_executed_for(job, "skipped", None).await;
            }
        }
    }

    async fn record_execution_run(&self, job: &CronJob, status: &str) {
        let row = build_run_row(&job.cron_job_id, status);
        if let Err(err) = self.repo.insert_run_pruned(&job.user_id, &row).await {
            error!(
                job_id = %job.cron_job_id,
                status,
                error = %err,
                "Failed to record cron execution history"
            );
        }
    }

    async fn update_job_after_success(
        &self,
        job: &CronJob,
        conversation_id: &str,
    ) -> Result<(), SuccessFinalizationFailure> {
        let job_id = job.cron_job_id.clone();
        let existing_row = match self.repo.get_by_cron_job_id(&job.user_id, &job_id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Err(SuccessFinalizationFailure {
                    message: "cron job disappeared while finalizing a successful execution".into(),
                    execution_already_counted: false,
                    clear_conversation_id: false,
                });
            }
            Err(e) => {
                error!(job_id, error = %e, "Failed to read job for run_count");
                return Err(SuccessFinalizationFailure {
                    message: format!("failed to read cron job after successful execution: {e}"),
                    execution_already_counted: false,
                    clear_conversation_id: false,
                });
            }
        };
        let now = now_ms();
        // Persist the conversation_id back onto the job the first time an
        // "existing" job is materialized (lazy binding). Subsequent runs then
        // reuse the same conversation, matching the UX where the job is the
        // continuation anchor. Unbound is represented only by `None`.
        let new_conversation_key = match ConversationId::parse(conversation_id) {
            Ok(conversation_id) => conversation_id.into_string(),
            Err(error) => {
                error!(job_id, error = %error, "refusing to persist an invalid cron conversation id");
                return Err(SuccessFinalizationFailure {
                    message: format!(
                        "successful cron execution returned an invalid conversation id: {error}"
                    ),
                    execution_already_counted: false,
                    clear_conversation_id: false,
                });
            }
        };
        let needs_conversation_bind =
            should_bind_success_conversation(job.execution_mode, existing_row.conversation_id.as_deref());
        let params = UpdateCronJobParams {
            last_run_at: Some(Some(now)),
            last_status: Some(Some("ok".into())),
            last_error: Some(None),
            retry_count: Some(0),
            run_count: Some(existing_row.run_count + 1),
            // `Some(Some(id))` sets the logical relation; `None` leaves it
            // unchanged. Only bind on the first materialization of a lazy
            // "existing" job.
            conversation_id: needs_conversation_bind.then_some(Some(new_conversation_key)),
            ..Default::default()
        };
        if let Err(e) = self.repo.update(&job.user_id, &job_id, &params).await {
            error!(job_id, error = %e, "Failed to update job after success");
            return Err(SuccessFinalizationFailure {
                message: format!("failed to persist successful cron execution: {e}"),
                execution_already_counted: false,
                clear_conversation_id: false,
            });
        }

        if needs_conversation_bind {
            let bind_result = self
                .executor
                .bind_cron_job_to_conversation(&job.user_id, conversation_id, &job_id)
                .await;
            if let Err(bind_error) = bind_result {
                let error_message = format!(
                    "failed to bind lazily-created conversation to cron job: {bind_error}"
                );
                error!(
                    job_id,
                    conversation_id,
                    error = %error_message,
                    "Failed to bind lazily-created conversation"
                );
                return Err(SuccessFinalizationFailure {
                    message: error_message,
                    execution_already_counted: true,
                    clear_conversation_id: true,
                });
            }
        }

        Ok(())
    }

    async fn handle_success_finalization_failure(
        &self,
        job: &CronJob,
        failure: SuccessFinalizationFailure,
        reschedule: bool,
    ) {
        self.mark_success_finalization_failure(job, &failure).await;
        self.record_execution_run(job, "error").await;
        if reschedule {
            self.reschedule_after_execution(job).await;
        }
        self.emit_persisted_job_updated_for(job).await;
        self.emit_job_executed_for(job, "error", Some(&failure.message))
            .await;
    }

    async fn mark_success_finalization_failure(
        &self,
        job: &CronJob,
        failure: &SuccessFinalizationFailure,
    ) {
        let row = match self
            .repo
            .get_by_cron_job_id(&job.user_id, &job.cron_job_id)
            .await
        {
            Ok(Some(row)) => row,
            Ok(None) => {
                error!(
                    job_id = %job.cron_job_id,
                    error = %failure.message,
                    "Cron job disappeared while recording success-finalization failure"
                );
                return;
            }
            Err(read_error) => {
                error!(
                    job_id = %job.cron_job_id,
                    error = %read_error,
                    execution_error = %failure.message,
                    "Failed to read cron job while recording success-finalization failure"
                );
                return;
            }
        };
        let params = success_finalization_failure_params(&row, failure, now_ms());
        if let Err(update_error) = self
            .repo
            .update(&job.user_id, &job.cron_job_id, &params)
            .await
        {
            error!(
                job_id = %job.cron_job_id,
                error = %update_error,
                execution_error = %failure.message,
                "Failed to persist cron success-finalization failure"
            );
        }
    }

    async fn update_job_after_error(&self, job: &CronJob, message: &str) {
        let job_id = job.cron_job_id.clone();
        let run_count = match self.repo.get_by_cron_job_id(&job.user_id, &job_id).await {
            Ok(Some(r)) => r.run_count,
            Ok(None) => return,
            Err(e) => {
                error!(job_id, error = %e, "Failed to read job for run_count");
                return;
            }
        };
        let now = now_ms();
        let params = UpdateCronJobParams {
            last_run_at: Some(Some(now)),
            last_status: Some(Some("error".into())),
            last_error: Some(Some(message.to_owned())),
            retry_count: Some(0),
            run_count: Some(run_count + 1),
            ..Default::default()
        };
        if let Err(e) = self.repo.update(&job.user_id, &job_id, &params).await {
            error!(job_id, error = %e, "Failed to update job after error");
        }
    }

    async fn reschedule_after_execution(&self, job: &CronJob) {
        let is_at = matches!(job.schedule, CronSchedule::At { .. });
        if is_at {
            let params = UpdateCronJobParams {
                enabled: Some(false),
                next_run_at: Some(None),
                ..Default::default()
            };
            if let Err(e) = self.repo.update(&job.user_id, &job.cron_job_id, &params).await {
                error!(job_id = %job.cron_job_id, error = %e, "Failed to disable at-type job");
            }
            self.scheduler.cancel_job(&job.cron_job_id);

            info!(job_id = %job.cron_job_id, "At-type job executed, auto-disabled");
            return;
        }

        let now = now_ms();
        let next = compute_next_run(&job.schedule, now);
        let updated = CronJob {
            next_run_at: next,
            ..job.clone()
        };
        let params = UpdateCronJobParams {
            next_run_at: Some(next),
            ..Default::default()
        };
        if let Err(e) = self.repo.update(&job.user_id, &job.cron_job_id, &params).await {
            error!(job_id = %job.cron_job_id, error = %e, "Failed to update next_run_at");
        }
        self.scheduler.reschedule_job(&updated);
    }

    async fn record_missed_execution(&self, job: &CronJob) {
        let params = UpdateCronJobParams {
            last_status: Some(Some("missed".into())),
            last_error: Some(None),
            retry_count: Some(0),
            ..Default::default()
        };
        if let Err(err) = self.repo.update(&job.user_id, &job.cron_job_id, &params).await {
            error!(
                job_id = %job.cron_job_id,
                error = %err,
                "Failed to mark cron job as missed"
            );
        }
        self.record_execution_run(job, "missed").await;
    }

    async fn insert_missed_job_tips(&self, job: &CronJob) {
        let Some(conversation_id) = job.conversation_id.as_deref() else { return };

        let content = format!(
            "Scheduled task \"{}\" was missed while the system was unavailable. It was not run automatically.",
            job.name
        );

        match self
            .executor
            .insert_tips_message(
                &job.user_id,
                conversation_id,
                &content,
                "warning",
            )
            .await
        {
            Ok(()) => {
                self.emitter.emit_conversation_tips(
                    &job.user_id,
                    conversation_id,
                    &content,
                    "warning",
                );
            }
            Err(err) => {
                warn!(
                    job_id = %job.cron_job_id,
                    conversation_id,
                    error = %err,
                    "Failed to persist missed-job tips message"
                );
            }
        }
    }

    async fn reschedule_after_missed(&self, job: &CronJob) {
        let is_at = matches!(job.schedule, CronSchedule::At { .. });
        if is_at {
            let params = UpdateCronJobParams {
                enabled: Some(false),
                next_run_at: Some(None),
                ..Default::default()
            };
            if let Err(err) = self.repo.update(&job.user_id, &job.cron_job_id, &params).await {
                error!(
                    job_id = %job.cron_job_id,
                    error = %err,
                    "Failed to disable missed at-type job"
                );
            }
            self.scheduler.cancel_job(&job.cron_job_id);
            return;
        }

        let next = compute_next_run(&job.schedule, now_ms());
        let params = UpdateCronJobParams {
            next_run_at: Some(next),
            ..Default::default()
        };
        if let Err(err) = self.repo.update(&job.user_id, &job.cron_job_id, &params).await {
            error!(
                job_id = %job.cron_job_id,
                error = %err,
                "Failed to reschedule missed cron job"
            );
            return;
        }

        let updated = CronJob {
            next_run_at: next,
            ..job.clone()
        };
        self.scheduler.reschedule_job(&updated);
    }

    fn schedule_retry(&self, job: &CronJob, next_run: i64) {
        let retry_job = CronJob {
            schedule: CronSchedule::At {
                at_ms: next_run,
                description: None,
            },
            next_run_at: Some(next_run),
            ..job.clone()
        };
        self.scheduler.schedule_job(&retry_job);
    }

    /// Compatibility entry point for callers that have not deleted the
    /// Conversation aggregate yet. The Conversation repository owns the
    /// production cascade and returns captured IDs for
    /// [`Self::cleanup_deleted_jobs`].
    pub async fn delete_jobs_by_conversation(&self, user_id: &str, conversation_id: &str) {
        let user_id = match validate_cron_user_id(user_id) {
            Ok(user_id) => user_id,
            Err(error) => {
                error!(conversation_id, error = %error, "refusing cron cascade for invalid caller");
                return;
            }
        };
        let conversation_id = match validate_conversation_id(conversation_id) {
            Ok(conversation_id) => conversation_id,
            Err(error) => {
                error!(conversation_id, error = %error, "refusing cron cascade for invalid conversation id");
                return;
            }
        };
        let jobs = match self.repo.list_by_conversation(user_id, conversation_id).await {
            Ok(rows) => rows,
            Err(e) => {
                error!(conversation_id, error = %e, "Failed to list cron jobs for cascade delete");
                return;
            }
        };

        match self
            .repo
            .delete_by_conversation(user_id, conversation_id)
            .await
        {
            Err(error) => {
                error!(
                    conversation_id,
                    error = %error,
                    "Failed to cascade-delete cron jobs"
                );
            }
            Ok(_) => {
                let job_ids = jobs.iter().map(|row| row.cron_job_id.clone()).collect::<Vec<_>>();
                self.cleanup_deleted_jobs(user_id, &job_ids).await;
                if !job_ids.is_empty() {
                    info!(
                        conversation_id,
                        count = job_ids.len(),
                        "Cascade-deleted cron jobs for conversation"
                    );
                }
            }
        }
    }

    /// Cancel process-local timers, emit lifecycle events, and remove generated
    /// skill files for Cron rows that have already been deleted durably.
    pub async fn cleanup_deleted_jobs(&self, user_id: &str, job_ids: &[String]) {
        let user_id = match validate_cron_user_id(user_id) {
            Ok(user_id) => user_id,
            Err(error) => {
                error!(error = %error, "refusing cron cleanup for invalid caller");
                return;
            }
        };
        for job_id in job_ids {
            if CronJobId::parse(job_id).is_err() {
                continue;
            }
            self.scheduler.cancel_job(job_id);
            self.emitter.emit_job_removed(user_id, job_id);
            if let Err(error) = delete_skill_file(&self.data_dir, job_id).await {
                warn!(
                    job_id,
                    error = %error,
                    "Cron row was cascade-deleted; generated skill directory is orphaned and will be retried by startup reconciliation"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// OnConversationDelete implementation (cascade delete)
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl nomifun_common::OnConversationDelete for CronService {
    async fn on_conversation_deleted(&self, user_id: &str, conversation_id: &str) {
        if let Some(job_ids) =
            nomifun_conversation::service::current_deleted_cron_job_ids()
        {
            self.cleanup_deleted_jobs(user_id, &job_ids).await;
        } else {
            self.delete_jobs_by_conversation(user_id, conversation_id).await;
        }
    }
}

// ---------------------------------------------------------------------------
// ICronService implementation (for middleware)
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl nomifun_conversation::response_middleware::ICronService for CronService {
    async fn create_job(
        &self,
        user_id: &str,
        conversation_id: &str,
        params: &nomifun_conversation::response_middleware::CronCreateParams,
    ) -> nomifun_conversation::response_middleware::CronCommandResult {
        if nomifun_common::ConversationId::try_from(conversation_id).is_err() {
            return nomifun_conversation::response_middleware::CronCommandResult {
                success: false,
                message: format!("invalid conversation id '{conversation_id}'"),
            };
        }

        let schedule_dto = CronScheduleDto::Cron {
            expr: params.schedule.clone(),
            tz: None,
            description: Some(params.schedule_description.clone()),
        };

        let (agent_type, conversation_title, agent_config) = match self
            .executor
            .get_conversation_row(conversation_id)
            .await
        {
            Ok(Some(row)) if row.user_id == user_id => {
                let title = Some(row.name.clone());
                let (agent_type, agent_config) = match build_agent_config_from_conversation(&row) {
                    Ok(config) => config,
                    Err(error) => {
                        return nomifun_conversation::response_middleware::CronCommandResult {
                            success: false,
                            message: error.to_string(),
                        };
                    }
                };
                (agent_type, title, agent_config)
            }
            Ok(Some(_)) | Ok(None) => {
                return nomifun_conversation::response_middleware::CronCommandResult {
                    success: false,
                    message: format!(
                        "conversation '{conversation_id}' is not owned by the caller"
                    ),
                };
            }
            Err(err) => {
                return nomifun_conversation::response_middleware::CronCommandResult {
                    success: false,
                    message: err.to_string(),
                };
            }
        };

        let req = CreateCronJobRequest {
            name: params.name.clone(),
            description: None,
            schedule: schedule_dto,
            prompt: None,
            message: Some(params.message.clone()),
            conversation_id: Some(conversation_id.to_owned()),
            conversation_title,
            agent_type,
            created_by: "agent".to_owned(),
            execution_mode: Some("existing".to_owned()),
            agent_config,
        };

        match self.add_job(user_id, req).await {
            Ok(job) => nomifun_conversation::response_middleware::CronCommandResult {
                success: true,
                message: format!("Created cron job '{}' ({})", job.name, job.cron_job_id),
            },
            Err(e) => nomifun_conversation::response_middleware::CronCommandResult {
                success: false,
                message: e.to_string(),
            },
        }
    }

    async fn update_job(
        &self,
        user_id: &str,
        conversation_id: &str,
        params: &nomifun_conversation::response_middleware::CronUpdateParams,
    ) -> nomifun_conversation::response_middleware::CronCommandResult {
        if ConversationId::parse(conversation_id).is_err() {
            return nomifun_conversation::response_middleware::CronCommandResult {
                success: false,
                message: format!("invalid conversation id '{conversation_id}'"),
            };
        }
        let conversation = match self.executor.get_conversation_row(conversation_id).await {
            Ok(Some(row)) if row.user_id == user_id => row,
            Ok(Some(_)) | Ok(None) => {
                return nomifun_conversation::response_middleware::CronCommandResult {
                    success: false,
                    message: format!(
                        "conversation '{conversation_id}' is not owned by the caller"
                    ),
                };
            }
            Err(error) => {
                return nomifun_conversation::response_middleware::CronCommandResult {
                    success: false,
                    message: error.to_string(),
                };
            }
        };
        let cron_job_id = match validate_cron_job_id(&params.job_id) {
            Ok(cron_job_id) => cron_job_id,
            Err(error) => {
                return nomifun_conversation::response_middleware::CronCommandResult {
                    success: false,
                    message: error.to_string(),
                };
            }
        };
        let row = match self
            .repo
            .get_by_cron_job_id(user_id, &cron_job_id)
            .await
        {
            Ok(Some(row)) => row,
            Ok(None) => {
                return nomifun_conversation::response_middleware::CronCommandResult {
                    success: false,
                    message: format!("Cron job not found: {cron_job_id}"),
                };
            }
            Err(error) => {
                return nomifun_conversation::response_middleware::CronCommandResult {
                    success: false,
                    message: error.to_string(),
                };
            }
        };
        if row.execution_mode != ExecutionMode::Existing.as_str()
            || row.conversation_id.as_deref() != Some(conversation_id)
            || conversation.cron_job_id.as_deref() != Some(cron_job_id.as_str())
        {
            return nomifun_conversation::response_middleware::CronCommandResult {
                success: false,
                message: format!(
                    "cron job '{cron_job_id}' is not bound to conversation '{conversation_id}'"
                ),
            };
        }

        let req = UpdateCronJobRequest {
            name: Some(params.name.clone()),
            description: None,
            enabled: None,
            schedule: Some(CronScheduleDto::Cron {
                expr: params.schedule.clone(),
                tz: None,
                description: Some(params.schedule_description.clone()),
            }),
            message: Some(params.message.clone()),
            agent_config: None,
            conversation_title: None,
            max_retries: None,
        };

        match self.update_job(user_id, &cron_job_id, req).await {
            Ok(job) => nomifun_conversation::response_middleware::CronCommandResult {
                success: true,
                message: format!("Updated cron job '{}' ({})", job.name, job.cron_job_id),
            },
            Err(e) => nomifun_conversation::response_middleware::CronCommandResult {
                success: false,
                message: e.to_string(),
            },
        }
    }

    async fn list_jobs(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> nomifun_conversation::response_middleware::CronCommandResult {
        if nomifun_common::ConversationId::try_from(conversation_id).is_err() {
            return nomifun_conversation::response_middleware::CronCommandResult {
                success: true,
                message: format!("No cron jobs found for conversation '{}'.", conversation_id),
            };
        }
        let query = ListCronJobsQuery {
            conversation_id: Some(conversation_id.to_owned()),
        };
        match self.list_jobs(user_id, &query).await {
            Ok(jobs) => {
                if jobs.is_empty() {
                    return nomifun_conversation::response_middleware::CronCommandResult {
                        success: true,
                        message: format!(
                            "No cron jobs found for conversation '{}'.",
                            conversation_id
                        ),
                    };
                }

                let lines: Vec<String> = jobs
                    .iter()
                    .map(|j| {
                        let status = if j.enabled { "enabled" } else { "disabled" };
                        format!("- {} ({}) [{}]", j.name, j.cron_job_id, status)
                    })
                    .collect();

                nomifun_conversation::response_middleware::CronCommandResult {
                    success: true,
                    message: format!(
                        "Found {} cron job(s) for conversation '{}':\n{}",
                        jobs.len(),
                        conversation_id,
                        lines.join("\n")
                    ),
                }
            }
            Err(e) => nomifun_conversation::response_middleware::CronCommandResult {
                success: false,
                message: e.to_string(),
            },
        }
    }

    async fn delete_job(
        &self,
        user_id: &str,
        job_id: &str,
    ) -> nomifun_conversation::response_middleware::CronCommandResult {
        match self.remove_job(user_id, job_id).await {
            Ok(()) => nomifun_conversation::response_middleware::CronCommandResult {
                success: true,
                message: format!("Deleted cron job '{job_id}'"),
            },
            Err(e) => nomifun_conversation::response_middleware::CronCommandResult {
                success: false,
                message: e.to_string(),
            },
        }
    }

}

fn build_agent_config_from_conversation(
    row: &nomifun_db::models::ConversationRow,
) -> Result<(String, Option<nomifun_api_types::CronAgentConfigDto>), nomifun_common::AppError> {
    let extra = serde_json::from_str::<serde_json::Value>(&row.extra).map_err(|error| {
        nomifun_common::AppError::Internal(format!(
            "conversation {} has invalid extra JSON: {error}",
            row.conversation_id
        ))
    })?;
    if !extra.is_object() {
        return Err(nomifun_common::AppError::Internal(format!(
            "conversation {} extra must be a JSON object",
            row.conversation_id
        )));
    }
    // Both interactive `send_message` and the cron executor parse
    // `conversation.model` via the same helper. Keeping the cron-side
    // `agent_config.provider_id` derivation in sync with that parser
    // prevents the cached vendor-label fallback (`"nomi"`) from
    // sneaking back in (Sentry ELECTRON-1HM).
    let model_resolved =
        nomifun_conversation::runtime_options::provider_model_from_conversation_row(row)?;
    let model = model_resolved.as_ref();

    let backend = (row.r#type != "nomi").then(|| {
        get_string(&extra, "backend")
            .or_else(|| {
                model
                    .map(|value| value.provider_id.clone())
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| row.r#type.clone())
    });
    let provider_id = (row.r#type == "nomi")
        .then(|| {
            model
                .map(|value| value.provider_id.clone())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    nomifun_common::AppError::BadRequest(
                        "the bound nomi conversation has no canonical provider/model selection"
                            .into(),
                    )
                })
        })
        .transpose()?;

    let preset_id = get_string(&extra, "preset_id");
    let custom_agent_id = get_string(&extra, "custom_agent_id");
    let preset_revision = extra.get("preset_revision").and_then(serde_json::Value::as_i64);
    let preset_snapshot = extra
        .get("preset_snapshot")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| {
            nomifun_common::AppError::Internal(format!(
                "conversation {} has invalid preset_snapshot: {error}",
                row.conversation_id
            ))
        })?;

    let agent_type_enum =
        serde_json::from_value::<AgentType>(serde_json::Value::String(row.r#type.clone()))
            .map_err(|_| {
                nomifun_common::AppError::Internal(format!(
                    "conversation {} has unknown agent type '{}'",
                    row.conversation_id, row.r#type
                ))
            })?;
    // Backend is the ACP/agent vendor label (e.g. "claude"). Nomi intentionally
    // leaves it unset and carries its model provider in `provider_id`.
    let full_auto_mode = agent_type_enum
        .full_auto_mode_id(backend.as_deref())
        .to_owned();
    let agent_config = nomifun_api_types::CronAgentConfigDto {
        backend,
        name: get_string(&extra, "agent_name").unwrap_or_else(|| row.name.clone()),
        cli_path: get_string(&extra, "cli_path").or_else(|| {
            extra
                .get("gateway")
                .and_then(|gateway| gateway.get("cli_path"))
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        }),
        custom_agent_id,
        preset_id,
        preset_revision,
        preset_snapshot,
        mode: Some(full_auto_mode),
        model: if row.r#type == "nomi" {
            Some(
                model
                    .and_then(|value| {
                        value
                            .use_model
                            .clone()
                            .or_else(|| (!value.model.is_empty()).then(|| value.model.clone()))
                    })
                    .ok_or_else(|| {
                        nomifun_common::AppError::BadRequest(
                            "the bound nomi conversation has no canonical model selection"
                                .into(),
                        )
                    })?,
            )
        } else {
            get_string(&extra, "current_model_id")
        },
        provider_id,
        config_options: None,
        workspace: get_string(&extra, "workspace"),
        clear_context_each_run: false,
    };

    Ok((row.r#type.clone(), Some(agent_config)))
}
fn get_string(extra: &serde_json::Value, key: &str) -> Option<String> {
    extra
        .get(key)
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
        .filter(|value| !value.is_empty())
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Nomi cron jobs require `agent_config.provider_id` to be set —
/// the executor uses it to look up the provider row and build the agent.
/// Reject add/update requests that would produce an invalid nomi job.
///
/// The literal `"nomi"` is also rejected because it is an agent type, never a
/// provider business ID.
#[cfg(test)]
fn validate_nomi_agent_config(
    agent_type: &str,
    agent_config: Option<&nomifun_api_types::CronAgentConfigDto>,
) -> Result<(), CronError> {
    validate_nomi_agent_selection(
        agent_type,
        agent_config.and_then(|config| config.backend.as_deref()),
        agent_config.and_then(|config| config.provider_id.as_deref()),
        agent_config.and_then(|config| config.model.as_deref()),
    )
}

fn validate_agent_config_shape(
    agent_type: &str,
    agent_config: Option<&nomifun_api_types::CronAgentConfigDto>,
) -> Result<(), CronError> {
    let Some(config) = agent_config else {
        return Ok(());
    };
    if agent_type == "nomi" {
        validate_nomi_agent_selection(
            agent_type,
            config.backend.as_deref(),
            config.provider_id.as_deref(),
            config.model.as_deref(),
        )
    } else if config.provider_id.is_some() {
        Err(CronError::InvalidAgentConfig(
            "agent_config.provider_id is only valid for Nomi jobs".into(),
        ))
    } else {
        Ok(())
    }
}

fn validate_nomi_agent_selection(
    agent_type: &str,
    agent_backend: Option<&str>,
    provider_id: Option<&str>,
    model: Option<&str>,
) -> Result<(), CronError> {
    if agent_type != "nomi" {
        return Ok(());
    }
    if agent_backend.is_some() {
        return Err(CronError::InvalidAgentConfig(
            "agent_config.backend is reserved for ACP/agent backends; Nomi jobs must use agent_config.provider_id"
                .into(),
        ));
    }
    let provider_id = provider_id.unwrap_or("");
    if provider_id.is_empty() {
        return Err(CronError::InvalidAgentConfig(
            "the bound nomi conversation has no model configured (agent_config.provider_id is required); \
             set the conversation's model first, then create the job"
                .into(),
        ));
    }
    ProviderId::try_from(provider_id).map_err(|error| {
        CronError::InvalidAgentConfig(format!(
            "agent_config.provider_id must be a canonical UUIDv7 for nomi jobs: {error}"
        ))
    })?;
    let raw_model_id = model.unwrap_or("");
    let model = raw_model_id.trim();
    if model.is_empty() {
        return Err(CronError::InvalidAgentConfig(
            "agent_config.model must be a non-empty trimmed model key for nomi jobs".into(),
        ));
    }
    if model != raw_model_id {
        return Err(CronError::InvalidAgentConfig(
            "agent_config.model must not contain surrounding whitespace".into(),
        ));
    }
    Ok(())
}

/// Where a nomi cron job's model comes from. Used to pick the right validation
/// without performing any I/O, so the routing decision is unit-testable on its
/// own.
#[derive(Debug, PartialEq, Eq)]
enum NomiModelCheck {
    /// Not a nomi job — no model validation applies.
    Skip,
    /// `agent_config.provider_id` is the model source; apply the static check.
    AgentConfig,
    /// The bound conversation is the model source; load it and confirm it has a
    /// model.
    BoundConversation,
}

/// Decide how a nomi agent job's model must be validated. Pure (no I/O).
///
/// An `Existing` job bound to a non-empty `conversation_id` resolves its model
/// from that conversation at run time (`executor::execute_inner`), so
/// `agent_config` need not carry one — the desktop "指定会话" flow legitimately
/// omits it. Everything else (a new conversation, or a lazy-bind `Existing` job
/// with no conversation yet) relies on `agent_config.provider_id`.
fn nomi_model_check(
    agent_type: &str,
    execution_mode: ExecutionMode,
    conversation_id: Option<&str>,
) -> NomiModelCheck {
    if agent_type != "nomi" {
        return NomiModelCheck::Skip;
    }
    if matches!(execution_mode, ExecutionMode::Existing) && conversation_id.is_some() {
        NomiModelCheck::BoundConversation
    } else {
        NomiModelCheck::AgentConfig
    }
}

/// Reduce the incoming cron config bag to the only fields a model-only Nomi
/// schedule needs. Provider/model selection remains available; every field
/// that can select a host runtime, path, preset, skill, approval mode or custom
/// process is discarded before validation and persistence.
fn clamp_model_only_cron_config(config: &mut nomifun_api_types::CronAgentConfigDto) {
    config.cli_path = None;
    config.custom_agent_id = None;
    config.preset_id = None;
    config.preset_revision = None;
    config.preset_snapshot = None;
    config.mode = None;
    config.config_options = None;
    config.workspace = None;
}

fn parse_execution_mode(mode: Option<&str>) -> Result<ExecutionMode, CronError> {
    match mode {
        None | Some("existing") => Ok(ExecutionMode::Existing),
        Some(s) => ExecutionMode::from_str(s),
    }
}

fn validate_skill_body_content(content: &str) -> Result<(), CronError> {
    let trimmed = content.trim();

    if trimmed.is_empty() {
        return Err(CronError::InvalidSkillContent(
            "content must not be empty".into(),
        ));
    }

    let lower = trimmed.to_lowercase();
    for pattern in PLACEHOLDER_PATTERNS {
        if lower.starts_with(pattern) {
            return Err(CronError::InvalidSkillContent(
                "content appears to be placeholder text".into(),
            ));
        }
    }

    Ok(())
}

fn schedule_description(schedule: &CronSchedule) -> Option<&str> {
    match schedule {
        CronSchedule::At { description, .. }
        | CronSchedule::Every { description, .. }
        | CronSchedule::Cron { description, .. } => description.as_deref(),
    }
}

fn canonical_skill_content(job: &CronJob, content: &str) -> Result<String, CronError> {
    if validate_skill_content(content).is_ok() {
        return Ok(content.to_owned());
    }
    let description = job
        .description
        .clone()
        .unwrap_or_else(|| format!("Saved cron skill for {}", job.name));
    let content = build_skill_content(
        &job.name,
        &description,
        content.trim(),
        schedule_description(&job.schedule),
    );
    validate_skill_content(&content)?;
    Ok(content)
}

fn should_bind_success_conversation(
    execution_mode: ExecutionMode,
    existing_conversation_id: Option<&str>,
) -> bool {
    matches!(execution_mode, ExecutionMode::Existing) && existing_conversation_id.is_none()
}

fn build_update_params(
    job: &CronJob,
    req: &UpdateCronJobRequest,
) -> Result<UpdateCronJobParams, CronError> {
    let (schedule_kind, schedule_value, schedule_tz, schedule_description) =
        if req.schedule.is_some() {
            let (k, v, tz, d) = schedule_to_row_fields(&job.schedule);
            (Some(k), Some(v), Some(tz), Some(d))
        } else {
            (None, None, None, None)
        };

    let agent_config = req
        .agent_config
        .as_ref()
        .map(|c| {
            let config = CronAgentConfig {
                backend: c.backend.clone(),
                name: c.name.clone(),
                cli_path: c.cli_path.clone(),
                custom_agent_id: c.custom_agent_id.clone(),
                preset_id: c.preset_id.clone(),
                preset_revision: c.preset_revision,
                preset_snapshot: c.preset_snapshot.clone(),
                mode: c.mode.clone(),
                model: c.model.clone(),
                provider_id: c.provider_id.clone(),
                config_options: c.config_options.clone(),
                workspace: c.workspace.clone(),
                clear_context_each_run: c.clear_context_each_run,
            };
            serde_json::to_string(&config).map(Some)
        })
        .transpose()?;

    Ok(UpdateCronJobParams {
        name: req.name.clone(),
        enabled: req.enabled,
        schedule_kind,
        schedule_value,
        schedule_tz,
        schedule_description,
        payload_message: req.message.clone(),
        // Execution mode is immutable after creation. The public update DTO
        // intentionally has no execution_mode field.
        execution_mode: None,
        agent_config,
        preset_id: req
            .agent_config
            .as_ref()
            .map(|config| config.preset_id.clone()),
        preset_revision: req
            .agent_config
            .as_ref()
            .map(|config| config.preset_revision),
        preset_snapshot: req
            .agent_config
            .as_ref()
            .map(|config| {
                config
                    .preset_snapshot
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
            })
            .transpose()?,
        conversation_id: None,
        conversation_title: req.conversation_title.as_ref().map(|t| Some(t.clone())),
        agent_type: None,
        skill_content: None,
        description: req.description.as_ref().map(|value| Some(value.clone())),
        next_run_at: if req.schedule.is_some() || req.enabled.is_some() {
            Some(job.next_run_at)
        } else {
            None
        },
        last_run_at: None,
        last_status: None,
        last_error: None,
        run_count: None,
        retry_count: None,
        max_retries: req.max_retries,
    })
}

fn restore_update_params(row: &CronJobRow) -> UpdateCronJobParams {
    UpdateCronJobParams {
        name: Some(row.name.clone()),
        enabled: Some(row.enabled),
        schedule_kind: Some(row.schedule_kind.clone()),
        schedule_value: Some(row.schedule_value.clone()),
        schedule_tz: Some(row.schedule_tz.clone()),
        schedule_description: Some(row.schedule_description.clone()),
        payload_message: Some(row.payload_message.clone()),
        execution_mode: Some(row.execution_mode.clone()),
        agent_config: Some(row.agent_config.clone()),
        preset_id: Some(row.preset_id.clone()),
        preset_revision: Some(row.preset_revision),
        preset_snapshot: Some(row.preset_snapshot.clone()),
        conversation_id: Some(row.conversation_id.clone()),
        conversation_title: Some(row.conversation_title.clone()),
        agent_type: Some(row.agent_type.clone()),
        skill_content: Some(row.skill_content.clone()),
        description: Some(row.description.clone()),
        next_run_at: Some(row.next_run_at),
        last_run_at: Some(row.last_run_at),
        last_status: Some(row.last_status.clone()),
        last_error: Some(row.last_error.clone()),
        run_count: Some(row.run_count),
        retry_count: Some(row.retry_count),
        max_retries: Some(row.max_retries),
    }
}

fn build_run_row(job_id: &str, status: &str) -> CronJobRunRow {
    debug_assert!(CronJobId::parse(job_id).is_ok());
    let now = now_ms();
    CronJobRunRow {
        id: 0,
        cron_job_run_id: CronJobRunId::new().into_string(),
        cron_job_id: job_id.to_owned(),
        executed_at_ms: now,
        status: status.to_owned(),
        created_at_ms: now,
    }
}

fn success_finalization_failure_params(
    row: &CronJobRow,
    failure: &SuccessFinalizationFailure,
    now: i64,
) -> UpdateCronJobParams {
    UpdateCronJobParams {
        last_run_at: Some(Some(now)),
        last_status: Some(Some("error".into())),
        last_error: Some(Some(failure.message.clone())),
        retry_count: Some(0),
        run_count: (!failure.execution_already_counted).then_some(row.run_count + 1),
        conversation_id: failure.clear_conversation_id.then_some(None),
        ..Default::default()
    }
}

fn cron_run_to_response(row: CronJobRunRow) -> Result<CronJobRunResponse, CronError> {
    let cron_job_run_id = CronJobRunId::parse(row.cron_job_run_id).map_err(|error| {
        CronError::Scheduler(format!("invalid persisted cron job run id: {error}"))
    })?;
    let cron_job_id = CronJobId::parse(row.cron_job_id).map_err(|error| {
        CronError::Scheduler(format!("invalid persisted cron job id in run history: {error}"))
    })?;
    Ok(CronJobRunResponse {
        cron_job_run_id: cron_job_run_id.into_string(),
        cron_job_id: cron_job_id.into_string(),
        executed_at_ms: row.executed_at_ms,
        status: row.status,
    })
}

fn schedule_from_dto_with_existing_timezone(
    dto: &CronScheduleDto,
    existing: &CronSchedule,
) -> CronSchedule {
    match dto {
        CronScheduleDto::Cron {
            expr,
            tz,
            description,
        } => CronSchedule::Cron {
            expr: expr.clone(),
            tz: tz.clone().or_else(|| match existing {
                CronSchedule::Cron { tz, .. } => tz.clone(),
                _ => None,
            }),
            description: description.clone(),
        },
        _ => schedule_from_dto(dto),
    }
}

fn schedule_to_row_fields(
    schedule: &CronSchedule,
) -> (String, String, Option<String>, Option<String>) {
    match schedule {
        CronSchedule::At { at_ms, description } => (
            "at".to_owned(),
            at_ms.to_string(),
            None,
            description.clone(),
        ),
        CronSchedule::Every {
            every_ms,
            description,
        } => (
            "every".to_owned(),
            every_ms.to_string(),
            None,
            description.clone(),
        ),
        CronSchedule::Cron {
            expr,
            tz,
            description,
        } => (
            "cron".to_owned(),
            expr.clone(),
            tz.clone(),
            description.clone(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
    const USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    // -- validate_skill_body_content -------------------------------------------

    #[test]
    fn validate_skill_empty_content() {
        let err = validate_skill_body_content("").unwrap_err();
        assert!(matches!(err, CronError::InvalidSkillContent(_)));
    }

    #[test]
    fn validate_skill_whitespace_only() {
        let err = validate_skill_body_content("   \n  ").unwrap_err();
        assert!(matches!(err, CronError::InvalidSkillContent(_)));
    }

    #[test]
    fn validate_skill_placeholder_todo() {
        let err = validate_skill_body_content("TODO: fill in later").unwrap_err();
        assert!(matches!(err, CronError::InvalidSkillContent(_)));
    }

    #[test]
    fn validate_skill_placeholder_fill_in() {
        let err = validate_skill_body_content("Fill in your instructions here").unwrap_err();
        assert!(matches!(err, CronError::InvalidSkillContent(_)));
    }

    #[test]
    fn validate_skill_placeholder_replace() {
        let err = validate_skill_body_content("Replace this with your skill").unwrap_err();
        assert!(matches!(err, CronError::InvalidSkillContent(_)));
    }

    #[test]
    fn validate_skill_valid_content() {
        assert!(validate_skill_body_content("---\nname: test\n---\nDo something useful").is_ok());
    }

    #[test]
    fn validate_skill_valid_short() {
        assert!(validate_skill_body_content("Run daily report").is_ok());
    }

    // -- validate_nomi_agent_config ----------------------------------------

    fn agent_cfg_dto(provider_id: Option<&str>) -> nomifun_api_types::CronAgentConfigDto {
        nomifun_api_types::CronAgentConfigDto {
            backend: None,
            name: "provider".into(),
            cli_path: None,
            custom_agent_id: None,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            mode: None,
            model: Some("gpt-4o".into()),
            provider_id: provider_id.map(ToOwned::to_owned),
            config_options: None,
            workspace: None,
            clear_context_each_run: false,
        }
    }

    #[test]
    fn validate_nomi_accepts_valid_config() {
        let cfg = agent_cfg_dto(Some(PROVIDER_ID));
        assert!(validate_nomi_agent_config("nomi", Some(&cfg)).is_ok());
    }

    #[test]
    fn validate_nomi_rejects_missing_config() {
        let err = validate_nomi_agent_config("nomi", None).unwrap_err();
        assert!(matches!(err, CronError::InvalidAgentConfig(_)));
    }

    #[test]
    fn validate_nomi_rejects_empty_backend() {
        let mut cfg = agent_cfg_dto(Some(PROVIDER_ID));
        cfg.backend = Some(String::new());
        let err = validate_nomi_agent_config("nomi", Some(&cfg)).unwrap_err();
        assert!(matches!(err, CronError::InvalidAgentConfig(_)));
    }

    #[test]
    fn validate_nomi_rejects_whitespace_backend() {
        let mut cfg = agent_cfg_dto(Some(PROVIDER_ID));
        cfg.backend = Some("   ".into());
        let err = validate_nomi_agent_config("nomi", Some(&cfg)).unwrap_err();
        assert!(matches!(err, CronError::InvalidAgentConfig(_)));
    }

    #[test]
    fn validate_nomi_rejects_noncanonical_provider_id() {
        let cfg = agent_cfg_dto(Some("provider-safe"));
        let err = validate_nomi_agent_config("nomi", Some(&cfg)).unwrap_err();
        assert!(matches!(err, CronError::InvalidAgentConfig(_)));
    }

    #[test]
    fn validate_nomi_rejects_missing_model_id() {
        let mut cfg = agent_cfg_dto(Some(PROVIDER_ID));
        cfg.model = None;
        let err = validate_nomi_agent_config("nomi", Some(&cfg)).unwrap_err();
        assert!(matches!(err, CronError::InvalidAgentConfig(_)));
        assert!(err.to_string().contains("model"), "{err}");
    }

    #[test]
    fn validate_nomi_rejects_whitespace_model_id() {
        let mut cfg = agent_cfg_dto(Some(PROVIDER_ID));
        cfg.model = Some(" gpt-4o ".into());
        let err = validate_nomi_agent_config("nomi", Some(&cfg)).unwrap_err();
        assert!(matches!(err, CronError::InvalidAgentConfig(_)));
        assert!(err.to_string().contains("model"), "{err}");
    }

    /// A vendor label is never a provider ID.
    #[test]
    fn validate_nomi_rejects_placeholder_nomi_backend() {
        let mut cfg = agent_cfg_dto(Some(PROVIDER_ID));
        cfg.backend = Some("nomi".into());
        let err = validate_nomi_agent_config("nomi", Some(&cfg)).unwrap_err();
        assert!(matches!(err, CronError::InvalidAgentConfig(_)));
        assert!(
            err.to_string().contains("reserved for ACP/agent backends"),
            "{err}"
        );
    }

    #[test]
    fn validate_nomi_placeholder_check_trims_whitespace() {
        let mut cfg = agent_cfg_dto(Some(PROVIDER_ID));
        cfg.backend = Some("  nomi  ".into());
        let err = validate_nomi_agent_config("nomi", Some(&cfg)).unwrap_err();
        assert!(matches!(err, CronError::InvalidAgentConfig(_)));
    }

    #[test]
    fn validate_nomi_ignores_non_nomi_type() {
        // ACP / other types may legitimately omit agent_config or leave backend empty.
        assert!(validate_nomi_agent_config("acp", None).is_ok());
        let cfg = agent_cfg_dto(None);
        assert!(validate_nomi_agent_config("claude", Some(&cfg)).is_ok());
    }

    // -- nomi_model_check (execution-mode-aware routing) ----------------------

    #[test]
    fn nomi_model_check_skips_non_nomi() {
        const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
        // Non-nomi jobs never carry a nomi model requirement, regardless of mode.
        assert_eq!(
            nomi_model_check("acp", ExecutionMode::Existing, Some(CONVERSATION_ID)),
            NomiModelCheck::Skip
        );
        assert_eq!(
            nomi_model_check("claude", ExecutionMode::NewConversation, None),
            NomiModelCheck::Skip
        );
    }

    #[test]
    fn nomi_existing_bound_conversation_is_model_source() {
        const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
        // 指定会话 / reuse an existing conversation: the model comes from the
        // bound conversation at run time, so agent_config is NOT what we check.
        // This is the case the desktop create flow hit (agent_config omitted).
        assert_eq!(
            nomi_model_check("nomi", ExecutionMode::Existing, Some(CONVERSATION_ID)),
            NomiModelCheck::BoundConversation
        );
    }

    #[test]
    fn nomi_new_conversation_requires_agent_config() {
        const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
        // A fresh conversation is created from agent_config, so its backend
        // (provider_id) must be present.
        assert_eq!(
            nomi_model_check(
                "nomi",
                ExecutionMode::NewConversation,
                Some(CONVERSATION_ID),
            ),
            NomiModelCheck::AgentConfig
        );
    }

    #[test]
    fn nomi_existing_without_bound_conversation_requires_agent_config() {
        // Lazy-bind Existing job (no conversation yet): the first run creates a
        // new conversation from agent_config, so agent_config.provider_id is required.
        assert_eq!(
            nomi_model_check("nomi", ExecutionMode::Existing, None),
            NomiModelCheck::AgentConfig
        );
    }

    // -- parse_execution_mode -------------------------------------------------

    #[test]
    fn parse_mode_none_defaults_to_existing() {
        assert_eq!(parse_execution_mode(None).unwrap(), ExecutionMode::Existing);
    }

    #[test]
    fn parse_mode_existing() {
        assert_eq!(
            parse_execution_mode(Some("existing")).unwrap(),
            ExecutionMode::Existing
        );
    }

    #[test]
    fn parse_mode_new_conversation() {
        assert_eq!(
            parse_execution_mode(Some("new_conversation")).unwrap(),
            ExecutionMode::NewConversation
        );
    }

    #[test]
    fn parse_mode_invalid() {
        let err = parse_execution_mode(Some("parallel")).unwrap_err();
        assert!(matches!(err, CronError::InvalidExecutionMode(_)));
    }

    #[test]
    fn only_lazy_existing_success_binds_the_materialized_conversation() {
        assert!(should_bind_success_conversation(
            ExecutionMode::Existing,
            None
        ));
        assert!(!should_bind_success_conversation(
            ExecutionMode::Existing,
            Some(CONVERSATION_ID)
        ));
        assert!(!should_bind_success_conversation(
            ExecutionMode::NewConversation,
            None
        ));
        assert!(!should_bind_success_conversation(
            ExecutionMode::NewConversation,
            Some(CONVERSATION_ID)
        ));
    }

    // -- build_update_params --------------------------------------------------

    fn sample_job() -> CronJob {
        CronJob {
            cron_job_id: JOB_ID.into(),
            user_id: USER_ID.into(),
            name: "Test".into(),
            enabled: true,
            schedule: CronSchedule::Every {
                every_ms: 60000,
                description: None,
            },
            message: "do something".into(),
            execution_mode: ExecutionMode::Existing,
            agent_config: None,
            conversation_id: Some(CONVERSATION_ID.into()),
            conversation_title: None,
            agent_type: "acp".into(),
            created_by: CreatedBy::User,
            skill_content: None,
            description: None,
            created_at: 1000,
            updated_at: 2000,
            next_run_at: Some(61000),
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
        }
    }

    #[test]
    fn build_run_row_records_minimal_execution_fact() {
        let before = now_ms();
        let row = build_run_row(JOB_ID, "ok");
        let after = now_ms();

        assert_eq!(row.id, 0);
        assert_eq!(row.cron_job_id, JOB_ID);
        assert_eq!(row.status, "ok");
        assert!(row.executed_at_ms >= before);
        assert!(row.executed_at_ms <= after);
        assert_eq!(row.created_at_ms, row.executed_at_ms);
    }

    #[test]
    fn lazy_bind_failure_compensation_marks_error_without_double_counting() {
        let mut row = cron_job_to_row(&sample_job()).unwrap();
        row.run_count = 4;
        let params = success_finalization_failure_params(
            &row,
            &SuccessFinalizationFailure {
                message: "bind failed".into(),
                execution_already_counted: true,
                clear_conversation_id: true,
            },
            1234,
        );

        assert_eq!(params.last_run_at, Some(Some(1234)));
        assert_eq!(params.last_status, Some(Some("error".into())));
        assert_eq!(params.last_error, Some(Some("bind failed".into())));
        assert_eq!(params.retry_count, Some(0));
        assert_eq!(params.run_count, None);
        assert_eq!(params.conversation_id, Some(None));
    }

    #[test]
    fn pre_persist_success_finalization_failure_counts_the_execution_once() {
        let mut row = cron_job_to_row(&sample_job()).unwrap();
        row.run_count = 4;
        let params = success_finalization_failure_params(
            &row,
            &SuccessFinalizationFailure {
                message: "write failed".into(),
                execution_already_counted: false,
                clear_conversation_id: false,
            },
            1234,
        );

        assert_eq!(params.run_count, Some(5));
        assert_eq!(params.conversation_id, None);
    }

    #[test]
    fn build_update_params_name_only() {
        let job = sample_job();
        let req = UpdateCronJobRequest {
            name: Some("New Name".into()),
            description: None,
            enabled: None,
            schedule: None,
            message: None,
            agent_config: None,
            conversation_title: None,
            max_retries: None,
        };
        let params = build_update_params(&job, &req).unwrap();
        assert_eq!(params.name.as_deref(), Some("New Name"));
        assert!(params.enabled.is_none());
        assert!(params.schedule_kind.is_none());
        assert!(params.next_run_at.is_none());
    }

    #[test]
    fn build_update_params_with_schedule_change() {
        let job = CronJob {
            schedule: CronSchedule::Cron {
                expr: "0 0 9 * * *".into(),
                tz: Some("UTC".into()),
                description: Some("daily".into()),
            },
            next_run_at: Some(99999),
            ..sample_job()
        };
        let req = UpdateCronJobRequest {
            name: None,
            description: None,
            enabled: None,
            schedule: Some(CronScheduleDto::Cron {
                expr: "0 0 9 * * *".into(),
                tz: Some("UTC".into()),
                description: Some("daily".into()),
            }),
            message: None,
            agent_config: None,
            conversation_title: None,
            max_retries: None,
        };
        let params = build_update_params(&job, &req).unwrap();
        assert_eq!(params.schedule_kind.as_deref(), Some("cron"));
        assert_eq!(params.schedule_value.as_deref(), Some("0 0 9 * * *"));
        assert!(params.next_run_at.is_some());
    }

    #[test]
    fn preserves_existing_cron_timezone_when_update_omits_tz() {
        let existing = CronSchedule::Cron {
            expr: "0 0 9 * * *".into(),
            tz: Some("Asia/Shanghai".into()),
            description: Some("daily".into()),
        };
        let dto = CronScheduleDto::Cron {
            expr: "0 30 9 * * *".into(),
            tz: None,
            description: Some("daily".into()),
        };

        let schedule = schedule_from_dto_with_existing_timezone(&dto, &existing);

        assert_eq!(
            schedule,
            CronSchedule::Cron {
                expr: "0 30 9 * * *".into(),
                tz: Some("Asia/Shanghai".into()),
                description: Some("daily".into()),
            }
        );
    }

    #[test]
    fn build_update_params_enabled_change_triggers_next_run() {
        let job = sample_job();
        let req = UpdateCronJobRequest {
            name: None,
            description: None,
            enabled: Some(false),
            schedule: None,
            message: None,
            agent_config: None,
            conversation_title: None,
            max_retries: None,
        };
        let params = build_update_params(&job, &req).unwrap();
        assert_eq!(params.enabled, Some(false));
        assert!(params.next_run_at.is_some());
    }

    #[test]
    fn build_update_params_description_only() {
        let job = sample_job();
        let req = UpdateCronJobRequest {
            name: None,
            description: Some("Updated description".into()),
            enabled: None,
            schedule: None,
            message: None,
            agent_config: None,
            conversation_title: None,
            max_retries: None,
        };
        let params = build_update_params(&job, &req).unwrap();
        assert_eq!(
            params
                .description
                .as_ref()
                .and_then(|value| value.as_deref()),
            Some("Updated description")
        );
    }

    // -- schedule_to_row_fields -----------------------------------------------

    #[test]
    fn row_fields_at() {
        let (kind, value, tz, desc) = schedule_to_row_fields(&CronSchedule::At {
            at_ms: 5000,
            description: Some("once".into()),
        });
        assert_eq!(kind, "at");
        assert_eq!(value, "5000");
        assert!(tz.is_none());
        assert_eq!(desc.as_deref(), Some("once"));
    }

    #[test]
    fn row_fields_every() {
        let (kind, value, tz, desc) = schedule_to_row_fields(&CronSchedule::Every {
            every_ms: 30000,
            description: None,
        });
        assert_eq!(kind, "every");
        assert_eq!(value, "30000");
        assert!(tz.is_none());
        assert!(desc.is_none());
    }

    #[test]
    fn row_fields_cron() {
        let (kind, value, tz, desc) = schedule_to_row_fields(&CronSchedule::Cron {
            expr: "0 0 * * * *".into(),
            tz: Some("UTC".into()),
            description: Some("hourly".into()),
        });
        assert_eq!(kind, "cron");
        assert_eq!(value, "0 0 * * * *");
        assert_eq!(tz.as_deref(), Some("UTC"));
        assert_eq!(desc.as_deref(), Some("hourly"));
    }
}
