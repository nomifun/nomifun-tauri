//! Wire contracts for reusable collaboration inputs.
//!
//! A template is authoring data only. Instantiation copies its participants
//! into an Agent Execution and never leaves a live template reference behind.

use nomifun_common::{AdaptationPolicy, DecisionPolicy, DelegationPolicy, PlanGate};
use serde::{Deserialize, Serialize};

use crate::webhook::double_option;
use crate::{
    ExecutionModelRef, ParticipantCapability, ParticipantConstraints, PlannedExecutionStep,
    PresetOverrides, ResolvedPresetSnapshot,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExecutionTemplate {
    #[serde(deserialize_with = "crate::serde_util::deserialize_execution_template_id")]
    pub execution_template_id: String,
    pub name: String,
    pub description: Option<String>,
    pub max_parallel: Option<i64>,
    pub work_dir: Option<String>,
    pub context: Option<serde_json::Value>,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExecutionTemplateParticipant {
    #[serde(deserialize_with = "crate::agent_execution::deserialize_uuidv7_id")]
    pub template_participant_id: String,
    #[serde(deserialize_with = "crate::serde_util::deserialize_agent_id")]
    pub source_agent_id: String,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_preset_id"
    )]
    pub preset_id: Option<String>,
    pub preset_revision: Option<i64>,
    pub preset_snapshot: Option<ResolvedPresetSnapshot>,
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
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExecutionTemplateDetail {
    #[serde(flatten)]
    pub template: AgentExecutionTemplate,
    pub participants: Vec<AgentExecutionTemplateParticipant>,
}

/// Authoring input for one candidate Agent. A caller may either round-trip an
/// existing frozen `preset_snapshot`, or provide `preset_id` + overrides and
/// let the server resolve a fresh execution-step snapshot before persistence.
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentExecutionTemplateParticipantInput {
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_agent_id"
    )]
    pub source_agent_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_preset_id"
    )]
    pub preset_id: Option<String>,
    #[serde(default)]
    pub preset_snapshot: Option<ResolvedPresetSnapshot>,
    #[serde(default)]
    pub preset_overrides: Option<PresetOverrides>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_provider_id"
    )]
    pub provider_id: Option<String>,
    #[serde(default, deserialize_with = "crate::serde_util::deserialize_optional_model_name")]
    pub model: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub capability: Option<ParticipantCapability>,
    #[serde(default)]
    pub constraints: Option<ParticipantConstraints>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    #[serde(default)]
    pub disabled_builtin_skills: Vec<String>,
    #[serde(default)]
    pub sort_order: Option<i64>,
}

impl<'de> Deserialize<'de> for AgentExecutionTemplateParticipantInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(
                default,
                deserialize_with = "crate::serde_util::deserialize_optional_agent_id"
            )]
            source_agent_id: Option<String>,
            #[serde(
                default,
                deserialize_with = "crate::serde_util::deserialize_optional_preset_id"
            )]
            preset_id: Option<String>,
            #[serde(default)]
            preset_snapshot: Option<ResolvedPresetSnapshot>,
            #[serde(default)]
            preset_overrides: Option<PresetOverrides>,
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
            #[serde(default)]
            role: Option<String>,
            #[serde(default)]
            capability: Option<ParticipantCapability>,
            #[serde(default)]
            constraints: Option<ParticipantConstraints>,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            system_prompt: Option<String>,
            #[serde(default)]
            enabled_skills: Vec<String>,
            #[serde(default)]
            disabled_builtin_skills: Vec<String>,
            #[serde(default)]
            sort_order: Option<i64>,
        }

        let wire = Wire::deserialize(deserializer)?;
        crate::serde_util::validate_optional_provider_model_pair(
            wire.provider_id.as_deref(),
            wire.model.as_deref(),
        )
        .map_err(serde::de::Error::custom)?;
        Ok(Self {
            source_agent_id: wire.source_agent_id,
            preset_id: wire.preset_id,
            preset_snapshot: wire.preset_snapshot,
            preset_overrides: wire.preset_overrides,
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
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAgentExecutionTemplateRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub max_parallel: Option<i64>,
    #[serde(default)]
    pub work_dir: Option<String>,
    #[serde(default)]
    pub context: Option<serde_json::Value>,
    #[serde(default)]
    pub participants: Vec<AgentExecutionTemplateParticipantInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAgentExecutionTemplateRequest {
    pub expected_version: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_parallel: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub work_dir: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub context: Option<Option<serde_json::Value>>,
    #[serde(default)]
    pub participants: Option<Vec<AgentExecutionTemplateParticipantInput>>,
}

/// Runtime choices that are intentionally not retained by the template.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateExecutionFromTemplateRequest {
    pub goal: String,
    #[serde(default)]
    pub work_dir: Option<String>,
    #[serde(default)]
    pub max_parallel: Option<i64>,
    #[serde(default)]
    pub delegation_policy: DelegationPolicy,
    #[serde(default)]
    pub plan_gate: PlanGate,
    #[serde(default)]
    pub adaptation_policy: AdaptationPolicy,
    #[serde(default)]
    pub decision_policy: DecisionPolicy,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_conversation_id"
    )]
    pub lead_conversation_id: Option<String>,
    /// Optional deterministic lead selection inside the template's existing
    /// participant authority. When omitted, sort_order=0 remains the lead;
    /// when supplied, the matching participant is promoted or the request is
    /// rejected. This never adds an out-of-template model.
    #[serde(default)]
    pub lead_model: Option<ExecutionModelRef>,
    #[serde(default)]
    pub steps: Option<Vec<PlannedExecutionStep>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    #[test]
    fn template_inputs_reject_noncanonical_durable_references() {
        let participant = serde_json::json!({
            "provider_id": PROVIDER_ID,
            "model": "model-a"
        });
        assert!(
            serde_json::from_value::<AgentExecutionTemplateParticipantInput>(participant).is_ok()
        );
        assert!(
            serde_json::from_value::<AgentExecutionTemplateParticipantInput>(serde_json::json!({
                "provider_id": "provider-a",
                "model": "model-a"
            }))
            .is_err()
        );

        for invalid in [
            "nomi",
            "agent_builtin_nomi",
            "agent_0190f5fe-7c00-7a00-8000-000000000114",
        ] {
            assert!(
                serde_json::from_value::<AgentExecutionTemplateParticipantInput>(
                    serde_json::json!({
                        "source_agent_id": invalid,
                        "provider_id": PROVIDER_ID,
                        "model": "model-a"
                    })
                )
                .is_err()
            );
        }

        assert!(
            serde_json::from_value::<CreateExecutionFromTemplateRequest>(serde_json::json!({
                "goal": "ship",
                "lead_conversation_id": CONVERSATION_ID
            }))
            .is_ok()
        );
        assert!(
            serde_json::from_value::<CreateExecutionFromTemplateRequest>(serde_json::json!({
                "goal": "ship",
                "lead_conversation_id": "conversation-1"
            }))
            .is_err()
        );
    }

    #[test]
    fn template_participant_input_requires_provider_and_model_together() {
        for invalid in [
            serde_json::json!({"provider_id": PROVIDER_ID}),
            serde_json::json!({"model": "model-a"}),
            serde_json::json!({"provider_id": PROVIDER_ID, "model": " model-a"}),
        ] {
            assert!(
                serde_json::from_value::<AgentExecutionTemplateParticipantInput>(invalid).is_err()
            );
        }
    }
}
