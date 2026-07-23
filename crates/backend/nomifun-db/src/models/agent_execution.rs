use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct AgentExecutionRow {
    pub execution_id: String,
    pub user_id: String,
    pub goal: String,
    pub status: String,
    pub plan_gate: String,
    pub adaptation_policy: String,
    pub decision_policy: String,
    pub delegation_policy: String,
    pub max_parallel: i64,
    pub work_dir: Option<String>,
    pub initial_plan_input: String,
    pub summary: Option<String>,
    pub total_tokens: Option<i64>,
    pub version: i64,
    pub plan_revision: i64,
    pub event_sequence: i64,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<TimestampMs>,
    pub deleted_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct AgentExecutionParticipantRow {
    pub participant_id: String,
    pub execution_id: String,
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
    pub introduced_in_revision: i64,
    pub retired_in_revision: Option<i64>,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct AgentExecutionStepRow {
    pub step_id: String,
    pub execution_id: String,
    pub title: String,
    pub spec: String,
    pub role: Option<String>,
    pub tool_policy: String,
    pub kind: String,
    pub agent_mode: Option<String>,
    pub profile: Option<String>,
    pub fanout_group: Option<String>,
    pub control_policy: Option<String>,
    /// Private recursion budget marker. It is derived by the repository when
    /// an active Attempt appends work and is never accepted from wire DTOs.
    pub delegation_depth: i64,
    pub status: String,
    pub assigned_participant_id: Option<String>,
    pub assignment_score: Option<f64>,
    pub assignment_rationale: Option<String>,
    pub assignment_source: Option<String>,
    pub assignment_locked: bool,
    pub failure_policy: String,
    pub preset_prompt: Option<String>,
    pub graph_x: Option<f64>,
    pub graph_y: Option<f64>,
    pub dispatch_after: Option<TimestampMs>,
    pub version: i64,
    pub introduced_in_revision: i64,
    pub superseded_in_revision: Option<i64>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct AgentExecutionStepDependencyRow {
    pub execution_id: String,
    pub blocker_step_id: String,
    pub blocked_step_id: String,
    pub introduced_in_revision: i64,
    pub superseded_in_revision: Option<i64>,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct AgentExecutionAttemptRow {
    pub attempt_id: String,
    pub execution_id: String,
    pub step_id: String,
    pub attempt_no: i64,
    pub participant_id: Option<String>,
    pub status: String,
    pub trigger_reason: String,
    pub effective_config: String,
    pub question: Option<String>,
    pub error: Option<String>,
    pub output_summary: Option<String>,
    pub output_files: String,
    pub tokens: Option<i64>,
    pub retry_after: Option<TimestampMs>,
    pub runtime_state: Option<String>,
    pub started_at: Option<TimestampMs>,
    pub finished_at: Option<TimestampMs>,
    pub version: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct ConversationExecutionLinkRow {
    pub conversation_id: String,
    pub execution_id: String,
    pub relation: String,
    pub step_id: Option<String>,
    pub attempt_id: Option<String>,
    pub active: bool,
    pub cleanup_completed_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct AgentExecutionEventRow {
    pub execution_id: String,
    pub sequence: i64,
    pub event_type: String,
    pub step_id: Option<String>,
    pub attempt_id: Option<String>,
    pub actor_type: String,
    pub actor_id: Option<String>,
    pub actor_conversation_id: Option<String>,
    pub actor_attempt_id: Option<String>,
    pub on_behalf_of_user_id: String,
    pub payload: String,
    pub created_at: TimestampMs,
    pub published_at: Option<TimestampMs>,
}

/// Owner-facing attempt view. `conversation_id` is derived from the active
/// attempt link; it is intentionally absent from the physical attempt table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExecutionAttemptDetailRow {
    pub attempt: AgentExecutionAttemptRow,
    pub conversation_id: Option<String>,
}

/// Owner-facing step view. Current attempt data is derived by attempt_no and
/// the attempt conversation is derived from `conversation_execution_links`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExecutionStepDetailRow {
    pub step: AgentExecutionStepRow,
    pub current_attempt: Option<AgentExecutionAttemptDetailRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExecutionDetailRows {
    pub execution: AgentExecutionRow,
    pub lead_conversation_id: Option<String>,
    pub participants: Vec<AgentExecutionParticipantRow>,
    pub steps: Vec<AgentExecutionStepRow>,
    pub dependencies: Vec<AgentExecutionStepDependencyRow>,
    pub attempts: Vec<AgentExecutionAttemptDetailRow>,
}
