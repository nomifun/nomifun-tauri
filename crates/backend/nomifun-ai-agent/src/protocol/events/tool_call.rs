use std::collections::HashSet;

use agent_client_protocol::schema::Meta as SdkMeta;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::artifact_store::PersistedArtifact;

/// Enforce the shared tool-name/arguments artifact contract at the normalized
/// runtime boundary. This is intentionally backend-agnostic: external runtimes
/// (OpenClaw, Remote, Nanobot) must not bypass the same minimum-count and MIME
/// rules merely because they did not run through `BackendOutputSink`.
pub fn validate_completed_artifact_contract(data: &ToolCallEventData) -> Result<(), String> {
    if data.status != ToolCallStatus::Completed {
        return Ok(());
    }
    validate_artifact_receipt_integrity(&data.name, &data.artifacts)?;
    let contract = nomi_agent::output::artifact_contract_with_input(&data.name, &data.args)
        .map_err(|error| format!("invalid artifact contract for tool '{}': {error}", data.name))?;
    let Some(contract) = contract else {
        return Ok(());
    };
    let mime_types = data
        .artifacts
        .iter()
        .map(|artifact| artifact.mime_type.as_str())
        .collect::<Vec<_>>();
    contract.validate_mimes(&mime_types).map_err(|error| {
        format!(
            "tool '{}' did not deliver its required verified artifacts: {error}",
            data.name
        )
    })
}

/// Validate identity and locator uniqueness independently of tool identity.
/// ACP updates may omit a title/raw tool name, but their untrusted receipt
/// batches must still satisfy the same UI-key and file-locator invariants.
pub fn validate_artifact_receipt_integrity(
    tool_name: &str,
    artifacts: &[PersistedArtifact],
) -> Result<(), String> {
    let mut artifact_ids = HashSet::with_capacity(artifacts.len());
    let mut canonical_paths = HashSet::with_capacity(artifacts.len());
    let mut relative_paths = HashSet::with_capacity(artifacts.len());
    for artifact in artifacts {
        if let Err(error) = nomifun_common::PersistedArtifactId::parse(&artifact.id) {
            return Err(format!(
                "tool '{}' reported a non-canonical artifact id '{}': {error}",
                tool_name, artifact.id
            ));
        }
        if !artifact_ids.insert(artifact.id.as_str()) {
            return Err(format!(
                "tool '{}' reported the same artifact id more than once: {}",
                tool_name, artifact.id
            ));
        }
        if !canonical_paths.insert(artifact.path.as_str()) {
            return Err(format!(
                "tool '{}' reported the same canonical artifact path more than once: {}",
                tool_name, artifact.path
            ));
        }
        if !relative_paths.insert(artifact.relative_path.as_str()) {
            return Err(format!(
                "tool '{}' reported the same workspace-relative artifact path more than once: {}",
                tool_name, artifact.relative_path
            ));
        }
    }
    Ok(())
}

/// Data for the `ToolCall` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallEventData {
    pub call_id: String,
    pub name: String,
    #[serde(default)]
    pub args: serde_json::Value,
    pub status: ToolCallStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Verified user-visible outputs. Inline base64 is never placed on the
    /// event bus or in conversation history; only durable metadata is stored.
    // Keep an explicit empty array on Running/Error correction frames. Live
    // consumers merge lifecycle updates by call_id; omitting this field could
    // otherwise leave an earlier completed receipt visible after failure.
    #[serde(default)]
    pub artifacts: Vec<PersistedArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpToolCallEventData {
    pub session_id: String,
    pub update: AcpToolCallUpdateData,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<SdkMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpToolCallUpdateData {
    #[serde(rename = "sessionUpdate")]
    pub session_update: AcpToolCallSessionUpdateKind,
    pub tool_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<AcpToolCallStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<AcpToolCallKind>,
    #[serde(rename = "rawInput", skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(rename = "rawOutput", skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<AcpToolCallContentItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<AcpToolCallLocationItem>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpToolCallSessionUpdateKind {
    ToolCall,
    ToolCallUpdate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpToolCallKind {
    Read,
    Edit,
    Execute,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AcpToolCallContentItem {
    Content {
        content: AcpToolCallTextBlock,
    },
    Diff {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
        new_text: String,
    },
    /// Inline ACP media/resource bytes after verified workspace persistence.
    Artifact {
        artifact: PersistedArtifact,
        #[serde(skip_serializing_if = "Option::is_none")]
        source_uri: Option<String>,
    },
    /// A provider-owned resource that is already addressable by URI. The URI
    /// is preserved instead of being silently discarded.
    ResourceLink {
        name: String,
        uri: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        size_bytes: Option<i64>,
    },
    Terminal {
        terminal_id: String,
    },
    /// Explicit delivery failure retained in the receipt. When this variant is
    /// emitted, the enclosing ACP tool status is forced to `failed`.
    ArtifactError {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpToolCallTextBlock {
    #[serde(rename = "type")]
    pub block_type: AcpToolCallTextBlockType,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpToolCallTextBlockType {
    Text,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpToolCallLocationItem {
    pub path: String,
}

/// Status of a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Running,
    Completed,
    Error,
}

/// A single entry in a `ToolGroup` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolGroupEntry {
    pub call_id: String,
    pub name: String,
    pub status: ToolCallStatus,
    #[serde(default)]
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_store::ArtifactKind;
    use nomifun_common::PersistedArtifactId;
    use serde_json::json;

    fn image(path: &str, relative_path: &str) -> PersistedArtifact {
        PersistedArtifact {
            id: PersistedArtifactId::new().into_string(),
            kind: ArtifactKind::Image,
            mime_type: "image/png".to_owned(),
            path: path.to_owned(),
            relative_path: relative_path.to_owned(),
            size_bytes: 1,
            sha256: "00".repeat(32),
        }
    }

    fn completed_images(artifacts: Vec<PersistedArtifact>) -> ToolCallEventData {
        ToolCallEventData {
            call_id: "call-images".to_owned(),
            name: "image_gen".to_owned(),
            args: json!({"count": 2}),
            status: ToolCallStatus::Completed,
            input: None,
            output: None,
            description: None,
            artifacts,
        }
    }

    #[test]
    fn duplicate_canonical_path_cannot_satisfy_requested_count() {
        let result = validate_completed_artifact_contract(&completed_images(vec![
            image("/workspace/a.png", "nomifun-artifacts/a.png"),
            image("/workspace/a.png", "nomifun-artifacts/alias.png"),
        ]));

        assert!(
            result
                .unwrap_err()
                .contains("same canonical artifact path more than once")
        );
    }

    #[test]
    fn empty_artifact_id_cannot_satisfy_requested_count() {
        let mut first = image("/workspace/a.png", "nomifun-artifacts/a.png");
        first.id = "   ".to_owned();
        let result = validate_completed_artifact_contract(&completed_images(vec![
            first,
            image("/workspace/b.png", "nomifun-artifacts/b.png"),
        ]));

        assert!(result.unwrap_err().contains("non-canonical artifact id"));
    }

    #[test]
    fn duplicate_artifact_id_cannot_satisfy_requested_count() {
        let first = image("/workspace/a.png", "nomifun-artifacts/a.png");
        let mut second = image("/workspace/b.png", "nomifun-artifacts/b.png");
        second.id = first.id.clone();
        let result = validate_completed_artifact_contract(&completed_images(vec![first, second]));

        assert!(
            result
                .unwrap_err()
                .contains("same artifact id more than once")
        );
    }

    #[test]
    fn duplicate_relative_path_cannot_satisfy_requested_count() {
        let mut second = image("/workspace/b.png", "nomifun-artifacts/a.png");
        second.id = PersistedArtifactId::new().into_string();
        let result = validate_completed_artifact_contract(&completed_images(vec![
            image("/workspace/a.png", "nomifun-artifacts/a.png"),
            second,
        ]));

        assert!(
            result
                .unwrap_err()
                .contains("same workspace-relative artifact path more than once")
        );
    }

    #[test]
    fn distinct_receipts_satisfy_requested_count() {
        validate_completed_artifact_contract(&completed_images(vec![
            image("/workspace/a.png", "nomifun-artifacts/a.png"),
            image("/workspace/b.png", "nomifun-artifacts/b.png"),
        ]))
        .unwrap();
    }
}
