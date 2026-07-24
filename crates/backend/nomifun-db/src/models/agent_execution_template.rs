use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Reusable, owner-scoped configuration for creating an Agent Execution.
///
/// A template is intentionally not runtime state: it has no status, plan,
/// scheduler lease, Attempt, or parent/child execution relation.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct AgentExecutionTemplateRow {
    pub execution_template_id: String,
    pub user_id: String,
    pub name: String,
    pub description: Option<String>,
    pub max_parallel: Option<i64>,
    pub work_dir: Option<String>,
    pub context: Option<String>,
    pub primary_participant_id: String,
    pub version: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Mutable authoring configuration for one candidate Agent in a template.
/// Runtime creation resolves and freezes this into an immutable
/// `AgentExecutionParticipantRow`.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct AgentExecutionTemplateParticipantRow {
    pub template_participant_id: String,
    pub template_id: String,
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
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExecutionTemplateDetailRows {
    pub template: AgentExecutionTemplateRow,
    pub participants: Vec<AgentExecutionTemplateParticipantRow>,
}
