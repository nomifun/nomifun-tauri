use async_trait::async_trait;
use nomifun_common::{
    AdaptationPolicy, AgentExecutionActor, AgentExecutionEventKind, AgentExecutionStatus,
    AgentStepMode, AgentToolPolicy, DecisionPolicy, DelegationPolicy, ExecutionAttemptStatus,
    ExecutionStepKind,
    ExecutionStepStatus, ParticipantAssignmentSource, PlanGate, StepFailurePolicy,
};

use crate::error::DbError;
use crate::models::{
    AgentExecutionAttemptDetailRow, AgentExecutionDetailRows,
    AgentExecutionEventRow, AgentExecutionParticipantRow, AgentExecutionRow,
    AgentExecutionStepDependencyRow, AgentExecutionStepDetailRow, AgentExecutionStepRow,
    ConversationExecutionLinkRow,
};

/// Validate the canonical JSON form shared by executable templates and
/// immutable runtime participants. Keeping this at the DB domain boundary
/// prevents repository entry points from drifting on concurrency authority.
pub(crate) fn validate_participant_constraints_json(raw: &str) -> Result<(), DbError> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|_| DbError::Conflict("participant constraints must be valid JSON".into()))?;
    let object = value.as_object().ok_or_else(|| {
        DbError::Conflict("participant constraints must be a JSON object".into())
    })?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "max_concurrency" | "allowed_profile_kinds"))
    {
        return Err(DbError::Conflict(
            "participant constraints use unknown or legacy fields".into(),
        ));
    }
    if let Some(value) = object.get("max_concurrency")
        && !value.is_null()
        && value.as_i64().is_none_or(|value| {
            !(1..=nomifun_common::MAX_AGENT_EXECUTION_PARALLELISM).contains(&value)
        })
    {
        return Err(DbError::Conflict(format!(
            "participant max_concurrency must be between 1 and {}",
            nomifun_common::MAX_AGENT_EXECUTION_PARALLELISM
        )));
    }
    if let Some(value) = object.get("allowed_profile_kinds")
        && !value.is_null()
    {
        let values = value.as_array().ok_or_else(|| {
            DbError::Conflict("participant allowed_profile_kinds must be an array".into())
        })?;
        if values
            .iter()
            .any(|value| value.as_str().is_none_or(|value| value.trim().is_empty()))
        {
            return Err(DbError::Conflict(
                "participant allowed_profile_kinds must contain non-empty strings".into(),
            ));
        }
    }
    Ok(())
}

/// Generation-unique ownership proof for scheduler-internal writes.
///
/// The SQLite repository validates this token and its unexpired lease inside
/// the same write transaction as the requested mutation. User commands do not
/// carry a token; they remain protected by their explicit aggregate CAS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentExecutionLeaseToken {
    owner: String,
}

impl AgentExecutionLeaseToken {
    pub fn new(owner: String) -> Self {
        debug_assert!(!owner.trim().is_empty());
        Self { owner }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }
}

#[derive(Debug, Clone)]
pub struct CreateAgentExecutionParams {
    pub goal: String,
    pub status: AgentExecutionStatus,
    pub plan_gate: PlanGate,
    pub adaptation_policy: AdaptationPolicy,
    pub decision_policy: DecisionPolicy,
    pub delegation_policy: DelegationPolicy,
    pub max_parallel: i64,
    pub work_dir: Option<String>,
    pub lead_conversation_id: Option<String>,
    /// Immutable tagged JSON command persisted in the same transaction as the
    /// Planning aggregate. Recovery must replay this value verbatim.
    pub initial_plan_input: String,
}

#[derive(Debug, Clone)]
pub struct NewAgentExecutionParticipant {
    pub participant_id: String,
    pub source_agent_id: String,
    pub preset_id: Option<String>,
    pub preset_revision: Option<i64>,
    pub preset_snapshot: Option<String>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub role: Option<String>,
    pub capability: Option<String>,
    pub constraints: Option<String>,
    pub description: Option<String>,
    pub system_prompt: Option<String>,
    pub enabled_skills: String,
    pub disabled_builtin_skills: String,
    pub sort_order: i64,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateAgentExecutionParams {
    pub goal: Option<String>,
    pub status: Option<AgentExecutionStatus>,
    pub max_parallel: Option<i64>,
    pub work_dir: Option<Option<String>>,
    pub summary: Option<Option<String>>,
    pub total_tokens: Option<Option<i64>>,
}

#[derive(Debug, Clone)]
pub struct NewAgentExecutionStep {
    pub step_id: String,
    pub title: String,
    pub spec: String,
    pub role: Option<String>,
    pub tool_policy: AgentToolPolicy,
    pub kind: ExecutionStepKind,
    pub agent_mode: Option<AgentStepMode>,
    pub profile: Option<String>,
    pub fanout_group: Option<String>,
    pub control_policy: Option<String>,
    pub status: ExecutionStepStatus,
    pub assigned_participant_id: Option<String>,
    pub assignment_score: Option<f64>,
    pub assignment_rationale: Option<String>,
    pub assignment_source: Option<ParticipantAssignmentSource>,
    pub assignment_locked: bool,
    pub failure_policy: StepFailurePolicy,
    pub preset_prompt: Option<String>,
    pub graph_x: Option<f64>,
    pub graph_y: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct NewAgentExecutionStepDependency {
    pub blocker_step_id: String,
    pub blocked_step_id: String,
}

/// Work appended by the currently running Agent Attempt. New dependencies are
/// deliberately batch-local; the repository derives the caller/downstream
/// gates and the private recursion depth under one write lock.
#[derive(Debug, Clone)]
pub struct AppendAgentExecutionStepsFromAttemptParams {
    /// Stable server-derived idempotency key for one canonical delegation
    /// request. It is not accepted from the model payload.
    pub operation_id: String,
    pub caller_conversation_id: String,
    pub caller_step_id: String,
    pub caller_attempt_id: String,
    pub expected_caller_step_version: i64,
    pub expected_caller_attempt_version: i64,
    pub new_steps: Vec<NewAgentExecutionStep>,
    pub new_dependencies: Vec<NewAgentExecutionStepDependency>,
}

#[derive(Debug, Clone)]
pub struct AppendAgentExecutionStepsFromAttemptResult {
    pub detail: AgentExecutionDetailRows,
    /// Original persisted ids, including on a replay whose freshly
    /// materialized candidate ids differ.
    pub added_step_ids: Vec<String>,
}

/// Owner/lead append command. Unlike Replan this never supersedes existing
/// graph history; it only introduces an independent batch at depth zero.
#[derive(Debug, Clone)]
pub struct AppendAgentExecutionStepsParams {
    pub new_steps: Vec<NewAgentExecutionStep>,
    /// Dependencies are local to `new_steps`. Existing active edges are
    /// retained verbatim by the repository.
    pub new_dependencies: Vec<NewAgentExecutionStepDependency>,
}

#[derive(Debug, Clone)]
pub struct ReconcileAgentExecutionPlanParams {
    pub goal: Option<String>,
    pub plan_gate: Option<PlanGate>,
    pub adaptation_policy: Option<AdaptationPolicy>,
    pub decision_policy: Option<DecisionPolicy>,
    pub delegation_policy: Option<DelegationPolicy>,
    pub keep_step_ids: Vec<String>,
    pub new_participants: Vec<NewAgentExecutionParticipant>,
    pub retire_participant_ids: Vec<String>,
    pub new_steps: Vec<NewAgentExecutionStep>,
    /// Complete dependency set for the new active revision. Edges may connect
    /// kept active steps and newly introduced steps.
    pub new_dependencies: Vec<NewAgentExecutionStepDependency>,
    pub execution_status: AgentExecutionStatus,
}

#[derive(Debug, Clone)]
pub struct CreateAgentExecutionAttemptParams {
    pub participant_id: Option<String>,
    pub start_immediately: bool,
    pub trigger_reason: String,
    pub effective_config: String,
    pub retry_after: Option<i64>,
    pub runtime_state: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AdoptAgentExecutionStepOutputParams {
    pub output_summary: String,
    pub output_files: String,
    pub tokens: Option<i64>,
    pub runtime_state: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AttemptConversationEffectParams {
    /// Complete replacement for the Agent attempt's typed effect queue.
    pub runtime_state: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AttemptConversationEffectResult {
    pub detail: AgentExecutionStepDetailRow,
    pub conversation_id: String,
}

#[derive(Debug, Clone)]
pub struct PendingConversationCleanup {
    pub execution_id: String,
    pub user_id: String,
    pub conversation_id: String,
}

/// Atomic reset requested when a Loop controller settles one iteration and
/// schedules the next one. `expected_steps` must be the complete active
/// descendant closure of `body_step_id`, excluding the controller itself.
/// The repository validates the closure and every version before changing any
/// step, so a stale control decision can never partially advance the loop.
#[derive(Debug, Clone)]
pub struct LoopRepeatResetParams {
    pub body_step_id: String,
    pub expected_steps: Vec<RetryAgentExecutionStep>,
}

#[derive(Debug, Clone)]
pub struct SettleAgentExecutionAttemptParams {
    pub attempt_status: ExecutionAttemptStatus,
    pub step_status: ExecutionStepStatus,
    pub execution_status: Option<AgentExecutionStatus>,
    pub question: Option<Option<String>>,
    pub error: Option<Option<String>>,
    pub output_summary: Option<Option<String>>,
    pub output_files: Option<String>,
    pub tokens: Option<Option<i64>>,
    pub retry_after: Option<Option<i64>>,
    pub runtime_state: Option<Option<String>>,
    pub started_at: Option<Option<i64>>,
    pub finished_at: Option<Option<i64>>,
    /// Present only for a Loop `Repeat` resolution. Attempt settlement and all
    /// body/downstream resets commit under the same transaction and aggregate
    /// version advance.
    pub loop_repeat_reset: Option<LoopRepeatResetParams>,
}

#[derive(Debug, Clone)]
pub struct RetryAgentExecutionStep {
    pub step_id: String,
    pub expected_step_version: i64,
}

#[derive(Debug, Clone)]
pub struct NewAgentExecutionEvent {
    pub event_type: AgentExecutionEventKind,
    pub step_id: Option<String>,
    pub attempt_id: Option<String>,
    pub actor: AgentExecutionActor,
    pub payload: String,
}

#[async_trait]
pub trait IAgentExecutionRepository: Send + Sync {
    async fn create_execution_with_participants(
        &self,
        user_id: &str,
        params: &CreateAgentExecutionParams,
        participants: &[NewAgentExecutionParticipant],
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionRow, DbError>;

    async fn get_execution(
        &self,
        user_id: &str,
        execution_id: &str,
    ) -> Result<Option<AgentExecutionRow>, DbError>;
    async fn get_execution_detail(
        &self,
        user_id: &str,
        execution_id: &str,
    ) -> Result<Option<AgentExecutionDetailRows>, DbError>;
    async fn list_executions(
        &self,
        user_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AgentExecutionRow>, DbError>;
    async fn list_recoverable_executions(
        &self,
        statuses: &[AgentExecutionStatus],
    ) -> Result<Vec<AgentExecutionRow>, DbError>;

    /// General aggregate mutation. It never reopens a terminal execution;
    /// settled-result reopening is intentionally confined to the versioned
    /// retry/adopt commands, and Cancelled is irreversible.
    async fn update_execution(
        &self,
        user_id: &str,
        execution_id: &str,
        expected_version: i64,
        lease: Option<&AgentExecutionLeaseToken>,
        params: &UpdateAgentExecutionParams,
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionRow, DbError>;
    /// Atomically freezes dispatch: queued attempts are cancelled, running
    /// attempts are interrupted, their active links become cleanup work, and
    /// running steps return to Pending. WaitingInput attempts/questions remain
    /// durable so Resume can restore the correct aggregate attention state.
    async fn pause_execution(
        &self,
        user_id: &str,
        execution_id: &str,
        expected_version: i64,
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionRow, DbError>;
    /// Resume from Paused to WaitingInput when any durable question remains,
    /// otherwise to Running. The decision and status CAS share one transaction.
    async fn resume_execution(
        &self,
        user_id: &str,
        execution_id: &str,
        expected_version: i64,
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionRow, DbError>;
    async fn cancel_execution(
        &self,
        user_id: &str,
        execution_id: &str,
        expected_version: i64,
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionDetailRows, DbError>;
    async fn delete_execution(
        &self,
        user_id: &str,
        execution_id: &str,
        expected_version: i64,
        event: &NewAgentExecutionEvent,
    ) -> Result<bool, DbError>;

    /// Lease methods return `Ok(Some(row))` on acquisition/renew/release and
    /// `Ok(None)` when the CAS/ownership/expiry predicate did not match.
    async fn try_acquire_lease(
        &self,
        execution_id: &str,
        expected_version: i64,
        owner: &str,
        expires_at: i64,
    ) -> Result<Option<AgentExecutionRow>, DbError>;
    async fn renew_lease(
        &self,
        execution_id: &str,
        owner: &str,
        expected_expires_at: i64,
        expires_at: i64,
    ) -> Result<Option<AgentExecutionRow>, DbError>;
    async fn release_lease(
        &self,
        execution_id: &str,
        owner: &str,
        expected_expires_at: i64,
    ) -> Result<Option<AgentExecutionRow>, DbError>;

    async fn list_participants(
        &self,
        user_id: &str,
        execution_id: &str,
    ) -> Result<Vec<AgentExecutionParticipantRow>, DbError>;
    /// Atomically applies a complete new active graph revision. Active attempts
    /// belonging to omitted steps are cancelled/interrupted in the same
    /// transaction; historical rows are superseded, never deleted.
    async fn reconcile_plan(
        &self,
        user_id: &str,
        execution_id: &str,
        expected_version: i64,
        params: &ReconcileAgentExecutionPlanParams,
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionDetailRows, DbError>;

    /// Atomically appends a batch of Steps from one active running Attempt.
    /// This command CASes the caller Step/Attempt, not the aggregate version,
    /// so unrelated parallel settlements cannot cause a false conflict. Batch
    /// leaves are inserted as blockers of the caller's still-Pending direct
    /// downstream Steps; already-started downstream work is never rewound.
    async fn find_steps_append_from_attempt(
        &self,
        user_id: &str,
        execution_id: &str,
        operation_id: &str,
    ) -> Result<Option<AppendAgentExecutionStepsFromAttemptResult>, DbError>;
    async fn append_steps_from_attempt(
        &self,
        user_id: &str,
        execution_id: &str,
        params: &AppendAgentExecutionStepsFromAttemptParams,
        event: &NewAgentExecutionEvent,
    ) -> Result<AppendAgentExecutionStepsFromAttemptResult, DbError>;

    /// Versioned user/lead append. It supports approval-gated, running,
    /// paused, and waiting aggregates, and reopens a settled non-cancelled
    /// result to Running. Existing Steps/dependencies are append-only history.
    async fn append_steps(
        &self,
        user_id: &str,
        execution_id: &str,
        expected_version: i64,
        params: &AppendAgentExecutionStepsParams,
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionDetailRows, DbError>;

    async fn get_step(
        &self,
        user_id: &str,
        execution_id: &str,
        step_id: &str,
    ) -> Result<Option<AgentExecutionStepRow>, DbError>;
    async fn get_step_detail(
        &self,
        user_id: &str,
        execution_id: &str,
        step_id: &str,
    ) -> Result<Option<AgentExecutionStepDetailRow>, DbError>;
    async fn list_steps(
        &self,
        user_id: &str,
        execution_id: &str,
    ) -> Result<Vec<AgentExecutionStepRow>, DbError>;
    async fn list_dependencies(
        &self,
        user_id: &str,
        execution_id: &str,
    ) -> Result<Vec<AgentExecutionStepDependencyRow>, DbError>;
    /// Scheduler-only lifecycle transition. Semantic step fields are absent
    /// from this boundary by construction; user edits replace the immutable
    /// snapshot through `reconcile_plan`.
    async fn transition_step_status(
        &self,
        user_id: &str,
        execution_id: &str,
        step_id: &str,
        expected_execution_version: i64,
        expected_step_version: i64,
        lease: Option<&AgentExecutionLeaseToken>,
        status: ExecutionStepStatus,
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionStepRow, DbError>;
    async fn reset_steps_for_retry(
        &self,
        user_id: &str,
        execution_id: &str,
        expected_execution_version: i64,
        steps: &[RetryAgentExecutionStep],
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionDetailRows, DbError>;
    async fn adopt_step_output(
        &self,
        user_id: &str,
        execution_id: &str,
        expected_execution_version: i64,
        step_id: &str,
        expected_step_version: i64,
        params: &AdoptAgentExecutionStepOutputParams,
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionStepDetailRow, DbError>;
    async fn resume_waiting_attempt(
        &self,
        user_id: &str,
        execution_id: &str,
        expected_execution_version: i64,
        step_id: &str,
        expected_step_version: i64,
        attempt_id: &str,
        expected_attempt_version: i64,
        params: &AttemptConversationEffectParams,
        event: &NewAgentExecutionEvent,
    ) -> Result<AttemptConversationEffectResult, DbError>;

    /// Persist a conversation side effect and its audit event before any
    /// external delivery.  The exact active attempt link is resolved in the
    /// same transaction and returned to the caller.
    async fn enqueue_attempt_conversation_effect(
        &self,
        user_id: &str,
        execution_id: &str,
        expected_execution_version: i64,
        step_id: &str,
        expected_step_version: i64,
        attempt_id: &str,
        expected_attempt_version: i64,
        params: &AttemptConversationEffectParams,
        event: &NewAgentExecutionEvent,
    ) -> Result<AttemptConversationEffectResult, DbError>;

    /// Replace/clear the effect queue after idempotent Conversation delivery.
    async fn acknowledge_attempt_conversation_effect(
        &self,
        user_id: &str,
        execution_id: &str,
        step_id: &str,
        attempt_id: &str,
        expected_attempt_version: i64,
        params: &AttemptConversationEffectParams,
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionStepDetailRow, DbError>;

    /// Scheduler-internal mutation. It CASes the step, then advances the
    /// aggregate version without requiring an execution-version snapshot so
    /// different ready steps can be persisted concurrently.
    async fn create_attempt(
        &self,
        user_id: &str,
        execution_id: &str,
        step_id: &str,
        expected_step_version: i64,
        lease: Option<&AgentExecutionLeaseToken>,
        params: &CreateAgentExecutionAttemptParams,
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionStepDetailRow, DbError>;
    /// Atomically starts a queued Agent attempt and creates its conversation
    /// link. Only step/attempt versions are CASed; the aggregate version is
    /// advanced unconditionally to avoid parallel-step false conflicts.
    async fn start_attempt(
        &self,
        user_id: &str,
        execution_id: &str,
        step_id: &str,
        expected_step_version: i64,
        attempt_id: &str,
        expected_attempt_version: i64,
        conversation_id: &str,
        lease: Option<&AgentExecutionLeaseToken>,
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionStepDetailRow, DbError>;
    /// Scheduler-internal settlement. Step and attempt are CASed and the
    /// aggregate version advances exactly once, with an optional execution
    /// status transition in that same update.
    async fn settle_attempt(
        &self,
        user_id: &str,
        execution_id: &str,
        step_id: &str,
        expected_step_version: i64,
        attempt_id: &str,
        expected_attempt_version: i64,
        lease: Option<&AgentExecutionLeaseToken>,
        params: &SettleAgentExecutionAttemptParams,
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionStepDetailRow, DbError>;
    async fn get_attempt(
        &self,
        user_id: &str,
        execution_id: &str,
        step_id: &str,
        attempt_id: &str,
    ) -> Result<Option<AgentExecutionAttemptDetailRow>, DbError>;
    async fn list_attempts(
        &self,
        user_id: &str,
        execution_id: &str,
        step_id: Option<&str>,
    ) -> Result<Vec<AgentExecutionAttemptDetailRow>, DbError>;

    async fn list_conversation_links(
        &self,
        user_id: &str,
        execution_id: &str,
    ) -> Result<Vec<ConversationExecutionLinkRow>, DbError>;
    /// Resolves both current and historical execution relations for a
    /// conversation. Soft-deleting an execution deactivates its links but does
    /// not erase transcript provenance needed by read-side projection.
    async fn resolve_conversation_link(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<ConversationExecutionLinkRow>, DbError>;
    /// Returns whether the conversation is an execution-attempt transcript
    /// owned by `user_id`. Unlike the runtime relation read-side, this audit
    /// guard deliberately includes inactive links and soft-deleted executions.
    async fn has_attempt_conversation_link(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<bool, DbError>;

    /// Durable external cleanup outbox derived from inactive attempt links.
    async fn list_pending_conversation_cleanups(
        &self,
        execution_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PendingConversationCleanup>, DbError>;
    async fn mark_conversation_cleanup_completed(
        &self,
        execution_id: &str,
        conversation_id: &str,
        completed_at: i64,
    ) -> Result<bool, DbError>;

    async fn append_event(
        &self,
        user_id: &str,
        execution_id: &str,
        expected_version: i64,
        event: &NewAgentExecutionEvent,
    ) -> Result<AgentExecutionEventRow, DbError>;
    async fn list_events(
        &self,
        user_id: &str,
        execution_id: &str,
        after_sequence: i64,
        limit: i64,
    ) -> Result<Vec<AgentExecutionEventRow>, DbError>;
    async fn list_unpublished_events(
        &self,
        limit: i64,
    ) -> Result<Vec<AgentExecutionEventRow>, DbError>;
    async fn mark_event_published(
        &self,
        execution_id: &str,
        sequence: i64,
        published_at: i64,
    ) -> Result<bool, DbError>;

    /// Owner-independent deletion guard for every current participant binding
    /// that can ever be scheduled again. Settled executions intentionally
    /// remain in this result because retry/adopt reopens them; only cancelled
    /// or tombstoned executions are permanently inert.
    async fn list_reopenable_provider_usages(
        &self,
        provider_id: &str,
    ) -> Result<Vec<(String, String)>, DbError>;
}
