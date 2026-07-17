//! The complete model-facing surface for persistent Agent collaboration.
//!
//! Three tools are enough: delegate, inspect, and update. They call the same
//! [`AgentExecutionEngine`](nomifun_agent_execution::AgentExecutionEngine) as
//! REST and never assemble persistence or scheduler state themselves.

use std::sync::Arc;

use nomifun_api_types::{
    AddExecutionStepsRequest, AdjustAgentExecutionRequest, ConfigureExecutionStepRequest,
    ConversationResponse, CreateAgentExecutionRequest, CreateExecutionFromTemplateRequest,
    ExecutionModelPool, ExecutionModelRef, ExecutionStepProfile, PlannedExecutionStep,
    ReassignExecutionStepRequest,
    RenameAgentExecutionRequest, ReplanAgentExecutionRequest, ResolvedPresetSnapshot,
    RetryExecutionStepRequest, SteerExecutionStepRequest, UpdateExecutionStepRequest,
    VersionedAgentExecutionCommand,
};
use nomifun_common::{
    AdaptationPolicy, AgentDelegationTask, AgentExecutionActor, AgentExecutionActorType,
    AgentExecutionEventKind, AgentExecutionReceipt, AgentExecutionStatus, AgentStepMode,
    AgentToolPolicy, DecisionPolicy, DelegationPolicy, ExecutionStepKind,
    ParallelDelegationRequest, PlanGate, StepFailurePolicy,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::{CallerCtx, GatewayDeps};
use crate::registry::{
    Capability, CapabilityMeta, DangerTier, Decision, Surface, default_decision,
};
use crate::server::{ok, require_user};
use crate::provider_support;

const MAX_EXPLICIT_STEPS: usize = 16;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ModelRefParam {
    provider_id: String,
    model: String,
}

impl From<ModelRefParam> for ExecutionModelRef {
    fn from(value: ModelRefParam) -> Self {
        Self {
            provider_id: value.provider_id,
            model: value.model,
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum ModelPoolParam {
    Single { model: ModelRefParam },
    Automatic,
    Range { models: Vec<ModelRefParam> },
}

impl From<ModelPoolParam> for ExecutionModelPool {
    fn from(value: ModelPoolParam) -> Self {
        match value {
            ModelPoolParam::Single { model } => Self::Single {
                model: model.into(),
            },
            ModelPoolParam::Automatic => Self::Automatic,
            ModelPoolParam::Range { models } => Self::Range {
                models: models.into_iter().map(Into::into).collect(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum DelegationPolicyParam {
    Disabled,
    Automatic,
    PreferParallel,
}

impl From<DelegationPolicyParam> for DelegationPolicy {
    fn from(value: DelegationPolicyParam) -> Self {
        match value {
            DelegationPolicyParam::Disabled => Self::Disabled,
            DelegationPolicyParam::Automatic => Self::Automatic,
            DelegationPolicyParam::PreferParallel => Self::PreferParallel,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum PlanGateParam {
    Automatic,
    RequireApproval,
}

impl From<PlanGateParam> for PlanGate {
    fn from(value: PlanGateParam) -> Self {
        match value {
            PlanGateParam::Automatic => Self::Automatic,
            PlanGateParam::RequireApproval => Self::RequireApproval,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum AdaptationPolicyParam {
    Fixed,
    Adaptive,
}

impl From<AdaptationPolicyParam> for AdaptationPolicy {
    fn from(value: AdaptationPolicyParam) -> Self {
        match value {
            AdaptationPolicyParam::Fixed => Self::Fixed,
            AdaptationPolicyParam::Adaptive => Self::Adaptive,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum DecisionPolicyParam {
    Automatic,
    AskUser,
}

impl From<DecisionPolicyParam> for DecisionPolicy {
    fn from(value: DecisionPolicyParam) -> Self {
        match value {
            DecisionPolicyParam::Automatic => Self::Automatic,
            DecisionPolicyParam::AskUser => Self::AskUser,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum PlannedDelegationStrategy {
    Planned,
}

/// Platform-only aggregate settings belong to planned execution creation;
/// `strategy=parallel` uses the exact shared DTO accepted by embedded hosts.
#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PlannedDelegationRequest {
    /// Exactly `planned`. Planned requests never include `tasks` or
    /// `synthesize`; those fields belong only to `strategy=parallel`.
    #[serde(rename = "strategy")]
    _strategy: PlannedDelegationStrategy,
    /// Complete objective for the lead Agent to decompose into a dependency DAG.
    goal: String,
    /// Optional execution directory. Omit to inherit the conversation workspace.
    #[serde(default)]
    work_dir: Option<String>,
    /// Optional model authority as a native JSON object, never a JSON string.
    #[serde(default)]
    model_pool: Option<ModelPoolParam>,
    /// Optional planning approval policy.
    #[serde(default)]
    plan_gate: Option<PlanGateParam>,
    /// Optional fixed/adaptive replanning policy.
    #[serde(default)]
    adaptation_policy: Option<AdaptationPolicyParam>,
    /// Optional integer concurrency limit.
    #[serde(default)]
    max_parallel: Option<i64>,
}

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
enum DelegateParams {
    /// Let the lead Agent create a dependency DAG from one goal.
    Planned(PlannedDelegationRequest),
    /// Execute independent Agent steps, optionally followed by one synthesis.
    Parallel(ParallelDelegationRequest),
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExecutionGetParams {
    execution_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum ExecutionUpdateParams {
    Replan {
        execution_id: String,
        expected_version: i64,
        #[serde(default)]
        goal: Option<String>,
        #[serde(default)]
        model_pool: Option<ModelPoolParam>,
        #[serde(default)]
        delegation_policy: Option<DelegationPolicyParam>,
        #[serde(default)]
        plan_gate: Option<PlanGateParam>,
        #[serde(default)]
        adaptation_policy: Option<AdaptationPolicyParam>,
        #[serde(default)]
        decision_policy: Option<DecisionPolicyParam>,
    },
    Adjust {
        execution_id: String,
        expected_version: i64,
        intent: String,
    },
    Add {
        execution_id: String,
        expected_version: i64,
        steps: Vec<AgentDelegationTask>,
        #[serde(default)]
        synthesize: bool,
    },
    Rename {
        execution_id: String,
        expected_version: i64,
        goal: String,
    },
    UpdateStep {
        execution_id: String,
        step_id: String,
        expected_execution_version: i64,
        expected_step_version: i64,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        spec: Option<String>,
    },
    Reassign {
        execution_id: String,
        step_id: String,
        expected_execution_version: i64,
        expected_step_version: i64,
        participant_id: String,
        #[serde(default)]
        locked: bool,
    },
    Configure {
        execution_id: String,
        step_id: String,
        expected_execution_version: i64,
        expected_step_version: i64,
        #[serde(default)]
        model: Option<ModelRefParam>,
        #[serde(default)]
        clear_model: bool,
        #[serde(default)]
        preset_prompt: Option<String>,
        #[serde(default)]
        clear_preset_prompt: bool,
    },
    Steer {
        execution_id: String,
        step_id: String,
        expected_execution_version: i64,
        expected_step_version: i64,
        text: String,
    },
    Retry {
        execution_id: String,
        step_id: String,
        expected_execution_version: i64,
        expected_step_version: i64,
    },
    Approve {
        execution_id: String,
        expected_version: i64,
    },
    Pause {
        execution_id: String,
        expected_version: i64,
    },
    Resume {
        execution_id: String,
        expected_version: i64,
    },
    Cancel {
        execution_id: String,
        expected_version: i64,
        /// Cancelling is the sole destructive operation in this multiplexed
        /// tool. Desktop and Remote require an explicit second call; Channel
        /// is hard-denied by the operation-aware surface gate.
        #[serde(default)]
        confirm: bool,
    },
    /// Available only inside an active execution-attempt conversation. A
    /// successful submission durably parks the attempt, requests immediate
    /// stop of this model turn, and forbids any later tool call or side effect:
    /// END the current turn immediately after this command returns.
    RequestUserDecision { question: String },
}

fn decision_waiting_projection() -> Value {
    json!({
        "status": "waiting_input",
        "message": "Question submitted and the attempt is parked. END this turn immediately; do not call another tool or continue work until the user answers."
    })
}

fn update_operation_gate(params: &ExecutionUpdateParams, surface: Surface) -> Option<Value> {
    let ExecutionUpdateParams::Cancel { confirm, .. } = params else {
        return None;
    };
    match (default_decision(surface, DangerTier::Destructive), *confirm) {
        (Decision::Deny, _) => Some(json!({
            "error": format!("'nomi_execution_update' cancel is not permitted on the {surface:?} surface")
        })),
        (Decision::Confirm, false) => Some(json!({
            "needs_confirmation": true,
            "tool": "nomi_execution_update",
            "operation": "cancel",
            "danger": "Destructive",
            "note": "Restate the exact execution to cancel, get explicit agreement, then call again with operation=cancel and confirm=true."
        })),
        (Decision::Allow, _) | (Decision::Confirm, true) => None,
    }
}

impl ExecutionUpdateParams {
    fn execution_id(&self) -> Option<&str> {
        match self {
            Self::Replan { execution_id, .. }
            | Self::Adjust { execution_id, .. }
            | Self::Add { execution_id, .. }
            | Self::Rename { execution_id, .. }
            | Self::UpdateStep { execution_id, .. }
            | Self::Reassign { execution_id, .. }
            | Self::Configure { execution_id, .. }
            | Self::Steer { execution_id, .. }
            | Self::Retry { execution_id, .. }
            | Self::Approve { execution_id, .. }
            | Self::Pause { execution_id, .. }
            | Self::Resume { execution_id, .. }
            | Self::Cancel { execution_id, .. } => Some(execution_id),
            Self::RequestUserDecision { .. } => None,
        }
    }
}

fn attempt_actor_allows_update(
    actor: &AgentExecutionActor,
    params: &ExecutionUpdateParams,
) -> bool {
    actor.attempt_id().is_none()
        || matches!(params, ExecutionUpdateParams::RequestUserDecision { .. })
}

struct CreateContext {
    conversation: Option<ConversationResponse>,
    lead_preset: Option<ResolvedPresetSnapshot>,
    lead_conversation_id: Option<String>,
    /// Present only when the calling Conversation is an active Attempt. Work
    /// is appended to this aggregate; no child execution is created.
    current_execution_id: Option<String>,
    /// Trusted top-level default selected when the Conversation was created.
    /// Attempt callers never re-read or re-apply it.
    template_id: Option<String>,
    model_pool: ExecutionModelPool,
    lead_model: Option<ExecutionModelRef>,
    delegation_policy: DelegationPolicy,
    plan_gate: PlanGate,
    adaptation_policy: AdaptationPolicy,
    decision_policy: DecisionPolicy,
    inherited_work_dir: Option<String>,
    actor: AgentExecutionActor,
}

fn caller_conversation_id(ctx: &CallerCtx) -> Result<String, String> {
    ctx.conversation_id
        .clone()
        .map(nomifun_common::ConversationId::into_string)
        .ok_or_else(|| "Agent Execution tools require a calling conversation".to_owned())
}

fn finite_pool_models(pool: &ExecutionModelPool) -> Option<Vec<ExecutionModelRef>> {
    match pool {
        ExecutionModelPool::Automatic => None,
        ExecutionModelPool::Single { model } => Some(vec![model.clone()]),
        ExecutionModelPool::Range { models } => Some(models.clone()),
    }
}

/// A model-facing delegation may narrow the caller's authority, never widen
/// it. `Automatic` means automatic selection *inside* the inherited finite
/// range when one exists; it does not recover account-wide model access.
fn narrow_model_pool(
    inherited: ExecutionModelPool,
    explicit: Option<ExecutionModelPool>,
) -> Result<ExecutionModelPool, String> {
    let Some(explicit) = explicit else {
        return Ok(inherited);
    };
    let inherited_models = finite_pool_models(&inherited);
    if matches!(explicit, ExecutionModelPool::Automatic) {
        return Ok(inherited);
    }
    explicit.validate()?;
    let explicit_models = finite_pool_models(&explicit).expect("non-automatic pool");
    for model in explicit_models {
        if inherited_models
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(&model))
        {
            return Err(format!(
                "model {}/{} is outside the calling conversation's allowed range",
                model.provider_id, model.model
            ));
        }
    }
    Ok(explicit)
}

async fn execution_model_authority(
    deps: &GatewayDeps,
    owner_id: &str,
    execution_id: &str,
) -> Result<ExecutionModelPool, nomifun_common::AppError> {
    let detail = deps
        .agent_execution_engine
        .get(owner_id, execution_id)
        .await?;
    let mut models = Vec::new();
    for participant in detail
        .participants
        .iter()
        .filter(|participant| participant.retired_in_revision.is_none())
    {
        if let (Some(provider_id), Some(model)) =
            (participant.provider_id.as_ref(), participant.model.as_ref())
        {
            let model_ref = ExecutionModelRef {
                provider_id: provider_id.clone(),
                model: model.clone(),
            };
            if !models.contains(&model_ref) {
                models.push(model_ref);
            }
        }
    }
    if models.is_empty() {
        return Err(nomifun_common::AppError::Conflict(
            "execution has no active model authority".to_owned(),
        ));
    }
    Ok(ExecutionModelPool::Range { models })
}

async fn narrow_execution_model_pool(
    deps: &GatewayDeps,
    owner_id: &str,
    execution_id: &str,
    explicit: ExecutionModelPool,
) -> Result<ExecutionModelPool, nomifun_common::AppError> {
    let inherited = execution_model_authority(deps, owner_id, execution_id).await?;
    narrow_model_pool(inherited, Some(explicit)).map_err(nomifun_common::AppError::BadRequest)
}

async fn create_context(
    deps: &GatewayDeps,
    ctx: &CallerCtx,
    explicit_pool: Option<ModelPoolParam>,
) -> Result<CreateContext, String> {
    if ctx.remote {
        return remote_create_context(deps, ctx, explicit_pool).await;
    }

    let conversation_id = caller_conversation_id(ctx)?;
    let conversation = deps
        .conversation_service
        .get(ctx.user_id.as_str(), &conversation_id)
        .await
        .map_err(|error| error.to_string())?;
    let conversation_id = conversation.id.clone();
    let lead_model = conversation.model.as_ref().map(|model| {
        ExecutionModelRef {
            provider_id: model.provider_id.clone(),
            model: model.use_model.clone().unwrap_or_else(|| model.model.clone()),
        }
    });
    let inherited_work_dir = conversation
        .extra
        .get("workspace")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let current_execution_id = deps
        .agent_execution_engine
        .execution_for_attempt_conversation(ctx.user_id.as_str(), &conversation_id)
        .await
        .map_err(|error| error.to_string())?;
    let actor = deps
        .agent_execution_engine
        .agent_caller_for_delegation(ctx.user_id.as_str(), &conversation_id)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(execution_id) = current_execution_id {
        let execution = deps
            .agent_execution_engine
            .get(ctx.user_id.as_str(), &execution_id)
            .await
            .map_err(|error| error.to_string())?;
        let inherited_model_pool = execution_model_authority(deps, ctx.user_id.as_str(), &execution_id)
            .await
            .map_err(|error| error.to_string())?;
        let model_pool = narrow_model_pool(
            inherited_model_pool,
            explicit_pool.map(ExecutionModelPool::from),
        )?;
        return Ok(CreateContext {
            lead_preset: None,
            conversation: Some(conversation),
            lead_conversation_id: None,
            current_execution_id: Some(execution_id),
            // Template defaults apply only to the top-level entry. Re-reading
            // one here would make mutable authoring data part of runtime state.
            template_id: None,
            model_pool,
            lead_model: None,
            delegation_policy: execution.execution.delegation_policy,
            plan_gate: execution.execution.plan_gate,
            adaptation_policy: execution.execution.adaptation_policy,
            decision_policy: execution.execution.decision_policy,
            inherited_work_dir: execution
                .execution
                .work_dir
                .clone()
                .or(inherited_work_dir),
            actor,
        });
    }
    if conversation.delegation_policy == DelegationPolicy::Disabled {
        return Err("delegation is disabled for this conversation".to_owned());
    }
    let inherited_model_pool = if let Some(pool) = conversation.execution_model_pool.clone() {
        pool
    } else if let Some(model) = conversation.model.as_ref() {
        ExecutionModelPool::Single {
            model: ExecutionModelRef {
                provider_id: model.provider_id.clone(),
                model: model.use_model.clone().unwrap_or_else(|| model.model.clone()),
            },
        }
    } else {
        ExecutionModelPool::Automatic
    };
    let model_pool = narrow_model_pool(inherited_model_pool, explicit_pool.map(Into::into))?;
    let template_id = conversation
        .execution_template_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let delegation_policy = conversation.delegation_policy;
    let decision_policy = conversation.decision_policy;
    Ok(CreateContext {
        lead_preset: conversation.preset_snapshot.clone(),
        conversation: Some(conversation),
        lead_conversation_id: Some(conversation_id),
        current_execution_id: None,
        template_id,
        model_pool,
        lead_model,
        delegation_policy,
        plan_gate: PlanGate::Automatic,
        adaptation_policy: AdaptationPolicy::Fixed,
        decision_policy,
        inherited_work_dir,
        actor,
    })
}

/// A Remote companion is already a stable Agent principal. Preserve its model,
/// preset and current collaboration policy as immutable execution input rather
/// than fabricating a Conversation (which would trigger an unnecessary model
/// turn when terminal reporting posts back to that Conversation).
async fn remote_create_context(
    deps: &GatewayDeps,
    ctx: &CallerCtx,
    explicit_pool: Option<ModelPoolParam>,
) -> Result<CreateContext, String> {
    let companion_id = ctx
        .companion_id
        .as_ref()
        .map(|id| id.as_str())
        .ok_or_else(|| "Remote delegation requires a companion-bound token".to_owned())?;
    let profile = deps
        .companion_service
        .get_companion(companion_id)
        .await
        .map_err(|error| error.to_string())?;
    let existing = match deps
        .companion_service
        .companion_active_thread(companion_id)
        .await
        .map_err(|error| error.to_string())?
    {
        Some(id) => deps.conversation_service.get(ctx.user_id.as_str(), &id).await.ok(),
        None => None,
    };
    let (model, _) = provider_support::resolve_nomi_model(deps, ctx, None, None)
        .await
        .map_err(|error| {
            error
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no model is available for Remote delegation")
                .to_owned()
        })?;
    let delegation_policy = existing
        .as_ref()
        .map(|conversation| conversation.delegation_policy)
        .unwrap_or(DelegationPolicy::Automatic);
    if delegation_policy == DelegationPolicy::Disabled {
        return Err("delegation is disabled for this companion".to_owned());
    }
    let lead_model = ExecutionModelRef {
        provider_id: model.provider_id.clone(),
        model: model.use_model.clone().unwrap_or_else(|| model.model.clone()),
    };
    let inherited_model_pool = existing
        .as_ref()
        .and_then(|conversation| conversation.execution_model_pool.clone())
        .unwrap_or_else(|| ExecutionModelPool::Single {
            model: lead_model.clone(),
        });
    let model_pool = narrow_model_pool(inherited_model_pool, explicit_pool.map(Into::into))?;
    let decision_policy = existing
        .as_ref()
        .map(|conversation| conversation.decision_policy)
        .unwrap_or(DecisionPolicy::Automatic);
    let inherited_work_dir = existing
        .as_ref()
        .and_then(|conversation| conversation.extra.get("workspace"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    Ok(CreateContext {
        conversation: None,
        lead_preset: profile.applied_preset,
        lead_conversation_id: None,
        current_execution_id: None,
        template_id: None,
        model_pool,
        lead_model: Some(lead_model),
        delegation_policy,
        plan_gate: PlanGate::Automatic,
        adaptation_policy: AdaptationPolicy::Fixed,
        decision_policy,
        inherited_work_dir,
        actor: AgentExecutionActor::external_agent(companion_id),
    })
}

fn explicit_plan(
    steps: Vec<AgentDelegationTask>,
    synthesize: bool,
) -> Result<Vec<PlannedExecutionStep>, String> {
    if steps.is_empty() || steps.len() > MAX_EXPLICIT_STEPS {
        return Err(format!(
            "parallel delegation requires 1-{MAX_EXPLICIT_STEPS} steps"
        ));
    }
    for (index, step) in steps.iter().enumerate() {
        step.validate()
            .map_err(|error| format!("parallel task {index}: {error}"))?;
    }
    let mut planned: Vec<PlannedExecutionStep> = steps
        .into_iter()
        .map(|step| PlannedExecutionStep {
            title: step.name,
            spec: step.prompt,
            profile: Some(ExecutionStepProfile {
                kind: "general".to_owned(),
                needs_vision: false,
                needs_long_context: false,
                needs_high_reasoning: false,
                bulk: true,
            }),
            kind: ExecutionStepKind::Agent,
            agent_mode: Some(AgentStepMode::Normal),
            depends_on: Vec::new(),
            // Explicit fan-out describes work, not a participant cardinality.
            // The router assigns every node inside the inherited model/preset
            // authority, including plans with more steps than participants.
            participant_index: None,
            assignment_rationale: Some("explicit parallel delegation".to_owned()),
            role: step.role,
            tool_policy: step.tool_policy,
            fanout_group: Some("explicit".to_owned()),
            control_policy: None,
            failure_policy: StepFailurePolicy::FailExecution,
        })
        .collect();
    if synthesize {
        planned.push(PlannedExecutionStep {
            title: "Synthesize results".to_owned(),
            spec: "Synthesize all upstream results into one coherent answer for the goal."
                .to_owned(),
            profile: None,
            kind: ExecutionStepKind::Agent,
            agent_mode: Some(AgentStepMode::Synthesis),
            depends_on: (0..planned.len()).collect(),
            participant_index: None,
            assignment_rationale: Some("synthesis".to_owned()),
            // Role is prompt/routing/display context; tool authority is the
            // separate typed policy below.
            role: Some("synthesis".to_owned()),
            tool_policy: AgentToolPolicy::ReadOnly,
            fanout_group: None,
            control_policy: None,
            failure_policy: StepFailurePolicy::FailExecution,
        });
    }
    Ok(planned)
}

fn delegate_receipt(
    execution_id: impl Into<String>,
    status: AgentExecutionStatus,
    message: impl Into<String>,
) -> AgentExecutionReceipt {
    AgentExecutionReceipt::new(execution_id, status, message)
}

async fn delegate(deps: Arc<GatewayDeps>, ctx: CallerCtx, params: DelegateParams) -> Value {
    let owner_id = match require_user(&ctx) {
        Ok(value) => value.to_owned(),
        Err(error) => return error,
    };
    let (goal, work_dir, model_pool, plan_gate, adaptation_policy, max_parallel, steps) =
        match params {
            DelegateParams::Planned(PlannedDelegationRequest {
                _strategy: _,
                goal,
                work_dir,
                model_pool,
                plan_gate,
                adaptation_policy,
                max_parallel,
            }) => (
                goal,
                work_dir,
                model_pool,
                plan_gate,
                adaptation_policy,
                max_parallel,
                None,
            ),
            DelegateParams::Parallel(request) => {
                if let Err(error) = request.validate() {
                    return json!({"error":error});
                }
                let ParallelDelegationRequest {
                    strategy: _,
                    tasks,
                    synthesize,
                } = request;
                let goal = format!(
                    "Complete the delegated work: {}",
                    tasks
                        .iter()
                        .map(|step| step.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let steps = match explicit_plan(tasks, synthesize) {
                    Ok(value) => Some(value),
                    Err(error) => return json!({"error":error}),
                };
                (goal, None, None, None, None, None, steps)
            }
    };
    let explicit_model_pool = model_pool.is_some();
    // Keep the caller's semantic request separate from the resolved effective
    // pool. The latter may change after a successful commit; replay identity
    // must not drift with mutable aggregate state.
    let requested_model_pool = model_pool.clone().map(ExecutionModelPool::from);
    let defaults = match create_context(&deps, &ctx, model_pool).await {
        Ok(value) => value,
        Err(error) => return json!({"error":error}),
    };
    let actor = defaults.actor.clone();
    let conversation = defaults.conversation.clone();
    let lead_preset = defaults.lead_preset.clone();
    if defaults.current_execution_id.is_some() {
        if work_dir.is_some()
            || plan_gate.is_some()
            || adaptation_policy.is_some()
            || max_parallel.is_some()
        {
            return json!({
                "error": "an active Attempt appends work to its current execution; work_dir, plan_gate, adaptation_policy, and max_parallel are aggregate settings and cannot be overridden"
            });
        }
        let conversation_id = match caller_conversation_id(&ctx) {
            Ok(value) => value,
            Err(error) => return json!({"error":error}),
        };
        return match deps
            .agent_execution_engine
            .delegate_from_attempt(
                &owner_id,
                &actor,
                &conversation_id,
                goal,
                defaults.model_pool,
                requested_model_pool,
                steps,
            )
            .await
        {
            Ok((detail, added_step_ids)) => ok(
                delegate_receipt(
                    detail.execution.id,
                    detail.execution.status,
                    "Delegated steps were appended to the current execution. End this Attempt turn now; the aggregate scheduler will run them when capacity is available.",
                )
                .with_step_ids(added_step_ids),
            ),
            Err(error) => json!({"error":error.to_string()}),
        };
    }
    if defaults.template_id.is_some() && explicit_model_pool {
        return json!({
            "error": "model_pool cannot override a selected execution template; edit the template participants or clear execution_template_id"
        });
    }
    let request = CreateAgentExecutionRequest {
        goal: goal.clone(),
        work_dir: work_dir.or(defaults.inherited_work_dir),
        model_pool: defaults.model_pool.clone(),
        delegation_policy: defaults.delegation_policy,
        plan_gate: plan_gate.map(Into::into).unwrap_or(defaults.plan_gate),
        adaptation_policy: adaptation_policy
            .map(Into::into)
            .unwrap_or(defaults.adaptation_policy),
        decision_policy: defaults.decision_policy,
        max_parallel,
        lead_conversation_id: defaults.lead_conversation_id,
        lead_model: defaults.lead_model.clone(),
        steps: steps.clone(),
    };
    let created = if let Some(template_id) = defaults.template_id.as_deref() {
        let template_request = CreateExecutionFromTemplateRequest {
            goal,
            work_dir: request.work_dir,
            max_parallel: request.max_parallel,
            delegation_policy: request.delegation_policy,
            plan_gate: request.plan_gate,
            adaptation_policy: request.adaptation_policy,
            decision_policy: request.decision_policy,
            lead_conversation_id: request.lead_conversation_id.clone(),
            lead_model: request.lead_model,
            steps,
        };
        match conversation.as_ref() {
            Some(conversation) => {
                deps.agent_execution_engine
                    .create_from_template_for_conversation(
                        &owner_id,
                        &actor,
                        conversation,
                        template_id,
                        template_request,
                    )
                    .await
            }
            None => Err(nomifun_common::AppError::BadRequest(
                "execution templates require a local authenticated conversation".to_owned(),
            )),
        }
    } else {
        match (conversation.as_ref(), request.lead_conversation_id.as_ref()) {
            (Some(conversation), Some(_)) => {
            deps.agent_execution_engine
                .create_from_conversation(&owner_id, &actor, conversation, request)
                .await
            }
            _ => {
                deps.agent_execution_engine
                    .create_for_agent(&owner_id, &actor, lead_preset.as_ref(), request)
                    .await
            }
        }
    };
    match created {
        Ok(execution) => ok(delegate_receipt(
            execution.id,
            execution.status,
            "Delegated work was accepted. It will continue in the collaboration panel; inspect it only when progress is needed.",
        )),
        Err(error) => json!({"error":error.to_string()}),
    }
}

/// Remote callers have no live attempt Conversation. A top-level execution's
/// immutable Created event records the companion Agent id in the same SQLite
/// transaction, so exact-aggregate authorization needs no parent lineage.
async fn authorize_remote_execution(
    deps: &GatewayDeps,
    ctx: &CallerCtx,
    execution_id: &str,
) -> Result<AgentExecutionActor, nomifun_common::AppError> {
    let companion_id = ctx
        .companion_id
        .as_ref()
        .map(|id| id.as_str())
        .ok_or_else(|| {
            nomifun_common::AppError::NotFound(format!("Agent Execution {execution_id}"))
        })?;
    deps.agent_execution_engine
        .get(ctx.user_id.as_str(), execution_id)
        .await?;
    let created = deps
        .agent_execution_engine
        .events(ctx.user_id.as_str(), execution_id, None, Some(1))
        .await
        .map_err(|_| nomifun_common::AppError::NotFound(format!("Agent Execution {execution_id}")))?
        .into_iter()
        .next();
    if !created.is_some_and(|event| {
        event.sequence == 1
            && event.event_type == AgentExecutionEventKind::Created
            && event.actor_type == AgentExecutionActorType::Agent
            && event.actor_id.as_deref() == Some(companion_id)
            && event.actor_conversation_id.is_none()
            && event.actor_attempt_id.is_none()
    }) {
        return Err(nomifun_common::AppError::NotFound(format!(
            "Agent Execution {execution_id}"
        )));
    }
    Ok(AgentExecutionActor::external_agent(companion_id))
}

async fn authorize_execution_caller(
    deps: &GatewayDeps,
    ctx: &CallerCtx,
    execution_id: &str,
) -> Result<AgentExecutionActor, nomifun_common::AppError> {
    if ctx.remote {
        authorize_remote_execution(deps, ctx, execution_id).await
    } else {
        let conversation_id = caller_conversation_id(ctx)
            .map_err(nomifun_common::AppError::BadRequest)?;
        deps.agent_execution_engine
            .authorize_agent_caller(ctx.user_id.as_str(), execution_id, &conversation_id)
            .await
    }
}

async fn execution_get(
    deps: Arc<GatewayDeps>,
    ctx: CallerCtx,
    params: ExecutionGetParams,
) -> Value {
    let owner_id = match require_user(&ctx) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if let Err(error) = authorize_execution_caller(&deps, &ctx, &params.execution_id).await {
        return json!({"error":error.to_string()});
    }
    match deps
        .agent_execution_engine
        .get(owner_id, &params.execution_id)
        .await
    {
        Ok(detail) => ok(detail),
        Err(error) => json!({"error":error.to_string()}),
    }
}

async fn execution_update(
    deps: Arc<GatewayDeps>,
    ctx: CallerCtx,
    params: ExecutionUpdateParams,
) -> Value {
    let owner_id = match require_user(&ctx) {
        Ok(value) => value.to_owned(),
        Err(error) => return error,
    };
    if let Some(gated) = update_operation_gate(&params, ctx.surface()) {
        return gated;
    }
    let actor = match params.execution_id() {
        Some(execution_id) => authorize_execution_caller(&deps, &ctx, execution_id).await,
        None if ctx.remote => Err(nomifun_common::AppError::BadRequest(
            "request_user_decision requires an active attempt conversation".to_owned(),
        )),
        None => match caller_conversation_id(&ctx) {
            Ok(conversation_id) => deps
                .agent_execution_engine
                .agent_caller_for_delegation(&owner_id, &conversation_id)
                .await,
            Err(error) => Err(nomifun_common::AppError::BadRequest(error)),
        },
    };
    let actor = match actor {
        Ok(value) => value,
        Err(error) => return json!({"error":error.to_string()}),
    };
    if !attempt_actor_allows_update(&actor, &params) {
        return json!({
            "error": "an execution Attempt may only use request_user_decision through nomi_execution_update; append work with nomi_delegate and leave aggregate lifecycle commands to the lead/user"
        });
    }
    let result: Result<Value, nomifun_common::AppError> = match params {
        ExecutionUpdateParams::Replan {
            execution_id,
            expected_version,
            goal,
            model_pool,
            delegation_policy,
            plan_gate,
            adaptation_policy,
            decision_policy,
        } => {
            let model_pool = match model_pool {
                Some(pool) => match narrow_execution_model_pool(
                    &deps,
                    &owner_id,
                    &execution_id,
                    pool.into(),
                )
                .await
                {
                    Ok(pool) => Some(pool),
                    Err(error) => return json!({"error":error.to_string()}),
                },
                None => None,
            };
            deps.agent_execution_engine
                .replan(
                    &owner_id,
                    &actor,
                    &execution_id,
                    ReplanAgentExecutionRequest {
                        goal,
                        model_pool,
                        delegation_policy: delegation_policy.map(Into::into),
                        plan_gate: plan_gate.map(Into::into),
                        adaptation_policy: adaptation_policy.map(Into::into),
                        decision_policy: decision_policy.map(Into::into),
                        expected_version,
                    },
                )
                .await
                .and_then(to_value)
        }
        ExecutionUpdateParams::Adjust {
            execution_id,
            expected_version,
            intent,
        } => deps
            .agent_execution_engine
            .adjust(
                &owner_id,
                &actor,
                &execution_id,
                AdjustAgentExecutionRequest {
                    intent,
                    expected_version,
                },
            )
            .await
            .and_then(to_value),
        ExecutionUpdateParams::Add {
            execution_id,
            expected_version,
            steps,
            synthesize,
        } => match explicit_plan(steps, synthesize) {
            Ok(steps) => deps
                .agent_execution_engine
                .add_steps(
                    &owner_id,
                    &actor,
                    &execution_id,
                    AddExecutionStepsRequest {
                        steps,
                        expected_version,
                    },
                )
                .await
                .and_then(to_value),
            Err(error) => Err(nomifun_common::AppError::BadRequest(error)),
        },
        ExecutionUpdateParams::Rename {
            execution_id,
            expected_version,
            goal,
        } => deps
            .agent_execution_engine
            .rename(
                &owner_id,
                &actor,
                &execution_id,
                RenameAgentExecutionRequest {
                    goal,
                    expected_version,
                },
            )
            .await
            .and_then(to_value),
        ExecutionUpdateParams::UpdateStep {
            execution_id,
            step_id,
            expected_execution_version,
            expected_step_version,
            title,
            spec,
        } => deps
            .agent_execution_engine
            .update_step(
                &owner_id,
                &actor,
                &execution_id,
                &step_id,
                UpdateExecutionStepRequest {
                    title,
                    spec,
                    expected_execution_version,
                    expected_step_version,
                },
            )
            .await
            .and_then(to_value),
        ExecutionUpdateParams::Reassign {
            execution_id,
            step_id,
            expected_execution_version,
            expected_step_version,
            participant_id,
            locked,
        } => deps
            .agent_execution_engine
            .reassign_step(
                &owner_id,
                &actor,
                &execution_id,
                &step_id,
                ReassignExecutionStepRequest {
                    participant_id,
                    locked,
                    expected_execution_version,
                    expected_step_version,
                },
            )
            .await
            .and_then(to_value),
        ExecutionUpdateParams::Configure {
            execution_id,
            step_id,
            expected_execution_version,
            expected_step_version,
            model,
            clear_model,
            preset_prompt,
            clear_preset_prompt,
        } => {
            let model: Option<Option<ExecutionModelRef>> = if clear_model {
                if model.is_some() {
                    return json!({"error":"model and clear_model cannot both be set"});
                }
                Some(None)
            } else {
                model.map(|value| Some(value.into()))
            };
            let model = match model {
                Some(Some(model)) => match narrow_execution_model_pool(
                    &deps,
                    &owner_id,
                    &execution_id,
                    ExecutionModelPool::Single {
                        model: model.clone(),
                    },
                )
                .await
                {
                    Ok(_) => Some(Some(model)),
                    Err(error) => return json!({"error":error.to_string()}),
                },
                other => other,
            };
            let prompt = if clear_preset_prompt {
                if preset_prompt.is_some() {
                    return json!({"error":"preset_prompt and clear_preset_prompt cannot both be set"});
                }
                Some(None)
            } else {
                preset_prompt.map(Some)
            };
            deps.agent_execution_engine
                .configure_step(
                    &owner_id,
                    &actor,
                    &execution_id,
                    &step_id,
                    ConfigureExecutionStepRequest {
                        model,
                        preset_prompt: prompt,
                        expected_execution_version,
                        expected_step_version,
                    },
                )
                .await
                .and_then(to_value)
        }
        ExecutionUpdateParams::Steer {
            execution_id,
            step_id,
            expected_execution_version,
            expected_step_version,
            text,
        } => deps
            .agent_execution_engine
            .steer_step(
                &owner_id,
                &actor,
                &execution_id,
                &step_id,
                SteerExecutionStepRequest {
                    text,
                    expected_execution_version,
                    expected_step_version,
                },
            )
            .await
            .map(|()| json!({"updated":true})),
        ExecutionUpdateParams::Retry {
            execution_id,
            step_id,
            expected_execution_version,
            expected_step_version,
        } => deps
            .agent_execution_engine
            .retry_step(
                &owner_id,
                &actor,
                &execution_id,
                &step_id,
                RetryExecutionStepRequest {
                    expected_execution_version,
                    expected_step_version,
                },
            )
            .await
            .and_then(to_value),
        ExecutionUpdateParams::Approve {
            execution_id,
            expected_version,
        } => deps
            .agent_execution_engine
            .approve(
                &owner_id,
                &actor,
                &execution_id,
                VersionedAgentExecutionCommand { expected_version },
            )
            .await
            .and_then(to_value),
        ExecutionUpdateParams::Pause {
            execution_id,
            expected_version,
        } => deps
            .agent_execution_engine
            .pause(
                &owner_id,
                &actor,
                &execution_id,
                VersionedAgentExecutionCommand { expected_version },
            )
            .await
            .and_then(to_value),
        ExecutionUpdateParams::Resume {
            execution_id,
            expected_version,
        } => deps
            .agent_execution_engine
            .resume(
                &owner_id,
                &actor,
                &execution_id,
                VersionedAgentExecutionCommand { expected_version },
            )
            .await
            .and_then(to_value),
        ExecutionUpdateParams::Cancel {
            execution_id,
            expected_version,
            confirm: _,
        } => deps
            .agent_execution_engine
            .cancel(
                &owner_id,
                &actor,
                &execution_id,
                VersionedAgentExecutionCommand { expected_version },
            )
            .await
            .and_then(to_value),
        ExecutionUpdateParams::RequestUserDecision { question } => {
            let conversation_id = match caller_conversation_id(&ctx) {
                Ok(value) => value,
                Err(_) => {
                    return json!({"error":"request_user_decision requires an active attempt conversation"});
                }
            };
            deps.agent_execution_engine
                .request_user_decision(&owner_id, &actor, &conversation_id, question)
                .await
                .map(|_| decision_waiting_projection())
        }
    };
    match result {
        Ok(value) => ok(value),
        Err(error) => json!({"error":error.to_string()}),
    }
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value, nomifun_common::AppError> {
    serde_json::to_value(value)
        .map_err(|error| nomifun_common::AppError::Internal(format!("serialize execution: {error}")))
}

pub(crate) fn register(out: &mut Vec<Capability>) {
    out.push(Capability::new::<DelegateParams, _, _>(
        CapabilityMeta::new(
            "nomi_delegate",
            "agent_execution",
            "Delegate work into one Agent Execution. Choose exactly one native-JSON shape. planned: strategy='planned' plus goal and optional work_dir/model_pool(object)/plan_gate/adaptation_policy/max_parallel(integer); never send tasks or synthesize. parallel: strategy='parallel' plus tasks(array of 1-16 objects) and optional synthesize(boolean); never send planned aggregate settings. At a top-level Conversation this creates an execution; inside an active Attempt it atomically appends Steps and returns immediately, so end the turn without polling. Returns the canonical execution receipt.",
            DangerTier::Write,
        ),
        |deps, ctx, params| delegate(deps, ctx, params),
    ));
    out.push(Capability::new::<ExecutionGetParams, _, _>(
        CapabilityMeta::new(
            "nomi_execution_get",
            "agent_execution",
            "Read one Agent Execution directly owned by or linked to the calling Agent: aggregate status, immutable participants, current and historical DAG revisions, and every attempt output/error/conversation.",
            DangerTier::Read,
        ),
        |deps, ctx, params| execution_get(deps, ctx, params),
    ));
    out.push(Capability::new::<ExecutionUpdateParams, _, _>(
        CapabilityMeta::new(
            "nomi_execution_update",
            "agent_execution",
            "Apply exactly one typed execution command to an Agent Execution directly owned by or linked to the caller, with optimistic versions. An active Attempt may only request_user_decision here; it must append work through nomi_delegate. User/top-level lead callers may replan, adjust, add, rename, update_step, reassign, configure, steer, retry, approve, pause, resume, or cancel. request_user_decision stops that turn immediately. Cancel is destructive: Desktop/Remote require confirm=true and Channel is denied.",
            DangerTier::Write,
        ),
        |deps, ctx, params| execution_update(deps, ctx, params),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;

    const ATTEMPT_ID: &str = "eattempt_0190f5fe-7c00-7a00-8000-000000000001";
    const CONVERSATION_ID: &str = "conv_0190f5fe-7c00-7a00-8000-000000000001";
    const EXECUTION_ID: &str = "exec_0190f5fe-7c00-7a00-8000-000000000001";
    const PROVIDER_ID_A: &str = "prov_0190f5fe-7c00-7a00-8000-000000000001";
    const PROVIDER_ID_B: &str = "prov_0190f5fe-7c00-7a00-8000-000000000002";
    const STEP_ID: &str = "execstep_0190f5fe-7c00-7a00-8000-000000000001";

    #[test]
    fn model_surface_is_exactly_three_execution_tools() {
        let registry = Registry::global();
        let expected = [
            "nomi_delegate",
            "nomi_execution_get",
            "nomi_execution_update",
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
        for surface in [Surface::Desktop, Surface::Remote, Surface::Channel] {
            let owner_visible = registry
                .tool_specs_for_caller(surface, Some(&["agent_execution"]), true)
                .into_iter()
                .map(|spec| spec.name)
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                owner_visible, expected,
                "the owner-visible {surface:?} execution domain must expose exactly three lifecycle tools"
            );

            let secondary_visible = registry
                .tool_specs_for_caller(surface, Some(&["agent_execution"]), false)
                .into_iter()
                .map(|spec| spec.name)
                .collect::<std::collections::BTreeSet<_>>();
            assert!(
                secondary_visible.is_empty(),
                "installation-scoped AgentExecution tools must be hidden from a secondary caller on {surface:?}: {secondary_visible:?}"
            );

            for name in &expected {
                assert!(
                    registry.tool_visible_for_caller(
                        surface,
                        Some(&["agent_execution"]),
                        true,
                        name,
                    ),
                    "owner must see {name} on {surface:?}"
                );
                assert!(
                    !registry.tool_visible_for_caller(
                        surface,
                        Some(&["agent_execution"]),
                        false,
                        name,
                    ),
                    "secondary caller must not see {name} on {surface:?}"
                );
            }
        }
    }

    #[test]
    fn explicit_parallel_plan_has_one_optional_synthesis_node() {
        let steps = explicit_plan(
            vec![
                AgentDelegationTask {
                    name: "A".to_owned(),
                    prompt: "a".to_owned(),
                    role: None,
                    tool_policy: AgentToolPolicy::Full,
                },
                AgentDelegationTask {
                    name: "B".to_owned(),
                    prompt: "b".to_owned(),
                    role: None,
                    tool_policy: AgentToolPolicy::ReadOnly,
                },
            ],
            true,
        )
        .unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[2].depends_on, vec![0, 1]);
        assert_eq!(steps[2].agent_mode, Some(AgentStepMode::Synthesis));
        assert_eq!(steps[0].tool_policy, AgentToolPolicy::Full);
        assert_eq!(steps[1].tool_policy, AgentToolPolicy::ReadOnly);
        assert_eq!(steps[2].tool_policy, AgentToolPolicy::ReadOnly);
        assert!(steps.iter().all(|step| step.participant_index.is_none()));
    }

    #[test]
    fn persistent_parallel_plan_uses_shared_blank_task_validation() {
        for task in [
            AgentDelegationTask {
                name: " ".to_owned(),
                prompt: "inspect".to_owned(),
                role: None,
                tool_policy: AgentToolPolicy::Full,
            },
            AgentDelegationTask {
                name: "A".to_owned(),
                prompt: "\n".to_owned(),
                role: None,
                tool_policy: AgentToolPolicy::Full,
            },
            AgentDelegationTask {
                name: "A".to_owned(),
                prompt: "inspect".to_owned(),
                role: Some("\t".to_owned()),
                tool_policy: AgentToolPolicy::Full,
            },
        ] {
            assert!(explicit_plan(vec![task], false).is_err());
        }
    }

    #[test]
    fn delegate_wire_cannot_supply_the_server_operation_identity() {
        assert!(
            serde_json::from_value::<DelegateParams>(json!({
                "strategy": "planned",
                "goal": "inspect",
                "operation_id": "model-chosen"
            }))
            .is_err()
        );
    }

    #[test]
    fn parallel_delegate_uses_the_shared_strict_request_without_aggregate_overrides() {
        let parsed = serde_json::from_value::<DelegateParams>(json!({
            "strategy": "parallel",
            "tasks": [{"name":"scan","prompt":"inspect"}],
            "synthesize": true
        }))
        .unwrap();
        assert!(matches!(parsed, DelegateParams::Parallel(_)));

        for forbidden in ["work_dir", "model_pool", "plan_gate", "adaptation_policy", "max_parallel"] {
            let mut request = json!({
                "strategy": "parallel",
                "tasks": [{"name":"scan","prompt":"inspect"}]
            });
            request
                .as_object_mut()
                .unwrap()
                .insert(forbidden.to_owned(), json!("model-controlled"));
            assert!(
                serde_json::from_value::<DelegateParams>(request).is_err(),
                "parallel request must reject aggregate override {forbidden}"
            );
        }
    }

    #[test]
    fn gateway_delegate_returns_the_canonical_receipt_without_deployment_marker() {
        let response = ok(
            delegate_receipt(
                EXECUTION_ID,
                AgentExecutionStatus::Running,
                "accepted",
            )
            .with_step_ids(vec![STEP_ID.to_owned()]),
        );
        let receipt = &response["result"];
        assert_eq!(receipt["execution_id"], EXECUTION_ID);
        assert_eq!(receipt["status"], "running");
        assert_eq!(receipt["message"], "accepted");
        assert_eq!(receipt["step_ids"][0], STEP_ID);
        assert!(receipt.get("mode").is_none());
        assert!(receipt.get("execution_mode").is_none());
        assert!(receipt.get("results").is_none());
    }

    #[test]
    fn persistent_role_context_accepts_custom_values_without_authority() {
        let steps = explicit_plan(
            vec![
                AgentDelegationTask {
                    name: "build".to_owned(),
                    prompt: "implement".to_owned(),
                    role: Some("builder".to_owned()),
                    tool_policy: AgentToolPolicy::Full,
                },
                AgentDelegationTask {
                    name: "domain".to_owned(),
                    prompt: "review".to_owned(),
                    role: Some("领域专家".to_owned()),
                    tool_policy: AgentToolPolicy::ReadOnly,
                },
            ],
            false,
        )
        .unwrap();
        assert_eq!(steps[0].role.as_deref(), Some("builder"));
        assert_eq!(steps[0].tool_policy, AgentToolPolicy::Full);
        assert_eq!(steps[1].role.as_deref(), Some("领域专家"));
        assert_eq!(steps[1].tool_policy, AgentToolPolicy::ReadOnly);
    }

    #[test]
    fn explicit_model_pool_can_only_narrow_inherited_authority() {
        let allowed = ExecutionModelRef {
            provider_id: PROVIDER_ID_A.to_owned(),
            model: "model-a".to_owned(),
        };
        let denied = ExecutionModelRef {
            provider_id: PROVIDER_ID_B.to_owned(),
            model: "model-b".to_owned(),
        };
        let inherited = ExecutionModelPool::Range {
            models: vec![allowed.clone()],
        };

        assert!(matches!(
            narrow_model_pool(inherited.clone(), Some(ExecutionModelPool::Automatic)).unwrap(),
            ExecutionModelPool::Range { .. }
        ));
        assert!(
            narrow_model_pool(
                inherited.clone(),
                Some(ExecutionModelPool::Single {
                    model: allowed.clone(),
                }),
            )
            .is_ok()
        );
        assert!(
            narrow_model_pool(
                inherited,
                Some(ExecutionModelPool::Single { model: denied }),
            )
            .is_err()
        );
    }

    #[test]
    fn attempt_actor_cannot_bypass_delegate_with_generic_graph_commands() {
        let actor = AgentExecutionActor::agent(CONVERSATION_ID, Some(ATTEMPT_ID.to_owned()));
        let add = ExecutionUpdateParams::Add {
            execution_id: EXECUTION_ID.to_owned(),
            expected_version: 1,
            steps: vec![AgentDelegationTask {
                name: "bypass".to_owned(),
                prompt: "must be rejected".to_owned(),
                role: None,
                tool_policy: AgentToolPolicy::Full,
            }],
            synthesize: false,
        };
        assert!(!attempt_actor_allows_update(&actor, &add));
        assert!(attempt_actor_allows_update(
            &actor,
            &ExecutionUpdateParams::RequestUserDecision {
                question: "choose".to_owned(),
            },
        ));
        assert!(attempt_actor_allows_update(
            &AgentExecutionActor::agent(CONVERSATION_ID, None),
            &add,
        ));
    }

    #[test]
    fn cancel_dispatch_gate_uses_the_destructive_surface_matrix() {
        let cancel = |confirm| ExecutionUpdateParams::Cancel {
            execution_id: EXECUTION_ID.to_owned(),
            expected_version: 3,
            confirm,
        };
        assert_eq!(
            update_operation_gate(&cancel(false), Surface::Desktop)
                .and_then(|value| value.get("needs_confirmation").cloned()),
            Some(json!(true))
        );
        assert!(update_operation_gate(&cancel(true), Surface::Desktop).is_none());
        assert_eq!(
            update_operation_gate(&cancel(false), Surface::Remote)
                .and_then(|value| value.get("needs_confirmation").cloned()),
            Some(json!(true))
        );
        assert!(update_operation_gate(&cancel(true), Surface::Remote).is_none());
        assert!(
            update_operation_gate(&cancel(true), Surface::Channel)
                .is_some_and(|value| value.get("error").is_some())
        );
    }

    #[test]
    fn decision_contract_parks_and_ends_the_model_turn() {
        let result = decision_waiting_projection();
        assert_eq!(result["status"], "waiting_input");
        assert!(result["message"].as_str().unwrap().contains("END this turn immediately"));

        let schema = serde_json::to_string(&schemars::schema_for!(ExecutionUpdateParams)).unwrap();
        assert!(schema.contains("END the current turn immediately"));
    }
}
