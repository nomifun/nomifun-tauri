//! Integration tests for ACP session MCP injection.
//!
//! Covers test-plan items SI-1 through SI-7: capability parsing,
//! format conversion, enabled-only filtering, and builtin server injection.

use std::collections::HashMap;

use nomifun_common::McpServerStatus;
use nomifun_mcp::{
    AcpMcpCapabilities, AcpSessionMcpServer, ImageGenConfig, McpServer, McpServerTransport, NameValuePair,
    build_builtin_image_gen_server, build_session_mcp_servers, parse_acp_mcp_capabilities,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_server(name: &str, enabled: bool, transport: McpServerTransport) -> McpServer {
    McpServer {
        // Injection keys on `name`, never the business ID; any stable value works here.
        mcp_server_id: nomifun_api_types::McpServerId::new(),
        name: name.into(),
        description: None,
        enabled,
        transport,
        tools: vec![],
        last_test_status: McpServerStatus::Disconnected,
        last_connected: None,
        original_json: None,
        builtin: false,
        created_at: 0,
        updated_at: 0,
    }
}

// ---------------------------------------------------------------------------
// SI-1: Full capabilities → all transports retained
// ---------------------------------------------------------------------------

#[test]
fn si_1_full_capabilities_retains_all_transports() {
    let caps = AcpMcpCapabilities {
        stdio: true,
        http: true,
        sse: true,
    };
    let servers = vec![
        make_server(
            "stdio-mcp",
            true,
            McpServerTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "test-server".into()],
                env: HashMap::from([("KEY".into(), "VAL".into())]),
            },
        ),
        make_server(
            "http-mcp",
            true,
            McpServerTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: HashMap::from([("Authorization".into(), "Bearer tok".into())]),
            },
        ),
        make_server(
            "sse-mcp",
            true,
            McpServerTransport::Sse {
                url: "https://example.com/sse".into(),
                headers: HashMap::new(),
            },
        ),
    ];

    let result = build_session_mcp_servers(&servers, &caps);
    assert_eq!(result.len(), 3, "all 3 transports should be retained");

    // Verify each type is present
    let has_stdio = result
        .iter()
        .any(|s| matches!(s, AcpSessionMcpServer::Stdio { name, .. } if name == "stdio-mcp"));
    let has_http = result
        .iter()
        .any(|s| matches!(s, AcpSessionMcpServer::Http { name, .. } if name == "http-mcp"));
    let has_sse = result
        .iter()
        .any(|s| matches!(s, AcpSessionMcpServer::Sse { name, .. } if name == "sse-mcp"));

    assert!(has_stdio, "stdio server missing");
    assert!(has_http, "http server missing");
    assert!(has_sse, "sse server missing");
}

// ---------------------------------------------------------------------------
// SI-2: stdio-only capabilities → only stdio retained
// ---------------------------------------------------------------------------

#[test]
fn si_2_stdio_only_keeps_stdio_servers() {
    let caps = AcpMcpCapabilities {
        stdio: true,
        http: false,
        sse: false,
    };
    let servers = vec![
        make_server(
            "stdio-mcp",
            true,
            McpServerTransport::Stdio {
                command: "npx".into(),
                args: vec![],
                env: HashMap::new(),
            },
        ),
        make_server(
            "http-mcp",
            true,
            McpServerTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: HashMap::new(),
            },
        ),
        make_server(
            "sse-mcp",
            true,
            McpServerTransport::Sse {
                url: "https://example.com/sse".into(),
                headers: HashMap::new(),
            },
        ),
    ];

    let result = build_session_mcp_servers(&servers, &caps);
    assert_eq!(result.len(), 1);
    assert!(matches!(&result[0], AcpSessionMcpServer::Stdio { name, .. } if name == "stdio-mcp"));
}

// ---------------------------------------------------------------------------
// SI-3: No capabilities → empty list
// ---------------------------------------------------------------------------

#[test]
fn si_3_no_capabilities_returns_empty() {
    let caps = AcpMcpCapabilities {
        stdio: false,
        http: false,
        sse: false,
    };
    let servers = vec![
        make_server(
            "s1",
            true,
            McpServerTransport::Stdio {
                command: "npx".into(),
                args: vec![],
                env: HashMap::new(),
            },
        ),
        make_server(
            "s2",
            true,
            McpServerTransport::Http {
                url: "https://example.com".into(),
                headers: HashMap::new(),
            },
        ),
    ];

    let result = build_session_mcp_servers(&servers, &caps);
    assert!(result.is_empty(), "no capabilities → empty result");
}

// ---------------------------------------------------------------------------
// SI-4: stdio server format conversion (env Record → Vec<{name,value}>)
// ---------------------------------------------------------------------------

#[test]
fn si_4_stdio_format_conversion() {
    let caps = AcpMcpCapabilities {
        stdio: true,
        http: false,
        sse: false,
    };
    let servers = vec![make_server(
        "test-stdio",
        true,
        McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "test-server".into()],
            env: HashMap::from([
                ("NODE_ENV".into(), "production".into()),
                ("DEBUG".into(), "true".into()),
            ]),
        },
    )];

    let result = build_session_mcp_servers(&servers, &caps);
    assert_eq!(result.len(), 1);

    match &result[0] {
        AcpSessionMcpServer::Stdio {
            name,
            command,
            args,
            env,
        } => {
            assert_eq!(name, "test-stdio");
            assert_eq!(command, "npx");
            assert_eq!(args, &["-y", "test-server"]);
            assert_eq!(env.len(), 2);
            // Sorted by name
            assert_eq!(env[0].name, "DEBUG");
            assert_eq!(env[0].value, "true");
            assert_eq!(env[1].name, "NODE_ENV");
            assert_eq!(env[1].value, "production");
        }
        _ => panic!("expected Stdio variant"),
    }

    // Verify JSON wire format
    let json = serde_json::to_value(&result[0]).unwrap();
    assert_eq!(json["type"], "stdio");
    assert_eq!(json["name"], "test-stdio");
    assert_eq!(json["command"], "npx");
    assert!(json["env"].is_array());
    assert_eq!(json["env"][0]["name"], "DEBUG");
    assert_eq!(json["env"][0]["value"], "true");
}

// ---------------------------------------------------------------------------
// SI-5: http server format conversion (headers Record → Vec<{name,value}>)
// ---------------------------------------------------------------------------

#[test]
fn si_5_http_format_conversion() {
    let caps = AcpMcpCapabilities {
        stdio: false,
        http: true,
        sse: false,
    };
    let servers = vec![make_server(
        "test-http",
        true,
        McpServerTransport::Http {
            url: "https://example.com/mcp".into(),
            headers: HashMap::from([
                ("Authorization".into(), "Bearer tok".into()),
                ("X-Custom".into(), "val".into()),
            ]),
        },
    )];

    let result = build_session_mcp_servers(&servers, &caps);
    assert_eq!(result.len(), 1);

    match &result[0] {
        AcpSessionMcpServer::Http { name, url, headers } => {
            assert_eq!(name, "test-http");
            assert_eq!(url, "https://example.com/mcp");
            assert_eq!(headers.len(), 2);
            // Sorted by name
            assert_eq!(headers[0].name, "Authorization");
            assert_eq!(headers[0].value, "Bearer tok");
            assert_eq!(headers[1].name, "X-Custom");
            assert_eq!(headers[1].value, "val");
        }
        _ => panic!("expected Http variant"),
    }

    // Verify JSON wire format
    let json = serde_json::to_value(&result[0]).unwrap();
    assert_eq!(json["type"], "http");
    assert_eq!(json["name"], "test-http");
    assert_eq!(json["url"], "https://example.com/mcp");
    assert!(json["headers"].is_array());
}

// ---------------------------------------------------------------------------
// SI-6: only enabled servers appear
// ---------------------------------------------------------------------------

#[test]
fn si_6_only_enabled_servers_in_result() {
    let caps = AcpMcpCapabilities {
        stdio: true,
        http: true,
        sse: true,
    };
    let servers = vec![
        make_server(
            "enabled-stdio",
            true,
            McpServerTransport::Stdio {
                command: "npx".into(),
                args: vec![],
                env: HashMap::new(),
            },
        ),
        make_server(
            "disabled-stdio",
            false,
            McpServerTransport::Stdio {
                command: "node".into(),
                args: vec![],
                env: HashMap::new(),
            },
        ),
        make_server(
            "enabled-http",
            true,
            McpServerTransport::Http {
                url: "https://example.com".into(),
                headers: HashMap::new(),
            },
        ),
        make_server(
            "disabled-http",
            false,
            McpServerTransport::Http {
                url: "https://other.com".into(),
                headers: HashMap::new(),
            },
        ),
    ];

    let result = build_session_mcp_servers(&servers, &caps);
    assert_eq!(result.len(), 2, "only 2 enabled servers should appear");

    let names: Vec<&str> = result
        .iter()
        .map(|s| match s {
            AcpSessionMcpServer::Stdio { name, .. } => name.as_str(),
            AcpSessionMcpServer::Http { name, .. } => name.as_str(),
            AcpSessionMcpServer::Sse { name, .. } => name.as_str(),
        })
        .collect();
    assert!(names.contains(&"enabled-stdio"));
    assert!(names.contains(&"enabled-http"));
    assert!(!names.contains(&"disabled-stdio"));
    assert!(!names.contains(&"disabled-http"));
}

// ---------------------------------------------------------------------------
// SI-7: builtin MCP injection (image generation with env vars)
// ---------------------------------------------------------------------------

#[test]
fn si_7_builtin_image_gen_injection() {
    let caps = AcpMcpCapabilities {
        stdio: true,
        http: true,
        sse: true,
    };

    let img_config = ImageGenConfig {
        model: Some("dall-e-3".into()),
        api_url: Some("https://api.openai.com/v1".into()),
        api_key: Some("sk-test-key".into()),
        size: Some("1024x1024".into()),
        quality: Some("hd".into()),
        style: Some("natural".into()),
    };

    // Build user servers
    let user_servers = vec![make_server(
        "user-mcp",
        true,
        McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "user-server".into()],
            env: HashMap::new(),
        },
    )];

    let mut session_servers = build_session_mcp_servers(&user_servers, &caps);

    // Inject builtin image gen server
    if let Some(builtin) = build_builtin_image_gen_server(&caps, "/usr/local/bin/nomifun-img-gen", &img_config) {
        session_servers.push(builtin);
    }

    assert_eq!(session_servers.len(), 2, "user server + builtin image gen");

    // Verify the builtin server
    let builtin = &session_servers[1];
    match builtin {
        AcpSessionMcpServer::Stdio { name, command, env, .. } => {
            assert_eq!(name, "nomifun-image-generation");
            assert_eq!(command, "/usr/local/bin/nomifun-img-gen");

            // Verify all 6 env vars are present
            assert_eq!(env.len(), 6);

            let env_map: HashMap<&str, &str> = env.iter().map(|p| (p.name.as_str(), p.value.as_str())).collect();
            assert_eq!(env_map["NOMIFUN_IMG_MODEL"], "dall-e-3");
            assert_eq!(env_map["NOMIFUN_IMG_API_URL"], "https://api.openai.com/v1");
            assert_eq!(env_map["NOMIFUN_IMG_API_KEY"], "sk-test-key");
            assert_eq!(env_map["NOMIFUN_IMG_SIZE"], "1024x1024");
            assert_eq!(env_map["NOMIFUN_IMG_QUALITY"], "hd");
            assert_eq!(env_map["NOMIFUN_IMG_STYLE"], "natural");
        }
        _ => panic!("expected Stdio variant for builtin"),
    }
}

// ---------------------------------------------------------------------------
// Capability parsing from various response formats
// ---------------------------------------------------------------------------

#[test]
fn parse_capabilities_from_real_response_shape() {
    // Simulates a realistic ACP backend response with nested capabilities
    let response = serde_json::json!({
        "status": "ok",
        "version": "1.2.3",
        "mcp_capabilities": {
            "stdio": true,
            "http": true,
            "sse": false
        }
    });

    let caps = parse_acp_mcp_capabilities(&response);
    assert!(caps.stdio);
    assert!(caps.http);
    assert!(!caps.sse);
}

#[test]
fn parse_capabilities_empty_response() {
    let response = serde_json::json!({});
    let caps = parse_acp_mcp_capabilities(&response);
    // Default: stdio only
    assert!(caps.stdio);
    assert!(!caps.http);
    assert!(!caps.sse);
}

// ---------------------------------------------------------------------------
// End-to-end: parse capabilities + build servers
// ---------------------------------------------------------------------------

#[test]
fn end_to_end_parse_then_build() {
    let acp_response = serde_json::json!({
        "mcp_capabilities": { "stdio": true, "http": true, "sse": false }
    });
    let caps = parse_acp_mcp_capabilities(&acp_response);

    let servers = vec![
        make_server(
            "stdio-srv",
            true,
            McpServerTransport::Stdio {
                command: "npx".into(),
                args: vec![],
                env: HashMap::new(),
            },
        ),
        make_server(
            "http-srv",
            true,
            McpServerTransport::Http {
                url: "https://example.com".into(),
                headers: HashMap::new(),
            },
        ),
        make_server(
            "sse-srv",
            true,
            McpServerTransport::Sse {
                url: "https://example.com/sse".into(),
                headers: HashMap::new(),
            },
        ),
    ];

    let result = build_session_mcp_servers(&servers, &caps);
    assert_eq!(result.len(), 2, "sse should be filtered out");

    let names: Vec<&str> = result
        .iter()
        .map(|s| match s {
            AcpSessionMcpServer::Stdio { name, .. } => name.as_str(),
            AcpSessionMcpServer::Http { name, .. } => name.as_str(),
            AcpSessionMcpServer::Sse { name, .. } => name.as_str(),
        })
        .collect();
    assert!(names.contains(&"stdio-srv"));
    assert!(names.contains(&"http-srv"));
    assert!(!names.contains(&"sse-srv"));
}

// ---------------------------------------------------------------------------
// JSON wire format verification
// ---------------------------------------------------------------------------

#[test]
fn wire_format_is_acp_compatible() {
    let servers = vec![
        AcpSessionMcpServer::Stdio {
            name: "test-stdio".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "server".into()],
            env: vec![NameValuePair {
                name: "K".into(),
                value: "V".into(),
            }],
        },
        AcpSessionMcpServer::Http {
            name: "test-http".into(),
            url: "https://example.com/mcp".into(),
            headers: vec![NameValuePair {
                name: "Auth".into(),
                value: "Bearer x".into(),
            }],
        },
    ];

    let json = serde_json::to_value(&servers).unwrap();
    let arr = json.as_array().unwrap();

    // stdio variant
    assert_eq!(arr[0]["type"], "stdio");
    assert_eq!(arr[0]["name"], "test-stdio");
    assert_eq!(arr[0]["command"], "npx");
    assert_eq!(arr[0]["args"][0], "-y");
    assert_eq!(arr[0]["env"][0]["name"], "K");
    assert_eq!(arr[0]["env"][0]["value"], "V");

    // http variant
    assert_eq!(arr[1]["type"], "http");
    assert_eq!(arr[1]["name"], "test-http");
    assert_eq!(arr[1]["url"], "https://example.com/mcp");
    assert_eq!(arr[1]["headers"][0]["name"], "Auth");
}
