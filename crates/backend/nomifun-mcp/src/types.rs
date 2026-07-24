use std::collections::HashMap;

use nomifun_api_types::{McpServerId, McpServerResponse, McpToolResponse, McpTransport};
use nomifun_common::{McpServerStatus, TimestampMs};
use nomifun_db::models::McpServerRow;

use crate::error::McpError;

// ---------------------------------------------------------------------------
// McpServerTransport — domain transport enum
// ---------------------------------------------------------------------------

/// Domain-layer MCP server transport configuration.
///
/// Mirrors `McpTransport` from `nomifun-api-types` but lives in the business
/// layer. Conversions are provided in both directions.
#[derive(Debug, Clone, PartialEq)]
pub enum McpServerTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    Sse {
        url: String,
        headers: HashMap<String, String>,
    },
    Http {
        url: String,
        headers: HashMap<String, String>,
    },
}

impl McpServerTransport {
    /// Returns the transport type as a string for DB storage.
    pub fn transport_type(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Sse { .. } => "sse",
            Self::Http { .. } => "http",
        }
    }

    /// Serializes the transport config to a JSON string for DB storage.
    ///
    /// Only serializes the variant-specific fields (command/args/env or
    /// url/headers), not the type discriminant.
    pub fn to_config_json(&self) -> Result<String, McpError> {
        let value = match self {
            Self::Stdio { command, args, env } => {
                serde_json::json!({ "command": command, "args": args, "env": env })
            }
            Self::Sse { url, headers } => {
                serde_json::json!({ "url": url, "headers": headers })
            }
            Self::Http { url, headers } => {
                serde_json::json!({ "url": url, "headers": headers })
            }
        };
        serde_json::to_string(&value).map_err(McpError::from)
    }

    /// Parses transport from DB fields (type string + config JSON).
    pub fn from_db(transport_type: &str, config_json: &str) -> Result<Self, McpError> {
        let value: serde_json::Value = serde_json::from_str(config_json).map_err(McpError::from)?;

        match transport_type {
            "stdio" => {
                let command = value["command"]
                    .as_str()
                    .ok_or_else(|| McpError::InvalidTransport("stdio: missing command".into()))?
                    .to_owned();
                let args = value["args"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let env = value["env"]
                    .as_object()
                    .map(|obj| {
                        obj.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(Self::Stdio { command, args, env })
            }
            "sse" => {
                let url = value["url"]
                    .as_str()
                    .ok_or_else(|| McpError::InvalidTransport("sse: missing url".into()))?
                    .to_owned();
                let headers = parse_headers_object(&value["headers"]);
                Ok(Self::Sse { url, headers })
            }
            "http" => {
                let url = value["url"]
                    .as_str()
                    .ok_or_else(|| McpError::InvalidTransport("http: missing url".into()))?
                    .to_owned();
                let headers = parse_headers_object(&value["headers"]);
                Ok(Self::Http { url, headers })
            }
            other => Err(McpError::InvalidTransport(format!("unknown transport type: {other}"))),
        }
    }
}

/// Helper: extract `HashMap<String, String>` from a JSON object value.
fn parse_headers_object(value: &serde_json::Value) -> HashMap<String, String> {
    value
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

// -- Conversions between domain and API transport --

impl From<McpTransport> for McpServerTransport {
    fn from(t: McpTransport) -> Self {
        match t {
            McpTransport::Stdio { command, args, env } => Self::Stdio { command, args, env },
            McpTransport::Sse { url, headers } => Self::Sse { url, headers },
            McpTransport::Http { url, headers } => Self::Http { url, headers },
        }
    }
}

impl From<McpServerTransport> for McpTransport {
    fn from(t: McpServerTransport) -> Self {
        match t {
            McpServerTransport::Stdio { command, args, env } => McpTransport::Stdio { command, args, env },
            McpServerTransport::Sse { url, headers } => McpTransport::Sse { url, headers },
            McpServerTransport::Http { url, headers } => McpTransport::Http { url, headers },
        }
    }
}

// ---------------------------------------------------------------------------
// McpTool — domain tool description
// ---------------------------------------------------------------------------

/// Domain-layer MCP tool description.
///
/// Populated after a successful connection test (`tools/list` response).
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<serde_json::Value>,
}

impl From<McpToolResponse> for McpTool {
    fn from(r: McpToolResponse) -> Self {
        Self {
            name: r.name,
            description: r.description,
            input_schema: r.input_schema,
        }
    }
}

impl From<McpTool> for McpToolResponse {
    fn from(t: McpTool) -> Self {
        McpToolResponse {
            name: t.name,
            description: t.description,
            input_schema: t.input_schema,
        }
    }
}

// ---------------------------------------------------------------------------
// McpServer — domain server model
// ---------------------------------------------------------------------------

/// Domain-layer MCP server model.
///
/// Constructed from `McpServerRow` (DB) by parsing JSON fields into
/// structured types. This is the primary type used across business logic.
#[derive(Debug, Clone)]
pub struct McpServer {
    pub mcp_server_id: McpServerId,
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub transport: McpServerTransport,
    pub tools: Vec<McpTool>,
    pub last_test_status: McpServerStatus,
    pub last_connected: Option<TimestampMs>,
    pub original_json: Option<String>,
    pub builtin: bool,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

impl McpServer {
    /// Converts a DB row into a domain model by parsing JSON fields.
    pub fn from_row(row: McpServerRow) -> Result<Self, McpError> {
        let transport = McpServerTransport::from_db(&row.transport_type, &row.transport_config)?;

        let tools = match row.tools.as_deref() {
            Some(json_str) if !json_str.is_empty() => {
                let tool_responses: Vec<McpToolResponse> = serde_json::from_str(json_str).map_err(McpError::from)?;
                tool_responses.into_iter().map(McpTool::from).collect()
            }
            _ => Vec::new(),
        };

        let last_test_status = parse_server_status(&row.last_test_status);

        Ok(Self {
            mcp_server_id: McpServerId::parse(row.mcp_server_id)
                .map_err(|error| McpError::InvalidTransport(format!("invalid MCP server ID: {error}")))?,
            name: row.name,
            description: row.description,
            enabled: row.enabled,
            transport,
            tools,
            last_test_status,
            last_connected: row.last_connected,
            original_json: row.original_json,
            builtin: row.builtin,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    /// Converts to the API response DTO.
    pub fn into_response(self) -> McpServerResponse {
        let tools = if self.tools.is_empty() {
            None
        } else {
            Some(self.tools.into_iter().map(McpToolResponse::from).collect())
        };

        McpServerResponse {
            mcp_server_id: self.mcp_server_id,
            name: self.name,
            description: self.description,
            enabled: self.enabled,
            transport: self.transport.into(),
            tools,
            last_test_status: self.last_test_status,
            last_connected: self.last_connected,
            original_json: self.original_json,
            builtin: self.builtin,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// Parse status string to enum, defaulting to `Disconnected` for unknown values.
fn parse_server_status(s: &str) -> McpServerStatus {
    match s {
        "connected" => McpServerStatus::Connected,
        "disconnected" => McpServerStatus::Disconnected,
        "error" => McpServerStatus::Error,
        "testing" => McpServerStatus::Testing,
        _ => McpServerStatus::Disconnected,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- McpServerTransport ---------------------------------------------------

    #[test]
    fn transport_type_string() {
        let stdio = McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec![],
            env: HashMap::new(),
        };
        assert_eq!(stdio.transport_type(), "stdio");

        let sse = McpServerTransport::Sse {
            url: "http://x".into(),
            headers: HashMap::new(),
        };
        assert_eq!(sse.transport_type(), "sse");

        let http = McpServerTransport::Http {
            url: "http://x".into(),
            headers: HashMap::new(),
        };
        assert_eq!(http.transport_type(), "http");
    }

    #[test]
    fn stdio_roundtrip_via_db() {
        let original = McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "@test/server".into()],
            env: HashMap::from([("NODE_ENV".into(), "production".into())]),
        };
        let json = original.to_config_json().unwrap();
        let parsed = McpServerTransport::from_db("stdio", &json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn sse_roundtrip_via_db() {
        let original = McpServerTransport::Sse {
            url: "https://example.com/sse".into(),
            headers: HashMap::from([("Authorization".into(), "Bearer tok".into())]),
        };
        let json = original.to_config_json().unwrap();
        let parsed = McpServerTransport::from_db("sse", &json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn http_roundtrip_via_db() {
        let original = McpServerTransport::Http {
            url: "https://example.com/mcp".into(),
            headers: HashMap::new(),
        };
        let json = original.to_config_json().unwrap();
        let parsed = McpServerTransport::from_db("http", &json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn from_db_stdio_minimal() {
        let json = r#"{"command":"node"}"#;
        let t = McpServerTransport::from_db("stdio", json).unwrap();
        assert_eq!(
            t,
            McpServerTransport::Stdio {
                command: "node".into(),
                args: vec![],
                env: HashMap::new(),
            }
        );
    }

    #[test]
    fn from_db_unknown_type_fails() {
        let result = McpServerTransport::from_db("websocket", "{}");
        assert!(matches!(result, Err(McpError::InvalidTransport(_))));
    }

    #[test]
    fn from_db_stdio_missing_command_fails() {
        let result = McpServerTransport::from_db("stdio", r#"{"args":[]}"#);
        assert!(matches!(result, Err(McpError::InvalidTransport(_))));
    }

    #[test]
    fn from_db_sse_missing_url_fails() {
        let result = McpServerTransport::from_db("sse", r#"{"headers":{}}"#);
        assert!(matches!(result, Err(McpError::InvalidTransport(_))));
    }

    #[test]
    fn from_db_http_missing_url_fails() {
        let result = McpServerTransport::from_db("http", r#"{"headers":{}}"#);
        assert!(matches!(result, Err(McpError::InvalidTransport(_))));
    }

    #[test]
    fn from_db_invalid_json_fails() {
        let result = McpServerTransport::from_db("stdio", "not json");
        assert!(matches!(result, Err(McpError::Json(_))));
    }

    // -- API transport conversions --------------------------------------------

    #[test]
    fn api_transport_roundtrip_stdio() {
        let domain = McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into()],
            env: HashMap::from([("K".into(), "V".into())]),
        };
        let api: McpTransport = domain.clone().into();
        let back: McpServerTransport = api.into();
        assert_eq!(back, domain);
    }

    #[test]
    fn api_transport_roundtrip_sse() {
        let domain = McpServerTransport::Sse {
            url: "http://x".into(),
            headers: HashMap::from([("H".into(), "V".into())]),
        };
        let api: McpTransport = domain.clone().into();
        let back: McpServerTransport = api.into();
        assert_eq!(back, domain);
    }

    #[test]
    fn api_transport_roundtrip_http() {
        let domain = McpServerTransport::Http {
            url: "http://x".into(),
            headers: HashMap::new(),
        };
        let api: McpTransport = domain.clone().into();
        let back: McpServerTransport = api.into();
        assert_eq!(back, domain);
    }

    // -- McpTool conversions --------------------------------------------------

    #[test]
    fn tool_from_response() {
        let resp = McpToolResponse {
            name: "read_file".into(),
            description: Some("Read a file".into()),
            input_schema: Some(serde_json::json!({"type": "object"})),
        };
        let tool = McpTool::from(resp);
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.description.as_deref(), Some("Read a file"));
        assert!(tool.input_schema.is_some());
    }

    #[test]
    fn tool_to_response() {
        let tool = McpTool {
            name: "write_file".into(),
            description: None,
            input_schema: None,
        };
        let resp = McpToolResponse::from(tool);
        assert_eq!(resp.name, "write_file");
        assert!(resp.description.is_none());
    }

    // -- McpServer::from_row --------------------------------------------------

    fn make_test_row(transport_type: &str, transport_config: &str, tools: Option<&str>, status: &str) -> McpServerRow {
        McpServerRow {
            mcp_server_id: nomifun_common::generate_id(),
            name: "test-server".into(),
            description: Some("A test server".into()),
            enabled: true,
            transport_type: transport_type.into(),
            transport_config: transport_config.into(),
            tools: tools.map(String::from),
            last_test_status: status.into(),
            last_connected: Some(1000),
            original_json: None,
            builtin: false,
            deleted_at: None,
            created_at: 500,
            updated_at: 600,
        }
    }

    #[test]
    fn from_row_stdio_with_tools() {
        let row = make_test_row(
            "stdio",
            r#"{"command":"npx","args":["-y","@test/server"],"env":{"K":"V"}}"#,
            Some(r#"[{"name":"read","description":"Read file"}]"#),
            "connected",
        );
        let server = McpServer::from_row(row).unwrap();

        assert!(nomifun_common::validate_uuidv7(server.mcp_server_id.as_str()).is_ok());
        assert_eq!(server.name, "test-server");
        assert!(server.enabled);
        assert_eq!(server.last_test_status, McpServerStatus::Connected);
        assert_eq!(server.tools.len(), 1);
        assert_eq!(server.tools[0].name, "read");
        match &server.transport {
            McpServerTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &["-y", "@test/server"]);
                assert_eq!(env.get("K").unwrap(), "V");
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn from_row_http_no_tools() {
        let row = make_test_row(
            "http",
            r#"{"url":"https://example.com/mcp","headers":{}}"#,
            None,
            "disconnected",
        );
        let server = McpServer::from_row(row).unwrap();

        assert!(server.tools.is_empty());
        assert_eq!(server.last_test_status, McpServerStatus::Disconnected);
        match &server.transport {
            McpServerTransport::Http { url, headers } => {
                assert_eq!(url, "https://example.com/mcp");
                assert!(headers.is_empty());
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn from_row_empty_tools_string() {
        let row = make_test_row("stdio", r#"{"command":"node"}"#, Some(""), "error");
        let server = McpServer::from_row(row).unwrap();
        assert!(server.tools.is_empty());
        assert_eq!(server.last_test_status, McpServerStatus::Error);
    }

    #[test]
    fn from_row_unknown_status_defaults_to_disconnected() {
        let row = make_test_row("stdio", r#"{"command":"node"}"#, None, "unknown_status");
        let server = McpServer::from_row(row).unwrap();
        assert_eq!(server.last_test_status, McpServerStatus::Disconnected);
    }

    #[test]
    fn from_row_invalid_transport_config_fails() {
        let row = make_test_row("stdio", "not json", None, "disconnected");
        let result = McpServer::from_row(row);
        assert!(result.is_err());
    }

    #[test]
    fn from_row_invalid_tools_json_fails() {
        let row = make_test_row("stdio", r#"{"command":"node"}"#, Some("not json"), "disconnected");
        let result = McpServer::from_row(row);
        assert!(result.is_err());
    }

    // -- McpServer::into_response ---------------------------------------------

    #[test]
    fn into_response_with_tools() {
        let server_id = nomifun_api_types::McpServerId::new();
        let server = McpServer {
            mcp_server_id: server_id.clone(),
            name: "test".into(),
            description: None,
            enabled: true,
            transport: McpServerTransport::Stdio {
                command: "npx".into(),
                args: vec![],
                env: HashMap::new(),
            },
            tools: vec![McpTool {
                name: "read_file".into(),
                description: None,
                input_schema: None,
            }],
            last_test_status: McpServerStatus::Connected,
            last_connected: Some(1000),
            original_json: None,
            builtin: false,
            created_at: 500,
            updated_at: 600,
        };
        let resp = server.into_response();
        assert_eq!(resp.mcp_server_id, server_id);
        assert!(resp.tools.is_some());
        assert_eq!(resp.tools.unwrap().len(), 1);
    }

    #[test]
    fn into_response_empty_tools_is_none() {
        let server = McpServer {
            mcp_server_id: McpServerId::new(),
            name: "test".into(),
            description: Some("desc".into()),
            enabled: false,
            transport: McpServerTransport::Http {
                url: "http://x".into(),
                headers: HashMap::new(),
            },
            tools: vec![],
            last_test_status: McpServerStatus::Disconnected,
            last_connected: None,
            original_json: None,
            builtin: false,
            created_at: 500,
            updated_at: 600,
        };
        let resp = server.into_response();
        assert!(resp.tools.is_none());
        assert_eq!(resp.description.as_deref(), Some("desc"));
    }

    // -- parse_server_status --------------------------------------------------

    #[test]
    fn parse_all_statuses() {
        assert_eq!(parse_server_status("connected"), McpServerStatus::Connected);
        assert_eq!(parse_server_status("disconnected"), McpServerStatus::Disconnected);
        assert_eq!(parse_server_status("error"), McpServerStatus::Error);
        assert_eq!(parse_server_status("testing"), McpServerStatus::Testing);
        assert_eq!(parse_server_status("garbage"), McpServerStatus::Disconnected);
    }
}
