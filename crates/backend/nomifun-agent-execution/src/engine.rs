//! The only public application facade for persistent Agent collaboration.
//!
//! HTTP handlers and model tools call this type. Planner, router, scheduler and
//! attempt runner are private strategies behind it, so callers cannot assemble
//! partial lifecycle writes or invent a second execution state machine.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nomifun_api_types::{
    AddExecutionStepsRequest, AdoptExecutionStepOutputRequest, AdjustAgentExecutionRequest,
    AgentExecution, AgentExecutionDetail, AgentExecutionEvent, AgentExecutionTemplate,
    AgentExecutionTemplateDetail, AgentExecutionTemplateParticipant,
    AgentExecutionTemplateParticipantInput, AnswerExecutionDecisionRequest,
    ConfigureExecutionStepRequest, ConversationResponse, CreateAgentExecutionRequest,
    CreateAgentExecutionTemplateRequest, CreateExecutionFromTemplateRequest, ExecutionModelPool,
    ExecutionParticipant, ExecutionStep, PlannedExecution, PresetOverrides, PresetTarget,
    ReassignExecutionStepRequest, RenameAgentExecutionRequest, ReplanAgentExecutionRequest,
    ResolvedPresetSnapshot, RetryExecutionStepRequest,
    SteerExecutionStepRequest, UpdateExecutionStepRequest, VersionedAgentExecutionCommand,
    UpdateAgentExecutionTemplateRequest, WorkspaceEntry,
};
use nomifun_common::{
    AgentExecutionActor, AgentExecutionEventKind, AgentExecutionId, AgentExecutionStatus,
    AgentExecutionTemplateId, AgentId, AppError, ConversationId, DecisionPolicy, EntityId,
    ExecutionAttemptStatus, ExecutionStepKind, ExecutionStepStatus, MAX_AGENT_EXECUTION_MODELS,
    MAX_AGENT_EXECUTION_PARALLELISM, MAX_AGENT_EXECUTION_PARTICIPANTS,
    MAX_AGENT_EXECUTION_STEPS, NOMI_AGENT_ID, ParticipantAssignmentSource, PlanGate,
    ProviderId, generate_id, now_ms,
};
use nomifun_db::{
    AdoptAgentExecutionStepOutputParams, AppendAgentExecutionStepsFromAttemptParams,
    AppendAgentExecutionStepsParams, CreateAgentExecutionParams, IAgentExecutionRepository,
    AttemptConversationEffectParams, CreateAgentExecutionTemplateParams,
    AgentExecutionTemplateDetailRows, AgentExecutionTemplateParticipantRow,
    AgentExecutionTemplateRow,
    IAgentExecutionTemplateRepository, IProviderRepository, NewAgentExecutionEvent,
    NewAgentExecutionParticipant, NewAgentExecutionTemplateParticipant,
    NewAgentExecutionStep, NewAgentExecutionStepDependency, ReconcileAgentExecutionPlanParams,
    RetryAgentExecutionStep, SettleAgentExecutionAttemptParams, UpdateAgentExecutionParams,
    UpdateAgentExecutionTemplateParams,
};
use nomifun_preset::PresetService;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::attempt_runner::AttemptRunner;
use crate::artifact_contract::validate_required_artifacts;
use crate::conversation_effect::AttemptConversationEffects;
use crate::domain_mapper;
use crate::event_publisher::{
    AgentExecutionEventPublisher, LeadThinkingKind, LeadThinkingPhase,
};
use crate::participant_resolver::ParticipantResolver;
use crate::participant_router::score_participant;
use crate::plan_materializer::{self, MaterializedPlan};
use crate::planner::{
    AdjustedDependency, AdjustedExecutionNode, AdjustedExecutionPlan, LeadThinkingThrottle,
    PlanProducer,
};
use crate::participant_router::rank_participants;
use crate::scheduler::{
    ConversationEffects, DEFAULT_ATTEMPT_TIMEOUT, DEFAULT_MAX_PARALLEL, ExecutionScheduler,
    ExecutionSchedulerDeps, terminal_transition_payload,
};

const PLAN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEFAULT_LIST_LIMIT: i64 = 50;
const MAX_LIST_LIMIT: i64 = 200;

#[derive(Serialize)]
struct AttemptDelegationOperation<'a> {
    /// Bump only when the semantic identity contract intentionally changes.
    schema: &'static str,
    execution_id: &'a str,
    caller_step_id: &'a str,
    caller_attempt_id: &'a str,
    goal: &'a str,
    requested_model_pool: Option<&'a ExecutionModelPool>,
    explicit_steps: Option<&'a [nomifun_api_types::PlannedExecutionStep]>,
}

/// Content-address one semantic delegation command inside one Attempt. The
/// model cannot submit this identity: the Engine derives it from trusted link
/// identity plus the normalized typed command. An identical replay therefore
/// returns the first persisted Step ids even after a response is lost, while a
/// genuinely different objective, model range, or explicit DAG is a new
/// operation. Callers that intentionally need duplicate work must describe it
/// as a distinct task.
fn attempt_delegation_operation_id(
    execution_id: &str,
    caller_step_id: &str,
    caller_attempt_id: &str,
    goal: &str,
    requested_model_pool: Option<&ExecutionModelPool>,
    explicit_steps: Option<&[nomifun_api_types::PlannedExecutionStep]>,
) -> Result<String, AppError> {
    let command = AttemptDelegationOperation {
        schema: "attempt-delegation-v1",
        execution_id,
        caller_step_id,
        caller_attempt_id,
        goal,
        requested_model_pool,
        explicit_steps,
    };
    let encoded = serde_json::to_vec(&command).map_err(|error| {
        AppError::Internal(format!("encode canonical Attempt delegation: {error}"))
    })?;
    Ok(format!("delegate:{:x}", Sha256::digest(encoded)))
}

/// Immutable input for the first Planning transition. Live creation and boot
/// recovery both reload this command from SQLite instead of relying on
/// transient request memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum InitialPlanningCommand {
    Automatic {
        /// Immutable supplemental planner input copied from an authoring
        /// template. It is persisted with the initial command so recovery
        /// never re-reads mutable template state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supplemental_context: Option<serde_json::Value>,
    },
    Explicit { plan: PlannedExecution },
}

pub(crate) struct AgentExecutionEngineDeps {
    pub(crate) repository: Arc<dyn IAgentExecutionRepository>,
    pub(crate) template_repository: Arc<dyn IAgentExecutionTemplateRepository>,
    pub(crate) provider_repository: Arc<dyn IProviderRepository>,
    pub(crate) preset_service: Arc<PresetService>,
    pub(crate) planner: Arc<dyn PlanProducer>,
    pub(crate) attempt_runner: Arc<dyn AttemptRunner>,
    pub(crate) publisher: AgentExecutionEventPublisher,
    pub(crate) data_dir: PathBuf,
    pub(crate) conversation_effects: Arc<dyn ConversationEffects>,
    pub(crate) attempt_timeout: Duration,
}

impl AgentExecutionEngineDeps {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        repository: Arc<dyn IAgentExecutionRepository>,
        template_repository: Arc<dyn IAgentExecutionTemplateRepository>,
        provider_repository: Arc<dyn IProviderRepository>,
        preset_service: Arc<PresetService>,
        planner: Arc<dyn PlanProducer>,
        attempt_runner: Arc<dyn AttemptRunner>,
        conversation_effects: Arc<dyn ConversationEffects>,
        publisher: AgentExecutionEventPublisher,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            repository,
            template_repository,
            provider_repository,
            preset_service,
            planner,
            attempt_runner,
            publisher,
            data_dir,
            conversation_effects,
            attempt_timeout: DEFAULT_ATTEMPT_TIMEOUT,
        }
    }
}

#[derive(Clone)]
pub struct AgentExecutionEngine {
    repository: Arc<dyn IAgentExecutionRepository>,
    template_repository: Arc<dyn IAgentExecutionTemplateRepository>,
    preset_service: Arc<PresetService>,
    resolver: ParticipantResolver,
    planner: Arc<dyn PlanProducer>,
    publisher: AgentExecutionEventPublisher,
    scheduler: ExecutionScheduler,
}

impl AgentExecutionEngine {
    pub(crate) fn from_dependencies(deps: AgentExecutionEngineDeps) -> Self {
        let resolver = ParticipantResolver::new(
            deps.provider_repository.clone(),
            deps.preset_service.clone(),
        );
        let mut scheduler_deps = ExecutionSchedulerDeps::new(
            deps.repository.clone(),
            deps.attempt_runner,
            deps.conversation_effects,
            deps.publisher.clone(),
            deps.data_dir,
        );
        scheduler_deps.attempt_timeout = deps.attempt_timeout;
        Self {
            repository: deps.repository,
            template_repository: deps.template_repository,
            preset_service: deps.preset_service,
            resolver,
            planner: deps.planner,
            publisher: deps.publisher,
            scheduler: ExecutionScheduler::new(scheduler_deps),
        }
    }

    pub async fn create(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        request: CreateAgentExecutionRequest,
    ) -> Result<AgentExecution, AppError> {
        self.create_inner(owner_id, actor, request, None).await
    }

    /// Create an execution from an authenticated calling Conversation.
    ///
    /// The Conversation remains the interaction boundary while its frozen
    /// preset is copied into the immutable lead Participant. This keeps one
    /// execution state machine without silently dropping the caller's rules,
    /// skills or knowledge bindings when work is delegated.
    pub async fn create_from_conversation(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        conversation: &ConversationResponse,
        request: CreateAgentExecutionRequest,
    ) -> Result<AgentExecution, AppError> {
        if request.lead_conversation_id.as_deref() != Some(conversation.conversation_id.as_str()) {
            return Err(AppError::BadRequest(
                "execution lead does not match the authenticated calling conversation".to_owned(),
            ));
        }
        self.create_inner(
            owner_id,
            actor,
            request,
            conversation.preset_snapshot.as_ref(),
        )
        .await
    }

    /// Create an execution for an authenticated Agent identity that does not
    /// have a Conversation boundary (for example a Remote companion token).
    /// The optional frozen preset enriches the immutable lead Participant; no
    /// synthetic Conversation is created and terminal reporting therefore
    /// cannot trigger an extra model turn.
    pub async fn create_for_agent(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        lead_preset: Option<&ResolvedPresetSnapshot>,
        request: CreateAgentExecutionRequest,
    ) -> Result<AgentExecution, AppError> {
        if request.lead_conversation_id.is_some() {
            return Err(AppError::BadRequest(
                "conversation-less Agent execution must not declare a lead conversation"
                    .to_owned(),
            ));
        }
        self.create_inner(owner_id, actor, request, lead_preset).await
    }

    /// Instantiate reusable authoring input into one independent execution.
    /// The template is read once and then forgotten: participants and planner
    /// context are copied into immutable execution-owned state, with no FK or
    /// recovery-time lookup back into mutable authoring data.
    pub async fn create_from_template(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        template_id: &str,
        request: CreateExecutionFromTemplateRequest,
    ) -> Result<AgentExecution, AppError> {
        self.create_from_template_inner(owner_id, actor, template_id, request, None)
            .await
    }

    /// Conversation-authenticated variant used by `nomi_delegate`. The
    /// caller's frozen preset is retained as the lead snapshot exactly as it is
    /// for a non-template execution.
    pub async fn create_from_template_for_conversation(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        conversation: &ConversationResponse,
        template_id: &str,
        request: CreateExecutionFromTemplateRequest,
    ) -> Result<AgentExecution, AppError> {
        if request.lead_conversation_id.as_deref() != Some(conversation.conversation_id.as_str()) {
            return Err(AppError::BadRequest(
                "execution lead does not match the authenticated calling conversation".to_owned(),
            ));
        }
        self.create_from_template_inner(
            owner_id,
            actor,
            template_id,
            request,
            conversation.preset_snapshot.as_ref(),
        )
        .await
    }

    async fn create_from_template_inner(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        template_id: &str,
        request: CreateExecutionFromTemplateRequest,
        lead_preset: Option<&ResolvedPresetSnapshot>,
    ) -> Result<AgentExecution, AppError> {
        let template_id = canonical_id::<AgentExecutionTemplateId>("template_id", template_id)?;
        let rows = self
            .template_repository
            .get_template(owner_id, &template_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Agent Execution Template {template_id}")))?;
        if rows.participants.len() > MAX_AGENT_EXECUTION_PARTICIPANTS {
            return Err(AppError::BadRequest(format!(
                "template contains {} participants; executions allow at most {MAX_AGENT_EXECUTION_PARTICIPANTS}",
                rows.participants.len()
            )));
        }
        let max_parallel = request.max_parallel.or(rows.template.max_parallel);
        // Validate the template-authored value at the runtime boundary even
        // when the request does not override it.
        validate_max_parallel(max_parallel)?;
        let context = rows
            .template
            .context
            .as_deref()
            .map(|raw| decode_json(raw, "template context"))
            .transpose()?;
        let mut participants = runtime_participants_from_template(&rows.participants)?;
        if let Some(snapshot) = lead_preset {
            ParticipantResolver::prepend_frozen_lead(
                &mut participants,
                snapshot,
                request.lead_model.as_ref(),
            )?;
        } else if let Some(lead_model) = request.lead_model.as_ref() {
            self.resolver
                .promote_lead_model(&mut participants, lead_model)?;
        }
        let execution_request = CreateAgentExecutionRequest {
            goal: request.goal,
            work_dir: request.work_dir.or(rows.template.work_dir),
            // Already resolved above. This field is not persisted and exists
            // only because ordinary creation accepts unresolved model input.
            model_pool: ExecutionModelPool::Automatic,
            delegation_policy: request.delegation_policy,
            plan_gate: request.plan_gate,
            adaptation_policy: request.adaptation_policy,
            decision_policy: request.decision_policy,
            max_parallel,
            lead_conversation_id: request.lead_conversation_id,
            lead_model: request.lead_model,
            steps: request.steps,
        };
        self.persist_execution(
            owner_id,
            actor,
            execution_request,
            participants,
            context,
        )
        .await
    }

    async fn create_inner(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        request: CreateAgentExecutionRequest,
        lead_preset: Option<&ResolvedPresetSnapshot>,
    ) -> Result<AgentExecution, AppError> {
        let mut participants = self
            .resolver
            .resolve(&request.model_pool, request.lead_model.as_ref())
            .await?;
        if let Some(snapshot) = lead_preset {
            ParticipantResolver::prepend_frozen_lead(
                &mut participants,
                snapshot,
                request.lead_model.as_ref(),
            )?;
        }
        self.persist_execution(owner_id, actor, request, participants, None)
            .await
    }

    async fn persist_execution(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        request: CreateAgentExecutionRequest,
        participants: Vec<NewAgentExecutionParticipant>,
        supplemental_context: Option<serde_json::Value>,
    ) -> Result<AgentExecution, AppError> {
        if let Some(conversation_id) = request.lead_conversation_id.as_deref() {
            canonical_id::<ConversationId>("lead_conversation_id", conversation_id)?;
        }
        let goal = non_empty("goal", request.goal)?;
        let max_parallel = validate_max_parallel(request.max_parallel)?;
        if participants.is_empty() || participants.len() > MAX_AGENT_EXECUTION_PARTICIPANTS {
            return Err(AppError::BadRequest(format!(
                "execution participants must contain 1-{MAX_AGENT_EXECUTION_PARTICIPANTS} snapshots"
            )));
        }
        if request.steps.as_ref().is_some_and(Vec::is_empty) {
            return Err(AppError::BadRequest(
                "explicit execution steps must not be empty".to_owned(),
            ));
        }
        let initial_plan = match request.steps {
            Some(steps) => InitialPlanningCommand::Explicit {
                plan: PlannedExecution { steps },
            },
            None => InitialPlanningCommand::Automatic {
                supplemental_context,
            },
        };
        // Do not create an aggregate that can never leave Planning. The
        // original declarative plan remains the persisted recovery input;
        // generated step IDs are intentionally materialized only at commit.
        if let InitialPlanningCommand::Explicit { plan } = &initial_plan {
            let resolved = participants_from_new("preflight", 0, &participants)?;
            plan_materializer::materialize(plan.clone(), &resolved)?;
        }
        let initial_plan_input = serde_json::to_string(&initial_plan)
            .map_err(|error| AppError::Internal(format!("encode initial planning input: {error}")))?;
        let work_dir = request
            .work_dir
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let row = self
            .repository
            .create_execution_with_participants(
                owner_id,
                &CreateAgentExecutionParams {
                    goal,
                    status: AgentExecutionStatus::Planning,
                    plan_gate: request.plan_gate,
                    adaptation_policy: request.adaptation_policy,
                    decision_policy: request.decision_policy,
                    delegation_policy: request.delegation_policy,
                    max_parallel,
                    work_dir,
                    lead_conversation_id: request.lead_conversation_id.clone(),
                    initial_plan_input,
                },
                &participants,
                &actor_event(
                    actor,
                    AgentExecutionEventKind::Created,
                    None,
                    None,
                    json!({"status":"planning"}),
                ),
            )
            .await?;
        self.publish().await;
        let execution = domain_mapper::execution(row, request.lead_conversation_id)?;
        self.spawn_initial_plan(owner_id.to_owned(), execution.execution_id.clone());
        Ok(execution)
    }

    pub async fn list_templates(
        &self,
        owner_id: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<AgentExecutionTemplate>, AppError> {
        self.template_repository
            .list_templates(
                owner_id,
                limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT),
                offset.unwrap_or(0).max(0),
            )
            .await?
            .into_iter()
            .map(map_template)
            .collect()
    }

    pub async fn get_template(
        &self,
        owner_id: &str,
        template_id: &str,
    ) -> Result<AgentExecutionTemplateDetail, AppError> {
        let template_id = canonical_id::<AgentExecutionTemplateId>("template_id", template_id)?;
        let rows = self
            .template_repository
            .get_template(owner_id, &template_id)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!("Agent Execution Template {template_id}"))
            })?;
        map_template_detail(rows)
    }

    pub async fn create_template(
        &self,
        owner_id: &str,
        request: CreateAgentExecutionTemplateRequest,
    ) -> Result<AgentExecutionTemplateDetail, AppError> {
        let name = non_empty("name", request.name)?;
        if let Some(max_parallel) = request.max_parallel {
            validate_max_parallel(Some(max_parallel))?;
        }
        let participants = self
            .resolve_template_participant_inputs(request.participants)
            .await?;
        let rows = self
            .template_repository
            .create_template(
                owner_id,
                &CreateAgentExecutionTemplateParams {
                    name,
                    description: normalize_optional(request.description),
                    max_parallel: request.max_parallel,
                    work_dir: normalize_optional(request.work_dir),
                    context: request
                        .context
                        .map(|value| encode_json(&value, "template context"))
                        .transpose()?,
                    participants,
                },
            )
            .await?;
        map_template_detail(rows)
    }

    pub async fn update_template(
        &self,
        owner_id: &str,
        template_id: &str,
        request: UpdateAgentExecutionTemplateRequest,
    ) -> Result<AgentExecutionTemplateDetail, AppError> {
        let template_id = canonical_id::<AgentExecutionTemplateId>("template_id", template_id)?;
        if let Some(Some(max_parallel)) = request.max_parallel {
            validate_max_parallel(Some(max_parallel))?;
        }
        let participants = match request.participants {
            Some(participants) => Some(
                self.resolve_template_participant_inputs(participants)
                    .await?,
            ),
            None => None,
        };
        let name = request
            .name
            .map(|value| non_empty("name", value))
            .transpose()?;
        let rows = self
            .template_repository
            .update_template(
                owner_id,
                &template_id,
                request.expected_version,
                &UpdateAgentExecutionTemplateParams {
                    name,
                    description: request.description.map(normalize_optional),
                    max_parallel: request.max_parallel,
                    work_dir: request.work_dir.map(normalize_optional),
                    context: request
                        .context
                        .map(|context| {
                            context
                                .map(|value| encode_json(&value, "template context"))
                                .transpose()
                        })
                        .transpose()?,
                    participants,
                },
            )
            .await?;
        map_template_detail(rows)
    }

    pub async fn delete_template(
        &self,
        owner_id: &str,
        template_id: &str,
        expected_version: i64,
    ) -> Result<(), AppError> {
        let template_id = canonical_id::<AgentExecutionTemplateId>("template_id", template_id)?;
        if self
            .template_repository
            .delete_template(owner_id, &template_id, expected_version)
            .await?
        {
            Ok(())
        } else {
            Err(AppError::NotFound(format!(
                "Agent Execution Template {template_id}"
            )))
        }
    }

    async fn resolve_template_participant_inputs(
        &self,
        inputs: Vec<AgentExecutionTemplateParticipantInput>,
    ) -> Result<Vec<NewAgentExecutionTemplateParticipant>, AppError> {
        if inputs.is_empty() || inputs.len() > MAX_AGENT_EXECUTION_PARTICIPANTS {
            return Err(AppError::BadRequest(format!(
                "template participants must contain 1-{MAX_AGENT_EXECUTION_PARTICIPANTS} entries"
            )));
        }
        let mut participants = Vec::with_capacity(inputs.len());
        for (index, input) in inputs.into_iter().enumerate() {
            participants.push(
                self.resolve_template_participant_input(input, index as i64)
                .await?,
            );
        }
        let models: HashSet<_> = participants
            .iter()
            .map(|participant| {
                (
                    participant.provider_id.as_deref(),
                    participant.model.as_deref(),
                )
            })
            .collect();
        if models.len() > MAX_AGENT_EXECUTION_MODELS {
            return Err(AppError::BadRequest(format!(
                "template participants exceed {MAX_AGENT_EXECUTION_MODELS} distinct models"
            )));
        }
        Ok(participants)
    }

    async fn resolve_template_participant_input(
        &self,
        input: AgentExecutionTemplateParticipantInput,
        default_sort_order: i64,
    ) -> Result<NewAgentExecutionTemplateParticipant, AppError> {
        let AgentExecutionTemplateParticipantInput {
            source_agent_id,
            preset_id,
            preset_snapshot,
            preset_overrides,
            provider_id,
            model,
            role,
            capability,
            constraints,
            description,
            system_prompt,
            enabled_skills,
            disabled_builtin_skills,
            sort_order,
        } = input;
        let snapshot = match (preset_snapshot, preset_id, preset_overrides) {
            (Some(snapshot), explicit_id, None) => {
                if explicit_id
                    .as_deref()
                    .is_some_and(|id| id != snapshot.preset_id)
                {
                    return Err(AppError::BadRequest(
                        "template participant preset_id does not match preset_snapshot"
                            .to_owned(),
                    ));
                }
                validate_template_snapshot(&snapshot)?;
                Some(snapshot)
            }
            (Some(_), _, Some(_)) => {
                return Err(AppError::BadRequest(
                    "preset_overrides cannot be combined with a frozen preset_snapshot"
                        .to_owned(),
                ));
            }
            (None, Some(preset_id), overrides) => Some(
                self.preset_service
                    .resolve(
                        &non_empty("preset_id", preset_id)?,
                        PresetTarget::ExecutionStep,
                        None,
                        overrides.unwrap_or_else(PresetOverrides::default),
                    )
                    .await?,
            ),
            (None, None, Some(_)) => {
                return Err(AppError::BadRequest(
                    "preset_overrides require preset_id".to_owned(),
                ));
            }
            (None, None, None) => None,
        };
        let snapshot_model = snapshot.as_ref().and_then(|snapshot| {
            let model = snapshot.resolved_model.as_ref()?;
            Some((model.provider_id.clone()?, model.model.clone()))
        });
        let (provider_id, model) = match (provider_id, model, snapshot_model) {
            (Some(provider_id), Some(model), _) => (
                Some(canonical_provider_id("provider_id", provider_id)?),
                Some(canonical_model_name("model", model)?),
            ),
            (None, None, Some((provider_id, model))) => (
                Some(canonical_provider_id("resolved provider_id", provider_id)?),
                Some(canonical_model_name("resolved model", model)?),
            ),
            (None, None, None) => {
                return Err(AppError::BadRequest(
                    "template participant must resolve a concrete provider and model".to_owned(),
                ));
            }
            _ => {
                return Err(AppError::BadRequest(
                    "template participant provider_id and model must be provided together"
                        .to_owned(),
                ));
            }
        };
        let preset_id = snapshot.as_ref().map(|snapshot| snapshot.preset_id.clone());
        let preset_revision = snapshot.as_ref().map(|snapshot| snapshot.preset_revision);
        let preset_snapshot = snapshot
            .as_ref()
            .map(|snapshot| encode_json(snapshot, "template preset snapshot"))
            .transpose()?;
        let source_agent_id = source_agent_id
            .or_else(|| {
                snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.resolved_agent_id.clone())
            })
            .unwrap_or_else(|| NOMI_AGENT_ID.to_owned());
        let source_agent_id =
            canonical_id::<AgentId>("source_agent_id", &source_agent_id)?;
        if let Some(constraints) = constraints.as_ref() {
            constraints.validate().map_err(AppError::BadRequest)?;
        }
        let role = normalize_optional(role)
            .or_else(|| snapshot.as_ref().map(|snapshot| snapshot.preset_name.clone()));
        let description = normalize_optional(description).or_else(|| {
            snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.routing_description.clone())
        });
        let system_prompt = normalize_optional(system_prompt).or_else(|| {
            snapshot
                .as_ref()
                .and_then(|snapshot| normalize_optional(Some(snapshot.instructions.clone())))
        });
        let enabled_skills = if enabled_skills.is_empty() {
            snapshot
                .as_ref()
                .map(|snapshot| snapshot.included_skills.clone())
                .unwrap_or_default()
        } else {
            enabled_skills
        };
        let disabled_builtin_skills = if disabled_builtin_skills.is_empty() {
            snapshot
                .as_ref()
                .map(|snapshot| snapshot.excluded_auto_skills.clone())
                .unwrap_or_default()
        } else {
            disabled_builtin_skills
        };
        Ok(NewAgentExecutionTemplateParticipant {
            template_participant_id:
                nomifun_common::AgentExecutionTemplateParticipantId::new().into_string(),
            source_agent_id,
            preset_id,
            preset_revision,
            preset_snapshot,
            provider_id,
            model,
            role,
            capability: capability
                .map(|value| encode_json(&value, "template participant capability"))
                .transpose()?,
            constraints: constraints
                .map(|value| encode_json(&value, "template participant constraints"))
                .transpose()?,
            description,
            system_prompt,
            enabled_skills: encode_json(&enabled_skills, "template participant skills")?,
            disabled_builtin_skills: encode_json(
                &disabled_builtin_skills,
                "template participant builtin exclusions",
            )?,
            sort_order: sort_order.unwrap_or(default_sort_order),
        })
    }

    pub async fn get(
        &self,
        owner_id: &str,
        execution_id: &str,
    ) -> Result<AgentExecutionDetail, AppError> {
        self.detail(owner_id, execution_id).await
    }

    /// Resolve the one aggregate an attempt Conversation participates in.
    /// A normal lead Conversation has no attempt relation and therefore starts
    /// a new top-level execution; an Agent attempt delegates by appending Steps
    /// to this returned aggregate.
    pub async fn execution_for_attempt_conversation(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<Option<String>, AppError> {
        canonical_id::<ConversationId>("conversation_id", conversation_id)?;
        let mut links = self
            .repository
            .resolve_conversation_link(owner_id, conversation_id)
            .await?
            .into_iter()
            // Attempt transcripts remain execution-owned audit records after
            // settlement. They must never fall through and become the lead of
            // a new aggregate; historical routing also keeps replay reachable.
            .filter(|link| link.relation == "attempt")
            .map(|link| link.execution_id);
        let execution = links.next();
        if links.next().is_some() {
            return Err(AppError::Internal(
                "conversation has multiple execution attempt relations".to_owned(),
            ));
        }
        Ok(execution)
    }

    /// Append delegated work to the aggregate that owns the calling Attempt.
    /// The repository re-validates the active link, caller Step/Attempt
    /// generations and private recursion depth in the same transaction. No
    /// aggregate version is accepted here, so unrelated parallel settlements
    /// cannot manufacture a spurious conflict.
    pub async fn delegate_from_attempt(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        conversation_id: &str,
        goal: String,
        model_pool: ExecutionModelPool,
        requested_model_pool: Option<ExecutionModelPool>,
        explicit_steps: Option<Vec<nomifun_api_types::PlannedExecutionStep>>,
    ) -> Result<(AgentExecutionDetail, Vec<String>), AppError> {
        canonical_id::<ConversationId>("conversation_id", conversation_id)?;
        let goal = non_empty("goal", goal)?;
        if explicit_steps.as_ref().is_some_and(Vec::is_empty) {
            return Err(AppError::BadRequest(
                "delegated steps must not be empty".to_owned(),
            ));
        }
        let actor_attempt_id = match actor {
            AgentExecutionActor::Agent {
                conversation_id: Some(actor_conversation_id),
                attempt_id: Some(actor_attempt_id),
                ..
            } if actor_conversation_id == conversation_id => {
                canonical_uuidv7("actor.attempt_id", actor_attempt_id)?
            }
            _ => {
                return Err(AppError::NotFound(
                    "active execution Attempt for Agent caller".to_owned(),
                ));
            }
        };
        let mut links = self
            .repository
            .resolve_conversation_link(owner_id, conversation_id)
            .await?
            .into_iter()
            .filter(|link| {
                link.relation == "attempt"
                    && link.attempt_id.as_deref() == Some(actor_attempt_id.as_str())
            });
        let link = links.next().ok_or_else(|| {
            AppError::NotFound("execution Attempt for Agent caller".to_owned())
        })?;
        if links.next().is_some() {
            return Err(AppError::Conflict(
                "Agent caller has multiple matching execution Attempts".to_owned(),
            ));
        }
        let caller_step_id = link.step_id.ok_or_else(|| {
            AppError::Internal("active Attempt link has no Step id".to_owned())
        })?;
        let caller_attempt_id = link.attempt_id.ok_or_else(|| {
            AppError::Internal("active Attempt link has no Attempt id".to_owned())
        })?;
        if actor_attempt_id != caller_attempt_id {
            return Err(AppError::NotFound(
                "execution Attempt for Agent caller".to_owned(),
            ));
        }
        let operation_id = attempt_delegation_operation_id(
            &link.execution_id,
            &caller_step_id,
            &caller_attempt_id,
            &goal,
            requested_model_pool.as_ref(),
            explicit_steps.as_deref(),
        )?;
        // Probe before any liveness/version check and before invoking Planner.
        // This is the fast replay path after a committed response was lost.
        if let Some(replayed) = self
            .repository
            .find_steps_append_from_attempt(owner_id, &link.execution_id, &operation_id)
            .await?
        {
            let added_step_ids = replayed.added_step_ids;
            let detail = domain_mapper::detail(replayed.detail)?;
            // A replay may be the first request after a crash between commit
            // and outbox publication; draining is idempotent.
            self.publish().await;
            if matches!(
                detail.execution.status,
                AgentExecutionStatus::Running | AgentExecutionStatus::WaitingInput
            ) {
                self.scheduler
                    .start(owner_id.to_owned(), detail.execution.execution_id.clone());
            }
            return Ok((detail, added_step_ids));
        }
        if !link.active {
            return Err(AppError::Conflict(
                "delegation requires the active linked Attempt".to_owned(),
            ));
        }
        let detail = self.detail(owner_id, &link.execution_id).await?;
        if !matches!(
            detail.execution.status,
            AgentExecutionStatus::Running | AgentExecutionStatus::WaitingInput
        ) {
            return Err(AppError::Conflict(
                "delegation requires an active Agent Execution".to_owned(),
            ));
        }
        if detail.execution.delegation_policy == nomifun_common::DelegationPolicy::Disabled {
            return Err(AppError::Conflict(
                "delegation is disabled for this Agent Execution".to_owned(),
            ));
        }
        let caller_step = current_step(&detail, &caller_step_id)?;
        if caller_step.status != ExecutionStepStatus::Running {
            return Err(AppError::Conflict(
                "delegation requires a running caller Step".to_owned(),
            ));
        }
        let caller_attempt = detail
            .attempts
            .iter()
            .find(|attempt| attempt.attempt_id == caller_attempt_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Execution attempt {caller_attempt_id}"))
            })?;
        if caller_attempt.status != ExecutionAttemptStatus::Running
            || caller_attempt.conversation_id.as_deref() != Some(conversation_id)
        {
            return Err(AppError::Conflict(
                "delegation requires the running linked Attempt".to_owned(),
            ));
        }
        let planning_participants = participants_for_model_pool(&detail, &model_pool)?;
        let plan = match explicit_steps {
            Some(steps) => PlannedExecution { steps },
            None => {
                let planner_goal = format!(
                    "Shared execution goal: {}\n\nDelegated objective from Step '{}': {}",
                    detail.execution.goal, caller_step.title, goal
                );
                self.produce_plan(
                    owner_id,
                    &detail.execution.execution_id,
                    &planner_goal,
                    &planning_participants,
                )
                .await?
            }
        };
        let materialized = plan_materializer::materialize(plan, &planning_participants)?;
        let active_step_count = detail
            .steps
            .iter()
            .filter(|step| step.superseded_in_revision.is_none())
            .count();
        validate_final_step_count(active_step_count, materialized.steps.len())?;
        let appended = self
            .repository
            .append_steps_from_attempt(
                owner_id,
                &detail.execution.execution_id,
                &AppendAgentExecutionStepsFromAttemptParams {
                    operation_id,
                    caller_conversation_id: conversation_id.to_owned(),
                    caller_step_id: caller_step_id.clone(),
                    caller_attempt_id: caller_attempt_id.clone(),
                    expected_caller_step_version: caller_step.version,
                    expected_caller_attempt_version: caller_attempt.version,
                    new_steps: materialized.steps,
                    new_dependencies: materialized.dependencies,
                },
                &actor_event(
                    actor,
                    AgentExecutionEventKind::PlanChanged,
                    Some(&caller_step_id),
                    Some(&caller_attempt_id),
                    json!({
                        "change":"delegated_steps_appended",
                    }),
                ),
            )
            .await?;
        self.publish().await;
        let added_step_ids = appended.added_step_ids;
        let detail = domain_mapper::detail(appended.detail)?;
        // Starting an already-owned scheduler is idempotent. In particular,
        // max_parallel=1 leaves the new Steps Pending until this caller turn
        // returns and releases its sole slot; there is no nested wait/deadlock.
        self.scheduler
            .start(owner_id.to_owned(), detail.execution.execution_id.clone());
        Ok((detail, added_step_ids))
    }

    /// Authorize a model caller against the target execution's active
    /// conversation relation and return its canonical audit identity.
    ///
    /// Owner scope alone is intentionally insufficient. Exactly one active
    /// lead or attempt link must match the target; zero is reported as not
    /// found and ambiguity fails closed.
    pub async fn authorize_agent_caller(
        &self,
        owner_id: &str,
        execution_id: &str,
        conversation_id: &str,
    ) -> Result<AgentExecutionActor, AppError> {
        canonical_id::<AgentExecutionId>("execution_id", execution_id)?;
        canonical_id::<ConversationId>("conversation_id", conversation_id)?;
        let links = self
            .repository
            .resolve_conversation_link(owner_id, conversation_id)
            .await?
            .into_iter()
            .filter(|link| link.active)
            .collect::<Vec<_>>();
        let target_links = links
            .iter()
            .filter(|link| link.execution_id == execution_id)
            .collect::<Vec<_>>();
        let link = target_links
            .first()
            .ok_or_else(|| AppError::NotFound(format!("Agent Execution {execution_id}")))?;
        if target_links.len() != 1 {
            return Err(AppError::Conflict(
                "Agent caller has ambiguous active links to the execution".to_owned(),
            ));
        }
        let attempt_id = match link.relation.as_str() {
            "lead" => {
                let attempt_links = links
                    .iter()
                    .filter(|candidate| candidate.relation == "attempt")
                    .collect::<Vec<_>>();
                if attempt_links.len() > 1 {
                    return Err(AppError::Conflict(
                        "Agent caller has multiple active attempt relations".to_owned(),
                    ));
                }
                attempt_links
                    .first()
                    .map(|candidate| {
                        candidate.attempt_id.clone().ok_or_else(|| {
                            AppError::Internal(
                                "active attempt link has no attempt id".to_owned(),
                            )
                        })
                    })
                    .transpose()?
            }
            "attempt" => Some(link.attempt_id.clone().ok_or_else(|| {
                AppError::Internal("active attempt link has no attempt id".to_owned())
            })?),
            _ => {
                return Err(AppError::Internal(
                    "active execution link has an unknown relation".to_owned(),
                ));
            }
        };
        Ok(AgentExecutionActor::agent(
            conversation_id,
            attempt_id,
        ))
    }

    /// Build canonical attribution before a delegation command. A top-level
    /// Conversation has no Attempt id; an Attempt transcript carries its exact
    /// id even after settlement so a committed delegation can be replayed and
    /// the audit Conversation can never become a new top-level lead.
    pub async fn agent_caller_for_delegation(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<AgentExecutionActor, AppError> {
        canonical_id::<ConversationId>("conversation_id", conversation_id)?;
        let mut attempts = self
            .repository
            .resolve_conversation_link(owner_id, conversation_id)
            .await?
            .into_iter()
            .filter(|link| link.relation == "attempt");
        let attempt_id = match attempts.next() {
            Some(link) => Some(link.attempt_id.ok_or_else(|| {
                AppError::Internal("attempt link has no attempt id".to_owned())
            })?),
            None => None,
        };
        if attempts.next().is_some() {
            return Err(AppError::Conflict(
                "Agent caller has multiple attempt relations".to_owned(),
            ));
        }
        Ok(AgentExecutionActor::agent(
            conversation_id,
            attempt_id.map(|id| id.to_string()),
        ))
    }

    pub async fn list(
        &self,
        owner_id: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<AgentExecution>, AppError> {
        let limit = limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT);
        let offset = offset.unwrap_or(0).max(0);
        let rows = self
            .repository
            .list_executions(owner_id, limit, offset)
            .await?;
        let mut executions = Vec::with_capacity(rows.len());
        for row in rows {
            let links = self
                .repository
                .list_conversation_links(owner_id, &row.execution_id)
                .await?;
            let lead = links
                .iter()
                .find(|link| link.active && link.relation == "lead")
                .map(|link| link.conversation_id.clone());
            executions.push(domain_mapper::execution(row, lead)?);
        }
        Ok(executions)
    }

    pub async fn events(
        &self,
        owner_id: &str,
        execution_id: &str,
        after_sequence: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Vec<AgentExecutionEvent>, AppError> {
        // Enforce owner scope even when the page happens to be empty.
        self.require_execution(owner_id, execution_id).await?;
        self.repository
            .list_events(
                owner_id,
                execution_id,
                after_sequence.unwrap_or(0).max(0),
                limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT),
            )
            .await?
            .into_iter()
            .map(domain_mapper::event)
            .collect()
    }

    pub async fn browse_workspace(
        &self,
        owner_id: &str,
        execution_id: &str,
        path: &str,
        search: Option<&str>,
    ) -> Result<Vec<WorkspaceEntry>, AppError> {
        let execution = self.require_execution(owner_id, execution_id).await?;
        let root = execution
            .work_dir
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "Agent Execution {execution_id} has no working directory"
                ))
            })?;
        nomifun_file::list_workspace_level(std::path::Path::new(root), path, search)
    }

    pub async fn approve(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        command: VersionedAgentExecutionCommand,
    ) -> Result<AgentExecution, AppError> {
        let current = self.require_execution(owner_id, execution_id).await?;
        require_status(current.status, &[AgentExecutionStatus::AwaitingApproval])?;
        let row = self
            .repository
            .update_execution(
                owner_id,
                execution_id,
                command.expected_version,
                None,
                &UpdateAgentExecutionParams {
                    status: Some(AgentExecutionStatus::Running),
                    ..Default::default()
                },
                &actor_event(
                    actor,
                    AgentExecutionEventKind::StatusChanged,
                    None,
                    None,
                    json!({"status":"running","reason":"plan_approved"}),
                ),
            )
            .await?;
        self.publish().await;
        self.scheduler
            .start(owner_id.to_owned(), execution_id.to_owned());
        domain_mapper::execution(row, current.lead_conversation_id)
    }

    pub async fn pause(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        command: VersionedAgentExecutionCommand,
    ) -> Result<AgentExecution, AppError> {
        let current = self.detail(owner_id, execution_id).await?;
        require_status(
            current.execution.status,
            &[
                AgentExecutionStatus::Running,
                AgentExecutionStatus::WaitingInput,
            ],
        )?;
        let row = self
            .repository
            .pause_execution(
                owner_id,
                execution_id,
                command.expected_version,
                &actor_event(
                    actor,
                    AgentExecutionEventKind::StatusChanged,
                    None,
                    None,
                    json!({"status":"paused"}),
                ),
            )
            .await?;
        self.scheduler.stop(execution_id);
        self.scheduler
            .cancel_conversations(owner_id, &current)
            .await;
        self.publish().await;
        domain_mapper::execution(row, current.execution.lead_conversation_id)
    }

    pub async fn resume(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        command: VersionedAgentExecutionCommand,
    ) -> Result<AgentExecution, AppError> {
        let current = self.detail(owner_id, execution_id).await?;
        require_status(current.execution.status, &[AgentExecutionStatus::Paused])?;
        let resumed_status = if current
            .attempts
            .iter()
            .any(|attempt| attempt.status == ExecutionAttemptStatus::WaitingInput)
        {
            AgentExecutionStatus::WaitingInput
        } else {
            AgentExecutionStatus::Running
        };
        let row = self
            .repository
            .resume_execution(
                owner_id,
                execution_id,
                command.expected_version,
                &actor_event(
                    actor,
                    AgentExecutionEventKind::StatusChanged,
                    None,
                    None,
                    json!({"status":resumed_status,"reason":"resumed"}),
                ),
            )
            .await?;
        self.publish().await;
        self.scheduler
            .start(owner_id.to_owned(), execution_id.to_owned());
        let execution = domain_mapper::execution(row, current.execution.lead_conversation_id)?;
        if execution.status != resumed_status {
            return Err(AppError::Internal(
                "atomic resume returned an unexpected aggregate status".to_owned(),
            ));
        }
        Ok(execution)
    }

    pub async fn cancel(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        command: VersionedAgentExecutionCommand,
    ) -> Result<AgentExecutionDetail, AppError> {
        let before = self.detail(owner_id, execution_id).await?;
        if before.execution.status.is_terminal() {
            return Err(AppError::Conflict(
                "a settled execution cannot be cancelled".to_owned(),
            ));
        }
        let rows = self
            .repository
            .cancel_execution(
                owner_id,
                execution_id,
                command.expected_version,
                &actor_event(
                    actor,
                    AgentExecutionEventKind::StatusChanged,
                    None,
                    None,
                    explicit_cancel_payload(),
                ),
            )
            .await?;
        self.scheduler.stop(execution_id);
        self.scheduler.cancel_conversations(owner_id, &before).await;
        // An explicit cancel is already part of the caller's synchronous turn.
        // Publishing its durable state is required, but projecting another
        // assistant result into the lead Conversation would duplicate the
        // caller's synchronous response. Only asynchronous terminal paths use
        // the lead-result outbox marker and `after_terminal_commit`.
        self.publish().await;
        domain_mapper::detail(rows)
    }

    pub async fn delete(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        command: VersionedAgentExecutionCommand,
    ) -> Result<(), AppError> {
        let before = self.detail(owner_id, execution_id).await?;
        if before.execution.version != command.expected_version {
            return Err(AppError::Conflict(
                "stale Agent Execution version".to_owned(),
            ));
        }
        if !self
            .repository
            .delete_execution(
                owner_id,
                execution_id,
                command.expected_version,
                &actor_event(
                    actor,
                    AgentExecutionEventKind::Deleted,
                    None,
                    None,
                    json!({"deleted":true}),
                ),
            )
            .await?
        {
            return Err(AppError::NotFound(format!(
                "Agent Execution {execution_id}"
            )));
        }
        self.scheduler.stop(execution_id);
        self.scheduler
            .cancel_conversations(owner_id, &before)
            .await;
        self.publish().await;
        Ok(())
    }

    pub async fn rename(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        request: RenameAgentExecutionRequest,
    ) -> Result<AgentExecution, AppError> {
        let goal = non_empty("goal", request.goal)?;
        let current = self.require_execution(owner_id, execution_id).await?;
        let row = self
            .repository
            .update_execution(
                owner_id,
                execution_id,
                request.expected_version,
                None,
                &UpdateAgentExecutionParams {
                    goal: Some(goal),
                    ..Default::default()
                },
                &actor_event(
                    actor,
                    AgentExecutionEventKind::StatusChanged,
                    None,
                    None,
                    json!({"change":"goal_renamed"}),
                ),
            )
            .await?;
        self.publish().await;
        domain_mapper::execution(row, current.lead_conversation_id)
    }

    pub async fn replan(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        request: ReplanAgentExecutionRequest,
    ) -> Result<AgentExecutionDetail, AppError> {
        let before = self.detail(owner_id, execution_id).await?;
        if before.execution.status.is_terminal() {
            return Err(AppError::Conflict(
                "retry or create a new execution instead of replanning a settled execution"
                    .to_owned(),
            ));
        }
        let goal_update = request
            .goal
            .map(|value| non_empty("goal", value))
            .transpose()?;
        let goal = goal_update
            .clone()
            .unwrap_or_else(|| before.execution.goal.clone());
        let new_participants = match request.model_pool.as_ref() {
            Some(pool) => self.resolver.resolve(pool, None).await?,
            None => Vec::new(),
        };
        let planning_participants = if new_participants.is_empty() {
            active_participants(&before)
        } else {
            participants_from_new(
                execution_id,
                before.execution.plan_revision + 1,
                &new_participants,
            )?
        };
        let plan = self
            .produce_plan(owner_id, execution_id, &goal, &planning_participants)
            .await?;
        let materialized = plan_materializer::materialize(plan, &planning_participants)?;
        let target_gate = request.plan_gate.unwrap_or(before.execution.plan_gate);
        let status = planned_status(target_gate);
        let retire_participant_ids = if new_participants.is_empty() {
            Vec::new()
        } else {
            before
                .participants
                .iter()
                .filter(|participant| participant.retired_in_revision.is_none())
                .map(|participant| participant.participant_id.clone())
                .collect()
        };
        let rows = self
            .repository
            .reconcile_plan(
                owner_id,
                execution_id,
                request.expected_version,
                &ReconcileAgentExecutionPlanParams {
                    goal: goal_update,
                    plan_gate: request.plan_gate,
                    adaptation_policy: request.adaptation_policy,
                    decision_policy: request.decision_policy,
                    delegation_policy: request.delegation_policy,
                    keep_step_ids: Vec::new(),
                    new_participants,
                    retire_participant_ids,
                    new_steps: materialized.steps,
                    new_dependencies: materialized.dependencies,
                    execution_status: status,
                },
                &actor_event(
                    actor,
                    AgentExecutionEventKind::PlanChanged,
                    None,
                    None,
                    json!({"change":"replanned"}),
                ),
            )
            .await?;
        self.scheduler.stop(execution_id);
        self.scheduler.cancel_conversations(owner_id, &before).await;
        self.publish().await;
        let detail = domain_mapper::detail(rows)?;
        if detail.execution.status == AgentExecutionStatus::Running {
            self.scheduler
                .start(owner_id.to_owned(), execution_id.to_owned());
        }
        Ok(detail)
    }

    pub async fn adjust(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        request: AdjustAgentExecutionRequest,
    ) -> Result<AgentExecutionDetail, AppError> {
        let intent = non_empty("intent", request.intent)?;
        let before = self.detail(owner_id, execution_id).await?;
        if !matches!(
            before.execution.status,
            AgentExecutionStatus::Running
                | AgentExecutionStatus::Paused
                | AgentExecutionStatus::AwaitingApproval
        ) {
            return Err(AppError::Conflict(
                "only a running, paused, or approval-gated execution can be adjusted".to_owned(),
            ));
        }
        let adjusted = self
            .produce_adjustment(owner_id, execution_id, &intent, &before)
            .await?;
        let (keep_step_ids, materialized, dependencies) =
            materialize_adjustment(adjusted, &before)?;
        validate_final_step_count(keep_step_ids.len(), materialized.steps.len())?;
        let status = match before.execution.status {
            AgentExecutionStatus::Paused => AgentExecutionStatus::Paused,
            AgentExecutionStatus::AwaitingApproval => AgentExecutionStatus::AwaitingApproval,
            _ => AgentExecutionStatus::Running,
        };
        let superseded: HashSet<String> = before
            .steps
            .iter()
            .filter(|step| step.superseded_in_revision.is_none())
            .map(|step| step.step_id.clone())
            .filter(|id| !keep_step_ids.contains(id))
            .collect();
        let rows = self
            .repository
            .reconcile_plan(
                owner_id,
                execution_id,
                request.expected_version,
                &ReconcileAgentExecutionPlanParams {
                    goal: None,
                    plan_gate: None,
                    adaptation_policy: None,
                    decision_policy: None,
                    delegation_policy: None,
                    keep_step_ids,
                    new_participants: Vec::new(),
                    retire_participant_ids: Vec::new(),
                    new_steps: materialized.steps,
                    new_dependencies: dependencies,
                    execution_status: status,
                },
                &actor_event(
                    actor,
                    AgentExecutionEventKind::PlanChanged,
                    None,
                    None,
                    json!({"change":"adjusted","intent":intent}),
                ),
            )
            .await?;
        self.scheduler.stop(execution_id);
        self.scheduler
            .cancel_conversations_for_steps(owner_id, &before, &superseded)
            .await;
        self.publish().await;
        let detail = domain_mapper::detail(rows)?;
        if detail.execution.status == AgentExecutionStatus::Running {
            self.scheduler
                .start(owner_id.to_owned(), execution_id.to_owned());
        }
        Ok(detail)
    }

    pub async fn add_steps(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        request: AddExecutionStepsRequest,
    ) -> Result<AgentExecutionDetail, AppError> {
        if request.steps.is_empty() {
            return Err(AppError::BadRequest("steps must not be empty".to_owned()));
        }
        if actor.attempt_id().is_some() {
            return Err(AppError::Conflict(
                "an Attempt Agent must append work through delegate_from_attempt".to_owned(),
            ));
        }
        let mut before = self.detail(owner_id, execution_id).await?;
        if before.execution.version != request.expected_version {
            return Err(AppError::Conflict("stale Agent Execution version".to_owned()));
        }
        if matches!(
            before.execution.status,
            AgentExecutionStatus::Planning | AgentExecutionStatus::Cancelled
        ) {
            return Err(AppError::Conflict(
                "steps cannot be added in the current execution state".to_owned(),
            ));
        }
        if before.execution.status.is_terminal() {
            self.scheduler
                .ensure_terminal_projection_delivered(owner_id, &before)
                .await?;
            before = self.detail(owner_id, execution_id).await?;
        }
        let active_step_count = before
            .steps
            .iter()
            .filter(|step| step.superseded_in_revision.is_none())
            .count();
        let materialized = plan_materializer::materialize(
            PlannedExecution {
                steps: request.steps,
            },
            &active_participants(&before),
        )?;
        validate_final_step_count(active_step_count, materialized.steps.len())?;
        let rows = self
            .repository
            .append_steps(
                owner_id,
                execution_id,
                before.execution.version,
                &AppendAgentExecutionStepsParams {
                    new_steps: materialized.steps,
                    new_dependencies: materialized.dependencies,
                },
                &actor_event(
                    actor,
                    AgentExecutionEventKind::PlanChanged,
                    None,
                    None,
                    json!({"change":"steps_added"}),
                ),
            )
            .await?;
        self.publish().await;
        let detail = domain_mapper::detail(rows)?;
        if matches!(
            detail.execution.status,
            AgentExecutionStatus::Running | AgentExecutionStatus::WaitingInput
        ) {
            self.scheduler
                .start(owner_id.to_owned(), execution_id.to_owned());
        }
        Ok(detail)
    }

    pub async fn update_step(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        step_id: &str,
        request: UpdateExecutionStepRequest,
    ) -> Result<ExecutionStep, AppError> {
        if request.title.is_none() && request.spec.is_none() {
            return Err(AppError::BadRequest("empty step update".to_owned()));
        }
        let title = request
            .title
            .map(|value| non_empty("title", value))
            .transpose()?;
        let spec = request
            .spec
            .map(|value| non_empty("spec", value))
            .transpose()?;
        let detail = self.detail(owner_id, execution_id).await?;
        let step = require_pending_agent_step(&detail, step_id)?;
        validate_step_command_versions(
            &detail,
            step,
            request.expected_execution_version,
            request.expected_step_version,
        )?;
        let mut replacement = replacement_step_snapshot(step)?;
        if let Some(title) = title {
            replacement.title = title;
        }
        if let Some(spec) = spec {
            replacement.spec = spec;
        }
        let payload = snapshot_replacement_payload(
            "content_updated",
            &detail,
            step,
            &replacement,
        )?;
        let old_step_id = step.step_id.clone();
        self.replace_pending_step_snapshot(
            owner_id,
            actor,
            detail,
            &old_step_id,
            request.expected_execution_version,
            replacement,
            Vec::new(),
            payload,
        )
        .await
    }

    pub async fn reassign_step(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        step_id: &str,
        request: ReassignExecutionStepRequest,
    ) -> Result<ExecutionStep, AppError> {
        canonical_uuidv7("participant_id", &request.participant_id)?;
        let detail = self.detail(owner_id, execution_id).await?;
        let step = require_pending_agent_step(&detail, step_id)?;
        let participant = detail
            .participants
            .iter()
            .find(|participant| {
                participant.participant_id == request.participant_id
                    && participant.retired_in_revision.is_none()
            })
            .ok_or_else(|| {
                AppError::BadRequest(
                    "participant is not active in this execution".to_owned(),
                )
            })?;
        if step
            .profile
            .as_ref()
            .is_some_and(|profile| score_participant(participant, profile).is_none())
        {
            return Err(AppError::BadRequest(
                "participant does not satisfy this step profile".to_owned(),
            ));
        }
        validate_step_command_versions(
            &detail,
            step,
            request.expected_execution_version,
            request.expected_step_version,
        )?;
        let mut replacement = replacement_step_snapshot(step)?;
        replacement.assigned_participant_id = Some(request.participant_id.clone());
        replacement.assignment_source = Some(ParticipantAssignmentSource::Manual);
        replacement.assignment_score = None;
        replacement.assignment_rationale = None;
        replacement.assignment_locked = request.locked;
        let payload = snapshot_replacement_payload(
            "participant_reassigned",
            &detail,
            step,
            &replacement,
        )?;
        let old_step_id = step.step_id.clone();
        self.replace_pending_step_snapshot(
            owner_id,
            actor,
            detail,
            &old_step_id,
            request.expected_execution_version,
            replacement,
            Vec::new(),
            payload,
        )
        .await
    }

    pub async fn configure_step(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        step_id: &str,
        request: ConfigureExecutionStepRequest,
    ) -> Result<ExecutionStep, AppError> {
        if request.model.is_none() && request.preset_prompt.is_none() {
            return Err(AppError::BadRequest("empty step configuration".to_owned()));
        }
        let detail = self.detail(owner_id, execution_id).await?;
        let step = current_step(&detail, step_id)?;
        if step.kind != ExecutionStepKind::Agent || step.status != ExecutionStepStatus::Pending {
            return Err(AppError::Conflict(
                "only a pending Agent step can be configured".to_owned(),
            ));
        }
        validate_step_command_versions(
            &detail,
            step,
            request.expected_execution_version,
            request.expected_step_version,
        )?;
        let (new_participant, assignment) = match request.model {
            None => (None, None),
            Some(Some(model)) => {
                if let Some(existing) = detail.participants.iter().find(|participant| {
                    participant.retired_in_revision.is_none()
                        && participant.preset_id.is_none()
                        && participant.provider_id.as_deref() == Some(model.provider_id.as_str())
                        && participant.model.as_deref() == Some(model.model.as_str())
                }) {
                    (
                        None,
                        Some(StepAssignment {
                            participant_id: existing.participant_id.clone(),
                            source: ParticipantAssignmentSource::Manual,
                            score: None,
                            rationale: Some("user-selected model".to_owned()),
                            locked: true,
                        }),
                    )
                } else {
                    let participant = self
                        .resolver
                        .resolve(&ExecutionModelPool::Single { model }, None)
                        .await?
                        .into_iter()
                        .find(|participant| participant.preset_id.is_none())
                        .ok_or_else(|| {
                            AppError::ProviderUnavailable(
                                "selected model produced no execution participant".to_owned(),
                            )
                        })?;
                    let assignment = StepAssignment {
                        participant_id: participant.participant_id.clone(),
                        source: ParticipantAssignmentSource::Manual,
                        score: None,
                        rationale: Some("user-selected model".to_owned()),
                        locked: true,
                    };
                    (Some(participant), Some(assignment))
                }
            }
            Some(None) => (
                None,
                Some(automatic_assignment(&detail, step)?),
            ),
        };
        let mut replacement = replacement_step_snapshot(step)?;
        if let Some(assignment) = assignment {
            replacement.assigned_participant_id = Some(assignment.participant_id);
            replacement.assignment_source = Some(assignment.source);
            replacement.assignment_score = assignment.score;
            replacement.assignment_rationale = assignment.rationale;
            replacement.assignment_locked = assignment.locked;
        }
        if let Some(preset_prompt) = request.preset_prompt {
            replacement.preset_prompt = preset_prompt;
        }
        let payload = snapshot_replacement_payload(
            "configured",
            &detail,
            step,
            &replacement,
        )?;
        let old_step_id = step.step_id.clone();
        self.replace_pending_step_snapshot(
            owner_id,
            actor,
            detail,
            &old_step_id,
            request.expected_execution_version,
            replacement,
            new_participant.into_iter().collect(),
            payload,
        )
        .await
    }

    /// Replace one pending node as a new immutable semantic snapshot. The
    /// repository commits the new plan revision, supersedes the old row and
    /// every old edge, introduces any new participant, rewrites the complete
    /// current DAG, and appends the audit event in one transaction.
    async fn replace_pending_step_snapshot(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        before: AgentExecutionDetail,
        old_step_id: &str,
        expected_execution_version: i64,
        replacement: NewAgentExecutionStep,
        new_participants: Vec<NewAgentExecutionParticipant>,
        event_payload: serde_json::Value,
    ) -> Result<ExecutionStep, AppError> {
        let replacement_id = replacement.step_id.clone();
        let keep_step_ids = before
            .steps
            .iter()
            .filter(|step| {
                step.superseded_in_revision.is_none() && step.step_id != old_step_id
            })
            .map(|step| step.step_id.clone())
            .collect();
        let dependencies = before
            .dependencies
            .iter()
            .filter(|dependency| dependency.superseded_in_revision.is_none())
            .map(|dependency| NewAgentExecutionStepDependency {
                blocker_step_id: replace_id(
                    &dependency.blocker_step_id,
                    old_step_id,
                    &replacement_id,
                ),
                blocked_step_id: replace_id(
                    &dependency.blocked_step_id,
                    old_step_id,
                    &replacement_id,
                ),
            })
            .collect();
        let execution_status = before.execution.status;
        let rows = self
            .repository
            .reconcile_plan(
                owner_id,
                &before.execution.execution_id,
                expected_execution_version,
                &ReconcileAgentExecutionPlanParams {
                    goal: None,
                    plan_gate: None,
                    adaptation_policy: None,
                    decision_policy: None,
                    delegation_policy: None,
                    keep_step_ids,
                    new_participants,
                    retire_participant_ids: Vec::new(),
                    new_steps: vec![replacement],
                    new_dependencies: dependencies,
                    execution_status,
                },
                &actor_event(
                    actor,
                    AgentExecutionEventKind::StepChanged,
                    Some(old_step_id),
                    None,
                    event_payload,
                ),
            )
            .await?;
        self.publish().await;
        let after = domain_mapper::detail(rows)?;
        if execution_status == AgentExecutionStatus::Running {
            self.scheduler
                .start(owner_id.to_owned(), before.execution.execution_id.clone());
        }
        after
            .steps
            .iter()
            .find(|step| {
                step.superseded_in_revision.is_none()
                    && step.introduced_in_revision == after.execution.plan_revision
            })
            .cloned()
            .ok_or_else(|| AppError::Internal("replacement step was not returned".to_owned()))
    }

    pub async fn retry_step(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        step_id: &str,
        request: RetryExecutionStepRequest,
    ) -> Result<AgentExecutionDetail, AppError> {
        let mut detail = self.detail(owner_id, execution_id).await?;
        if detail.execution.version != request.expected_execution_version {
            return Err(AppError::Conflict("stale Agent Execution version".to_owned()));
        }
        if detail.execution.status == AgentExecutionStatus::Cancelled {
            return Err(AppError::Conflict(
                "a cancelled Agent Execution cannot be reopened".to_owned(),
            ));
        }
        if detail.execution.status.is_terminal() {
            self.scheduler
                .ensure_terminal_projection_delivered(owner_id, &detail)
                .await?;
            detail = self.detail(owner_id, execution_id).await?;
        }
        let command_execution_version = detail.execution.version;
        let step = current_step(&detail, step_id)?;
        if step.version != request.expected_step_version {
            return Err(AppError::Conflict("stale execution step version".to_owned()));
        }
        let rows = self
            .repository
            .reset_steps_for_retry(
                owner_id,
                execution_id,
                command_execution_version,
                &[RetryAgentExecutionStep {
                    step_id: step_id.to_owned(),
                    expected_step_version: request.expected_step_version,
                }],
                &actor_event(
                    actor,
                    AgentExecutionEventKind::StepChanged,
                    Some(step_id),
                    None,
                    json!({"change":"retry_requested"}),
                ),
            )
            .await?;
        self.publish().await;
        self.scheduler
            .start(owner_id.to_owned(), execution_id.to_owned());
        domain_mapper::detail(rows)
    }

    pub async fn adopt_step_output(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        step_id: &str,
        request: AdoptExecutionStepOutputRequest,
    ) -> Result<AgentExecutionDetail, AppError> {
        let mut detail = self.detail(owner_id, execution_id).await?;
        if detail.execution.version != request.expected_execution_version {
            return Err(AppError::Conflict("stale Agent Execution version".to_owned()));
        }
        if detail.execution.status == AgentExecutionStatus::Cancelled {
            return Err(AppError::Conflict(
                "a cancelled Agent Execution cannot be reopened".to_owned(),
            ));
        }
        if detail.execution.status.is_terminal() {
            self.scheduler
                .ensure_terminal_projection_delivered(owner_id, &detail)
                .await?;
            detail = self.detail(owner_id, execution_id).await?;
        }
        let command_execution_version = detail.execution.version;
        let step = current_step(&detail, step_id)?;
        if step.version != request.expected_step_version {
            return Err(AppError::Conflict("stale execution step version".to_owned()));
        }
        let conversation_id = detail
            .attempts
            .iter()
            .filter(|attempt| attempt.step_id == step_id)
            .max_by_key(|attempt| attempt.attempt_no)
            .and_then(|attempt| attempt.conversation_id.clone())
            .ok_or_else(|| AppError::BadRequest("step has no Agent conversation to adopt".to_owned()))?;
        let output = self
            .scheduler
            .read_attempt_output(owner_id, &conversation_id)
            .await
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| AppError::BadRequest("Agent conversation has no final output".to_owned()))?;
        let output_files = self
            .scheduler
            .read_attempt_output_files(owner_id, &conversation_id)
            .await;
        validate_required_artifacts(&step.spec, &output_files).map_err(|error| {
            AppError::BadRequest(format!(
                "Agent conversation output cannot be adopted: {error}"
            ))
        })?;
        let output_files = serde_json::to_string(&output_files)
            .map_err(|error| AppError::Internal(format!("encode verified adopted output files: {error}")))?;
        self.repository
            .adopt_step_output(
                owner_id,
                execution_id,
                command_execution_version,
                step_id,
                request.expected_step_version,
                &AdoptAgentExecutionStepOutputParams {
                    output_summary: output,
                    output_files,
                    tokens: None,
                    runtime_state: None,
                },
                &actor_event(
                    actor,
                    AgentExecutionEventKind::AttemptChanged,
                    Some(step_id),
                    None,
                    json!({"change":"output_adopted"}),
                ),
            )
            .await?;
        self.publish().await;
        self.scheduler
            .start(owner_id.to_owned(), execution_id.to_owned());
        self.detail(owner_id, execution_id).await
    }

    pub async fn steer_step(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        step_id: &str,
        request: SteerExecutionStepRequest,
    ) -> Result<(), AppError> {
        let text = non_empty("text", request.text)?;
        let detail = self.detail(owner_id, execution_id).await?;
        let step = current_step(&detail, step_id)?;
        if detail.execution.version != request.expected_execution_version
            || step.version != request.expected_step_version
        {
            return Err(AppError::Conflict(
                "execution changed before the steer command".to_owned(),
            ));
        }
        let mut active_attempts = detail.attempts.iter().filter(|attempt| {
            attempt.step_id == step_id && attempt.status == ExecutionAttemptStatus::Running
        });
        let attempt = active_attempts
            .next()
            .ok_or_else(|| AppError::Conflict("step has no running Agent attempt".to_owned()))?;
        if active_attempts.next().is_some() {
            return Err(AppError::Internal(
                "step has multiple active Agent attempts".to_owned(),
            ));
        }
        let mut effects = attempt
            .runtime_state
            .clone()
            .map(serde_json::from_value::<AttemptConversationEffects>)
            .transpose()
            .map_err(|error| {
                AppError::Internal(format!("invalid persisted attempt conversation effects: {error}"))
            })?
            .unwrap_or_default();
        let operation_id = generate_id();
        effects.push_steer(operation_id.clone(), text.clone())?;
        let persisted = self
            .repository
            .enqueue_attempt_conversation_effect(
                owner_id,
                execution_id,
                request.expected_execution_version,
                step_id,
                request.expected_step_version,
                &attempt.attempt_id,
                attempt.version,
                &AttemptConversationEffectParams {
                    runtime_state: Some(effects.encode()?),
                },
                &actor_event(
                    actor,
                    AgentExecutionEventKind::StepChanged,
                    Some(step_id),
                    Some(&attempt.attempt_id),
                    json!({
                        "change":"conversation_effect_requested",
                        "effect":"steer",
                        "operation_id":operation_id,
                    }),
                ),
            )
            .await?;
        self.publish().await;

        // Delivery is attempted immediately for interactive latency, but the
        // committed runtime_state remains authoritative until acknowledgement.
        // Any crash or transport race is retried by scheduler recovery with the
        // same stable operation identity.
        let delivery = self
            .scheduler
            .steer_conversation(
                owner_id,
                &persisted.conversation_id,
                &operation_id,
                &text,
            )
            .await;
        if delivery.is_ok() {
            if let Some(persisted_attempt) = persisted.detail.current_attempt.as_ref() {
                if let Err(error) = self
                    .repository
                    .acknowledge_attempt_conversation_effect(
                        owner_id,
                        execution_id,
                        step_id,
                        &attempt.attempt_id,
                        persisted_attempt.attempt.version,
                        &AttemptConversationEffectParams { runtime_state: None },
                        &actor_event(
                            actor,
                            AgentExecutionEventKind::StepChanged,
                            Some(step_id),
                            Some(&attempt.attempt_id),
                            json!({
                                "change":"conversation_effect_delivered",
                                "effect":"steer",
                                "operation_id":operation_id,
                            }),
                        ),
                    )
                    .await
                {
                    tracing::warn!(%execution_id, %step_id, %error, "steer delivered; durable acknowledgement will be retried");
                }
            }
        } else if let Err(error) = delivery {
            tracing::warn!(%execution_id, %step_id, %error, "durable steer remains pending");
        }
        self.publish().await;
        self.scheduler
            .start(owner_id.to_owned(), execution_id.to_owned());
        Ok(())
    }

    pub async fn request_user_decision(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        conversation_id: &str,
        question: String,
    ) -> Result<AgentExecutionDetail, AppError> {
        let question = non_empty("question", question)?;
        let links = self
            .repository
            .resolve_conversation_link(owner_id, conversation_id)
            .await?;
        let mut links = links
            .into_iter()
            .filter(|link| link.active && link.relation == "attempt");
        let link = links
            .next()
            .ok_or_else(|| AppError::BadRequest("conversation is not an active execution attempt".to_owned()))?;
        if links.next().is_some() {
            return Err(AppError::Internal(
                "conversation has multiple active execution attempts".to_owned(),
            ));
        }
        let detail = self.detail(owner_id, &link.execution_id).await?;
        if detail.execution.decision_policy != DecisionPolicy::AskUser {
            return Err(AppError::Conflict(
                "this execution requires the Agent to decide automatically".to_owned(),
            ));
        }
        let step_id = link
            .step_id
            .ok_or_else(|| AppError::Internal("attempt link has no step".to_owned()))?;
        let attempt_id = link
            .attempt_id
            .ok_or_else(|| AppError::Internal("attempt link has no attempt".to_owned()))?;
        match actor {
            AgentExecutionActor::Agent {
                agent_id: _,
                conversation_id: actor_conversation_id,
                attempt_id: Some(actor_attempt_id),
            } if actor_conversation_id.as_deref() == Some(conversation_id)
                && canonical_uuidv7("actor.attempt_id", actor_attempt_id)? == attempt_id => {}
            _ => {
                return Err(AppError::NotFound(
                    "active execution attempt for Agent caller".to_owned(),
                ));
            }
        }
        let step = current_step(&detail, &step_id)?;
        let attempt = detail
            .attempts
            .iter()
            .find(|attempt| attempt.attempt_id == attempt_id)
            .ok_or_else(|| AppError::NotFound(format!("Execution attempt {attempt_id}")))?;
        if attempt.status != ExecutionAttemptStatus::Running {
            return Err(AppError::Conflict(
                "only a running attempt can request a decision".to_owned(),
            ));
        }
        let lease = self.scheduler.lease_token(&detail.execution.execution_id).ok_or_else(|| {
            AppError::Conflict(
                "the attempt scheduler no longer owns this execution; retry after recovery"
                    .to_owned(),
            )
        })?;
        let operation_id = generate_id();
        let mut effects = AttemptConversationEffects::default();
        effects.push_stop_turn(operation_id.clone())?;
        let persisted = self.repository
            .settle_attempt(
                owner_id,
                &detail.execution.execution_id,
                &step.step_id,
                step.version,
                &attempt.attempt_id,
                attempt.version,
                Some(&lease),
                &SettleAgentExecutionAttemptParams {
                    attempt_status: ExecutionAttemptStatus::WaitingInput,
                    step_status: ExecutionStepStatus::WaitingInput,
                    execution_status: Some(AgentExecutionStatus::WaitingInput),
                    question: Some(Some(question.clone())),
                    error: None,
                    output_summary: None,
                    output_files: None,
                    tokens: None,
                    retry_after: None,
                    runtime_state: Some(Some(effects.encode()?)),
                    started_at: None,
                    finished_at: None,
                    loop_repeat_reset: None,
                },
                &actor_event(
                    actor,
                    AgentExecutionEventKind::DecisionRequested,
                    Some(&step.step_id),
                    Some(&attempt.attempt_id),
                    json!({
                        "question":question,
                        "stop_turn_operation_id":operation_id,
                    }),
                ),
            )
            .await?;
        self.publish().await;
        // WaitingInput is committed before we request runtime stop. The
        // durable StopTurn effect survives transport failure/crash and is
        // retried under the same identity before any DecisionInput continuation.
        let delivery = self
            .scheduler
            .stop_attempt_turn(owner_id, conversation_id, &operation_id)
            .await;
        if delivery.is_ok() {
            if let Some(persisted_attempt) = persisted.current_attempt.as_ref()
                && let Err(error) = self
                    .repository
                    .acknowledge_attempt_conversation_effect(
                        owner_id,
                        &detail.execution.execution_id,
                        &step.step_id,
                        &attempt.attempt_id,
                        persisted_attempt.attempt.version,
                        &AttemptConversationEffectParams { runtime_state: None },
                        &actor_event(
                            actor,
                            AgentExecutionEventKind::StepChanged,
                            Some(&step.step_id),
                            Some(&attempt.attempt_id),
                            json!({
                                "change":"conversation_effect_delivered",
                                "effect":"stop_turn",
                                "operation_id":operation_id,
                            }),
                        ),
                    )
                    .await
            {
                tracing::warn!(
                    execution_id = %detail.execution.execution_id,
                    step_id = %step.step_id,
                    %error,
                    "turn stop delivered; durable acknowledgement will be retried"
                );
            }
        } else if let Err(error) = delivery {
            tracing::warn!(
                execution_id = %detail.execution.execution_id,
                step_id = %step.step_id,
                %error,
                "durable turn stop remains pending"
            );
        }
        self.publish().await;
        self.scheduler
            .start(owner_id.to_owned(), detail.execution.execution_id.clone());
        self.detail(owner_id, &detail.execution.execution_id).await
    }

    pub async fn answer_decision(
        &self,
        owner_id: &str,
        actor: &AgentExecutionActor,
        execution_id: &str,
        step_id: &str,
        attempt_id: &str,
        request: AnswerExecutionDecisionRequest,
    ) -> Result<AgentExecutionDetail, AppError> {
        canonical_id::<AgentExecutionId>("execution_id", execution_id)?;
        canonical_uuidv7("step_id", step_id)?;
        canonical_uuidv7("attempt_id", attempt_id)?;
        let answer = non_empty("answer", request.answer)?;
        let waiting = self
            .repository
            .get_attempt(owner_id, execution_id, step_id, attempt_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Execution attempt {attempt_id}")))?;
        if waiting.attempt.version != request.expected_attempt_version
            || waiting.attempt.status != ExecutionAttemptStatus::WaitingInput.to_string()
        {
            return Err(AppError::Conflict(
                "waiting attempt changed before the answer".to_owned(),
            ));
        }
        let mut effects = AttemptConversationEffects::decode(
            waiting.attempt.runtime_state.as_deref(),
        )?;
        let operation_id = generate_id();
        effects.push_decision(operation_id.clone(), answer)?;
        let resumed = self
            .repository
            .resume_waiting_attempt(
                owner_id,
                execution_id,
                request.expected_execution_version,
                step_id,
                request.expected_step_version,
                attempt_id,
                request.expected_attempt_version,
                &AttemptConversationEffectParams {
                    runtime_state: Some(effects.encode()?),
                },
                &actor_event(
                    actor,
                    AgentExecutionEventKind::DecisionAnswered,
                    Some(step_id),
                    Some(attempt_id),
                    json!({"answered":true,"operation_id":operation_id}),
                ),
            )
            .await?;
        let attempt = resumed
            .detail
            .current_attempt
            .ok_or_else(|| AppError::Internal("resumed attempt is missing".to_owned()))?;
        if attempt.conversation_id.as_deref() != Some(resumed.conversation_id.as_str()) {
            return Err(AppError::Internal(
                "resumed attempt conversation link changed inside its transaction".to_owned(),
            ));
        }
        self.publish().await;
        self.scheduler
            .start(owner_id.to_owned(), execution_id.to_owned());
        self.detail(owner_id, execution_id).await
    }

    pub async fn recover(&self) -> Result<(), AppError> {
        let statuses = [
            AgentExecutionStatus::Planning,
            AgentExecutionStatus::Running,
            AgentExecutionStatus::WaitingInput,
            AgentExecutionStatus::Completed,
            AgentExecutionStatus::CompletedWithFailures,
            AgentExecutionStatus::Failed,
            AgentExecutionStatus::Cancelled,
        ];
        for row in self
            .repository
            .list_recoverable_executions(&statuses)
            .await?
        {
            match row.status.parse::<AgentExecutionStatus>() {
                Ok(AgentExecutionStatus::Planning) => {
                    self.spawn_initial_plan(row.user_id, row.execution_id);
                }
                Ok(AgentExecutionStatus::Running | AgentExecutionStatus::WaitingInput) => {
                    self.scheduler.start(row.user_id, row.execution_id);
                }
                Ok(
                    AgentExecutionStatus::Completed
                    | AgentExecutionStatus::CompletedWithFailures
                    | AgentExecutionStatus::Failed
                    | AgentExecutionStatus::Cancelled,
                ) => {
                    self.scheduler
                        .after_terminal_commit(&row.user_id, &row.execution_id)
                        .await;
                }
                _ => {}
            }
        }
        self.scheduler.reconcile_conversation_cleanup(None).await;
        self.publish().await;
        Ok(())
    }

    pub fn is_active(&self, execution_id: &str) -> bool {
        self.scheduler.is_active(execution_id)
    }

    fn spawn_initial_plan(&self, owner_id: String, execution_id: String) {
        let engine = self.clone();
        tokio::spawn(async move {
            if let Err(error) = engine.plan_initial(&owner_id, &execution_id).await {
                tracing::error!(%execution_id, %error, "Agent Execution planning failed");
                engine
                    .fail_planning(&owner_id, &execution_id, &error.to_string())
                    .await;
            }
        });
    }

    async fn plan_initial(
        &self,
        owner_id: &str,
        execution_id: &str,
    ) -> Result<(), AppError> {
        let row = self
            .repository
            .get_execution(owner_id, execution_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Agent Execution {execution_id}")))?;
        let command: InitialPlanningCommand = serde_json::from_str(&row.initial_plan_input)
            .map_err(|error| {
                AppError::Internal(format!(
                    "invalid persisted initial planning input for {execution_id}: {error}"
                ))
            })?;
        let mut automatic_plan: Option<(String, String, PlannedExecution)> = None;
        loop {
            let detail = self.detail(owner_id, execution_id).await?;
            if detail.execution.status != AgentExecutionStatus::Planning {
                // A concurrent cancel, delete, or explicit replan won the CAS.
                // It is a normal lifecycle race, not a planning failure.
                return Ok(());
            }
            let participants = active_participants(&detail);
            let participant_signature = serde_json::to_string(&participants).map_err(|error| {
                AppError::Internal(format!(
                    "failed to fingerprint planning participants for {execution_id}: {error}"
                ))
            })?;
            let plan = match &command {
                InitialPlanningCommand::Explicit { plan } => plan.clone(),
                InitialPlanningCommand::Automatic {
                    supplemental_context,
                } => {
                    let planner_goal = planner_goal(
                        &detail.execution.goal,
                        supplemental_context.as_ref(),
                    )?;
                    if automatic_plan
                        .as_ref()
                        .is_none_or(|(goal, signature, _)| {
                            goal != &planner_goal
                                || signature != &participant_signature
                        })
                    {
                        let plan = self
                            .produce_plan(
                                owner_id,
                                execution_id,
                                &planner_goal,
                                &participants,
                            )
                            .await?;
                        automatic_plan = Some((
                            planner_goal,
                            participant_signature,
                            plan,
                        ));
                    }
                    automatic_plan
                        .as_ref()
                        .expect("automatic plan was initialized")
                        .2
                        .clone()
                }
            };
            let materialized = plan_materializer::materialize(plan, &participants)?;
            let status = planned_status(detail.execution.plan_gate);
            let result = self
                .repository
                .reconcile_plan(
                owner_id,
                execution_id,
                detail.execution.version,
                &ReconcileAgentExecutionPlanParams {
                    goal: None,
                    plan_gate: None,
                    adaptation_policy: None,
                    decision_policy: None,
                    delegation_policy: None,
                    keep_step_ids: Vec::new(),
                    new_participants: Vec::new(),
                    retire_participant_ids: Vec::new(),
                    new_steps: materialized.steps,
                    new_dependencies: materialized.dependencies,
                    execution_status: status,
                },
                &system_event(
                    AgentExecutionEventKind::PlanChanged,
                    None,
                    None,
                    json!({"status":status,"change":"initial_plan"}),
                ),
            )
                .await;
            match result {
                Ok(_) => {
                    self.publish().await;
                    if status == AgentExecutionStatus::Running {
                        self.scheduler
                            .start(owner_id.to_owned(), execution_id.to_owned());
                    }
                    return Ok(());
                }
                Err(nomifun_db::DbError::Conflict(_)) => {
                    // Reload and either submit against the latest version or,
                    // for automatic planning after a rename, regenerate from
                    // the new goal. Never convert a normal CAS race to Failed.
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    async fn produce_plan(
        &self,
        owner_id: &str,
        execution_id: &str,
        goal: &str,
        participants: &[ExecutionParticipant],
    ) -> Result<PlannedExecution, AppError> {
        let throttle = LeadThinkingThrottle::new(
            self.publisher.clone(),
            owner_id,
            execution_id,
            LeadThinkingPhase::Planning,
        );
        let sink = throttle.sink();
        let result = tokio::time::timeout(
            PLAN_TIMEOUT,
            self.planner.produce(goal, participants, Some(&sink)),
        )
        .await
        .map_err(|_| AppError::Timeout("Agent Execution planning timed out".to_owned()))?;
        throttle.flush();
        self.publisher.publish_lead_thinking(
            owner_id,
            execution_id,
            LeadThinkingPhase::Planning,
            LeadThinkingKind::Text,
            None,
            None,
            true,
        );
        result
    }

    async fn produce_adjustment(
        &self,
        owner_id: &str,
        execution_id: &str,
        intent: &str,
        detail: &AgentExecutionDetail,
    ) -> Result<AdjustedExecutionPlan, AppError> {
        let throttle = LeadThinkingThrottle::new(
            self.publisher.clone(),
            owner_id,
            execution_id,
            LeadThinkingPhase::Adjust,
        );
        let sink = throttle.sink();
        let result = tokio::time::timeout(
            PLAN_TIMEOUT,
            self.planner.adjust(intent, detail, Some(&sink)),
        )
        .await
        .map_err(|_| AppError::Timeout("Agent Execution adjustment timed out".to_owned()))?;
        throttle.flush();
        self.publisher.publish_lead_thinking(
            owner_id,
            execution_id,
            LeadThinkingPhase::Adjust,
            LeadThinkingKind::Text,
            None,
            None,
            true,
        );
        result
    }

    async fn fail_planning(&self, owner_id: &str, execution_id: &str, reason: &str) {
        loop {
            let Ok(current) = self.require_execution(owner_id, execution_id).await else {
                return;
            };
            if current.status != AgentExecutionStatus::Planning {
                return;
            }
            match self.repository.update_execution(
                owner_id,
                execution_id,
                current.version,
                None,
                &UpdateAgentExecutionParams {
                    status: Some(AgentExecutionStatus::Failed),
                    summary: Some(Some(reason.to_owned())),
                    ..Default::default()
                },
                &system_event(
                    AgentExecutionEventKind::StatusChanged,
                    None,
                    None,
                    terminal_transition_payload(
                        &current,
                        AgentExecutionStatus::Failed,
                        Some(reason),
                    ),
                ),
            )
            .await {
                Ok(_) => {
                    self.scheduler
                        .after_terminal_commit(owner_id, execution_id)
                        .await;
                    return;
                }
                Err(nomifun_db::DbError::Conflict(_)) => continue,
                Err(error) => {
                    tracing::warn!(%execution_id, %error, "failed to persist planning failure");
                    return;
                }
            }
        }
    }

    async fn require_execution(
        &self,
        owner_id: &str,
        execution_id: &str,
    ) -> Result<AgentExecution, AppError> {
        Ok(self.detail(owner_id, execution_id).await?.execution)
    }

    async fn detail(
        &self,
        owner_id: &str,
        execution_id: &str,
    ) -> Result<AgentExecutionDetail, AppError> {
        canonical_id::<AgentExecutionId>("execution_id", execution_id)?;
        let rows = self
            .repository
            .get_execution_detail(owner_id, execution_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Agent Execution {execution_id}")))?;
        domain_mapper::detail(rows)
    }

    async fn publish(&self) {
        self.publisher.drain(self.repository.clone()).await;
    }
}

fn planned_status(gate: PlanGate) -> AgentExecutionStatus {
    match gate {
        PlanGate::Automatic => AgentExecutionStatus::Running,
        PlanGate::RequireApproval => AgentExecutionStatus::AwaitingApproval,
    }
}

fn non_empty(field: &str, value: String) -> Result<String, AppError> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        Err(AppError::BadRequest(format!("{field} must not be empty")))
    } else {
        Ok(value)
    }
}

fn canonical_id<T: EntityId>(field: &str, value: &str) -> Result<String, AppError> {
    value
        .parse::<T>()
        .map(|id| id.as_str().to_owned())
        .map_err(|error| AppError::BadRequest(format!("invalid {field}: {error}")))
}

fn canonical_uuidv7(field: &str, value: &str) -> Result<String, AppError> {
    nomifun_common::validate_uuidv7(value)
        .map(|_| value.to_owned())
        .map_err(|error| AppError::BadRequest(format!("invalid {field}: {error}")))
}

fn canonical_provider_id(field: &str, value: String) -> Result<String, AppError> {
    ProviderId::try_from(value.as_str())
        .map(|_| value)
        .map_err(|_| AppError::BadRequest(format!("{field} must be a canonical ProviderId")))
}

fn canonical_model_name(field: &str, value: String) -> Result<String, AppError> {
    if value.is_empty() || value.trim() != value {
        Err(AppError::BadRequest(format!(
            "{field} must be trimmed and non-empty"
        )))
    } else {
        Ok(value)
    }
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn encode_json<T: Serialize + ?Sized>(value: &T, field: &str) -> Result<String, AppError> {
    serde_json::to_string(value)
        .map_err(|error| AppError::Internal(format!("encode {field}: {error}")))
}

fn decode_json<T: DeserializeOwned>(raw: &str, field: &str) -> Result<T, AppError> {
    serde_json::from_str(raw)
        .map_err(|error| AppError::Internal(format!("invalid persisted {field}: {error}")))
}

fn validate_template_snapshot(snapshot: &ResolvedPresetSnapshot) -> Result<(), AppError> {
    if snapshot.preset_id.trim().is_empty()
        || snapshot.preset_revision <= 0
        || snapshot.target != PresetTarget::ExecutionStep
    {
        return Err(AppError::BadRequest(
            "template participant preset_snapshot must be a valid execution_step snapshot"
                .to_owned(),
        ));
    }
    if let Some(model) = snapshot.resolved_model.as_ref() {
        if let Some(provider_id) = model.provider_id.as_deref()
            && ProviderId::try_from(provider_id).is_err()
        {
            return Err(AppError::BadRequest(
                "template participant preset_snapshot has a non-canonical provider_id"
                    .to_owned(),
            ));
        }
        if model.model.is_empty() || model.model.trim() != model.model {
            return Err(AppError::BadRequest(
                "template participant preset_snapshot has an invalid model".to_owned(),
            ));
        }
    }
    Ok(())
}

fn planner_goal(
    goal: &str,
    supplemental_context: Option<&serde_json::Value>,
) -> Result<String, AppError> {
    let Some(context) = supplemental_context else {
        return Ok(goal.to_owned());
    };
    Ok(format!(
        "{goal}\n\nSUPPLEMENTAL EXECUTION CONTEXT (authoring snapshot):\n{}",
        encode_json(context, "supplemental execution context")?
    ))
}

fn runtime_participants_from_template(
    rows: &[AgentExecutionTemplateParticipantRow],
) -> Result<Vec<NewAgentExecutionParticipant>, AppError> {
    let mut participants = Vec::with_capacity(rows.len());
    let mut distinct_models = HashSet::new();
    for row in rows {
        canonical_uuidv7(
            "template participant id",
            &row.template_participant_id,
        )?;
        canonical_id::<AgentExecutionTemplateId>(
            "template participant template_id",
            &row.template_id,
        )?;
        // Decode every structured field before copying it so corrupt authoring
        // data cannot become immutable runtime state. The exact serialized
        // snapshots are still copied verbatim; there is no lossy model-range
        // reconstruction.
        let snapshot = row
            .preset_snapshot
            .as_deref()
            .map(|snapshot| {
                decode_json::<ResolvedPresetSnapshot>(
                    snapshot,
                    "template participant preset snapshot",
                )
            })
            .transpose()?;
        if let Some(snapshot) = snapshot.as_ref() {
            validate_template_snapshot(&snapshot)?;
        }
        // The materialized participant row is the sole live provider binding.
        // A frozen preset may describe the historical resolution, but using it
        // as a runtime fallback would create a second provider truth and make
        // provider deletion/usage checks disagree with execution behavior.
        let (provider_id, model) = runtime_model_pair(
            row.provider_id.as_deref(),
            row.model.as_deref(),
            &row.template_participant_id,
        )?;
        distinct_models.insert((provider_id.clone(), model.clone()));
        if distinct_models.len() > MAX_AGENT_EXECUTION_MODELS {
            return Err(AppError::BadRequest(format!(
                "template resolves to more than {MAX_AGENT_EXECUTION_MODELS} distinct provider/model pairs"
            )));
        }
        if let Some(capability) = row.capability.as_deref() {
            let _: nomifun_api_types::ParticipantCapability =
                decode_json(capability, "template participant capability")?;
        }
        if let Some(constraints) = row.constraints.as_deref() {
            let constraints: nomifun_api_types::ParticipantConstraints =
                decode_json(constraints, "template participant constraints")?;
            constraints.validate().map_err(|error| {
                AppError::Internal(format!(
                    "invalid persisted template participant constraints: {error}"
                ))
            })?;
        }
        let _: Vec<String> = decode_json(&row.enabled_skills, "template participant skills")?;
        let _: Vec<String> = decode_json(
            &row.disabled_builtin_skills,
            "template participant builtin exclusions",
        )?;
        participants.push(NewAgentExecutionParticipant {
            participant_id: nomifun_common::AgentExecutionParticipantId::new().into_string(),
            source_agent_id: row.source_agent_id.clone(),
            preset_id: row.preset_id.clone(),
            preset_revision: row.preset_revision,
            preset_snapshot: row.preset_snapshot.clone(),
            provider_id: Some(provider_id),
            model: Some(model),
            role: row.role.clone(),
            capability: row.capability.clone(),
            constraints: row.constraints.clone(),
            description: row.description.clone(),
            system_prompt: row.system_prompt.clone(),
            enabled_skills: row.enabled_skills.clone(),
            disabled_builtin_skills: row.disabled_builtin_skills.clone(),
            sort_order: row.sort_order,
        });
    }
    Ok(participants)
}

fn runtime_model_pair(
    provider_id: Option<&str>,
    model: Option<&str>,
    participant_id: &str,
) -> Result<(String, String), AppError> {
    match (provider_id, model) {
        (Some(provider_id), Some(model))
            if ProviderId::try_from(provider_id).is_ok()
                && !model.is_empty()
                && model.trim() == model =>
        {
            Ok((provider_id.to_owned(), model.to_owned()))
        }
        (Some(_), None) | (None, Some(_)) => Err(AppError::BadRequest(format!(
            "template participant {participant_id} has an incomplete provider/model pair"
        ))),
        _ => Err(AppError::BadRequest(format!(
            "template participant {participant_id} has no executable provider/model pair"
        ))),
    }
}

fn map_template(row: AgentExecutionTemplateRow) -> Result<AgentExecutionTemplate, AppError> {
    canonical_id::<AgentExecutionTemplateId>(
        "template.execution_template_id",
        &row.execution_template_id,
    )?;
    Ok(AgentExecutionTemplate {
        execution_template_id: row.execution_template_id,
        name: row.name,
        description: row.description,
        max_parallel: row.max_parallel,
        work_dir: row.work_dir,
        context: row
            .context
            .as_deref()
            .map(|raw| decode_json(raw, "template context"))
            .transpose()?,
        version: row.version,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn map_template_participant(
    row: AgentExecutionTemplateParticipantRow,
) -> Result<AgentExecutionTemplateParticipant, AppError> {
    canonical_uuidv7(
        "template participant.template_participant_id",
        &row.template_participant_id,
    )?;
    canonical_id::<AgentExecutionTemplateId>(
        "template participant.template_id",
        &row.template_id,
    )?;
    let (provider_id, model) = runtime_model_pair(
        row.provider_id.as_deref(),
        row.model.as_deref(),
        &row.template_participant_id,
    )?;
    Ok(AgentExecutionTemplateParticipant {
        template_participant_id: row.template_participant_id,
        source_agent_id: row.source_agent_id,
        preset_id: row.preset_id,
        preset_revision: row.preset_revision,
        preset_snapshot: row
            .preset_snapshot
            .as_deref()
            .map(|raw| decode_json(raw, "template participant preset snapshot"))
            .transpose()?,
        provider_id: Some(provider_id),
        model: Some(model),
        role: row.role,
        capability: row
            .capability
            .as_deref()
            .map(|raw| decode_json(raw, "template participant capability"))
            .transpose()?,
        constraints: row
            .constraints
            .as_deref()
            .map(|raw| decode_json(raw, "template participant constraints"))
            .transpose()?,
        description: row.description,
        system_prompt: row.system_prompt,
        enabled_skills: decode_json(&row.enabled_skills, "template participant skills")?,
        disabled_builtin_skills: decode_json(
            &row.disabled_builtin_skills,
            "template participant builtin exclusions",
        )?,
        sort_order: row.sort_order,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn map_template_detail(
    rows: AgentExecutionTemplateDetailRows,
) -> Result<AgentExecutionTemplateDetail, AppError> {
    let template = map_template(rows.template)?;
    let participants = rows
        .participants
        .into_iter()
        .map(map_template_participant)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AgentExecutionTemplateDetail {
        template,
        participants,
    })
}

fn validate_max_parallel(value: Option<i64>) -> Result<i64, AppError> {
    let value = value.unwrap_or(DEFAULT_MAX_PARALLEL);
    if !(1..=MAX_AGENT_EXECUTION_PARALLELISM).contains(&value) {
        return Err(AppError::BadRequest(format!(
            "max_parallel must be between 1 and {MAX_AGENT_EXECUTION_PARALLELISM}"
        )));
    }
    Ok(value)
}

fn validate_final_step_count(kept: usize, introduced: usize) -> Result<(), AppError> {
    if kept.saturating_add(introduced) > MAX_AGENT_EXECUTION_STEPS {
        return Err(AppError::BadRequest(format!(
            "execution plan exceeds {MAX_AGENT_EXECUTION_STEPS} active steps"
        )));
    }
    Ok(())
}

fn require_status(current: AgentExecutionStatus, allowed: &[AgentExecutionStatus]) -> Result<(), AppError> {
    if allowed.contains(&current) {
        Ok(())
    } else {
        Err(AppError::Conflict(format!(
            "execution status {current} does not allow this command"
        )))
    }
}

fn active_participants(detail: &AgentExecutionDetail) -> Vec<ExecutionParticipant> {
    detail
        .participants
        .iter()
        .filter(|participant| participant.retired_in_revision.is_none())
        .cloned()
        .collect()
}

fn participants_for_model_pool(
    detail: &AgentExecutionDetail,
    pool: &ExecutionModelPool,
) -> Result<Vec<ExecutionParticipant>, AppError> {
    pool.validate().map_err(AppError::BadRequest)?;
    let active = active_participants(detail);
    let requested = match pool {
        ExecutionModelPool::Automatic => return Ok(active),
        ExecutionModelPool::Single { model } => vec![model.clone()],
        ExecutionModelPool::Range { models } => models.clone(),
    };
    if requested.is_empty() || requested.len() > MAX_AGENT_EXECUTION_MODELS {
        return Err(AppError::BadRequest(format!(
            "delegation model pool must contain 1-{MAX_AGENT_EXECUTION_MODELS} models"
        )));
    }
    for model in &requested {
        if !active.iter().any(|participant| {
            participant.provider_id.as_deref() == Some(model.provider_id.as_str())
                && participant.model.as_deref() == Some(model.model.as_str())
        }) {
            return Err(AppError::BadRequest(format!(
                "model {}/{} is outside the execution participant authority",
                model.provider_id, model.model
            )));
        }
    }
    let participants = active
        .into_iter()
        .filter(|participant| {
            requested.iter().any(|model| {
                participant.provider_id.as_deref() == Some(model.provider_id.as_str())
                    && participant.model.as_deref() == Some(model.model.as_str())
            })
        })
        .collect::<Vec<_>>();
    if participants.is_empty() {
        return Err(AppError::BadRequest(
            "delegation model pool resolves to no active participant".to_owned(),
        ));
    }
    Ok(participants)
}

fn active_dependencies(detail: &AgentExecutionDetail) -> Vec<NewAgentExecutionStepDependency> {
    detail
        .dependencies
        .iter()
        .filter(|dependency| dependency.superseded_in_revision.is_none())
        .map(|dependency| NewAgentExecutionStepDependency {
            blocker_step_id: dependency.blocker_step_id.clone(),
            blocked_step_id: dependency.blocked_step_id.clone(),
        })
        .collect()
}

fn current_step<'a>(
    detail: &'a AgentExecutionDetail,
    step_id: &str,
) -> Result<&'a ExecutionStep, AppError> {
    canonical_uuidv7("step_id", step_id)?;
    detail
        .steps
        .iter()
        .find(|step| step.step_id == step_id && step.superseded_in_revision.is_none())
        .ok_or_else(|| AppError::NotFound(format!("Execution step {step_id}")))
}

fn require_pending_agent_step<'a>(
    detail: &'a AgentExecutionDetail,
    step_id: &str,
) -> Result<&'a ExecutionStep, AppError> {
    let step = current_step(detail, step_id)?;
    if step.kind != ExecutionStepKind::Agent || step.status != ExecutionStepStatus::Pending {
        return Err(AppError::Conflict(
            "only a pending Agent step can be changed".to_owned(),
        ));
    }
    Ok(step)
}

#[derive(Debug, Clone)]
struct StepAssignment {
    participant_id: String,
    source: ParticipantAssignmentSource,
    score: Option<f64>,
    rationale: Option<String>,
    locked: bool,
}

fn validate_step_command_versions(
    detail: &AgentExecutionDetail,
    step: &ExecutionStep,
    expected_execution_version: i64,
    expected_step_version: i64,
) -> Result<(), AppError> {
    if detail.execution.version != expected_execution_version {
        return Err(AppError::Conflict("stale Agent Execution version".to_owned()));
    }
    if step.version != expected_step_version {
        return Err(AppError::Conflict("stale execution step version".to_owned()));
    }
    Ok(())
}

fn replacement_step_snapshot(step: &ExecutionStep) -> Result<NewAgentExecutionStep, AppError> {
    Ok(NewAgentExecutionStep {
        step_id: nomifun_common::AgentExecutionStepId::new().into_string(),
        title: step.title.clone(),
        spec: step.spec.clone(),
        role: step.role.clone(),
        tool_policy: step.tool_policy,
        kind: step.kind,
        agent_mode: step.agent_mode,
        profile: step
            .profile
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| AppError::Internal(format!("encode step profile: {error}")))?,
        fanout_group: step.fanout_group.clone(),
        control_policy: step
            .control_policy
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| AppError::Internal(format!("encode control policy: {error}")))?,
        status: ExecutionStepStatus::Pending,
        assigned_participant_id: step.assigned_participant_id.clone(),
        assignment_score: step.assignment_score,
        assignment_rationale: step.assignment_rationale.clone(),
        assignment_source: step.assignment_source,
        assignment_locked: step.assignment_locked,
        failure_policy: step.failure_policy,
        preset_prompt: step.preset_prompt.clone(),
        graph_x: step.graph_x,
        graph_y: step.graph_y,
    })
}

#[derive(Debug, Serialize)]
struct StepSnapshotReplacementAudit {
    change: &'static str,
    command: String,
    before: StepSnapshotAudit,
    after: StepSnapshotAudit,
}

#[derive(Debug, Serialize)]
struct StepSnapshotAudit {
    plan_revision: i64,
    title: String,
    spec: String,
    participant_id: Option<String>,
    assignment_source: Option<String>,
    assignment_locked: bool,
    preset_prompt: Option<String>,
}

fn snapshot_replacement_payload(
    command: &str,
    detail: &AgentExecutionDetail,
    before: &ExecutionStep,
    after: &NewAgentExecutionStep,
) -> Result<serde_json::Value, AppError> {
    serde_json::to_value(StepSnapshotReplacementAudit {
        change: "step_snapshot_replaced",
        command: command.to_owned(),
        before: StepSnapshotAudit {
            plan_revision: detail.execution.plan_revision,
            title: before.title.clone(),
            spec: before.spec.clone(),
            participant_id: before.assigned_participant_id.clone(),
            assignment_source: before.assignment_source.map(|value| value.to_string()),
            assignment_locked: before.assignment_locked,
            preset_prompt: before.preset_prompt.clone(),
        },
        after: StepSnapshotAudit {
            plan_revision: detail.execution.plan_revision + 1,
            title: after.title.clone(),
            spec: after.spec.clone(),
            participant_id: after.assigned_participant_id.clone(),
            assignment_source: after.assignment_source.map(|value| value.to_string()),
            assignment_locked: after.assignment_locked,
            preset_prompt: after.preset_prompt.clone(),
        },
    })
    .map_err(|error| AppError::Internal(format!("encode step replacement audit: {error}")))
}

fn replace_id(value: &str, old: &str, replacement: &str) -> String {
    if value == old {
        replacement.to_owned()
    } else {
        value.to_owned()
    }
}

fn automatic_assignment(
    detail: &AgentExecutionDetail,
    step: &ExecutionStep,
) -> Result<StepAssignment, AppError> {
    let active: Vec<ExecutionParticipant> = detail
        .participants
        .iter()
        .filter(|participant| participant.retired_in_revision.is_none())
        .cloned()
        .collect();
    let (participant, score, rationale) = if let Some(profile) = step.profile.as_ref() {
        if let Some(candidate) = rank_participants(&active, profile).first() {
            (
                &active[candidate.participant_index],
                Some(candidate.score),
                Some(candidate.rationale.clone()),
            )
        } else {
            return Err(AppError::BadRequest(
                "no active participant satisfies this step profile".to_owned(),
            ));
        }
    } else {
        (
            active.first().ok_or_else(|| {
                AppError::BadRequest("execution has no active participant".to_owned())
            })?,
            None,
            Some("default active participant".to_owned()),
        )
    };
    Ok(StepAssignment {
        participant_id: participant.participant_id.clone(),
        source: ParticipantAssignmentSource::Automatic,
        score,
        rationale,
        locked: false,
    })
}

fn participants_from_new(
    execution_id: &str,
    revision: i64,
    rows: &[NewAgentExecutionParticipant],
) -> Result<Vec<ExecutionParticipant>, AppError> {
    rows.iter()
        .map(|row| {
            Ok(ExecutionParticipant {
                participant_id: row.participant_id.clone(),
                execution_id: execution_id.to_owned(),
                source_agent_id: row.source_agent_id.clone(),
                preset_id: row.preset_id.clone(),
                preset_revision: row.preset_revision,
                preset_snapshot: parse_optional("preset_snapshot", row.preset_snapshot.as_deref())?,
                provider_id: row.provider_id.clone(),
                model: row.model.clone(),
                role: row.role.clone(),
                capability: parse_optional("capability", row.capability.as_deref())?,
                constraints: parse_optional("constraints", row.constraints.as_deref())?,
                description: row.description.clone(),
                system_prompt: row.system_prompt.clone(),
                enabled_skills: parse_json("enabled_skills", &row.enabled_skills)?,
                disabled_builtin_skills: parse_json(
                    "disabled_builtin_skills",
                    &row.disabled_builtin_skills,
                )?,
                sort_order: row.sort_order,
                introduced_in_revision: revision,
                retired_in_revision: None,
                created_at: now_ms(),
            })
        })
        .collect()
}

fn parse_json<T: DeserializeOwned>(field: &str, value: &str) -> Result<T, AppError> {
    serde_json::from_str(value)
        .map_err(|error| AppError::Internal(format!("invalid participant {field}: {error}")))
}

fn parse_optional<T: DeserializeOwned>(
    field: &str,
    value: Option<&str>,
) -> Result<Option<T>, AppError> {
    value.map(|value| parse_json(field, value)).transpose()
}

fn materialize_adjustment(
    adjusted: AdjustedExecutionPlan,
    detail: &AgentExecutionDetail,
) -> Result<(Vec<String>, MaterializedPlan, Vec<NewAgentExecutionStepDependency>), AppError> {
    let active_ids: HashSet<String> = detail
        .steps
        .iter()
        .filter(|step| step.superseded_in_revision.is_none())
        .map(|step| step.step_id.clone())
        .collect();
    let mut keep = Vec::new();
    let mut new_steps = Vec::new();
    let mut declared_dependencies = Vec::new();
    for node in adjusted.steps {
        match node {
            AdjustedExecutionNode::Keep { step_id } => {
                if !active_ids.contains(&step_id) || keep.contains(&step_id) {
                    return Err(AppError::BadRequest(format!(
                        "adjustment keeps an unknown or duplicate step {step_id}"
                    )));
                }
                keep.push(step_id);
            }
            AdjustedExecutionNode::New {
                mut step,
                dependencies,
            } => {
                if !step.depends_on.is_empty() {
                    return Err(AppError::BadRequest(
                        "adjusted new steps must declare dependencies in the outer dependency list"
                            .to_owned(),
                    ));
                }
                let new_index = new_steps.len();
                step.depends_on = dependencies
                    .iter()
                    .filter_map(|dependency| match dependency {
                        AdjustedDependency::New { index } => Some(*index),
                        AdjustedDependency::Existing { .. } => None,
                    })
                    .collect();
                declared_dependencies.push((new_index, dependencies));
                new_steps.push(step);
            }
        }
    }
    if keep.is_empty() && new_steps.is_empty() {
        return Err(AppError::BadRequest(
            "adjustment cannot remove every step".to_owned(),
        ));
    }
    let materialized = if new_steps.is_empty() {
        MaterializedPlan {
            steps: Vec::new(),
            dependencies: Vec::new(),
        }
    } else {
        plan_materializer::materialize(
            PlannedExecution { steps: new_steps },
            &active_participants(detail),
        )?
    };
    let keep_set: HashSet<String> = keep.iter().cloned().collect();
    let mut dependencies: Vec<NewAgentExecutionStepDependency> = active_dependencies(detail)
        .into_iter()
        .filter(|dependency| {
            keep_set.contains(&dependency.blocker_step_id)
                && keep_set.contains(&dependency.blocked_step_id)
        })
        .collect();
    dependencies.extend(materialized.dependencies.clone());
    for (new_index, declared) in declared_dependencies {
        let blocked = materialized
            .steps
            .get(new_index)
            .ok_or_else(|| AppError::Internal("adjustment index drift".to_owned()))?;
        for dependency in declared {
            if let AdjustedDependency::Existing { step_id } = dependency {
                if !keep_set.contains(&step_id) {
                    return Err(AppError::BadRequest(format!(
                        "new step depends on an existing step that was not kept: {step_id}"
                    )));
                }
                dependencies.push(NewAgentExecutionStepDependency {
                    blocker_step_id: step_id,
                    blocked_step_id: blocked.step_id.clone(),
                });
            }
        }
    }
    Ok((keep, materialized, dependencies))
}

fn actor_event(
    actor: &AgentExecutionActor,
    kind: AgentExecutionEventKind,
    step_id: Option<&str>,
    attempt_id: Option<&str>,
    payload: serde_json::Value,
) -> NewAgentExecutionEvent {
    NewAgentExecutionEvent {
        event_type: kind,
        step_id: step_id.map(str::to_owned),
        attempt_id: attempt_id.map(str::to_owned),
        actor: actor.clone(),
        payload: payload.to_string(),
    }
}

fn system_event(
    kind: AgentExecutionEventKind,
    step_id: Option<&str>,
    attempt_id: Option<&str>,
    payload: serde_json::Value,
) -> NewAgentExecutionEvent {
    NewAgentExecutionEvent {
        event_type: kind,
        step_id: step_id.map(str::to_owned),
        attempt_id: attempt_id.map(str::to_owned),
        actor: AgentExecutionActor::system(),
        payload: payload.to_string(),
    }
}

fn explicit_cancel_payload() -> serde_json::Value {
    json!({
        "status": AgentExecutionStatus::Cancelled,
        "reason": "cancelled_by_caller"
    })
}

#[cfg(test)]
mod tests {
    use super::{
        InitialPlanningCommand, attempt_delegation_operation_id, explicit_cancel_payload,
        runtime_model_pair, validate_max_parallel,
    };
    use nomifun_api_types::{ExecutionModelPool, ExecutionModelRef};
    use nomifun_common::MAX_AGENT_EXECUTION_PARALLELISM;

    const PROVIDER_A: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const PROVIDER_OVERRIDE: &str = "0190f5fe-7c00-7a00-8000-000000000002";

    #[test]
    fn initial_planning_command_round_trips_automatic_and_complete_explicit_input() {
        let automatic: InitialPlanningCommand =
            serde_json::from_str(r#"{"mode":"automatic"}"#).unwrap();
        assert_eq!(
            serde_json::to_value(automatic).unwrap(),
            serde_json::json!({"mode":"automatic"})
        );

        let raw = serde_json::json!({
            "mode": "explicit",
            "plan": {
                "steps": [{"title":"research", "spec":"collect evidence"}]
            }
        });
        let explicit: InitialPlanningCommand = serde_json::from_value(raw.clone()).unwrap();
        let InitialPlanningCommand::Explicit { plan } = &explicit else {
            panic!("explicit planning input changed mode");
        };
        assert_eq!(plan.steps.len(), 1);
        let encoded = serde_json::to_value(explicit).unwrap();
        assert_eq!(encoded["mode"], "explicit");
        assert_eq!(encoded["plan"]["steps"][0]["title"], "research");
        assert_eq!(encoded["plan"]["steps"][0]["spec"], "collect evidence");
    }

    #[test]
    fn max_parallel_rejects_values_instead_of_clamping_them() {
        assert_eq!(
            validate_max_parallel(Some(MAX_AGENT_EXECUTION_PARALLELISM)).unwrap(),
            MAX_AGENT_EXECUTION_PARALLELISM
        );
        assert!(validate_max_parallel(Some(0)).is_err());
        assert!(validate_max_parallel(Some(MAX_AGENT_EXECUTION_PARALLELISM + 1)).is_err());
    }

    #[test]
    fn explicit_cancel_does_not_enqueue_a_lead_report() {
        let payload = explicit_cancel_payload();
        assert_eq!(payload["status"], "cancelled");
        assert_eq!(payload["reason"], "cancelled_by_caller");
        assert!(payload.get("lead_report_operation_id").is_none());
    }

    #[test]
    fn attempt_delegation_identity_is_server_derived_and_semantic() {
        const CALLER_STEP_ID: &str = "0190f5fe-7c00-7a00-8000-000000000011";
        const CALLER_ATTEMPT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000012";
        let pool = ExecutionModelPool::Single {
            model: ExecutionModelRef {
                provider_id: PROVIDER_A.to_owned(),
                model: "model-a".to_owned(),
            },
        };
        let first = attempt_delegation_operation_id(
            "execution-1",
            CALLER_STEP_ID,
            CALLER_ATTEMPT_ID,
            "inspect the boundary",
            Some(&pool),
            None,
        )
        .unwrap();
        let replay = attempt_delegation_operation_id(
            "execution-1",
            CALLER_STEP_ID,
            CALLER_ATTEMPT_ID,
            "inspect the boundary",
            Some(&pool),
            None,
        )
        .unwrap();
        let different = attempt_delegation_operation_id(
            "execution-1",
            CALLER_STEP_ID,
            CALLER_ATTEMPT_ID,
            "implement the boundary",
            Some(&pool),
            None,
        )
        .unwrap();
        let inherited_pool_request = attempt_delegation_operation_id(
            "execution-1",
            CALLER_STEP_ID,
            CALLER_ATTEMPT_ID,
            "inspect the boundary",
            None,
            None,
        )
        .unwrap();

        assert_eq!(first, replay);
        assert_ne!(first, different);
        assert_ne!(first, inherited_pool_request);
        assert!(first.starts_with("delegate:"));
        assert_eq!(first.len(), "delegate:".len() + 64);
    }

    #[test]
    fn persisted_template_participant_uses_only_its_concrete_model_binding() {
        const PARTICIPANT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000021";
        assert_eq!(
            runtime_model_pair(Some(PROVIDER_A), Some("model-a"), PARTICIPANT_ID).unwrap(),
            (PROVIDER_A.to_owned(), "model-a".to_owned())
        );
        assert_eq!(
            runtime_model_pair(
                Some(PROVIDER_OVERRIDE),
                Some("model-override"),
                PARTICIPANT_ID,
            )
            .unwrap(),
            (PROVIDER_OVERRIDE.to_owned(), "model-override".to_owned())
        );
        assert!(runtime_model_pair(None, None, PARTICIPANT_ID).is_err());
        assert!(runtime_model_pair(Some(PROVIDER_A), None, PARTICIPANT_ID).is_err());
        assert!(runtime_model_pair(Some(PROVIDER_A), Some("  "), PARTICIPANT_ID).is_err());
        assert!(
            runtime_model_pair(Some(" provider"), Some("model"), PARTICIPANT_ID).is_err()
        );
        assert!(
            runtime_model_pair(Some(PROVIDER_A), Some("model "), PARTICIPANT_ID).is_err()
        );
    }
}
