//! Shared backend value types for the Agent Execution domain.
//!
//! These values complement the deployment-neutral request, receipt, status and
//! invocation types in `nomi-types`. Together they form the vocabulary used by
//! database constraints, the application state machine, HTTP DTOs, gateway
//! tools, embedded hosts and the UI wire contract.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub use nomi_types::agent::{AgentExecutionReceipt, AgentExecutionStatus, AgentToolPolicy};

/// Hard ceiling for concurrently executing steps in one Agent Execution.
/// Kept in the shared domain so API and persistence validation cannot drift.
pub const MAX_AGENT_EXECUTION_PARALLELISM: i64 = 64;

/// Maximum provider/model choices materialized by one execution request.
/// Automatic selection is deliberately bounded so adding providers cannot
/// silently inflate every planner prompt or make routing nondeterministic.
pub const MAX_AGENT_EXECUTION_MODELS: usize = 16;

/// Maximum active immutable Agent snapshots in one execution revision.
/// Preset-enriched variants share this budget with the base model choices.
pub const MAX_AGENT_EXECUTION_PARTICIPANTS: usize = 64;

/// Maximum number of nodes in the current (non-superseded) execution DAG.
/// Historical revisions do not count toward this limit: they are immutable
/// audit facts, while this ceiling bounds scheduler and planner complexity.
pub const MAX_AGENT_EXECUTION_STEPS: usize = 128;

/// Maximum number of recursive in-execution delegation hops. The depth is a
/// private Step fact derived by the repository; clients cannot choose it.
pub const MAX_AGENT_DELEGATION_DEPTH: i64 = 4;

/// Returned when persisted or wire data contains an unknown execution-domain
/// value. Callers must reject it instead of silently falling back to a more
/// permissive policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownAgentExecutionValue {
    kind: &'static str,
    value: String,
}

impl UnknownAgentExecutionValue {
    fn new(kind: &'static str, value: &str) -> Self {
        Self {
            kind,
            value: value.to_owned(),
        }
    }
}

impl fmt::Display for UnknownAgentExecutionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown {} value: {}", self.kind, self.value)
    }
}

impl std::error::Error for UnknownAgentExecutionValue {}

macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $($variant:ident => $wire:literal),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = UnknownAgentExecutionValue;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($wire => Ok(Self::$variant)),+,
                    _ => Err(UnknownAgentExecutionValue::new(stringify!($name), value)),
                }
            }
        }
    };
}

string_enum! {
    /// Principal type responsible for an execution-domain command.
    ///
    /// The concrete identity and optional Agent conversation/attempt context
    /// live in [`AgentExecutionActor`]; this enum is also the wire/database
    /// vocabulary for immutable audit events.
    pub enum AgentExecutionActorType {
        System => "system",
        User => "user",
        Agent => "agent",
    }
}

/// Explicit attribution carried by every Agent Execution mutation.
///
/// `System` is reserved for scheduler/recovery work. A user command carries
/// the authenticated user id. An Agent command always carries its stable
/// Agent id. Local conversation-backed Agents additionally carry their
/// calling conversation and, when the active execution link identifies it,
/// the concrete attempt. External Agents have no local conversation context.
/// The database derives `on_behalf_of_user_id` from the execution owner and
/// validates this actor inside the write transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentExecutionActor {
    System,
    User {
        user_id: String,
    },
    Agent {
        agent_id: String,
        #[serde(default)]
        conversation_id: Option<String>,
        #[serde(default)]
        attempt_id: Option<String>,
    },
}

impl AgentExecutionActor {
    pub fn system() -> Self {
        Self::System
    }

    pub fn user(user_id: impl Into<String>) -> Self {
        Self::User {
            user_id: user_id.into(),
        }
    }

    pub fn agent(conversation_id: impl Into<String>, attempt_id: Option<String>) -> Self {
        let conversation_id = conversation_id.into();
        Self::Agent {
            agent_id: conversation_id.clone(),
            conversation_id: Some(conversation_id),
            attempt_id,
        }
    }

    /// Identifies an Agent that has no local conversation/attempt context.
    /// Authorization of this stable id belongs to the caller-facing boundary;
    /// the execution event remains owner-scoped and durably attributable.
    pub fn external_agent(agent_id: impl Into<String>) -> Self {
        Self::Agent {
            agent_id: agent_id.into(),
            conversation_id: None,
            attempt_id: None,
        }
    }

    pub const fn actor_type(&self) -> AgentExecutionActorType {
        match self {
            Self::System => AgentExecutionActorType::System,
            Self::User { .. } => AgentExecutionActorType::User,
            Self::Agent { .. } => AgentExecutionActorType::Agent,
        }
    }

    /// Canonical durable actor id. Local Agents default this to their
    /// conversation id; external Agents retain their stable remote id.
    pub fn actor_id(&self) -> Option<String> {
        match self {
            Self::System => None,
            Self::User { user_id } => Some(user_id.clone()),
            Self::Agent { agent_id, .. } => Some(agent_id.clone()),
        }
    }

    pub fn conversation_id(&self) -> Option<&str> {
        match self {
            Self::Agent {
                conversation_id, ..
            } => conversation_id.as_deref(),
            _ => None,
        }
    }

    pub fn attempt_id(&self) -> Option<&str> {
        match self {
            Self::Agent { attempt_id, .. } => attempt_id.as_deref(),
            _ => None,
        }
    }
}

string_enum! {
    /// Whether and how strongly a conversation may delegate work.
    pub enum DelegationPolicy {
        Disabled => "disabled",
        Automatic => "automatic",
        PreferParallel => "prefer_parallel",
    }
}

impl Default for DelegationPolicy {
    fn default() -> Self {
        Self::Automatic
    }
}

string_enum! {
    /// Whether a generated plan must be approved before execution starts.
    pub enum PlanGate {
        Automatic => "automatic",
        RequireApproval => "require_approval",
    }
}

impl Default for PlanGate {
    fn default() -> Self {
        Self::Automatic
    }
}

string_enum! {
    /// Whether a running execution may expand or revise its plan autonomously.
    pub enum AdaptationPolicy {
        Fixed => "fixed",
        Adaptive => "adaptive",
    }
}

impl Default for AdaptationPolicy {
    fn default() -> Self {
        Self::Fixed
    }
}

string_enum! {
    /// How an Agent attempt handles a consequential decision it cannot safely infer.
    pub enum DecisionPolicy {
        Automatic => "automatic",
        AskUser => "ask_user",
    }
}

impl Default for DecisionPolicy {
    fn default() -> Self {
        Self::Automatic
    }
}

string_enum! {
    /// The scheduler behavior of a step. Synthesis is an Agent step mode, not a
    /// control-step kind.
    pub enum ExecutionStepKind {
        Agent => "agent",
        Verify => "verify",
        Judge => "judge",
        Loop => "loop",
    }
}

string_enum! {
    pub enum AgentStepMode {
        Normal => "normal",
        Synthesis => "synthesis",
    }
}

string_enum! {
    /// Aggregate lifecycle of a step across all of its attempts.
    pub enum ExecutionStepStatus {
        Pending => "pending",
        Running => "running",
        WaitingInput => "waiting_input",
        Completed => "completed",
        Failed => "failed",
        Skipped => "skipped",
        Cancelled => "cancelled",
    }
}

impl ExecutionStepStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Skipped | Self::Cancelled
        )
    }

    /// Legal step transitions. Completed/failed/skipped steps may be explicitly
    /// reset to pending for a user-requested rerun; cancellation is final.
    pub const fn can_transition_to(self, next: Self) -> bool {
        if self as u8 == next as u8 {
            return true;
        }
        match self {
            Self::Pending => matches!(
                next,
                Self::Running
                    | Self::Completed
                    | Self::Failed
                    | Self::Skipped
                    | Self::Cancelled
            ),
            Self::Running => matches!(
                next,
                Self::Pending
                    | Self::WaitingInput
                    | Self::Completed
                    | Self::Failed
                    | Self::Cancelled
            ),
            Self::WaitingInput => matches!(
                next,
                Self::Pending | Self::Running | Self::Completed | Self::Failed | Self::Cancelled
            ),
            Self::Completed => matches!(next, Self::Pending),
            Self::Failed | Self::Skipped => matches!(next, Self::Pending | Self::Completed),
            Self::Cancelled => false,
        }
    }
}

string_enum! {
    /// Lifecycle of one concrete Agent attempt for a step.
    pub enum ExecutionAttemptStatus {
        Queued => "queued",
        Running => "running",
        WaitingInput => "waiting_input",
        Completed => "completed",
        Failed => "failed",
        Cancelled => "cancelled",
        Interrupted => "interrupted",
    }
}

impl ExecutionAttemptStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }

    /// Complete lifecycle for one immutable execution attempt. A failed or
    /// interrupted attempt is never reset; a retry appends another attempt.
    pub const fn can_transition_to(self, next: Self) -> bool {
        if self as u8 == next as u8 {
            return true;
        }
        match self {
            // Interrupted means a concrete invocation started and was then
            // lost. A queued reservation has no started_at and can only start
            // or be cancelled without pretending work ran.
            Self::Queued => matches!(next, Self::Running | Self::Cancelled),
            Self::Running => matches!(
                next,
                Self::WaitingInput
                    | Self::Completed
                    | Self::Failed
                    | Self::Cancelled
                    | Self::Interrupted
            ),
            Self::WaitingInput => matches!(
                next,
                Self::Running | Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
            ),
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted => false,
        }
    }
}

string_enum! {
    pub enum ConversationExecutionRelation {
        Lead => "lead",
        Attempt => "attempt",
    }
}

string_enum! {
    pub enum StepFailurePolicy {
        FailExecution => "fail_execution",
        SkipDependents => "skip_dependents",
    }
}

string_enum! {
    pub enum ParticipantAssignmentSource {
        Planner => "planner",
        Automatic => "automatic",
        Manual => "manual",
    }
}

string_enum! {
    /// Durable outbox event categories. Realtime clients may use `change_kind`
    /// as a rendering hint, but revision remains the source of ordering.
    #[derive(TS)]
    #[ts(export_to = "../../../../ui/src/common/protocolBindings/")]
    pub enum AgentExecutionEventKind {
        Created => "created",
        StatusChanged => "status_changed",
        PlanChanged => "plan_changed",
        StepChanged => "step_changed",
        AttemptChanged => "attempt_changed",
        DecisionRequested => "decision_requested",
        DecisionAnswered => "decision_answered",
        Deleted => "deleted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use ts_rs::Config;

    #[test]
    fn export_bindings_agent_execution_event_kind() {
        let generated = AgentExecutionEventKind::export_to_string(&Config::default())
            .expect("AgentExecutionEventKind must export to TypeScript");
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../ui/src/common/protocolBindings/AgentExecutionEventKind.ts");
        let unchanged = std::fs::read_to_string(&path)
            .map(|current| current == generated)
            .unwrap_or(false);
        if !unchanged {
            std::fs::write(&path, generated).expect("write AgentExecutionEventKind binding");
        }
    }

    #[test]
    fn values_round_trip_without_fallbacks() {
        for (wire, expected) in [
            ("planning", AgentExecutionStatus::Planning),
            ("awaiting_approval", AgentExecutionStatus::AwaitingApproval),
            ("completed_with_failures", AgentExecutionStatus::CompletedWithFailures),
        ] {
            assert_eq!(wire.parse::<AgentExecutionStatus>().unwrap(), expected);
            assert_eq!(expected.as_str(), wire);
        }
        assert!("unknown".parse::<AgentExecutionStatus>().is_err());
        assert_eq!(
            "read_only".parse::<AgentToolPolicy>().unwrap(),
            AgentToolPolicy::ReadOnly
        );
        assert!("admin".parse::<AgentToolPolicy>().is_err());
    }

    #[test]
    fn only_explicitly_reopenable_execution_results_can_return_to_running() {
        assert!(AgentExecutionStatus::Planning.can_transition_to(AgentExecutionStatus::Running));
        assert!(
            AgentExecutionStatus::Running
                .can_transition_to(AgentExecutionStatus::AwaitingApproval)
        );
        assert!(AgentExecutionStatus::Running.can_transition_to(AgentExecutionStatus::Paused));
        assert!(
            AgentExecutionStatus::Paused.can_transition_to(AgentExecutionStatus::WaitingInput)
        );
        assert!(AgentExecutionStatus::Completed.can_transition_to(AgentExecutionStatus::Running));
        assert!(AgentExecutionStatus::Failed.can_transition_to(AgentExecutionStatus::Running));
        assert!(!AgentExecutionStatus::Cancelled.can_transition_to(AgentExecutionStatus::Running));
    }

    #[test]
    fn settled_step_can_only_reenter_through_explicit_pending_reset() {
        assert!(ExecutionStepStatus::Completed.can_transition_to(ExecutionStepStatus::Pending));
        assert!(!ExecutionStepStatus::Completed.can_transition_to(ExecutionStepStatus::Running));
        assert!(ExecutionStepStatus::Failed.can_transition_to(ExecutionStepStatus::Completed));
        assert!(!ExecutionStepStatus::Cancelled.can_transition_to(ExecutionStepStatus::Pending));
    }

    #[test]
    fn queued_attempt_can_start_or_cancel_but_cannot_claim_interruption() {
        assert!(ExecutionAttemptStatus::Queued.can_transition_to(ExecutionAttemptStatus::Running));
        assert!(
            ExecutionAttemptStatus::Queued.can_transition_to(ExecutionAttemptStatus::Cancelled)
        );
        assert!(
            !ExecutionAttemptStatus::Queued
                .can_transition_to(ExecutionAttemptStatus::Interrupted)
        );
        assert!(
            ExecutionAttemptStatus::Running
                .can_transition_to(ExecutionAttemptStatus::Interrupted)
        );
    }

    #[test]
    fn agent_actor_keeps_stable_identity_separate_from_local_context() {
        let local = AgentExecutionActor::agent(
            "019bffffffff-7abc-8def-8123-456789abcdef",
            Some("attempt_1".to_owned()),
        );
        assert_eq!(
            local.actor_id().as_deref(),
            Some("019bffffffff-7abc-8def-8123-456789abcdef")
        );
        assert_eq!(
            local.conversation_id(),
            Some("019bffffffff-7abc-8def-8123-456789abcdef")
        );
        assert_eq!(local.attempt_id(), Some("attempt_1"));

        let external = AgentExecutionActor::external_agent("companion_1");
        assert_eq!(external.actor_id().as_deref(), Some("companion_1"));
        assert_eq!(external.conversation_id(), None);
        assert_eq!(external.attempt_id(), None);
        let value = serde_json::to_value(&external).unwrap();
        assert_eq!(value["type"], "agent");
        assert_eq!(value["agent_id"], "companion_1");
        assert!(value["conversation_id"].is_null());
        assert!(value["attempt_id"].is_null());
        assert_eq!(
            serde_json::from_value::<AgentExecutionActor>(value).unwrap(),
            external
        );
    }

    #[test]
    fn durable_execution_event_vocabulary_is_exactly_eight_facts() {
        let kinds = [
            AgentExecutionEventKind::Created,
            AgentExecutionEventKind::StatusChanged,
            AgentExecutionEventKind::PlanChanged,
            AgentExecutionEventKind::StepChanged,
            AgentExecutionEventKind::AttemptChanged,
            AgentExecutionEventKind::DecisionRequested,
            AgentExecutionEventKind::DecisionAnswered,
            AgentExecutionEventKind::Deleted,
        ];
        let wires: std::collections::BTreeSet<&str> =
            kinds.iter().map(|kind| kind.as_str()).collect();
        assert_eq!(wires.len(), 8, "event facts must stay unique");
        assert_eq!(
            wires,
            std::collections::BTreeSet::from([
                "attempt_changed",
                "created",
                "decision_answered",
                "decision_requested",
                "deleted",
                "plan_changed",
                "status_changed",
                "step_changed",
            ])
        );
        assert!("migrated".parse::<AgentExecutionEventKind>().is_err());
        assert!("run_started".parse::<AgentExecutionEventKind>().is_err());
        assert!("worker_changed".parse::<AgentExecutionEventKind>().is_err());
    }
}
