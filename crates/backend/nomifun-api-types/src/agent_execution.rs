//! Wire contracts for persistent Agent Execution.
//!
//! Product callers deal with an Agent delegating work; planning, routing and
//! scheduling remain internal strategies. The DTOs deliberately contain no
//! parallel alias entities; every lifecycle fact belongs to AgentExecution.

use nomifun_common::{
    AdaptationPolicy, AgentExecutionActorType, AgentExecutionEventKind, AgentExecutionStatus,
    AgentStepMode, AgentToolPolicy, DecisionPolicy, DelegationPolicy, ExecutionAttemptStatus,
    ExecutionStepKind, ExecutionStepStatus, MAX_AGENT_EXECUTION_MODELS,
    MAX_AGENT_EXECUTION_PARALLELISM, ParticipantAssignmentSource, PlanGate, ProviderId,
    StepFailurePolicy,
};
use serde::{Deserialize, Serialize};

use crate::webhook::double_option;

pub(crate) fn deserialize_uuidv7_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
    nomifun_common::validate_uuidv7(&value)
        .map(|_| value)
        .map_err(serde::de::Error::custom)
}

pub(crate) fn deserialize_optional_uuidv7_id<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    value
        .map(|value| {
            nomifun_common::validate_uuidv7(&value)
                .map(|_| value)
                .map_err(serde::de::Error::custom)
        })
        .transpose()
}

/// A provider/model pair that may execute a participant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionModelRef {
    #[serde(deserialize_with = "crate::serde_util::deserialize_provider_id")]
    pub provider_id: String,
    #[serde(deserialize_with = "crate::serde_util::deserialize_model_name")]
    pub model: String,
}

/// Model selection input resolved into immutable participants before an
/// execution is persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionModelPool {
    Single { model: ExecutionModelRef },
    Automatic,
    Range { models: Vec<ExecutionModelRef> },
}

impl ExecutionModelPool {
    /// Validate the one model-pool wire contract shared by Conversation and
    /// AgentExecution. Persistence and service boundaries call this before a
    /// value can become durable; downstream code must not invent a second
    /// normalization vocabulary.
    pub fn validate(&self) -> Result<(), String> {
        let models: &[ExecutionModelRef] = match self {
            Self::Automatic => return Ok(()),
            Self::Single { model } => std::slice::from_ref(model),
            Self::Range { models } => {
                if models.is_empty() {
                    return Err("execution model range requires at least one model".to_owned());
                }
                if models.len() > MAX_AGENT_EXECUTION_MODELS {
                    return Err(format!(
                        "execution model range exceeds {MAX_AGENT_EXECUTION_MODELS} models"
                    ));
                }
                models
            }
        };

        let mut seen = std::collections::HashSet::with_capacity(models.len());
        for model in models {
            if ProviderId::try_from(model.provider_id.as_str()).is_err()
                || model.model.trim().is_empty()
                || model.model.trim() != model.model
            {
                return Err(
                    "execution model references require a canonical provider_id and trimmed model"
                        .to_owned(),
                );
            }
            if !seen.insert((&model.provider_id, &model.model)) {
                return Err("execution model pool contains a duplicate model".to_owned());
            }
        }
        Ok(())
    }

    /// Whether a concrete lead belongs to this authority. Automatic selection
    /// is account-catalog based and therefore accepts the configured lead;
    /// finite pools require an exact provider/model pair.
    pub fn contains(&self, candidate: &ExecutionModelRef) -> bool {
        match self {
            Self::Automatic => true,
            Self::Single { model } => model == candidate,
            Self::Range { models } => models.contains(candidate),
        }
    }
}

/// Declarative routing hints for one execution participant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticipantCapability {
    #[serde(default)]
    pub strengths: Vec<String>,
    #[serde(default)]
    pub modalities: Vec<String>,
    #[serde(default)]
    pub tools: bool,
    pub reasoning: String,
    pub cost_tier: String,
    pub speed_tier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticipantConstraints {
    pub max_concurrency: Option<i64>,
    /// Hard routing allow-list matched against `ExecutionStepProfile.kind`.
    /// This is intentionally profile vocabulary (for example `research` or
    /// `coding`), not the execution graph's Agent/Verify/Judge/Loop node kind.
    pub allowed_profile_kinds: Option<Vec<String>>,
}

impl ParticipantConstraints {
    /// Validate the one participant-concurrency boundary shared by templates
    /// and immutable execution snapshots. A participant cannot be allowed to
    /// exceed the aggregate's own scheduler parallelism ceiling.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_concurrency.is_some_and(|value| {
            !(1..=MAX_AGENT_EXECUTION_PARALLELISM).contains(&value)
        }) {
            return Err(format!(
                "participant max_concurrency must be between 1 and {}",
                MAX_AGENT_EXECUTION_PARALLELISM
            ));
        }
        Ok(())
    }
}

/// Immutable, execution-scoped snapshot of an Agent configuration. It is not a
/// reusable team member and exposes no update endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionParticipant {
    #[serde(deserialize_with = "deserialize_uuidv7_id")]
    pub participant_id: String,
    #[serde(deserialize_with = "crate::serde_util::deserialize_execution_id")]
    pub execution_id: String,
    #[serde(deserialize_with = "crate::serde_util::deserialize_agent_id")]
    pub source_agent_id: String,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_preset_id"
    )]
    pub preset_id: Option<String>,
    pub preset_revision: Option<i64>,
    pub preset_snapshot: Option<crate::ResolvedPresetSnapshot>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_provider_id"
    )]
    pub provider_id: Option<String>,
    #[serde(default, deserialize_with = "crate::serde_util::deserialize_optional_model_name")]
    pub model: Option<String>,
    pub role: Option<String>,
    pub capability: Option<ParticipantCapability>,
    pub constraints: Option<ParticipantConstraints>,
    pub description: Option<String>,
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    #[serde(default)]
    pub disabled_builtin_skills: Vec<String>,
    pub sort_order: i64,
    pub introduced_in_revision: i64,
    pub retired_in_revision: Option<i64>,
    pub created_at: i64,
}

impl<'de> Deserialize<'de> for ExecutionParticipant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(deserialize_with = "deserialize_uuidv7_id")]
            participant_id: String,
            #[serde(deserialize_with = "crate::serde_util::deserialize_execution_id")]
            execution_id: String,
            #[serde(deserialize_with = "crate::serde_util::deserialize_agent_id")]
            source_agent_id: String,
            #[serde(
                default,
                deserialize_with = "crate::serde_util::deserialize_optional_preset_id"
            )]
            preset_id: Option<String>,
            preset_revision: Option<i64>,
            preset_snapshot: Option<crate::ResolvedPresetSnapshot>,
            #[serde(
                default,
                deserialize_with = "crate::serde_util::deserialize_optional_provider_id"
            )]
            provider_id: Option<String>,
            #[serde(
                default,
                deserialize_with = "crate::serde_util::deserialize_optional_model_name"
            )]
            model: Option<String>,
            role: Option<String>,
            capability: Option<ParticipantCapability>,
            constraints: Option<ParticipantConstraints>,
            description: Option<String>,
            system_prompt: Option<String>,
            #[serde(default)]
            enabled_skills: Vec<String>,
            #[serde(default)]
            disabled_builtin_skills: Vec<String>,
            sort_order: i64,
            introduced_in_revision: i64,
            retired_in_revision: Option<i64>,
            created_at: i64,
        }

        let wire = Wire::deserialize(deserializer)?;
        crate::serde_util::validate_optional_provider_model_pair(
            wire.provider_id.as_deref(),
            wire.model.as_deref(),
        )
        .map_err(serde::de::Error::custom)?;
        Ok(Self {
            participant_id: wire.participant_id,
            execution_id: wire.execution_id,
            source_agent_id: wire.source_agent_id,
            preset_id: wire.preset_id,
            preset_revision: wire.preset_revision,
            preset_snapshot: wire.preset_snapshot,
            provider_id: wire.provider_id,
            model: wire.model,
            role: wire.role,
            capability: wire.capability,
            constraints: wire.constraints,
            description: wire.description,
            system_prompt: wire.system_prompt,
            enabled_skills: wire.enabled_skills,
            disabled_builtin_skills: wire.disabled_builtin_skills,
            sort_order: wire.sort_order,
            introduced_in_revision: wire.introduced_in_revision,
            retired_in_revision: wire.retired_in_revision,
            created_at: wire.created_at,
        })
    }
}

/// Persistent execution aggregate shown in the conversation's collaboration
/// panel. `version` is the optimistic-concurrency token for every mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExecution {
    #[serde(deserialize_with = "crate::serde_util::deserialize_execution_id")]
    pub execution_id: String,
    pub goal: String,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_conversation_id"
    )]
    pub lead_conversation_id: Option<String>,
    pub work_dir: Option<String>,
    pub delegation_policy: DelegationPolicy,
    pub plan_gate: PlanGate,
    pub adaptation_policy: AdaptationPolicy,
    pub decision_policy: DecisionPolicy,
    pub max_parallel: i64,
    pub status: AgentExecutionStatus,
    pub summary: Option<String>,
    pub total_tokens: Option<i64>,
    pub version: i64,
    pub plan_revision: i64,
    /// Latest committed outbox sequence for refetch deduplication.
    pub event_sequence: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum VerificationPolicy {
    Majority,
    Unanimous,
    AtLeast { count: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeAggregation {
    Mean,
    Borda,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LoopStopPolicy {
    MaxIterations,
    Predicate { done_marker: String },
    Stable { quiet_rounds: usize },
    Approved,
}

/// Durable policy for a non-Agent control step. Loop iteration output belongs
/// to an attempt's runtime state, never in this policy object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StepControlPolicy {
    Verify { vote: VerificationPolicy },
    Judge {
        aggregation: JudgeAggregation,
        candidate_count: Option<usize>,
    },
    Loop {
        max_iterations: usize,
        stop: LoopStopPolicy,
    },
}

/// A dependency-DAG node. Current participant routing is part of the step; it
/// is not represented by a separate Assignment object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionStep {
    #[serde(deserialize_with = "deserialize_uuidv7_id")]
    pub step_id: String,
    #[serde(deserialize_with = "crate::serde_util::deserialize_execution_id")]
    pub execution_id: String,
    pub title: String,
    pub spec: String,
    pub profile: Option<ExecutionStepProfile>,
    pub kind: ExecutionStepKind,
    pub agent_mode: Option<AgentStepMode>,
    pub status: ExecutionStepStatus,
    /// Explicit runtime tool narrowing. `role` remains a free-form description
    /// and is never interpreted as an authorization value.
    pub tool_policy: AgentToolPolicy,
    pub role: Option<String>,
    pub fanout_group: Option<String>,
    pub control_policy: Option<StepControlPolicy>,
    pub failure_policy: StepFailurePolicy,
    #[serde(default, deserialize_with = "deserialize_optional_uuidv7_id")]
    pub assigned_participant_id: Option<String>,
    pub assignment_source: Option<ParticipantAssignmentSource>,
    pub assignment_score: Option<f64>,
    pub assignment_rationale: Option<String>,
    pub assignment_locked: bool,
    pub preset_prompt: Option<String>,
    pub graph_x: Option<f64>,
    pub graph_y: Option<f64>,
    /// Scheduler gate for an automatic retry. A manual retry clears this
    /// operational field without rewriting the immutable prior attempt.
    pub dispatch_after: Option<i64>,
    pub introduced_in_revision: i64,
    pub superseded_in_revision: Option<i64>,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStepProfile {
    pub kind: String,
    pub needs_vision: bool,
    pub needs_long_context: bool,
    pub needs_high_reasoning: bool,
    pub bulk: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionStepDependency {
    #[serde(deserialize_with = "crate::serde_util::deserialize_execution_id")]
    pub execution_id: String,
    #[serde(deserialize_with = "deserialize_uuidv7_id")]
    pub blocker_step_id: String,
    #[serde(deserialize_with = "deserialize_uuidv7_id")]
    pub blocked_step_id: String,
    pub introduced_in_revision: i64,
    pub superseded_in_revision: Option<i64>,
}

/// One concrete Agent invocation. Retries append attempts; they never overwrite
/// the prior transcript, output, error, token count, or actual participant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionAttempt {
    #[serde(deserialize_with = "deserialize_uuidv7_id")]
    pub attempt_id: String,
    #[serde(deserialize_with = "crate::serde_util::deserialize_execution_id")]
    pub execution_id: String,
    #[serde(deserialize_with = "deserialize_uuidv7_id")]
    pub step_id: String,
    pub attempt_no: i64,
    #[serde(default, deserialize_with = "deserialize_optional_uuidv7_id")]
    pub participant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_conversation_id"
    )]
    pub conversation_id: Option<String>,
    pub status: ExecutionAttemptStatus,
    pub trigger_reason: String,
    pub effective_config: serde_json::Value,
    pub question: Option<String>,
    pub error: Option<String>,
    pub output_summary: Option<String>,
    #[serde(default)]
    pub output_files: Vec<String>,
    pub tokens: Option<i64>,
    pub retry_after: Option<i64>,
    pub runtime_state: Option<serde_json::Value>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExecutionDetail {
    pub execution: AgentExecution,
    pub participants: Vec<ExecutionParticipant>,
    pub steps: Vec<ExecutionStep>,
    pub dependencies: Vec<ExecutionStepDependency>,
    pub attempts: Vec<ExecutionAttempt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExecutionEvent {
    #[serde(deserialize_with = "crate::serde_util::deserialize_execution_id")]
    pub execution_id: String,
    pub sequence: i64,
    pub event_type: AgentExecutionEventKind,
    #[serde(default, deserialize_with = "deserialize_optional_uuidv7_id")]
    pub step_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_uuidv7_id")]
    pub attempt_id: Option<String>,
    pub actor_type: AgentExecutionActorType,
    pub actor_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_conversation_id"
    )]
    pub actor_conversation_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_uuidv7_id")]
    pub actor_attempt_id: Option<String>,
    #[serde(deserialize_with = "crate::serde_util::deserialize_user_id")]
    pub on_behalf_of_user_id: String,
    pub payload: serde_json::Value,
    pub created_at: i64,
}

/// Owner-scoped realtime hint emitted only after the corresponding durable
/// outbox event commits. Clients order/refetch by `(execution_id, sequence)`;
/// `change_kind` is the same canonical event vocabulary returned by REST.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExecutionChangedEvent {
    #[serde(deserialize_with = "crate::serde_util::deserialize_execution_id")]
    pub execution_id: String,
    pub sequence: i64,
    pub change_kind: AgentExecutionEventKind,
}

/// Planner output before identifiers are allocated. Dependencies and participant
/// preferences use zero-based indices and are resolved atomically on persist.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedExecutionStep {
    pub title: String,
    pub spec: String,
    #[serde(default)]
    pub profile: Option<ExecutionStepProfile>,
    #[serde(default = "default_agent_step_kind")]
    pub kind: ExecutionStepKind,
    #[serde(default)]
    pub agent_mode: Option<AgentStepMode>,
    #[serde(default)]
    pub depends_on: Vec<usize>,
    #[serde(default)]
    pub participant_index: Option<usize>,
    #[serde(default)]
    pub assignment_rationale: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    /// Explicit tool authority for this Step. Omission preserves the caller's
    /// inherited tool boundary; it never grants capabilities the caller lacks.
    #[serde(default)]
    pub tool_policy: AgentToolPolicy,
    #[serde(default)]
    pub fanout_group: Option<String>,
    #[serde(default)]
    pub control_policy: Option<StepControlPolicy>,
    #[serde(default = "default_failure_policy")]
    pub failure_policy: StepFailurePolicy,
}

fn default_agent_step_kind() -> ExecutionStepKind {
    ExecutionStepKind::Agent
}

fn default_failure_policy() -> StepFailurePolicy {
    StepFailurePolicy::FailExecution
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedExecution {
    pub steps: Vec<PlannedExecutionStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAgentExecutionRequest {
    pub goal: String,
    #[serde(default)]
    pub work_dir: Option<String>,
    pub model_pool: ExecutionModelPool,
    #[serde(default = "default_delegation_policy")]
    pub delegation_policy: DelegationPolicy,
    #[serde(default = "default_plan_gate")]
    pub plan_gate: PlanGate,
    #[serde(default = "default_adaptation_policy")]
    pub adaptation_policy: AdaptationPolicy,
    #[serde(default = "default_decision_policy")]
    pub decision_policy: DecisionPolicy,
    #[serde(default)]
    pub max_parallel: Option<i64>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_conversation_id"
    )]
    pub lead_conversation_id: Option<String>,
    /// Preferred lead model for top-level creation. Attempt-local delegation
    /// appends Steps to the existing aggregate and does not create another
    /// execution or submit this field again.
    #[serde(default)]
    pub lead_model: Option<ExecutionModelRef>,
    /// Omit to let the planner decompose `goal`; provide a non-empty list for
    /// explicit fan-out/DAG delegation through the same execution path.
    #[serde(default)]
    pub steps: Option<Vec<PlannedExecutionStep>>,
}

fn default_delegation_policy() -> DelegationPolicy {
    DelegationPolicy::Automatic
}

fn default_plan_gate() -> PlanGate {
    PlanGate::Automatic
}

fn default_adaptation_policy() -> AdaptationPolicy {
    AdaptationPolicy::Fixed
}

fn default_decision_policy() -> DecisionPolicy {
    DecisionPolicy::Automatic
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReassignExecutionStepRequest {
    #[serde(deserialize_with = "deserialize_uuidv7_id")]
    pub participant_id: String,
    #[serde(default)]
    pub locked: bool,
    pub expected_execution_version: i64,
    pub expected_step_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureExecutionStepRequest {
    /// Absent keeps the assignment, an object selects a model, and explicit
    /// null unlocks the step and restores automatic participant routing.
    #[serde(default, deserialize_with = "double_option")]
    pub model: Option<Option<ExecutionModelRef>>,
    /// Absent keeps the current prompt, explicit null clears it.
    #[serde(default, deserialize_with = "double_option")]
    pub preset_prompt: Option<Option<String>>,
    pub expected_execution_version: i64,
    pub expected_step_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdjustAgentExecutionRequest {
    pub intent: String,
    pub expected_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameAgentExecutionRequest {
    pub goal: String,
    pub expected_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteerExecutionStepRequest {
    pub text: String,
    pub expected_execution_version: i64,
    pub expected_step_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionedAgentExecutionCommand {
    pub expected_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplanAgentExecutionRequest {
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub model_pool: Option<ExecutionModelPool>,
    #[serde(default)]
    pub delegation_policy: Option<DelegationPolicy>,
    #[serde(default)]
    pub plan_gate: Option<PlanGate>,
    #[serde(default)]
    pub adaptation_policy: Option<AdaptationPolicy>,
    #[serde(default)]
    pub decision_policy: Option<DecisionPolicy>,
    pub expected_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddExecutionStepsRequest {
    pub steps: Vec<PlannedExecutionStep>,
    pub expected_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateExecutionStepRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub spec: Option<String>,
    pub expected_execution_version: i64,
    pub expected_step_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryExecutionStepRequest {
    pub expected_execution_version: i64,
    pub expected_step_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptExecutionStepOutputRequest {
    pub expected_execution_version: i64,
    pub expected_step_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerExecutionDecisionRequest {
    pub answer: String,
    pub expected_execution_version: i64,
    pub expected_step_version: i64,
    pub expected_attempt_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentExecutionEventsQuery {
    #[serde(default)]
    pub after_sequence: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const PARTICIPANT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const EXECUTION_ID: &str = "0190f5fe-7c00-7a00-8000-000000000002";

    #[test]
    fn execution_request_ids_are_strict_at_deserialization() {
        let valid = serde_json::json!({
            "goal": "ship",
            "model_pool": {
                "mode": "single",
                "model": { "provider_id": PROVIDER_ID, "model": "model-a" }
            },
            "lead_conversation_id": CONVERSATION_ID
        });
        assert!(serde_json::from_value::<CreateAgentExecutionRequest>(valid).is_ok());

        for invalid in ["1", "conversation-1", "0190f5fe-7c00-4a00-8000-000000000001"] {
            let value = serde_json::json!({
                "goal": "ship",
                "model_pool": { "mode": "automatic" },
                "lead_conversation_id": invalid
            });
            assert!(serde_json::from_value::<CreateAgentExecutionRequest>(value).is_err());
        }

        assert!(
            serde_json::from_value::<ReassignExecutionStepRequest>(serde_json::json!({
                "participant_id": PARTICIPANT_ID,
                "expected_execution_version": 1,
                "expected_step_version": 2
            }))
            .is_ok()
        );
        assert!(
            serde_json::from_value::<ReassignExecutionStepRequest>(serde_json::json!({
                "participant_id": "participant-1",
                "expected_execution_version": 1,
                "expected_step_version": 2
            }))
            .is_err()
        );
    }

    #[test]
    fn execution_model_ref_rejects_noncanonical_provider_id_values() {
        for provider_id in [
            serde_json::json!(42),
            serde_json::json!("openai"),
            serde_json::json!("550e8400-e29b-41d4-a716-446655440000"),
            serde_json::json!("0190F5FE-7C00-7A00-8000-000000000001"),
            serde_json::json!("provider_0190f5fe-7c00-7a00-8000-000000000001"),
        ] {
            assert!(
                serde_json::from_value::<ExecutionModelRef>(serde_json::json!({
                    "provider_id": provider_id,
                    "model": "model-a"
                }))
                .is_err()
            );
        }
    }

    #[test]
    fn agent_execution_rejects_noncanonical_and_removed_generic_id() {
        let valid = serde_json::json!({
            "execution_id": EXECUTION_ID,
            "goal": "ship",
            "lead_conversation_id": CONVERSATION_ID,
            "work_dir": null,
            "delegation_policy": "automatic",
            "plan_gate": "automatic",
            "adaptation_policy": "fixed",
            "decision_policy": "automatic",
            "max_parallel": 1,
            "status": "planning",
            "summary": null,
            "total_tokens": null,
            "version": 1,
            "plan_revision": 1,
            "event_sequence": 0,
            "created_at": 1,
            "updated_at": 2
        });
        assert!(serde_json::from_value::<AgentExecution>(valid.clone()).is_ok());

        for execution_id in [
            serde_json::json!(42),
            serde_json::json!("execution-1"),
            serde_json::json!("550e8400-e29b-41d4-a716-446655440000"),
            serde_json::json!("0190F5FE-7C00-7A00-8000-000000000002"),
            serde_json::json!("execution_0190f5fe-7c00-7a00-8000-000000000002"),
        ] {
            let mut raw = valid.clone();
            raw["execution_id"] = execution_id;
            assert!(serde_json::from_value::<AgentExecution>(raw).is_err());
        }

        let mut legacy = valid;
        legacy["id"] = legacy["execution_id"].take();
        legacy.as_object_mut().unwrap().remove("execution_id");
        assert!(serde_json::from_value::<AgentExecution>(legacy).is_err());
    }

    #[test]
    fn execution_participant_rejects_noncanonical_ids_by_field() {
        let valid = serde_json::json!({
            "participant_id": PARTICIPANT_ID,
            "execution_id": EXECUTION_ID,
            "source_agent_id": "0190f5fe-7c00-7a00-8000-000000000003",
            "preset_id": "0190f5fe-7c00-7a00-8000-000000000004",
            "preset_revision": 1,
            "preset_snapshot": null,
            "provider_id": PROVIDER_ID,
            "model": "model-a",
            "role": null,
            "capability": null,
            "constraints": null,
            "description": null,
            "system_prompt": null,
            "enabled_skills": [],
            "disabled_builtin_skills": [],
            "sort_order": 0,
            "introduced_in_revision": 1,
            "retired_in_revision": null,
            "created_at": 1
        });
        assert!(serde_json::from_value::<ExecutionParticipant>(valid.clone()).is_ok());

        for field in [
            "participant_id",
            "execution_id",
            "source_agent_id",
            "preset_id",
            "provider_id",
        ] {
            let mut raw = valid.clone();
            raw[field] = serde_json::json!("550e8400-e29b-41d4-a716-446655440000");
            assert!(serde_json::from_value::<ExecutionParticipant>(raw).is_err());
        }
    }

    #[test]
    fn execution_participant_rejects_non_uuid_source_agent_id() {
        let participant = serde_json::json!({
            "participant_id": PARTICIPANT_ID,
            "execution_id": EXECUTION_ID,
            "source_agent_id": "nomi",
            "preset_id": null,
            "preset_revision": null,
            "preset_snapshot": null,
            "provider_id": null,
            "model": null,
            "role": null,
            "capability": null,
            "constraints": null,
            "description": null,
            "system_prompt": null,
            "enabled_skills": [],
            "disabled_builtin_skills": [],
            "sort_order": 0,
            "introduced_in_revision": 0,
            "retired_in_revision": null,
            "created_at": 1
        });
        assert!(serde_json::from_value::<ExecutionParticipant>(participant).is_err());
    }

    #[test]
    fn planned_step_rejects_the_removed_pattern_config_bag() {
        let value = serde_json::json!({
            "title": "verify",
            "spec": "",
            "kind": "verify",
            "pattern_config": "{\"vote\":\"majority\"}"
        });
        assert!(serde_json::from_value::<PlannedExecutionStep>(value).is_err());
    }

    #[test]
    fn role_is_free_form_but_tool_policy_is_strictly_typed() {
        let custom_role = serde_json::json!({
            "title": "implement",
            "spec": "change the code",
            "role": "后端架构师",
            "tool_policy": "full"
        });
        let step: PlannedExecutionStep = serde_json::from_value(custom_role).unwrap();
        assert_eq!(step.role.as_deref(), Some("后端架构师"));
        assert_eq!(step.tool_policy, AgentToolPolicy::Full);

        let unknown_policy = serde_json::json!({
            "title": "implement",
            "spec": "change the code",
            "role": "builder",
            "tool_policy": "admin"
        });
        assert!(serde_json::from_value::<PlannedExecutionStep>(unknown_policy).is_err());
    }

    #[test]
    fn control_policy_is_strictly_tagged() {
        let value = serde_json::json!({
            "kind": "loop",
            "max_iterations": 4,
            "stop": { "kind": "approved" }
        });
        assert!(matches!(
            serde_json::from_value::<StepControlPolicy>(value).unwrap(),
            StepControlPolicy::Loop { max_iterations: 4, .. }
        ));
    }

    #[test]
    fn shared_model_pool_validation_rejects_ambiguous_inputs() {
        assert!(ExecutionModelPool::Automatic.validate().is_ok());
        assert!(
            ExecutionModelPool::Range { models: vec![] }
                .validate()
                .is_err()
        );
        assert!(
            ExecutionModelPool::Range {
                models: vec![
                    ExecutionModelRef {
                        provider_id: PROVIDER_ID.into(),
                        model: "model".into(),
                    },
                    ExecutionModelRef {
                        provider_id: PROVIDER_ID.into(),
                        model: "model".into(),
                    },
                ],
            }
            .validate()
            .is_err()
        );
        assert!(
            ExecutionModelPool::Single {
                model: ExecutionModelRef {
                    provider_id: " provider".into(),
                    model: "model".into(),
                },
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn participant_concurrency_uses_the_shared_parallelism_ceiling() {
        for accepted in [None, Some(1), Some(MAX_AGENT_EXECUTION_PARALLELISM)] {
            assert!(
                ParticipantConstraints {
                    max_concurrency: accepted,
                    allowed_profile_kinds: None,
                }
                .validate()
                .is_ok()
            );
        }
        for rejected in [Some(0), Some(MAX_AGENT_EXECUTION_PARALLELISM + 1)] {
            assert!(
                ParticipantConstraints {
                    max_concurrency: rejected,
                    allowed_profile_kinds: None,
                }
                .validate()
                .is_err()
            );
        }
    }
}
