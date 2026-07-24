use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Request body for detecting an ACP CLI executable.
///
/// `backend` is a vendor label (e.g. "claude"). The service resolves it
/// against the `agent_metadata` catalog.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectCliRequest {
    pub backend: String,
}

/// Response for CLI detection.
#[derive(Debug, Serialize)]
pub struct DetectCliResponse {
    /// Path to the detected CLI, `None` if not found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Request body for ACP health check.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpHealthCheckRequest {
    pub backend: String,
}

/// Response for ACP health check.
#[derive(Debug, Serialize)]
pub struct AcpHealthCheckResponse {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response for agent session mode.
#[derive(Debug, Serialize)]
pub struct AgentModeResponse {
    pub mode: String,
    pub initialized: bool,
}

/// Request body for setting session mode.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetModeRequest {
    pub mode: String,
}

/// Request body for setting ACP session model.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetModelRequest {
    pub model_id: String,
}

/// A single available model entry in the frontend-facing model info response.
#[derive(Debug, Clone, Serialize)]
pub struct ModelInfoEntry {
    pub id: String,
    pub label: String,
}

/// Frontend-compatible model info response.
///
/// Maps from the SDK's camelCase `SessionModelState` to the snake_case
/// `AcpModelInfo` format the renderer expects.
#[derive(Debug, Serialize)]
pub struct GetModelInfoResponse {
    pub model_info: Option<ModelInfoPayload>,
}

/// Inner model info payload matching the frontend's `AcpModelInfo` type.
#[derive(Debug, Clone, Serialize)]
pub struct ModelInfoPayload {
    pub current_model_id: Option<String>,
    pub current_model_label: Option<String>,
    pub available_models: Vec<ModelInfoEntry>,
}

/// Request body for probing model information.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeModelRequest {
    pub backend: String,
}

/// Request body for probing a custom ACP agent.
///
/// Two-step check: Step 1 resolves `command` on `$PATH`; Step 2 spawns
/// the CLI and performs an ACP `initialize` handshake. The same
/// function is called from the dedicated endpoint (manual test button)
/// and from the create/update path (test-on-save).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TryConnectCustomAgentRequest {
    pub command: String,
    #[serde(default)]
    pub acp_args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Outcome of [`TryConnectCustomAgentRequest`].
///
/// Tagged enum: `step` distinguishes the three states the frontend's
/// Alert component renders (success → green, fail_cli → red,
/// fail_acp → yellow). `error` carries a human-readable reason for the
/// two failure variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum TryConnectCustomAgentResponse {
    Success,
    FailCli { error: String },
    FailAcp { error: String },
}

/// Query parameters for workspace browse.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceBrowseQuery {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
}

/// A file or directory entry in the workspace browse response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub entry_type: String,
}

/// Request body for side question.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SideQuestionRequest {
    pub question: String,
}

/// Response for side question.
#[derive(Debug, Serialize)]
pub struct SideQuestionResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detect_cli_request_serde() {
        let json = json!({ "backend": "claude" });
        let req: DetectCliRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.backend, "claude");
    }

    #[test]
    fn detect_cli_response_with_path() {
        let resp = DetectCliResponse {
            path: Some("/usr/local/bin/claude".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["path"], "/usr/local/bin/claude");
    }

    #[test]
    fn detect_cli_response_without_path() {
        let resp = DetectCliResponse { path: None };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("path").is_none());
    }

    #[test]
    fn health_check_response_available() {
        let resp = AcpHealthCheckResponse {
            available: true,
            latency: Some(120),
            error: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["available"], true);
        assert_eq!(json["latency"], 120);
        assert!(json.get("error").is_none());
    }

    #[test]
    fn health_check_response_unavailable() {
        let resp = AcpHealthCheckResponse {
            available: false,
            latency: None,
            error: Some("CLI not found".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["available"], false);
        assert_eq!(json["error"], "CLI not found");
    }

    #[test]
    fn set_mode_request_serde() {
        let json = json!({ "mode": "code" });
        let req: SetModeRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.mode, "code");
    }

    #[test]
    fn set_model_request_serde() {
        let json = json!({ "model_id": "claude-sonnet-4" });
        let req: SetModelRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.model_id, "claude-sonnet-4");
    }

    #[test]
    fn try_connect_custom_agent_request_serde() {
        let json = json!({
            "command": "/path/to/agent",
            "acp_args": ["--flag"],
            "env": { "KEY": "value" }
        });
        let req: TryConnectCustomAgentRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.command, "/path/to/agent");
        assert_eq!(req.acp_args, vec!["--flag"]);
        assert_eq!(req.env.get("KEY"), Some(&"value".into()));
    }

    #[test]
    fn try_connect_custom_agent_request_defaults() {
        let json = json!({ "command": "/bin/test" });
        let req: TryConnectCustomAgentRequest = serde_json::from_value(json).unwrap();
        assert!(req.acp_args.is_empty());
        assert!(req.env.is_empty());
    }

    #[test]
    fn try_connect_response_tag_serializes() {
        use super::TryConnectCustomAgentResponse;
        let ok = TryConnectCustomAgentResponse::Success;
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            serde_json::json!({"step":"success"})
        );

        let fail = TryConnectCustomAgentResponse::FailCli {
            error: "not found".into(),
        };
        assert_eq!(
            serde_json::to_value(&fail).unwrap(),
            serde_json::json!({"step":"fail_cli","error":"not found"})
        );
    }

    #[test]
    fn probe_model_request_serde() {
        let json = json!({ "backend": "claude" });
        let req: ProbeModelRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.backend, "claude");
    }
}
