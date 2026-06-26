//! Orchestration ("智能编排") request/response DTOs: fleets, fleet members,
//! capability profiles, and orchestration workspaces. Plain serde only (no
//! ts-rs in P0).

use serde::{Deserialize, Serialize};

use crate::webhook::double_option;

/// A fleet (编队) as returned to clients: a named group of agent members with an
/// optional parallelism cap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fleet {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub max_parallel: Option<i64>,
    pub members: Vec<FleetMember>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One member of a fleet: an agent reference plus its routing hints, capability
/// profile, and constraints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetMember {
    pub id: String,
    pub agent_id: String,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub role_hint: Option<String>,
    pub capability_profile: Option<CapabilityProfile>,
    pub constraints: Option<MemberConstraints>,
    pub sort_order: i64,
}

/// Declarative capability profile used to route tasks to a member.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityProfile {
    pub strengths: Vec<String>,
    pub modalities: Vec<String>,
    pub tools: bool,
    pub reasoning: String,
    pub cost_tier: String,
    pub speed_tier: String,
}

/// Per-member runtime constraints applied by the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberConstraints {
    pub max_concurrency: Option<i64>,
    pub cost_tier: Option<String>,
    pub allowed_task_kinds: Option<Vec<String>>,
}

/// An orchestration workspace: a named scope with an optional default fleet and
/// working directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchWorkspace {
    pub id: String,
    pub name: String,
    pub default_fleet_id: Option<String>,
    pub workspace_dir: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Create a fleet with an initial set of members.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFleetRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub max_parallel: Option<i64>,
    #[serde(default)]
    pub members: Vec<FleetMemberInput>,
}

/// Partial update of a fleet. The `Option<Option<T>>` patch fields distinguish
/// "absent" (keep current) from explicit `null` (clear) via [`double_option`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateFleetRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_parallel: Option<Option<i64>>,
    #[serde(default)]
    pub members: Option<Vec<FleetMemberInput>>,
}

/// Member payload used when creating or replacing fleet members.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetMemberInput {
    pub agent_id: String,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub role_hint: Option<String>,
    #[serde(default)]
    pub capability_profile: Option<CapabilityProfile>,
    #[serde(default)]
    pub constraints: Option<MemberConstraints>,
    #[serde(default)]
    pub sort_order: Option<i64>,
}

/// Create an orchestration workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    #[serde(default)]
    pub default_fleet_id: Option<String>,
    #[serde(default)]
    pub workspace_dir: Option<String>,
}

/// Partial update of an orchestration workspace. `default_fleet_id` uses
/// [`double_option`]: absent keeps the current binding, explicit `null` clears it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateWorkspaceRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub default_fleet_id: Option<Option<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_fleet_request_round_trips() {
        let req = CreateFleetRequest {
            name: "research-fleet".to_string(),
            description: Some("multi-agent research".to_string()),
            max_parallel: Some(3),
            members: vec![FleetMemberInput {
                agent_id: "agent_abc".to_string(),
                provider_id: Some("provider_xyz".to_string()),
                model: Some("claude-opus".to_string()),
                role_hint: Some("lead".to_string()),
                capability_profile: Some(CapabilityProfile {
                    strengths: vec!["analysis".to_string(), "writing".to_string()],
                    modalities: vec!["text".to_string()],
                    tools: true,
                    reasoning: "high".to_string(),
                    cost_tier: "premium".to_string(),
                    speed_tier: "medium".to_string(),
                }),
                constraints: Some(MemberConstraints {
                    max_concurrency: Some(2),
                    cost_tier: Some("premium".to_string()),
                    allowed_task_kinds: Some(vec!["research".to_string()]),
                }),
                sort_order: Some(0),
            }],
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let back: CreateFleetRequest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.name, "research-fleet");
        assert_eq!(back.description.as_deref(), Some("multi-agent research"));
        assert_eq!(back.max_parallel, Some(3));
        assert_eq!(back.members.len(), 1);

        let member = &back.members[0];
        assert_eq!(member.agent_id, "agent_abc");
        assert_eq!(member.provider_id.as_deref(), Some("provider_xyz"));
        assert_eq!(member.model.as_deref(), Some("claude-opus"));
        assert_eq!(member.role_hint.as_deref(), Some("lead"));
        assert_eq!(member.sort_order, Some(0));

        let profile = member.capability_profile.as_ref().expect("profile");
        assert_eq!(profile.strengths, vec!["analysis", "writing"]);
        assert_eq!(profile.modalities, vec!["text"]);
        assert!(profile.tools);
        assert_eq!(profile.reasoning, "high");
        assert_eq!(profile.cost_tier, "premium");
        assert_eq!(profile.speed_tier, "medium");

        let constraints = member.constraints.as_ref().expect("constraints");
        assert_eq!(constraints.max_concurrency, Some(2));
        assert_eq!(constraints.cost_tier.as_deref(), Some("premium"));
        assert_eq!(
            constraints.allowed_task_kinds.as_ref().map(|v| v.as_slice()),
            Some(["research".to_string()].as_slice())
        );
    }

    #[test]
    fn update_fleet_request_distinguishes_clear_from_absent() {
        // Explicit null => clear (present-as-null): Some(None).
        let clear: UpdateFleetRequest =
            serde_json::from_str(r#"{"description": null}"#).expect("deserialize clear");
        assert_eq!(clear.description, Some(None), "explicit null must be Some(None)");
        // Other fields absent => None (keep).
        assert_eq!(clear.name, None);
        assert_eq!(clear.max_parallel, None);
        assert!(clear.members.is_none());

        // Key absent => keep current: None.
        let keep: UpdateFleetRequest = serde_json::from_str(r#"{}"#).expect("deserialize keep");
        assert_eq!(keep.description, None, "absent key must be None");

        // Explicit value => set: Some(Some(v)).
        let set: UpdateFleetRequest =
            serde_json::from_str(r#"{"description": "new"}"#).expect("deserialize set");
        assert_eq!(set.description, Some(Some("new".to_string())));

        // max_parallel patch semantics.
        let clear_mp: UpdateFleetRequest =
            serde_json::from_str(r#"{"max_parallel": null}"#).expect("deserialize clear mp");
        assert_eq!(clear_mp.max_parallel, Some(None));
        let set_mp: UpdateFleetRequest =
            serde_json::from_str(r#"{"max_parallel": 5}"#).expect("deserialize set mp");
        assert_eq!(set_mp.max_parallel, Some(Some(5)));
    }

    #[test]
    fn update_workspace_request_clears_default_fleet() {
        let clear: UpdateWorkspaceRequest =
            serde_json::from_str(r#"{"default_fleet_id": null}"#).expect("deserialize clear");
        assert_eq!(clear.default_fleet_id, Some(None));

        let keep: UpdateWorkspaceRequest =
            serde_json::from_str(r#"{}"#).expect("deserialize keep");
        assert_eq!(keep.default_fleet_id, None);
    }
}
