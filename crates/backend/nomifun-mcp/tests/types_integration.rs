//! Integration tests for nomifun-mcp core types.
//!
//! Tests the public API surface: McpServer construction from DB rows,
//! transport parsing/serialization, and response conversion.

use std::collections::HashMap;

use nomifun_common::McpServerStatus;
use nomifun_db::models::McpServerRow;
use nomifun_mcp::{McpServer, McpServerTransport, McpTool};

// ---------------------------------------------------------------------------
// McpServer::from_row — full pipeline tests
// ---------------------------------------------------------------------------

fn row(transport_type: &str, transport_config: &str, tools: Option<&str>, status: &str) -> McpServerRow {
    McpServerRow {
        mcp_server_id: nomifun_common::generate_id(),
        name: "integration-test".into(),
        description: Some("Integration test server".into()),
        enabled: true,
        transport_type: transport_type.into(),
        transport_config: transport_config.into(),
        tools: tools.map(String::from),
        last_test_status: status.into(),
        last_connected: Some(9999),
        original_json: Some(r#"{"name":"integration-test"}"#.into()),
        builtin: false,
        deleted_at: None,
        created_at: 1000,
        updated_at: 2000,
    }
}

#[test]
fn stdio_server_full_pipeline() {
    let config = serde_json::json!({
        "command": "npx",
        "args": ["-y", "@modelcontextprotocol/server-everything"],
        "env": { "NODE_ENV": "test", "DEBUG": "mcp:*" }
    });
    let tools_json = serde_json::json!([
        { "name": "echo", "description": "Echo input back" },
        { "name": "add", "description": "Add numbers", "input_schema": { "type": "object" } }
    ]);

    let r = row("stdio", &config.to_string(), Some(&tools_json.to_string()), "connected");
    let server = McpServer::from_row(r).unwrap();

    // Verify all fields
    assert!(nomifun_common::validate_uuidv7(server.mcp_server_id.as_str()).is_ok());
    assert_eq!(server.name, "integration-test");
    assert_eq!(server.description.as_deref(), Some("Integration test server"));
    assert!(server.enabled);
    assert_eq!(server.last_test_status, McpServerStatus::Connected);
    assert_eq!(server.last_connected, Some(9999));
    assert!(!server.builtin);

    // Verify transport
    match &server.transport {
        McpServerTransport::Stdio { command, args, env } => {
            assert_eq!(command, "npx");
            assert_eq!(args.len(), 2);
            assert_eq!(env.len(), 2);
            assert_eq!(env["NODE_ENV"], "test");
        }
        _ => panic!("expected Stdio transport"),
    }

    // Verify tools
    assert_eq!(server.tools.len(), 2);
    assert_eq!(server.tools[0].name, "echo");
    assert!(server.tools[1].input_schema.is_some());

    // Convert to API response and verify
    let server_id = server.mcp_server_id.clone();
    let resp = server.into_response();
    assert_eq!(resp.mcp_server_id, server_id);
    assert_eq!(resp.last_test_status, McpServerStatus::Connected);
    assert!(resp.tools.is_some());
    assert_eq!(resp.tools.unwrap().len(), 2);
}

#[test]
fn http_server_with_headers() {
    let config = serde_json::json!({
        "url": "https://mcp.example.com/v1",
        "headers": { "Authorization": "Bearer secret123", "X-Custom": "value" }
    });

    let r = row("http", &config.to_string(), None, "disconnected");
    let server = McpServer::from_row(r).unwrap();

    match &server.transport {
        McpServerTransport::Http { url, headers } => {
            assert_eq!(url, "https://mcp.example.com/v1");
            assert_eq!(headers.len(), 2);
            assert_eq!(headers["Authorization"], "Bearer secret123");
        }
        _ => panic!("expected Http transport"),
    }

    // Response should have no tools
    let resp = server.into_response();
    assert!(resp.tools.is_none());
}

#[test]
fn sse_server_minimal() {
    let config = serde_json::json!({ "url": "https://sse.example.com/events" });
    let r = row("sse", &config.to_string(), None, "testing");
    let server = McpServer::from_row(r).unwrap();

    assert_eq!(server.last_test_status, McpServerStatus::Testing);
    match &server.transport {
        McpServerTransport::Sse { url, headers } => {
            assert_eq!(url, "https://sse.example.com/events");
            assert!(headers.is_empty());
        }
        _ => panic!("expected Sse transport"),
    }
}

// ---------------------------------------------------------------------------
// Transport DB roundtrip: domain -> JSON -> DB -> domain
// ---------------------------------------------------------------------------

#[test]
fn transport_db_roundtrip_preserves_all_fields() {
    let transports = vec![
        McpServerTransport::Stdio {
            command: "python3".into(),
            args: vec!["-m".into(), "mcp_server".into()],
            env: HashMap::from([
                ("PYTHONPATH".into(), "/usr/lib/python3".into()),
                ("LOG_LEVEL".into(), "debug".into()),
            ]),
        },
        McpServerTransport::Sse {
            url: "https://sse.example.com/mcp".into(),
            headers: HashMap::from([
                ("Authorization".into(), "Bearer tok".into()),
                ("Accept".into(), "text/event-stream".into()),
            ]),
        },
        McpServerTransport::Http {
            url: "https://http.example.com/mcp".into(),
            headers: HashMap::new(),
        },
    ];

    for original in transports {
        let ttype = original.transport_type();
        let json = original.to_config_json().unwrap();
        let reconstructed = McpServerTransport::from_db(ttype, &json).unwrap();
        assert_eq!(reconstructed, original, "roundtrip failed for {ttype}");
    }
}

// ---------------------------------------------------------------------------
// Error scenarios
// ---------------------------------------------------------------------------

#[test]
fn invalid_transport_type_is_rejected() {
    let r = row("grpc", r#"{"endpoint":"localhost:50051"}"#, None, "disconnected");
    let err = McpServer::from_row(r).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unknown transport type"), "got: {msg}");
}

#[test]
fn malformed_transport_json_is_rejected() {
    let r = row("stdio", "{broken json", None, "disconnected");
    let err = McpServer::from_row(r).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("JSON"), "got: {msg}");
}

#[test]
fn malformed_tools_json_is_rejected() {
    let r = row(
        "stdio",
        r#"{"command":"node"}"#,
        Some("[{not valid json}]"),
        "connected",
    );
    let err = McpServer::from_row(r).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("JSON"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// McpTool construction
// ---------------------------------------------------------------------------

#[test]
fn tool_fields_preserved_through_conversion() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" }
        },
        "required": ["path"]
    });

    let tool = McpTool {
        name: "read_file".into(),
        description: Some("Read a file from disk".into()),
        input_schema: Some(schema.clone()),
    };

    // Domain -> API response
    let resp: nomifun_api_types::McpToolResponse = tool.clone().into();
    assert_eq!(resp.name, "read_file");
    assert_eq!(resp.description.as_deref(), Some("Read a file from disk"));
    assert_eq!(resp.input_schema, Some(schema.clone()));

    // API response -> domain
    let back: McpTool = resp.into();
    assert_eq!(back, tool);
}
