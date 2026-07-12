use std::collections::HashMap;

use nomifun_common::{McpServerStatus, McpSource, TimestampMs};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// A. Transport types
// ---------------------------------------------------------------------------

/// MCP server transport configuration (tagged union).
///
/// `http` represents Streamable HTTP (the MCP standard); `sse` is legacy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpTransport {
    #[serde(rename = "stdio")]
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        env: HashMap<String, String>,
    },
    #[serde(rename = "sse")]
    Sse {
        url: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        headers: HashMap<String, String>,
    },
    #[serde(rename = "http")]
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        headers: HashMap<String, String>,
    },
}

// ---------------------------------------------------------------------------
// B. Tool description
// ---------------------------------------------------------------------------

/// MCP tool description returned from connection tests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolResponse {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// C. Server CRUD — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/mcp/servers` — create (or upsert by name).
#[derive(Debug, Deserialize)]
pub struct CreateMcpServerRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub transport: McpTransport,
    #[serde(default)]
    pub original_json: Option<String>,
    #[serde(default)]
    pub builtin: bool,
}

/// Request item for `POST /api/mcp/servers/import`.
///
/// Import can preserve the enabled state from a legacy source. Plain create
/// intentionally does not accept `enabled`; the UI persists the default
/// enabled flag in a follow-up toggle request.
#[derive(Debug, Deserialize)]
pub struct ImportMcpServerRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub transport: McpTransport,
    #[serde(default)]
    pub original_json: Option<String>,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Request body for `PUT /api/mcp/servers/:id` — partial update.
#[derive(Debug, Deserialize)]
pub struct UpdateMcpServerRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_nullable")]
    pub description: Option<Option<String>>,
    #[serde(default)]
    pub transport: Option<McpTransport>,
    #[serde(default, deserialize_with = "deserialize_optional_nullable")]
    pub original_json: Option<Option<String>>,
    #[serde(default)]
    pub builtin: Option<bool>,
}

/// Request body for `POST /api/mcp/servers/import` — batch import.
#[derive(Debug, Deserialize)]
pub struct BatchImportMcpServersRequest {
    pub servers: Vec<ImportMcpServerRequest>,
}

// ---------------------------------------------------------------------------
// D. Server CRUD — Response DTOs
// ---------------------------------------------------------------------------

/// Full MCP server configuration response.
///
/// `id` is the host-local INTEGER primary key (number on the wire, matching
/// the remote/webhook DTOs). Detected (non-DB) servers returned by the
/// agent-config scan carry the sentinel `0` — see [`DetectedMcpServerEntry`].
#[derive(Debug, Clone, Serialize)]
pub struct McpServerResponse {
    pub id: i64,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
    pub transport: McpTransport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<McpToolResponse>>,
    pub last_test_status: McpServerStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_connected: Option<TimestampMs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_json: Option<String>,
    pub builtin: bool,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Detected MCP server entry for one-click import.
///
/// The flattened `server` is *not* a persisted DB entity: its `id` is the
/// sentinel `0` (detected servers have no host-local primary key). Clients
/// identify and import these by `name`/`transport`, never by `id`.
#[derive(Debug, Clone, Serialize)]
pub struct DetectedMcpServerEntry {
    #[serde(flatten)]
    pub server: McpServerResponse,
    pub importable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_skip_reason: Option<String>,
}

/// Detected MCP servers for a single agent.
#[derive(Debug, Clone, Serialize)]
pub struct DetectedMcpServerResponse {
    pub source: McpSource,
    pub servers: Vec<DetectedMcpServerEntry>,
}

// ---------------------------------------------------------------------------
// E. Connection test
// ---------------------------------------------------------------------------

/// Request body for `POST /api/mcp/test-connection`.
#[derive(Debug, Deserialize)]
pub struct TestMcpConnectionRequest {
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_i64_from_string_or_integer"
    )]
    pub id: Option<i64>,
    pub name: String,
    pub transport: McpTransport,
}

/// Authentication method detected during connection test.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpAuthMethod {
    Oauth,
    Basic,
}

/// Machine-readable error code for MCP connection test failures.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum McpConnectionTestErrorCode {
    CommandNotFound,
    CommandPermissionDenied,
    CommandStartFailed,
    ConnectionFailed,
    HttpError,
    Timeout,
    RpcError,
    ProtocolError,
}

impl McpConnectionTestErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CommandNotFound => "MCP_COMMAND_NOT_FOUND",
            Self::CommandPermissionDenied => "MCP_COMMAND_PERMISSION_DENIED",
            Self::CommandStartFailed => "MCP_COMMAND_START_FAILED",
            Self::ConnectionFailed => "MCP_CONNECTION_FAILED",
            Self::HttpError => "MCP_HTTP_ERROR",
            Self::Timeout => "MCP_TIMEOUT",
            Self::RpcError => "MCP_RPC_ERROR",
            Self::ProtocolError => "MCP_PROTOCOL_ERROR",
        }
    }
}

/// Result of an MCP server connection test.
#[derive(Debug, Clone, Serialize)]
pub struct McpConnectionTestResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<McpToolResponse>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<McpConnectionTestErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_auth: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<McpAuthMethod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub www_authenticate: Option<String>,
}

// ---------------------------------------------------------------------------
// G. OAuth
// ---------------------------------------------------------------------------

/// Request body for `POST /api/mcp/oauth/check-status`.
#[derive(Debug, Deserialize)]
pub struct OAuthCheckStatusRequest {
    pub server_url: String,
}

/// Response for OAuth status check.
#[derive(Debug, Serialize)]
pub struct OAuthStatusResponse {
    pub authenticated: bool,
}

/// Request body for `POST /api/mcp/oauth/login`.
#[derive(Debug, Deserialize)]
pub struct OAuthLoginRequest {
    pub server_url: String,
}

/// Response for OAuth login initiation.
#[derive(Debug, Serialize)]
pub struct OAuthLoginResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Request body for `POST /api/mcp/oauth/logout`.
#[derive(Debug, Deserialize)]
pub struct OAuthLogoutRequest {
    pub server_url: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Deserialize `Option<Option<T>>`:
/// - JSON field absent → `None` (keep current value)
/// - JSON `null` → `Some(None)` (clear the value)
/// - JSON value → `Some(Some(value))` (set new value)
fn deserialize_optional_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    let value: Option<T> = Option::deserialize(deserializer)?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- McpTransport serde --------------------------------------------------

    #[test]
    fn test_stdio_transport_serde() {
        let t = McpTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "test-server".into()],
            env: HashMap::from([("KEY".into(), "value".into())]),
        };
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["type"], "stdio");
        assert_eq!(json["command"], "npx");
        assert_eq!(json["args"], serde_json::json!(["-y", "test-server"]));
        assert_eq!(json["env"]["KEY"], "value");

        let parsed: McpTransport = serde_json::from_value(json).unwrap();
        match parsed {
            McpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["-y", "test-server"]);
                assert_eq!(env.get("KEY").unwrap(), "value");
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn test_http_transport_serde() {
        let t = McpTransport::Http {
            url: "https://example.com/mcp".into(),
            headers: HashMap::new(),
        };
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["type"], "http");
        assert_eq!(json["url"], "https://example.com/mcp");
        assert!(json.get("headers").is_none()); // empty map skipped
    }

    #[test]
    fn test_sse_transport_with_headers() {
        let t = McpTransport::Sse {
            url: "https://example.com/sse".into(),
            headers: HashMap::from([("Authorization".into(), "Bearer xxx".into())]),
        };
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["type"], "sse");
        assert_eq!(json["headers"]["Authorization"], "Bearer xxx");
    }

    #[test]
    fn test_stdio_transport_minimal() {
        let json = serde_json::json!({
            "type": "stdio",
            "command": "node"
        });
        let t: McpTransport = serde_json::from_value(json).unwrap();
        match t {
            McpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "node");
                assert!(args.is_empty());
                assert!(env.is_empty());
            }
            _ => panic!("expected Stdio"),
        }
    }

    // -- CreateMcpServerRequest -----------------------------------------------

    #[test]
    fn test_create_request_deserialization() {
        let json = serde_json::json!({
            "name": "test-mcp",
            "description": "A test server",
            "transport": {
                "type": "stdio",
                "command": "npx",
                "args": ["-y", "@test/server"]
            }
        });
        let req: CreateMcpServerRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.name, "test-mcp");
        assert_eq!(req.description.as_deref(), Some("A test server"));
        assert!(!req.builtin);
    }

    #[test]
    fn test_create_request_missing_name() {
        let json = serde_json::json!({
            "transport": { "type": "stdio", "command": "node" }
        });
        let result = serde_json::from_value::<CreateMcpServerRequest>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_create_request_missing_transport() {
        let json = serde_json::json!({ "name": "test" });
        let result = serde_json::from_value::<CreateMcpServerRequest>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_create_request_invalid_transport_type() {
        let json = serde_json::json!({
            "name": "test",
            "transport": { "type": "invalid", "command": "x" }
        });
        let result = serde_json::from_value::<CreateMcpServerRequest>(json);
        assert!(result.is_err());
    }

    // -- UpdateMcpServerRequest -----------------------------------------------

    #[test]
    fn test_update_request_partial() {
        let json = serde_json::json!({ "name": "new-name" });
        let req: UpdateMcpServerRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.name.as_deref(), Some("new-name"));
        assert!(req.description.is_none()); // absent → keep
        assert!(req.transport.is_none());
    }

    #[test]
    fn test_update_request_null_description() {
        let json = serde_json::json!({ "description": null });
        let req: UpdateMcpServerRequest = serde_json::from_value(json).unwrap();
        // null → Some(None) → clear
        assert_eq!(req.description, Some(None));
    }

    #[test]
    fn test_update_request_set_description() {
        let json = serde_json::json!({ "description": "new desc" });
        let req: UpdateMcpServerRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.description, Some(Some("new desc".into())));
    }

    // -- McpServerResponse ----------------------------------------------------

    #[test]
    fn test_server_response_serialization() {
        let resp = McpServerResponse {
            id: 123,
            name: "test".into(),
            description: None,
            enabled: true,
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec![],
                env: HashMap::new(),
            },
            tools: None,
            last_test_status: McpServerStatus::Disconnected,
            last_connected: None,
            original_json: None,
            builtin: false,
            created_at: 1000,
            updated_at: 2000,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["id"], 123);
        assert_eq!(json["enabled"], true);
        assert_eq!(json["last_test_status"], "disconnected");
        assert!(json.get("description").is_none()); // None skipped
        assert!(json.get("tools").is_none());
    }

    // -- BatchImportMcpServersRequest -----------------------------------------

    #[test]
    fn test_batch_import_request() {
        let json = serde_json::json!({
            "servers": [
                { "name": "a", "transport": { "type": "stdio", "command": "a" } },
                { "name": "b", "transport": { "type": "http", "url": "http://b" } }
            ]
        });
        let req: BatchImportMcpServersRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.servers.len(), 2);
    }

    #[test]
    fn test_batch_import_empty() {
        let json = serde_json::json!({ "servers": [] });
        let req: BatchImportMcpServersRequest = serde_json::from_value(json).unwrap();
        assert!(req.servers.is_empty());
    }

    // -- McpConnectionTestResult ----------------------------------------------

    #[test]
    fn test_connection_test_success() {
        let result = McpConnectionTestResult {
            success: true,
            tools: Some(vec![McpToolResponse {
                name: "read_file".into(),
                description: Some("Read a file".into()),
                input_schema: None,
            }]),
            error: None,
            code: None,
            details: None,
            needs_auth: None,
            auth_method: None,
            www_authenticate: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["tools"][0]["name"], "read_file");
        assert!(json.get("error").is_none());
        assert!(json.get("needs_auth").is_none());
    }

    #[test]
    fn test_connection_test_needs_auth() {
        let result = McpConnectionTestResult {
            success: false,
            tools: None,
            error: None,
            code: None,
            details: None,
            needs_auth: Some(true),
            auth_method: Some(McpAuthMethod::Oauth),
            www_authenticate: Some("Bearer realm=\"mcp\"".into()),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["needs_auth"], true);
        assert_eq!(json["auth_method"], "oauth");
        assert_eq!(json["www_authenticate"], "Bearer realm=\"mcp\"");
    }

    #[test]
    fn test_connection_test_error_code_serialization() {
        let result = McpConnectionTestResult {
            success: false,
            tools: None,
            error: Some("Command not found: npx".into()),
            code: Some(McpConnectionTestErrorCode::CommandNotFound),
            details: Some(serde_json::json!({ "command": "npx", "runtime": "node" })),
            needs_auth: None,
            auth_method: None,
            www_authenticate: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["code"], "COMMAND_NOT_FOUND");
        assert_eq!(json["details"]["command"], "npx");
        assert_eq!(
            McpConnectionTestErrorCode::CommandNotFound.as_str(),
            "MCP_COMMAND_NOT_FOUND"
        );
    }

    // -- TestMcpConnectionRequest ---------------------------------------------

    #[test]
    fn test_connection_request_accepts_numeric_id() {
        let body = r#"{"id":1,"name":"test-server","transport":{"type":"http","url":"https://example.com/mcp"}}"#;
        let req: TestMcpConnectionRequest =
            serde_json::from_str(body).expect("numeric MCP catalog id must deserialize");
        assert_eq!(req.id, Some(1));
        assert_eq!(req.name, "test-server");
    }

    #[test]
    fn test_connection_request_accepts_legacy_decimal_string_id() {
        let json = serde_json::json!({
            "id": "123",
            "name": "test-server",
            "transport": { "type": "http", "url": "https://example.com/mcp" }
        });
        let req: TestMcpConnectionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.id, Some(123));
    }

    #[test]
    fn test_connection_request_allows_missing_or_null_id() {
        for id_fragment in ["", r#""id":null,"#] {
            let body = format!(
                r#"{{{id_fragment}"name":"test-server","transport":{{"type":"http","url":"https://example.com/mcp"}}}}"#
            );
            let req: TestMcpConnectionRequest = serde_json::from_str(&body).unwrap();
            assert!(req.id.is_none());
        }
    }

    #[test]
    fn test_connection_request_rejects_invalid_id_shapes() {
        for invalid_id in [
            serde_json::json!("mcp_123"),
            serde_json::json!(1.5),
            serde_json::json!(true),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!(u64::MAX),
        ] {
            let json = serde_json::json!({
                "id": invalid_id,
                "name": "test-server",
                "transport": { "type": "http", "url": "https://example.com/mcp" }
            });
            assert!(serde_json::from_value::<TestMcpConnectionRequest>(json).is_err());
        }
    }

    // -- OAuth DTOs -----------------------------------------------------------

    #[test]
    fn test_oauth_status_response() {
        let resp = OAuthStatusResponse { authenticated: true };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["authenticated"], true);
    }

    #[test]
    fn test_oauth_login_response() {
        let resp = OAuthLoginResponse {
            success: false,
            error: Some("discovery failed".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["error"], "discovery failed");
    }

    // -- DetectedMcpServerResponse --------------------------------------------

    #[test]
    fn test_detected_server_response() {
        let resp = DetectedMcpServerResponse {
            source: McpSource::Claude,
            servers: vec![],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["source"], "claude");
        assert_eq!(json["servers"], serde_json::json!([]));
    }

    #[test]
    fn test_detected_server_entry() {
        let entry = DetectedMcpServerEntry {
            server: McpServerResponse {
                id: 0,
                name: "test".into(),
                description: None,
                enabled: false,
                transport: McpTransport::Http {
                    url: "https://example.com/mcp".into(),
                    headers: HashMap::new(),
                },
                tools: None,
                last_test_status: McpServerStatus::Disconnected,
                last_connected: None,
                original_json: None,
                builtin: false,
                created_at: 0,
                updated_at: 0,
            },
            importable: false,
            import_skip_reason: Some("Needs authentication".into()),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["importable"], false);
        assert_eq!(json["import_skip_reason"], "Needs authentication");
        assert_eq!(json["name"], "test");
    }
}
