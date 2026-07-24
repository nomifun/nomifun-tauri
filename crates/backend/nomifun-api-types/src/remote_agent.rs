use nomifun_common::{RemoteAgentAuthType, RemoteAgentId, RemoteAgentProtocol, RemoteAgentStatus, TimestampMs};
use serde::{Deserialize, Serialize};

/// Request body for creating a remote agent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRemoteAgentRequest {
    pub name: String,
    pub protocol: RemoteAgentProtocol,
    pub url: String,
    pub auth_type: RemoteAgentAuthType,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub allow_insecure: bool,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Request body for updating a remote agent (partial update).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRemoteAgentRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub protocol: Option<RemoteAgentProtocol>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub auth_type: Option<RemoteAgentAuthType>,
    /// `Some(Some("token"))` → set, `Some(None)` → clear, `None` → keep.
    /// serde maps `"authToken": null` to `Some(None)` and absent field to `None`.
    #[serde(default, deserialize_with = "deserialize_optional_nullable")]
    pub auth_token: Option<Option<String>>,
    #[serde(default)]
    pub allow_insecure: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_nullable")]
    pub avatar: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_nullable")]
    pub description: Option<Option<String>>,
}

/// Remote agent response for list endpoint (auth_token omitted).
#[derive(Debug, Serialize)]
pub struct RemoteAgentListItem {
    pub remote_agent_id: RemoteAgentId,
    pub name: String,
    pub protocol: RemoteAgentProtocol,
    pub url: String,
    pub auth_type: RemoteAgentAuthType,
    pub allow_insecure: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: RemoteAgentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_connected_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Remote agent response for detail endpoint (auth_token masked, device keys visible).
#[derive(Debug, Serialize)]
pub struct RemoteAgentResponse {
    pub remote_agent_id: RemoteAgentId,
    pub name: String,
    pub protocol: RemoteAgentProtocol,
    pub url: String,
    pub auth_type: RemoteAgentAuthType,
    /// Masked token: `***xxxx` (last 4 chars visible).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    pub allow_insecure: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_public_key: Option<String>,
    pub status: RemoteAgentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_connected_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Request body for testing a remote agent WebSocket connection.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestRemoteAgentConnectionRequest {
    pub url: String,
    #[serde(default)]
    pub auth_type: Option<RemoteAgentAuthType>,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub allow_insecure: bool,
}

/// Response for OpenClaw handshake.
#[derive(Debug, Serialize)]
pub struct HandshakeResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Deserialize `Option<Option<T>>`:
/// - JSON field absent → `None` (keep current value)
/// - JSON `null` → `Some(None)` (clear the value)
/// - JSON value → `Some(Some(value))` (set new value)
fn deserialize_optional_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    // When serde reaches this function, the field is present in JSON.
    // Deserialize the value: null → None, value → Some(value).
    let value: Option<T> = Option::deserialize(deserializer)?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::{HandshakeResponse, RemoteAgentListItem, RemoteAgentResponse};
    use nomifun_common::{RemoteAgentAuthType, RemoteAgentProtocol, RemoteAgentStatus};

    #[test]
    fn handshake_response_omits_empty_error() {
        let value = serde_json::to_value(HandshakeResponse {
            status: "ok".into(),
            error: None,
        })
        .unwrap();

        assert_eq!(value, serde_json::json!({ "status": "ok" }));
    }

    #[test]
    fn handshake_response_includes_pairing_error() {
        let value = serde_json::to_value(HandshakeResponse {
            status: "pending_approval".into(),
            error: Some("PAIRING_REQUIRED".into()),
        })
        .unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "status": "pending_approval",
                "error": "PAIRING_REQUIRED"
            })
        );
    }

    #[test]
    fn remote_agent_list_item_serializes_only_named_wire_id() {
        let remote_agent_id = nomifun_common::RemoteAgentId::new();
        let value = serde_json::to_value(RemoteAgentListItem {
            remote_agent_id: remote_agent_id.clone(),
            name: "remote".into(),
            protocol: RemoteAgentProtocol::OpenClaw,
            url: "wss://example.invalid".into(),
            auth_type: RemoteAgentAuthType::None,
            allow_insecure: false,
            avatar: None,
            description: None,
            status: RemoteAgentStatus::Unknown,
            last_connected_at: None,
            created_at: 1,
            updated_at: 2,
        })
        .unwrap();
        assert_eq!(value["remote_agent_id"], remote_agent_id.as_str());
        assert!(value.get("id").is_none());
    }

    #[test]
    fn remote_agent_response_serializes_only_named_wire_id() {
        let remote_agent_id = nomifun_common::RemoteAgentId::new();
        let value = serde_json::to_value(RemoteAgentResponse {
            remote_agent_id: remote_agent_id.clone(),
            name: "remote".into(),
            protocol: RemoteAgentProtocol::OpenClaw,
            url: "wss://example.invalid".into(),
            auth_type: RemoteAgentAuthType::None,
            auth_token: None,
            allow_insecure: false,
            avatar: None,
            description: None,
            device_id: None,
            device_public_key: None,
            status: RemoteAgentStatus::Unknown,
            last_connected_at: None,
            created_at: 1,
            updated_at: 2,
        })
        .unwrap();

        assert_eq!(value["remote_agent_id"], remote_agent_id.as_str());
        assert!(value.get("id").is_none());
    }
}
