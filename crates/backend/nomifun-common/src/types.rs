use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// EnvVar / CommandSpec
// ---------------------------------------------------------------------------

/// A name=value environment variable pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// A command with its arguments and environment variables.
///
/// This is the common building block shared by CLI agent spawning,
/// MCP server transports, and agent discovery types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandSpec {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<EnvVar>,
    pub cwd: Option<String>,
}

// ---------------------------------------------------------------------------
// ProviderWithModel
// ---------------------------------------------------------------------------

/// Model selection config — references a provider and a specific model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderWithModel {
    pub provider_id: String,
    pub model: String,
    pub use_model: Option<String>,
}

/// A pending tool-call confirmation item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Confirmation {
    pub id: String,
    pub call_id: String,
    pub title: Option<String>,
    pub action: Option<String>,
    pub description: String,
    pub command_type: Option<String>,
    pub options: Vec<ConfirmationOption>,
    /// Optional inline preview image (a `data:image/png;base64,...` URL). Used by the
    /// browser takeover approval so a silent (headless) session can still show the user
    /// the current page they are approving an irreversible action on. `None` for all
    /// non-browser confirmations. `#[serde(default)]` keeps older payloads deserializable
    /// and omits the field from the wire when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
}

/// A single option within a confirmation dialog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmationOption {
    pub label: String,
    pub value: serde_json::Value,
    pub params: Option<HashMap<String, String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_with_model_serde() {
        let p = ProviderWithModel {
            provider_id: "openai-1".into(),
            model: "gpt-4".into(),
            use_model: Some("gpt-4-turbo".into()),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["provider_id"], "openai-1");
        assert_eq!(json["model"], "gpt-4");
        assert_eq!(json["use_model"], "gpt-4-turbo");
    }

    #[test]
    fn test_confirmation_serde() {
        let c = Confirmation {
            id: "c1".into(),
            call_id: "call1".into(),
            title: Some("Run command?".into()),
            action: None,
            description: "Execute shell command".into(),
            command_type: Some("bash".into()),
            options: vec![ConfirmationOption {
                label: "Allow".into(),
                value: serde_json::json!(true),
                params: None,
            }],
            screenshot: None,
        };
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["call_id"], "call1");
        assert_eq!(json["command_type"], "bash");
    }
}
