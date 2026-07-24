//! Request/response types for Custom Agent CRUD endpoints.
//!
//! Custom agents are user-defined rows in the `agent_metadata` table.
//! They share the same storage and spawn path as builtin agents, but are
//! owned/edited via `/api/agents/custom/*` endpoints exposed to the
//! settings UI (F-CAGENT-04 / -05 / -12 / -13 / -14 in the frontend
//! PRD).

use serde::{Deserialize, Serialize};

use crate::agent_discovery::{AgentEnvEntry, BehaviorPolicy};

/// Payload shared by `POST /api/agents/custom` and
/// `PUT  /api/agents/custom/{id}`.
///
/// Field coverage matches the frontend editor (F-CAGENT-07/-08/-09/-10):
/// name/command required; icon/args/env optional; `advanced` carries the
/// subset of `AgentMetadata` columns exposed via the JSON advanced panel.
/// Unknown keys at either level are rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomAgentUpsertRequest {
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<AgentEnvEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advanced: Option<CustomAgentAdvancedOverrides>,
}

/// Optional overrides exposed through the JSON advanced editor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomAgentAdvancedOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_skills_dirs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior_policy: Option<BehaviorPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Request body for `PATCH /api/agents/{id}/enabled`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetEnabledRequest {
    pub enabled: bool,
}

/// Response body for `DELETE /api/agents/custom/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteCustomAgentResponse {
    pub deleted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn advanced_rejects_unknown_keys() {
        let payload = json!({
            "yolo_id": "bypassPermissions",
            "unknown_field": 42,
            "another": "ignored"
        });
        assert!(serde_json::from_value::<CustomAgentAdvancedOverrides>(payload).is_err());
    }

    #[test]
    fn upsert_request_minimal_payload() {
        let payload = json!({
            "name": "My Agent",
            "command": "my-cli"
        });
        let req: CustomAgentUpsertRequest = serde_json::from_value(payload).unwrap();
        assert_eq!(req.name, "My Agent");
        assert_eq!(req.command, "my-cli");
        assert!(req.args.is_empty());
        assert!(req.env.is_empty());
        assert!(req.advanced.is_none());
    }
}
