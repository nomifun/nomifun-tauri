use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, RwLock, Weak};

use dashmap::{DashMap, mapref::entry::Entry};
use nomifun_api_types::{
    CreateCronJobRequest, CronJobResponse, CronJobRunResponse, CronScheduleDto, HasSkillResponse,
    ListCronJobsQuery, RunNowResponse, SaveCronSkillRequest, UpdateCronJobRequest,
};
use nomifun_common::{
    AgentType, AppError, ConversationId, CronJobId, CronJobRunId, ExecutionAuthority, ProviderId,
    UserId, now_ms,
    workspace_path_has_edge_whitespace_segment,
};
use nomifun_conversation::service::{
    BackgroundTurnReconciliationDisposition, PublicTurnDeliveryState,
};
use nomifun_db::{
    AdvanceCronOccurrenceParams, CRON_RUN_HISTORY_LIMIT, CronJobRunRow, ICronRepository,
    FinalizeCronRunOutcome, FinalizeCronRunParams, ReserveCronRunParams, UpdateCronJobParams,
    models::CronJobRow,
};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{error, info, warn};

use crate::events::CronEventEmitter;

use crate::error::CronError;
use crate::executor::{ExecutionResult, JobExecutor};
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
    job_gates: Arc<DashMap<String, Weak<AsyncMutex<()>>>>,
    active_scheduled_runs: Arc<DashMap<String, ()>>,
}

#[derive(Debug, Default)]
struct CronJobRunProjection {
    last_run_at: Option<i64>,
    last_status: Option<String>,
    last_error: Option<Option<String>>,
    increment_run_count: bool,
    reset_retry_count: bool,
    bind_job_conversation_if_unbound: bool,
}

struct ActiveScheduledRunGuard {
    runs: Arc<DashMap<String, ()>>,
    run_id: String,
}

impl Drop for ActiveScheduledRunGuard {
    fn drop(&mut self) {
        self.runs.remove(&self.run_id);
    }
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
            job_gates: Arc::new(DashMap::new()),
            active_scheduled_runs: Arc::new(DashMap::new()),
        }
    }

    /// Return the process-local mutation/admission gate for one durable job.
    ///
    /// Weak values let idle jobs disappear without retaining a mutex forever.
    /// Dead keys are pruned opportunistically; a live waiter holds a strong
    /// reference, so pruning cannot split its critical section.
    fn job_gate(&self, job_id: &str) -> Arc<AsyncMutex<()>> {
        if self.job_gates.len() >= 1_024 {
            self.job_gates
                .retain(|_, gate| gate.strong_count() > 0);
        }
        match self.job_gates.entry(job_id.to_owned()) {
            Entry::Vacant(entry) => {
                let gate = Arc::new(AsyncMutex::new(()));
                entry.insert(Arc::downgrade(&gate));
                gate
            }
            Entry::Occupied(mut entry) => match entry.get().upgrade() {
                Some(gate) => gate,
                None => {
                    let gate = Arc::new(AsyncMutex::new(()));
                    entry.insert(Arc::downgrade(&gate));
                    gate
                }
            },
        }
    }

    fn mark_scheduled_run_active(&self, run_id: &str) -> ActiveScheduledRunGuard {
        self.active_scheduled_runs.insert(run_id.to_owned(), ());
        ActiveScheduledRunGuard {
            runs: Arc::clone(&self.active_scheduled_runs),
            run_id: run_id.to_owned(),
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
            schedule_revision: 1,
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
        let gate = self.job_gate(&job_id);
        let _job_guard = gate.lock().await;
        let replaces_timer = req.schedule.is_some() || req.enabled.is_some();
        let existing_row = self
            .repo
            .get_by_cron_job_id(user_id, &job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_string()))?;
        let previous_row = existing_row.clone();
        let mut job = cron_job_from_row(existing_row)?;
        let previous_job = job.clone();
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

        if replaces_timer {
            job.schedule_revision = job.schedule_revision.checked_add(1).ok_or_else(|| {
                CronError::Scheduler(format!(
                    "cron job {} schedule revision overflowed",
                    job.cron_job_id
                ))
            })?;
            job.next_run_at = compute_next_run(&job.schedule, now_ms());
        }
        // A post-write conversation-bind failure is compensated with another
        // generation, never by decrementing back to the old revision (which
        // would create an ABA identity and could collide with an old durable
        // occurrence reservation).
        let compensation_schedule_revision = if replaces_timer {
            Some(job.schedule_revision.checked_add(1).ok_or_else(|| {
                CronError::Scheduler(format!(
                    "cron job {} has no revision available for safe compensation",
                    job.cron_job_id
                ))
            })?)
        } else {
            None
        };

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

        let mut params = build_update_params(&job, &req)?;
        if replaces_timer {
            // The gate prevents an already-dispatched callback from crossing
            // admission while this generation is replaced. Cancellation is
            // deliberately delayed until every fallible validation has
            // completed so a rejected update cannot silently lose the old
            // timer.
            params.expected_schedule_revision = Some(previous_row.schedule_revision);
            self.scheduler.cancel_job_for_owner(&job_id, user_id);
        }
        if let Err(update_error) = self.repo.update(user_id, &job_id, &params).await {
            if replaces_timer {
                self.restore_timer_from_authoritative_row(
                    user_id,
                    &job_id,
                    Some(&previous_job),
                )
                .await;
            }
            return Err(update_error.into());
        }

        if let Err(bind_error) = self.bind_existing_conversation_if_needed(&job).await {
            let compensation = restore_update_params(
                &previous_row,
                Some(job.schedule_revision),
                compensation_schedule_revision,
            );
            if let Err(compensation_error) =
                self.repo.update(user_id, &job_id, &compensation).await
            {
                if replaces_timer {
                    self.restore_timer_from_authoritative_row(user_id, &job_id, Some(&job))
                        .await;
                }
                return Err(CronError::Scheduler(format!(
                    "failed to bind existing conversation for cron job {job_id}: {bind_error}; \
                     failed to restore the previous cron job state: {compensation_error}"
                )));
            }
            if replaces_timer {
                self.restore_timer_from_authoritative_row(user_id, &job_id, Some(&previous_job))
                    .await;
            }
            return Err(bind_error);
        }
        if replaces_timer {
            self.scheduler.schedule_job(&job);
        }
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
        let gate = self.job_gate(&job_id);
        let _job_guard = gate.lock().await;
        let job = self.get_job(user_id, &job_id).await?;
        self.scheduler.cancel_job_for_owner(&job_id, user_id);
        if let Err(delete_error) = self.repo.delete(user_id, &job_id).await {
            self.restore_timer_from_authoritative_row(user_id, &job_id, Some(&job))
                .await;
            return Err(delete_error.into());
        }
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
        // Re-initialization is also a generation boundary. Revoke every old
        // callback synchronously before filesystem or database reconciliation.
        self.scheduler.cancel_all();
        self.reconcile_skill_files().await;

        let rows = match self.repo.list_enabled_for_scheduler().await {
            Ok(rows) => rows,
            Err(e) => {
                error!(error = %e, "Failed to load enabled cron jobs");
                return;
            }
        };

        let mut eligible = 0u32;
        let mut orphans = 0u32;
        for row in rows {
            let db_id = row.id;
            let raw_cron_job_id = row.cron_job_id.clone();
            let cron_job_id = match validate_cron_job_id(&raw_cron_job_id) {
                Ok(cron_job_id) => cron_job_id,
                Err(error) => {
                    error!(
                        db_id,
                        cron_job_id = %raw_cron_job_id,
                        error = %error,
                        "Init: invalid cron job id"
                    );
                    continue;
                }
            };
            let gate = self.job_gate(&cron_job_id);
            let _job_guard = gate.lock().await;
            // `list_enabled_for_scheduler` is only a work list. A user update
            // or delete may have committed while this init pass waited for
            // the per-job gate, so never make timer decisions from that stale
            // snapshot.
            let current_row = match self
                .repo
                .get_by_cron_job_id_for_scheduler(&cron_job_id)
                .await
            {
                Ok(Some(current_row)) => current_row,
                Ok(None) => continue,
                Err(error) => {
                    error!(
                        db_id,
                        cron_job_id = %cron_job_id,
                        error = %error,
                        "Init: failed to reload authoritative cron job"
                    );
                    continue;
                }
            };
            let job = match cron_job_from_row(current_row) {
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

            eligible += 1;
        }

        // This is the only process-start path. Recover every crash-ambiguous
        // admission before installing a timer: a durable `reserved` row may
        // already have emitted effects, so boot must settle it and never
        // redrive it. `handle_system_resume` then repairs missed/future timers
        // from the newly settled state.
        self.recover_interrupted_runs_on_boot().await;
        self.handle_system_resume().await;

        info!(scheduled = eligible, orphans, "Cron service initialized");
    }

    async fn recover_interrupted_runs_on_boot(&self) {
        let reservations = match self.repo.list_reserved_runs_for_scheduler().await {
            Ok(reservations) => reservations,
            Err(error) => {
                error!(error = %error, "Boot: failed to enumerate interrupted cron runs");
                return;
            }
        };

        for reservation in reservations {
            // Pre-upgrade code could update `cron_jobs` and die before it
            // settled the exact reservation. No timestamp or aggregate status
            // can identify which run performed that write, so legacy rows are
            // permanently fail-closed instead of risking a double projection.
            if reservation.job_projection_state != "pending" {
                warn!(
                    job_id = %reservation.cron_job_id,
                    run_id = %reservation.cron_job_run_id,
                    projection_state = %reservation.job_projection_state,
                    "Boot: legacy Cron reservation remains quarantined because its job projection is unknowable"
                );
                continue;
            }

            let row = match self
                .repo
                .get_by_cron_job_id_for_scheduler(&reservation.cron_job_id)
                .await
            {
                Ok(Some(row)) => row,
                Ok(None) => {
                    warn!(
                        job_id = %reservation.cron_job_id,
                        run_id = %reservation.cron_job_run_id,
                        "Boot: Cron reservation has no owning job and remains quarantined"
                    );
                    continue;
                }
                Err(error) => {
                    error!(
                        job_id = %reservation.cron_job_id,
                        run_id = %reservation.cron_job_run_id,
                        error = %error,
                        "Boot: failed to load interrupted Cron job; reservation remains non-terminal"
                    );
                    continue;
                }
            };
            let job = match cron_job_from_row(row) {
                Ok(job) => job,
                Err(error) => {
                    error!(
                        job_id = %reservation.cron_job_id,
                        run_id = %reservation.cron_job_run_id,
                        error = %error,
                        "Boot: invalid Cron job keeps its exact reservation quarantined"
                    );
                    continue;
                }
            };
            let gate = self.job_gate(&job.cron_job_id);
            let _job_guard = gate.lock().await;

            let recovered = self
                .recover_exact_reserved_run(&job, &reservation)
                .await;
            let Some((status, result_error)) = recovered else {
                continue;
            };

            if reservation.trigger_kind == "scheduled" {
                let Some(planned_at_ms) = reservation.planned_at_ms else {
                    error!(
                        job_id = %job.cron_job_id,
                        run_id = %reservation.cron_job_run_id,
                        "Boot: terminal scheduled reservation lost its immutable planned time"
                    );
                    continue;
                };
                self.advance_after_terminal_occurrence(
                    &job,
                    &reservation.cron_job_run_id,
                    planned_at_ms,
                )
                .await;
            }
            self.emit_persisted_job_updated_for(&job).await;
            self.emit_job_executed_for(&job, status, result_error.as_deref())
                .await;
        }
    }

    async fn recover_exact_reserved_run(
        &self,
        job: &CronJob,
        reservation: &nomifun_db::models::CronRunReservationRow,
    ) -> Option<(&'static str, Option<String>)> {
        let Some(conversation_id) = reservation.conversation_id.as_deref() else {
            // Current-protocol execution attaches its Conversation before the
            // first send call. An unattached `pending` row therefore proves
            // that no Conversation claim was possible.
            let message = "cron execution was interrupted before its exact Conversation was attached; automatic redrive is forbidden";
            return self
                .finalize_run_once(
                    job,
                    &reservation.cron_job_run_id,
                    "error",
                    None,
                    Some(message),
                    error_run_projection(message),
                )
                .await
                .then(|| ("error", Some(message.to_owned())));
        };

        let mut state = match self
            .executor
            .public_turn_delivery_state(
                &job.user_id,
                conversation_id,
                &reservation.cron_job_run_id,
            )
            .await
        {
            Ok(state) => state,
            Err(error) => {
                warn!(
                    job_id = %job.cron_job_id,
                    run_id = %reservation.cron_job_run_id,
                    conversation_id,
                    error = %error,
                    "Boot: exact Conversation receipt cannot be read; Cron reservation remains non-terminal"
                );
                return None;
            }
        };

        if matches!(state, PublicTurnDeliveryState::Accepted { .. }) {
            match self
                .executor
                .reconcile_accepted_turn_on_boot(
                    &job.user_id,
                    conversation_id,
                    &reservation.cron_job_run_id,
                )
                .await
            {
                Ok(BackgroundTurnReconciliationDisposition::ReconciledOrTerminalReRead) => {
                    state = match self
                        .executor
                        .public_turn_delivery_state(
                            &job.user_id,
                            conversation_id,
                            &reservation.cron_job_run_id,
                        )
                        .await
                    {
                        Ok(state) => state,
                        Err(error) => {
                            warn!(
                                job_id = %job.cron_job_id,
                                run_id = %reservation.cron_job_run_id,
                                conversation_id,
                                error = %error,
                                "Boot: reconciled Conversation receipt cannot be re-read; Cron remains non-terminal"
                            );
                            return None;
                        }
                    };
                }
                Ok(
                    BackgroundTurnReconciliationDisposition::LiveExactOwnerWait
                    | BackgroundTurnReconciliationDisposition::ExternalProofRequiredFailClosed
                    | BackgroundTurnReconciliationDisposition::StaleConflict,
                ) => {
                    warn!(
                        job_id = %job.cron_job_id,
                        run_id = %reservation.cron_job_run_id,
                        conversation_id,
                        "Boot: accepted Conversation turn lacks exact terminal proof; Cron remains quarantined"
                    );
                    return None;
                }
                Err(error) => {
                    warn!(
                        job_id = %job.cron_job_id,
                        run_id = %reservation.cron_job_run_id,
                        conversation_id,
                        error = %error,
                        "Boot: accepted Conversation turn reconciliation failed; Cron remains quarantined"
                    );
                    return None;
                }
            }
        }

        match state {
            PublicTurnDeliveryState::Missing => {
                // Attachment precedes the send call. Missing receipt after a
                // process restart is exact proof that the receiver never
                // accepted this occurrence.
                let message = "cron execution was interrupted before its exact Conversation turn was accepted; automatic redrive is forbidden";
                self.finalize_run_once(
                    job,
                    &reservation.cron_job_run_id,
                    "error",
                    Some(conversation_id),
                    Some(message),
                    error_run_projection(message),
                )
                .await
                .then(|| ("error", Some(message.to_owned())))
            }
            PublicTurnDeliveryState::Accepted { .. } => {
                warn!(
                    job_id = %job.cron_job_id,
                    run_id = %reservation.cron_job_run_id,
                    conversation_id,
                    "Boot: accepted Conversation receipt remained non-terminal after reconciliation"
                );
                None
            }
            PublicTurnDeliveryState::Completed(delivery) => match delivery.result_ok {
                Some(true) => {
                    let (status, result_error, projection) =
                        match self.success_run_projection(job, conversation_id).await {
                            Ok(projection) => ("ok", None, projection),
                            Err(message) => {
                                let projection = error_run_projection(&message);
                                ("error", Some(message), projection)
                            }
                        };
                    self.finalize_run_once(
                        job,
                        &reservation.cron_job_run_id,
                        status,
                        Some(conversation_id),
                        result_error.as_deref(),
                        projection,
                    )
                    .await
                    .then_some((status, result_error))
                }
                Some(false) => {
                    let message = delivery
                        .result_error
                        .or(delivery.result_text)
                        .unwrap_or_else(|| {
                            "the exact Conversation turn completed with an unknown error"
                                .to_owned()
                        });
                    self.finalize_run_once(
                        job,
                        &reservation.cron_job_run_id,
                        "error",
                        Some(conversation_id),
                        Some(&message),
                        error_run_projection(&message),
                    )
                    .await
                    .then(|| ("error", Some(message)))
                }
                None => {
                    warn!(
                        job_id = %job.cron_job_id,
                        run_id = %reservation.cron_job_run_id,
                        conversation_id,
                        "Boot: completed Conversation receipt has no durable result; Cron remains quarantined"
                    );
                    None
                }
            },
        }
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

    /// Compatibility entry point for direct callers. Production timers call
    /// [`Self::tick_occurrence`] with the revision and planned time captured
    /// when the timer was installed.
    pub async fn tick(&self, expected_user_id: &str, job_id: &str) {
        let Ok(Some(row)) = self
            .repo
            .get_by_cron_job_id_for_scheduler(job_id)
            .await
        else {
            return;
        };
        let Some(planned_at_ms) = row.next_run_at else {
            return;
        };
        self.tick_occurrence(
            expected_user_id,
            job_id,
            row.schedule_revision,
            planned_at_ms,
        )
        .await;
    }

    /// Admit and execute one exact installed-schedule occurrence.
    ///
    /// The durable reservation is inserted before Conversation creation,
    /// runtime/knowledge preparation, message admission, or event emission.
    /// Only the INSERT winner may execute. An existing `reserved` row is
    /// absorbing: it may represent effects that escaped before a crash and is
    /// therefore never automatically redriven.
    pub async fn tick_occurrence(
        &self,
        expected_user_id: &str,
        job_id: &str,
        expected_schedule_revision: i64,
        planned_at_ms: i64,
    ) {
        let Some(generation) = self.scheduler.current_generation_for(
            job_id,
            expected_user_id,
            expected_schedule_revision,
            planned_at_ms,
        ) else {
            info!(
                job_id,
                expected_schedule_revision,
                planned_at_ms,
                "Tick: callback no longer belongs to an installed timer"
            );
            return;
        };
        self.tick_occurrence_with_generation(
            expected_user_id,
            job_id,
            expected_schedule_revision,
            planned_at_ms,
            generation,
        )
        .await;
    }

    /// Production timer entry point. The process-local installation generation
    /// is checked before every database await and again immediately before the
    /// durable claim. Resume, disable and reschedule revoke it synchronously.
    pub async fn tick_occurrence_with_generation(
        &self,
        expected_user_id: &str,
        job_id: &str,
        expected_schedule_revision: i64,
        planned_at_ms: i64,
        generation: u64,
    ) {
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
        let gate = self.job_gate(&job_id);
        let job_guard = gate.lock().await;
        if !self
            .scheduler
            .is_current_generation(&job_id, expected_user_id, generation)
        {
            info!(
                job_id,
                generation,
                planned_at_ms,
                "Tick: revoked timer generation absorbed before database lookup"
            );
            return;
        }
        let row = match self.repo.get_by_cron_job_id_for_scheduler(&job_id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                warn!(job_id, "Tick: job not found, cancelling timer");
                self.scheduler
                    .cancel_generation(&job_id, expected_user_id, generation);
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
                .cancel_generation(&job_id, expected_user_id, generation);
            return;
        }
        if row.schedule_revision != expected_schedule_revision {
            info!(
                job_id,
                expected_schedule_revision,
                actual_schedule_revision = row.schedule_revision,
                planned_at_ms,
                "Tick: stale schedule callback absorbed"
            );
            return;
        }

        let job = match cron_job_from_row(row) {
            Ok(j) => j,
            Err(e) => {
                error!(job_id, error = %e, "Tick: failed to parse job");
                self.scheduler
                    .cancel_generation(&job_id, expected_user_id, generation);
                return;
            }
        };

        if !job.enabled {
            info!(job_id, "Tick: job disabled, skipping");
            self.scheduler
                .cancel_generation(&job_id, expected_user_id, generation);
            return;
        }

        // The row read above is diagnostic only. This second local fence and
        // the repository's INSERT..SELECT CAS are the actual admission
        // authority; no model/runtime side effect occurs before both succeed.
        if !self
            .scheduler
            .is_current_generation(&job_id, expected_user_id, generation)
        {
            info!(
                job_id,
                generation,
                planned_at_ms,
                "Tick: timer was revoked before durable occurrence claim"
            );
            return;
        }

        let operation_key = format!(
            "cron:scheduled:{}:{}:{}",
            job.cron_job_id, expected_schedule_revision, planned_at_ms
        );
        let request_fingerprint = format!(
            "scheduled:v1:{}:{}:{}",
            job.cron_job_id, expected_schedule_revision, planned_at_ms
        );
        let reservation = match self
            .repo
            .reserve_run(
                &job.user_id,
                &ReserveCronRunParams {
                    cron_job_run_id: CronJobRunId::new().into_string(),
                    cron_job_id: job.cron_job_id.clone(),
                    trigger_kind: "scheduled".to_owned(),
                    operation_key,
                    request_fingerprint,
                    schedule_revision: Some(expected_schedule_revision),
                    planned_at_ms: Some(planned_at_ms),
                    now: now_ms(),
                },
            )
            .await
        {
            Ok((reservation, true)) => reservation,
            Ok((reservation, false)) => {
                info!(
                    job_id,
                    run_id = %reservation.cron_job_run_id,
                    status = %reservation.status,
                    "Tick: durable occurrence replay absorbed"
                );
                return;
            }
            Err(error) => {
                error!(
                    job_id,
                    expected_schedule_revision,
                    planned_at_ms,
                    error = %error,
                    "Tick: failed to reserve durable occurrence"
                );
                return;
            }
        };

        // `reserve_run` is a durable authority proof, but it is an await point.
        // Resume may synchronously revoke the process-local installation while
        // that transaction is in flight. Re-prove both the local generation
        // and the exact persisted occurrence before the first executor side
        // effect.
        let admission_failure = if !self
            .scheduler
            .is_current_generation(&job_id, expected_user_id, generation)
        {
            Some("timer generation was revoked after durable reservation")
        } else {
            match self.repo.get_by_cron_job_id_for_scheduler(&job_id).await {
                Ok(Some(current))
                    if current.user_id == expected_user_id
                        && current.enabled
                        && current.schedule_revision == expected_schedule_revision
                        && current.next_run_at == Some(planned_at_ms) =>
                {
                    None
                }
                Ok(Some(_)) => Some(
                    "persisted schedule changed after durable reservation and before execution",
                ),
                Ok(None) => Some("cron job disappeared after durable reservation"),
                Err(read_error) => {
                    error!(
                        job_id,
                        run_id = %reservation.cron_job_run_id,
                        error = %read_error,
                        "Tick: failed post-reservation authority read"
                    );
                    Some("post-reservation schedule authority could not be verified")
                }
            }
        };
        if let Some(reason) = admission_failure {
            warn!(
                job_id,
                run_id = %reservation.cron_job_run_id,
                expected_schedule_revision,
                planned_at_ms,
                reason,
                "Tick: reserved occurrence was revoked before executor admission"
            );
            let _ = self
                .finalize_run_once(
                    &job,
                    &reservation.cron_job_run_id,
                    "skipped",
                    job.conversation_id.as_deref(),
                    Some(reason),
                    CronJobRunProjection::default(),
                )
                .await;
            return;
        }

        let active_run = self.scheduler.commit_if_current_generation(
            &job_id,
            expected_user_id,
            generation,
            || self.mark_scheduled_run_active(&reservation.cron_job_run_id),
        );
        let Some(_active_run) = active_run else {
            let reason =
                "timer generation was revoked during post-reservation authority verification";
            warn!(
                job_id,
                run_id = %reservation.cron_job_run_id,
                expected_schedule_revision,
                planned_at_ms,
                "Tick: final atomic executor admission lost to timer revocation"
            );
            let _ = self
                .finalize_run_once(
                    &job,
                    &reservation.cron_job_run_id,
                    "skipped",
                    job.conversation_id.as_deref(),
                    Some(reason),
                    CronJobRunProjection::default(),
                )
                .await;
            return;
        };
        drop(job_guard);
        // Resolve (and, for new-conversation jobs, create) the exact target
        // before model execution, then durably attach it to this occurrence.
        // Boot recovery must never guess a Conversation from mutable job state:
        // the reservation's immutable run id + attached conversation id are
        // the only coordinates of its exact delivery receipt.
        let prepared = match self
            .executor
            .prepare_run_now(&job, &reservation.cron_job_run_id)
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                self.handle_execution_result(
                    job,
                    &reservation.cron_job_run_id,
                    planned_at_ms,
                    ExecutionResult::Error {
                        message: error.to_string(),
                    },
                )
                .await;
                return;
            }
        };
        let conversation_id = prepared.conversation_id.clone();
        if let Err(error) = self
            .repo
            .attach_run_conversation(
                &job.user_id,
                &reservation.cron_job_run_id,
                &conversation_id,
                now_ms(),
            )
            .await
        {
            self.handle_execution_result(
                job,
                &reservation.cron_job_run_id,
                planned_at_ms,
                ExecutionResult::Error {
                    message: error.to_string(),
                },
            )
            .await;
            return;
        }
        let result = self.executor.execute_prepared(&job, prepared).await;
        self.handle_execution_result(
            job,
            &reservation.cron_job_run_id,
            planned_at_ms,
            result,
        )
        .await;
    }

    pub async fn handle_system_resume(&self) {
        // This must be the first operation in the resume path. Timer futures
        // can be released by the OS before the resume event is delivered; their
        // already-dispatched callbacks carry generations that are now revoked.
        self.scheduler.cancel_all();
        let rows = match self.repo.list_enabled_for_scheduler().await {
            Ok(r) => r,
            Err(e) => {
                error!(error = %e, "Resume: failed to load enabled jobs");
                return;
            }
        };

        for row in rows {
            let db_id = row.id;
            let raw_cron_job_id = row.cron_job_id.clone();
            let cron_job_id = match validate_cron_job_id(&raw_cron_job_id) {
                Ok(cron_job_id) => cron_job_id,
                Err(error) => {
                    error!(
                        db_id,
                        cron_job_id = %raw_cron_job_id,
                        error = %error,
                        "Resume: invalid cron job id"
                    );
                    continue;
                }
            };
            let gate = self.job_gate(&cron_job_id);
            let _job_guard = gate.lock().await;
            // The enabled list is a work list only. Reload after taking the
            // gate so a concurrent user update/delete cannot be overwritten by
            // a stale resume snapshot or stale timer installation.
            let current_row = match self
                .repo
                .get_by_cron_job_id_for_scheduler(&cron_job_id)
                .await
            {
                Ok(Some(current_row)) => current_row,
                Ok(None) => continue,
                Err(error) => {
                    error!(
                        db_id,
                        cron_job_id = %cron_job_id,
                        error = %error,
                        "Resume: failed to reload authoritative cron job"
                    );
                    continue;
                }
            };
            let job = match cron_job_from_row(current_row) {
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
            if !job.enabled {
                self.scheduler.schedule_job(&job);
                continue;
            }
            let now = now_ms();

            if let Some(next_run) = job.next_run_at
                && next_run < now
            {
                let existing = match self
                    .repo
                    .get_scheduled_run_reservation(
                        &job.user_id,
                        &job.cron_job_id,
                        job.schedule_revision,
                        next_run,
                    )
                    .await
                {
                    Ok(existing) => existing,
                    Err(error) => {
                        error!(
                            job_id = %job.cron_job_id,
                            schedule_revision = job.schedule_revision,
                            planned_at_ms = next_run,
                            error = %error,
                            "Resume: failed to inspect durable occurrence"
                        );
                        continue;
                    }
                };

                if let Some(reservation) = existing {
                    if reservation.status == "reserved" {
                        if self
                            .active_scheduled_runs
                            .contains_key(&reservation.cron_job_run_id)
                        {
                            info!(
                                job_id = %job.cron_job_id,
                                run_id = %reservation.cron_job_run_id,
                                "Resume: live admitted execution owns this reservation; leaving finalization to it"
                            );
                            continue;
                        }
                        // Resume has no terminal authority. The job summary may
                        // have been written before its exact Conversation
                        // receipt or may describe another occurrence. Only the
                        // startup receipt reconciler may settle a detached
                        // reservation after proving that exact receipt terminal.
                        warn!(
                            job_id = %job.cron_job_id,
                            run_id = %reservation.cron_job_run_id,
                            conversation_id = ?reservation.conversation_id,
                            "Resume: detached Cron reservation remains quarantined pending exact Conversation receipt reconciliation"
                        );
                    } else {
                        // The terminal reservation is the authoritative
                        // outcome. A crash after settlement may have prevented
                        // timer installation, so repair only the timer.
                        self.advance_after_terminal_occurrence(
                            &job,
                            &reservation.cron_job_run_id,
                            next_run,
                        )
                        .await;
                    }
                    continue;
                }

                info!(
                    job_id = %job.cron_job_id,
                    conversation_id = ?job.conversation_id,
                    "Resume: missed job detected, marking missed without auto-execution"
                );
                let operation_key = format!(
                    "cron:scheduled:{}:{}:{}",
                    job.cron_job_id, job.schedule_revision, next_run
                );
                let request_fingerprint = format!(
                    "scheduled:v1:{}:{}:{}",
                    job.cron_job_id, job.schedule_revision, next_run
                );
                let reservation = match self
                    .repo
                    .reserve_run(
                        &job.user_id,
                        &ReserveCronRunParams {
                            cron_job_run_id: CronJobRunId::new().into_string(),
                            cron_job_id: job.cron_job_id.clone(),
                            trigger_kind: "scheduled".to_owned(),
                            operation_key,
                            request_fingerprint,
                            schedule_revision: Some(job.schedule_revision),
                            planned_at_ms: Some(next_run),
                            now,
                        },
                    )
                    .await
                {
                    Ok((reservation, true)) => reservation,
                    Ok((_reservation, false)) => continue,
                    Err(error) => {
                        error!(
                            job_id = %job.cron_job_id,
                            error = %error,
                            "Resume: failed to reserve missed occurrence"
                        );
                        continue;
                    }
                };
                if self
                    .finalize_run_once(
                        &job,
                        &reservation.cron_job_run_id,
                        "missed",
                        job.conversation_id.as_deref(),
                        None,
                        missed_run_projection(),
                    )
                    .await
                {
                    self.insert_missed_job_tips(&job).await;
                    self.advance_after_terminal_occurrence(
                        &job,
                        &reservation.cron_job_run_id,
                        next_run,
                    )
                    .await;
                    self.emit_persisted_job_updated_for(&job).await;
                    self.emit_job_executed_for(&job, "missed", None).await;
                }
                continue;
            }

            self.scheduler.reschedule_job(&job);
        }

        info!("System resume: all cron timers rescheduled");
    }

    pub async fn run_now(
        &self,
        user_id: &str,
        job_id: &str,
        operation_id: &str,
    ) -> Result<RunNowResponse, CronError> {
        let user_id = validate_cron_user_id(user_id)?;
        let job_id = validate_cron_job_id(job_id)?;
        if operation_id.is_empty()
            || operation_id.len() > 256
            || !operation_id.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(CronError::App(AppError::BadRequest(
                "cron run-now operation identity must contain 1..=256 visible ASCII bytes"
                    .to_owned(),
            )));
        }
        let row = self
            .repo
            .get_by_cron_job_id(user_id, &job_id)
            .await?
            .ok_or_else(|| CronError::JobNotFound(job_id.to_string()))?;
        let job = cron_job_from_row(row)?;

        let operation_key = format!("cron:run-now:{user_id}:{operation_id}");
        let request_fingerprint =
            format!("run-now:v1:{user_id}:{}", job.cron_job_id);
        let (reservation, inserted) = self
            .repo
            .reserve_run(
                user_id,
                &ReserveCronRunParams {
                    cron_job_run_id: CronJobRunId::new().into_string(),
                    cron_job_id: job.cron_job_id.clone(),
                    trigger_kind: "run_now".to_owned(),
                    operation_key,
                    request_fingerprint,
                    schedule_revision: None,
                    planned_at_ms: None,
                    now: now_ms(),
                },
            )
            .await?;
        if !inserted {
            if let Some(conversation_id) = reservation.conversation_id {
                validate_conversation_id(&conversation_id)?;
                return Ok(RunNowResponse { conversation_id });
            }
            let message = reservation.result_error.unwrap_or_else(|| {
                "cron run-now was already admitted without a recoverable conversation; automatic redrive is forbidden".to_owned()
            });
            if reservation.status == "reserved" {
                let _ = self
                    .finalize_run_once(
                        &job,
                        &reservation.cron_job_run_id,
                        "error",
                        None,
                        Some(&message),
                        CronJobRunProjection::default(),
                    )
                    .await;
            }
            return Err(CronError::App(AppError::Conflict(message)));
        }

        let prepared = match self
            .executor
            .prepare_run_now(&job, &reservation.cron_job_run_id)
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                let message = error.to_string();
                let _ = self
                    .finalize_run_once(
                        &job,
                        &reservation.cron_job_run_id,
                        "error",
                        None,
                        Some(&message),
                        CronJobRunProjection::default(),
                    )
                    .await;
                return Err(error);
            }
        };
        let conversation_id = prepared.conversation_id.clone();
        validate_conversation_id(&conversation_id)?;
        if let Err(error) = self
            .repo
            .attach_run_conversation(
                user_id,
                &reservation.cron_job_run_id,
                &conversation_id,
                now_ms(),
            )
            .await
        {
            let message = error.to_string();
            let _ = self
                .finalize_run_once(
                    &job,
                    &reservation.cron_job_run_id,
                    "error",
                    None,
                    Some(&message),
                    CronJobRunProjection::default(),
                )
                .await;
            return Err(error.into());
        }
        let service = self.clone();
        let run_id = reservation.cron_job_run_id;

        tokio::spawn(async move {
            let result = service.executor.execute_prepared(&job, prepared).await;
            service.handle_run_now_result(&job, &run_id, result).await;
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

    /// Reinstall only the row that is currently authoritative in SQLite after
    /// a timer-bearing mutation fails. The fallback is used solely when the
    /// repair read itself is unavailable.
    async fn restore_timer_from_authoritative_row(
        &self,
        user_id: &str,
        job_id: &str,
        fallback: Option<&CronJob>,
    ) {
        match self.repo.get_by_cron_job_id(user_id, job_id).await {
            Ok(Some(row)) => match cron_job_from_row(row) {
                Ok(job) => self.scheduler.schedule_job(&job),
                Err(parse_error) => {
                    error!(
                        job_id,
                        error = %parse_error,
                        "Failed to parse authoritative cron row while restoring its timer"
                    );
                    self.scheduler.cancel_job_for_owner(job_id, user_id);
                }
            },
            Ok(None) => self.scheduler.cancel_job_for_owner(job_id, user_id),
            Err(read_error) => {
                error!(
                    job_id,
                    error = %read_error,
                    "Failed to reload authoritative cron row while restoring its timer"
                );
                if let Some(job) = fallback {
                    self.scheduler.schedule_job(job);
                }
            }
        }
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

    async fn finalize_run_once(
        &self,
        job: &CronJob,
        run_id: &str,
        status: &str,
        conversation_id: Option<&str>,
        result_error: Option<&str>,
        projection: CronJobRunProjection,
    ) -> bool {
        match self
            .repo
            .finalize_run_with_job_projection(
                &job.user_id,
                &FinalizeCronRunParams {
                    cron_job_run_id: run_id.to_owned(),
                    status: status.to_owned(),
                    conversation_id: conversation_id.map(str::to_owned),
                    result_error: result_error.map(str::to_owned),
                    now: now_ms(),
                    last_run_at: projection.last_run_at,
                    last_status: projection.last_status,
                    last_error: projection.last_error,
                    increment_run_count: projection.increment_run_count,
                    reset_retry_count: projection.reset_retry_count,
                    bind_job_conversation_if_unbound: projection
                        .bind_job_conversation_if_unbound,
                },
            )
            .await
        {
            Ok(FinalizeCronRunOutcome::Applied) => true,
            Ok(FinalizeCronRunOutcome::AlreadyApplied) => false,
            Ok(FinalizeCronRunOutcome::LegacyProjectionUnknown) => {
                warn!(
                    job_id = %job.cron_job_id,
                    run_id,
                    status,
                    "Refusing to guess whether a legacy Cron run already updated its job projection"
                );
                false
            }
            Err(error) => {
                error!(
                    job_id = %job.cron_job_id,
                    run_id,
                    status,
                    error = %error,
                    "Failed to atomically finalize durable cron run and job projection"
                );
                false
            }
        }
    }

    async fn handle_execution_result(
        &self,
        job: CronJob,
        run_id: &str,
        expected_planned_at_ms: i64,
        result: ExecutionResult,
    ) {
        let gate = self.job_gate(&job.cron_job_id);
        let _job_guard = gate.lock().await;

        match result {
            ExecutionResult::Success { conversation_id } => {
                let (status, error, projection) =
                    match self.success_run_projection(&job, &conversation_id).await {
                        Ok(projection) => ("ok", None, projection),
                        Err(message) => {
                            let projection = error_run_projection(&message);
                            ("error", Some(message), projection)
                        }
                    };
                if !self
                    .finalize_run_once(
                        &job,
                        run_id,
                        status,
                        Some(&conversation_id),
                        error.as_deref(),
                        projection,
                    )
                    .await
                {
                    return;
                }
                self.advance_after_terminal_occurrence(
                    &job,
                    run_id,
                    expected_planned_at_ms,
                )
                .await;
                self.emit_persisted_job_updated_for(&job).await;
                self.emit_job_executed_for(&job, status, error.as_deref())
                    .await;
            }
            ExecutionResult::Retrying { attempt } => {
                // Busy is observed before this occurrence starts any model
                // side effect. Settle it instead of leaving an in-process
                // sleeping retry: a suspend/resume or concurrent recovery can
                // otherwise terminally absorb the reservation while that
                // sleeper later wakes and executes behind the durable fence.
                warn!(
                    job_id = %job.cron_job_id,
                    run_id,
                    attempt,
                    "Cron occurrence was busy; settling skipped without automatic redrive"
                );
                if !self
                    .finalize_run_once(
                        &job,
                        run_id,
                        "skipped",
                        job.conversation_id.as_deref(),
                        None,
                        skipped_run_projection(),
                    )
                    .await
                {
                    return;
                }
                self.advance_after_terminal_occurrence(
                    &job,
                    run_id,
                    expected_planned_at_ms,
                )
                .await;
                self.emit_persisted_job_updated_for(&job).await;
                self.emit_job_executed_for(&job, "skipped", None).await;
            }
            ExecutionResult::Skipped => {
                if !self
                    .finalize_run_once(
                        &job,
                        run_id,
                        "skipped",
                        job.conversation_id.as_deref(),
                        None,
                        skipped_run_projection(),
                    )
                    .await
                {
                    return;
                }
                self.advance_after_terminal_occurrence(
                    &job,
                    run_id,
                    expected_planned_at_ms,
                )
                .await;
                self.emit_persisted_job_updated_for(&job).await;
                self.emit_job_executed_for(&job, "skipped", None).await;
            }
            ExecutionResult::Error { message } => {
                if !self
                    .finalize_run_once(
                        &job,
                        run_id,
                        "error",
                        job.conversation_id.as_deref(),
                        Some(&message),
                        error_run_projection(&message),
                    )
                    .await
                {
                    return;
                }
                self.advance_after_terminal_occurrence(
                    &job,
                    run_id,
                    expected_planned_at_ms,
                )
                .await;
                self.emit_persisted_job_updated_for(&job).await;
                self.emit_job_executed_for(&job, "error", Some(&message))
                    .await;
            }
            ExecutionResult::Quarantined { message } => {
                warn!(
                    job_id = %job.cron_job_id,
                    run_id,
                    reason = %message,
                    "Cron occurrence remains durably reserved because its accepted Conversation turn is not terminal"
                );
            }
        }
    }

    async fn handle_run_now_result(
        &self,
        job: &CronJob,
        run_id: &str,
        result: ExecutionResult,
    ) {
        let gate = self.job_gate(&job.cron_job_id);
        let _job_guard = gate.lock().await;
        match result {
            ExecutionResult::Success { conversation_id } => {
                let (status, error, projection) =
                    match self.success_run_projection(job, &conversation_id).await {
                        Ok(projection) => ("ok", None, projection),
                        Err(message) => {
                            let projection = error_run_projection(&message);
                            ("error", Some(message), projection)
                        }
                    };
                if !self
                    .finalize_run_once(
                        job,
                        run_id,
                        status,
                        Some(&conversation_id),
                        error.as_deref(),
                        projection,
                    )
                    .await
                {
                    return;
                }
                self.emit_persisted_job_updated_for(job).await;
                self.emit_job_executed_for(job, status, error.as_deref())
                    .await;
            }
            ExecutionResult::Error { message } => {
                if !self
                    .finalize_run_once(
                        job,
                        run_id,
                        "error",
                        job.conversation_id.as_deref(),
                        Some(&message),
                        error_run_projection(&message),
                    )
                    .await
                {
                    return;
                }
                self.emit_persisted_job_updated_for(job).await;
                self.emit_job_executed_for(job, "error", Some(&message))
                    .await;
            }
            ExecutionResult::Retrying { attempt } => {
                warn!(
                    job_id = %job.cron_job_id,
                    run_id,
                    attempt,
                    "Cron run-now was busy; settling skipped without automatic redrive"
                );
                if !self
                    .finalize_run_once(
                        job,
                        run_id,
                        "skipped",
                        job.conversation_id.as_deref(),
                        None,
                        skipped_run_projection(),
                    )
                    .await
                {
                    return;
                }
                self.emit_persisted_job_updated_for(job).await;
                self.emit_job_executed_for(job, "skipped", None).await;
            }
            ExecutionResult::Skipped => {
                if !self
                    .finalize_run_once(
                        job,
                        run_id,
                        "skipped",
                        job.conversation_id.as_deref(),
                        None,
                        skipped_run_projection(),
                    )
                    .await
                {
                    return;
                }
                self.emit_persisted_job_updated_for(job).await;
                self.emit_job_executed_for(job, "skipped", None).await;
            }
            ExecutionResult::Quarantined { message } => {
                warn!(
                    job_id = %job.cron_job_id,
                    run_id,
                    reason = %message,
                    "Cron run-now remains durably reserved because its accepted Conversation turn is not terminal"
                );
            }
        }
    }

    async fn success_run_projection(
        &self,
        job: &CronJob,
        conversation_id: &str,
    ) -> Result<CronJobRunProjection, String> {
        let job_id = job.cron_job_id.clone();
        let existing_row = match self.repo.get_by_cron_job_id(&job.user_id, &job_id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Err(
                    "cron job disappeared while finalizing a successful execution".into(),
                );
            }
            Err(e) => {
                error!(job_id, error = %e, "Failed to read job for run_count");
                return Err(format!(
                    "failed to read cron job after successful execution: {e}"
                ));
            }
        };
        match ConversationId::parse(conversation_id) {
            Ok(_) => {}
            Err(error) => {
                error!(job_id, error = %error, "refusing to persist an invalid cron conversation id");
                return Err(format!(
                    "successful cron execution returned an invalid conversation id: {error}"
                ));
            }
        }

        // The Conversation-side relation is committed first and is
        // idempotent. If the process dies here, startup replays only the exact
        // job projection transaction; it never resends the already-completed
        // turn. The reverse job relation is then bound in that same atomic
        // transaction as run settlement and run_count increment.
        let needs_conversation_bind = should_bind_success_conversation(
            job.execution_mode,
            existing_row.conversation_id.as_deref(),
        );
        if needs_conversation_bind {
            if let Err(bind_error) = self
                .executor
                .bind_cron_job_to_conversation(&job.user_id, conversation_id, &job_id)
                .await
            {
                let error_message = format!(
                    "failed to bind lazily-created conversation to cron job: {bind_error}"
                );
                error!(
                    job_id,
                    conversation_id,
                    error = %error_message,
                    "Failed to bind lazily-created conversation"
                );
                return Err(error_message);
            }
        }

        let now = now_ms();
        Ok(CronJobRunProjection {
            last_run_at: Some(now),
            last_status: Some("ok".to_owned()),
            last_error: Some(None),
            increment_run_count: true,
            reset_retry_count: true,
            bind_job_conversation_if_unbound: needs_conversation_bind,
        })
    }

    /// Advance and install only the successor of the exact terminal durable
    /// occurrence. Callers hold the per-job gate.
    async fn advance_after_terminal_occurrence(
        &self,
        job: &CronJob,
        run_id: &str,
        expected_planned_at_ms: i64,
    ) {
        let is_at = matches!(job.schedule, CronSchedule::At { .. });
        let next_run_at = (!is_at)
            .then(|| compute_next_run(&job.schedule, now_ms()))
            .flatten();
        let disable = is_at || next_run_at.is_none();
        match self
            .repo
            .advance_scheduled_occurrence(
                &job.user_id,
                &AdvanceCronOccurrenceParams {
                    cron_job_run_id: run_id.to_owned(),
                    cron_job_id: job.cron_job_id.clone(),
                    expected_schedule_revision: job.schedule_revision,
                    expected_planned_at_ms,
                    next_run_at,
                    disable,
                    now: now_ms(),
                },
            )
            .await
        {
            Ok(Some(row)) => match cron_job_from_row(row) {
                Ok(updated) => {
                    self.scheduler.schedule_job(&updated);
                    if is_at {
                        info!(
                            job_id = %job.cron_job_id,
                            run_id,
                            "At-type occurrence committed and auto-disabled"
                        );
                    }
                }
                Err(parse_error) => error!(
                    job_id = %job.cron_job_id,
                    run_id,
                    error = %parse_error,
                    "Advanced cron occurrence produced an invalid aggregate; timer not installed"
                ),
            },
            Ok(None) => info!(
                job_id = %job.cron_job_id,
                run_id,
                expected_schedule_revision = job.schedule_revision,
                expected_planned_at_ms,
                "Stale cron finalizer absorbed without touching the successor timer"
            ),
            Err(advance_error) => error!(
                job_id = %job.cron_job_id,
                run_id,
                expected_schedule_revision = job.schedule_revision,
                expected_planned_at_ms,
                error = %advance_error,
                "Failed to atomically advance terminal cron occurrence"
            ),
        }
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
            let gate = self.job_gate(job_id);
            let _job_guard = gate.lock().await;
            self.scheduler.cancel_job_for_owner(job_id, user_id);
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
        expected_schedule_revision: None,
        name: req.name.clone(),
        enabled: req.enabled,
        schedule_revision: (req.schedule.is_some() || req.enabled.is_some())
            .then_some(job.schedule_revision),
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

fn restore_update_params(
    row: &CronJobRow,
    expected_schedule_revision: Option<i64>,
    replacement_schedule_revision: Option<i64>,
) -> UpdateCronJobParams {
    UpdateCronJobParams {
        expected_schedule_revision,
        name: Some(row.name.clone()),
        enabled: Some(row.enabled),
        schedule_revision: Some(replacement_schedule_revision.unwrap_or(row.schedule_revision)),
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

#[cfg(test)]
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

fn error_run_projection(message: &str) -> CronJobRunProjection {
    CronJobRunProjection {
        last_run_at: Some(now_ms()),
        last_status: Some("error".to_owned()),
        last_error: Some(Some(message.to_owned())),
        increment_run_count: true,
        reset_retry_count: true,
        bind_job_conversation_if_unbound: false,
    }
}

fn skipped_run_projection() -> CronJobRunProjection {
    CronJobRunProjection {
        last_status: Some("skipped".to_owned()),
        reset_retry_count: true,
        ..Default::default()
    }
}

fn missed_run_projection() -> CronJobRunProjection {
    CronJobRunProjection {
        last_status: Some("missed".to_owned()),
        last_error: Some(None),
        reset_retry_count: true,
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
            schedule_revision: 1,
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
