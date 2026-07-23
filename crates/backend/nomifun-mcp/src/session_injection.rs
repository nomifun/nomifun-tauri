use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::types::{McpServer, McpServerTransport};

// ---------------------------------------------------------------------------
// NameValuePair
// ---------------------------------------------------------------------------

/// A name-value pair for environment variables and HTTP headers
/// in the ACP session MCP server format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NameValuePair {
    pub name: String,
    pub value: String,
}

// ---------------------------------------------------------------------------
// AcpSessionMcpServer
// ---------------------------------------------------------------------------

/// ACP session MCP server configuration.
///
/// This is the wire format expected by the ACP backend when creating
/// a new session with MCP servers injected. Two shapes:
///
/// - **Stdio**: command-based MCP servers (command + args + env)
/// - **Http / Sse**: URL-based MCP servers (url + optional headers)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AcpSessionMcpServer {
    Stdio {
        name: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<NameValuePair>,
    },
    Http {
        name: String,
        url: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<NameValuePair>,
    },
    Sse {
        name: String,
        url: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<NameValuePair>,
    },
}

// ---------------------------------------------------------------------------
// AcpMcpCapabilities
// ---------------------------------------------------------------------------

/// ACP backend MCP capability declaration.
///
/// Describes which transport types the ACP backend supports for
/// spawning MCP servers during a session.
#[derive(Debug, Clone, PartialEq)]
pub struct AcpMcpCapabilities {
    pub stdio: bool,
    pub http: bool,
    pub sse: bool,
}

impl AcpMcpCapabilities {
    /// Returns true if no transport type is supported.
    pub fn is_empty(&self) -> bool {
        !self.stdio && !self.http && !self.sse
    }
}

impl Default for AcpMcpCapabilities {
    fn default() -> Self {
        Self {
            stdio: true,
            http: false,
            sse: false,
        }
    }
}

// ---------------------------------------------------------------------------
// ImageGenConfig
// ---------------------------------------------------------------------------

/// Configuration for the builtin image generation MCP server.
///
/// Values are injected as environment variables when building
/// the builtin MCP server config for ACP sessions.
#[derive(Debug, Clone, Default)]
pub struct ImageGenConfig {
    pub model: Option<String>,
    pub api_url: Option<String>,
    pub api_key: Option<String>,
    pub size: Option<String>,
    pub quality: Option<String>,
    pub style: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse ACP MCP capabilities from an ACP backend response.
///
/// Looks for capabilities under `mcp_capabilities`, `mcpCapabilities`,
/// or `mcp` keys. Returns default capabilities (stdio only) when the
/// field is missing or not an object.
pub fn parse_acp_mcp_capabilities(response: &serde_json::Value) -> AcpMcpCapabilities {
    let caps = response
        .get("mcp_capabilities")
        .or_else(|| response.get("mcpCapabilities"))
        .or_else(|| response.get("mcp"));

    let Some(caps) = caps else {
        return AcpMcpCapabilities::default();
    };

    let http = bool_field(caps, "http");
    let sse = bool_field(caps, "sse");
    let stdio = bool_field(caps, "stdio") || http || sse;

    AcpMcpCapabilities { stdio, http, sse }
}

/// Build ACP session MCP server configs from domain servers.
///
/// Filters to only enabled servers whose transport type is supported
/// by the ACP backend, then converts to the ACP wire format.
pub fn build_session_mcp_servers(servers: &[McpServer], capabilities: &AcpMcpCapabilities) -> Vec<AcpSessionMcpServer> {
    servers
        .iter()
        .filter(|s| s.enabled)
        .filter_map(|s| convert_server(s, capabilities))
        .collect()
}

/// Build the builtin image generation MCP server config.
///
/// Returns `None` if the ACP backend doesn't support stdio transport
/// or if `command` is empty.
pub fn build_builtin_image_gen_server(
    capabilities: &AcpMcpCapabilities,
    command: &str,
    config: &ImageGenConfig,
) -> Option<AcpSessionMcpServer> {
    if !capabilities.stdio || command.is_empty() {
        return None;
    }

    let env = build_image_gen_env(config);

    Some(AcpSessionMcpServer::Stdio {
        name: "nomifun-image-generation".into(),
        command: command.to_owned(),
        args: Vec::new(),
        env,
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Extract a boolean field from a JSON value, defaulting to false.
fn bool_field(value: &serde_json::Value, key: &str) -> bool {
    value.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Convert a domain `McpServer` to `AcpSessionMcpServer`.
///
/// Returns `None` if the server's transport type is not supported
/// by the given capabilities.
fn convert_server(server: &McpServer, capabilities: &AcpMcpCapabilities) -> Option<AcpSessionMcpServer> {
    match &server.transport {
        McpServerTransport::Stdio { command, args, env } if capabilities.stdio => Some(AcpSessionMcpServer::Stdio {
            name: server.name.clone(),
            command: command.clone(),
            args: args.clone(),
            env: hashmap_to_pairs(env),
        }),
        McpServerTransport::Http { url, headers } if capabilities.http => Some(AcpSessionMcpServer::Http {
            name: server.name.clone(),
            url: url.clone(),
            headers: hashmap_to_pairs(headers),
        }),
        McpServerTransport::Sse { url, headers } if capabilities.sse => Some(AcpSessionMcpServer::Sse {
            name: server.name.clone(),
            url: url.clone(),
            headers: hashmap_to_pairs(headers),
        }),
        _ => None,
    }
}

/// Convert a `HashMap<String, String>` to a sorted `Vec<NameValuePair>`.
///
/// Sorted by key for deterministic serialization.
fn hashmap_to_pairs(map: &HashMap<String, String>) -> Vec<NameValuePair> {
    let mut pairs: Vec<NameValuePair> = map
        .iter()
        .map(|(k, v)| NameValuePair {
            name: k.clone(),
            value: v.clone(),
        })
        .collect();
    pairs.sort_by(|a, b| a.name.cmp(&b.name));
    pairs
}

/// Build environment variable pairs for the image generation server.
///
/// Sorted by name for deterministic output, consistent with `hashmap_to_pairs`.
fn build_image_gen_env(config: &ImageGenConfig) -> Vec<NameValuePair> {
    let entries: [(&str, &Option<String>); 6] = [
        ("NOMIFUN_IMG_API_KEY", &config.api_key),
        ("NOMIFUN_IMG_API_URL", &config.api_url),
        ("NOMIFUN_IMG_MODEL", &config.model),
        ("NOMIFUN_IMG_QUALITY", &config.quality),
        ("NOMIFUN_IMG_SIZE", &config.size),
        ("NOMIFUN_IMG_STYLE", &config.style),
    ];

    entries
        .into_iter()
        .filter_map(|(name, value)| {
            value.as_ref().map(|v| NameValuePair {
                name: name.into(),
                value: v.clone(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::McpServerTransport;
    use nomifun_common::McpServerStatus;

    // -- helpers --

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

    fn stdio_transport(cmd: &str) -> McpServerTransport {
        McpServerTransport::Stdio {
            command: cmd.into(),
            args: vec!["-y".into(), "@test/server".into()],
            env: HashMap::from([("NODE_ENV".into(), "production".into())]),
        }
    }

    fn http_transport(url: &str) -> McpServerTransport {
        McpServerTransport::Http {
            url: url.into(),
            headers: HashMap::from([("Authorization".into(), "Bearer tok".into())]),
        }
    }

    fn sse_transport(url: &str) -> McpServerTransport {
        McpServerTransport::Sse {
            url: url.into(),
            headers: HashMap::new(),
        }
    }

    fn all_caps() -> AcpMcpCapabilities {
        AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        }
    }

    // -- AcpMcpCapabilities ---------------------------------------------------

    #[test]
    fn capabilities_default_is_stdio_only() {
        let caps = AcpMcpCapabilities::default();
        assert!(caps.stdio);
        assert!(!caps.http);
        assert!(!caps.sse);
    }

    #[test]
    fn capabilities_is_empty() {
        let empty = AcpMcpCapabilities {
            stdio: false,
            http: false,
            sse: false,
        };
        assert!(empty.is_empty());
        assert!(!AcpMcpCapabilities::default().is_empty());
    }

    // -- parse_acp_mcp_capabilities -------------------------------------------

    #[test]
    fn parse_full_capabilities() {
        let resp = serde_json::json!({
            "mcp_capabilities": { "stdio": true, "http": true, "sse": true }
        });
        let caps = parse_acp_mcp_capabilities(&resp);
        assert_eq!(caps, all_caps());
    }

    #[test]
    fn parse_camel_case_key() {
        let resp = serde_json::json!({
            "mcpCapabilities": { "stdio": true, "http": false, "sse": true }
        });
        let caps = parse_acp_mcp_capabilities(&resp);
        assert!(caps.stdio);
        assert!(!caps.http);
        assert!(caps.sse);
    }

    #[test]
    fn parse_mcp_shorthand_key() {
        let resp = serde_json::json!({
            "mcp": { "stdio": false, "http": true, "sse": false }
        });
        let caps = parse_acp_mcp_capabilities(&resp);
        assert!(caps.stdio);
        assert!(caps.http);
        assert!(!caps.sse);
    }

    #[test]
    fn parse_missing_capabilities_returns_default() {
        let resp = serde_json::json!({ "other": "data" });
        let caps = parse_acp_mcp_capabilities(&resp);
        assert_eq!(caps, AcpMcpCapabilities::default());
    }

    #[test]
    fn parse_partial_capabilities_defaults_missing_to_false() {
        let resp = serde_json::json!({
            "mcp_capabilities": { "stdio": true }
        });
        let caps = parse_acp_mcp_capabilities(&resp);
        assert!(caps.stdio);
        assert!(!caps.http);
        assert!(!caps.sse);
    }

    #[test]
    fn parse_http_support_implies_stdio() {
        let resp = serde_json::json!({
            "mcp_capabilities": { "http": true, "sse": false }
        });
        let caps = parse_acp_mcp_capabilities(&resp);
        assert!(caps.stdio);
        assert!(caps.http);
        assert!(!caps.sse);
    }

    #[test]
    fn parse_priority_mcp_capabilities_over_mcp() {
        let resp = serde_json::json!({
            "mcp_capabilities": { "stdio": true, "http": true, "sse": true },
            "mcp": { "stdio": false, "http": false, "sse": false }
        });
        let caps = parse_acp_mcp_capabilities(&resp);
        assert_eq!(caps, all_caps());
    }

    // -- convert_server -------------------------------------------------------

    #[test]
    fn convert_stdio_server() {
        let server = make_server("test", true, stdio_transport("npx"));
        let result = convert_server(&server, &all_caps());
        assert!(result.is_some());
        let acp = result.unwrap();
        match acp {
            AcpSessionMcpServer::Stdio {
                name,
                command,
                args,
                env,
            } => {
                assert_eq!(name, "test");
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["-y", "@test/server"]);
                assert_eq!(env.len(), 1);
                assert_eq!(env[0].name, "NODE_ENV");
                assert_eq!(env[0].value, "production");
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn convert_http_server() {
        let server = make_server("http-test", true, http_transport("https://example.com/mcp"));
        let result = convert_server(&server, &all_caps());
        assert!(result.is_some());
        match result.unwrap() {
            AcpSessionMcpServer::Http { name, url, headers } => {
                assert_eq!(name, "http-test");
                assert_eq!(url, "https://example.com/mcp");
                assert_eq!(headers.len(), 1);
                assert_eq!(headers[0].name, "Authorization");
                assert_eq!(headers[0].value, "Bearer tok");
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn convert_sse_server() {
        let server = make_server("sse-test", true, sse_transport("https://example.com/sse"));
        let result = convert_server(&server, &all_caps());
        assert!(result.is_some());
        match result.unwrap() {
            AcpSessionMcpServer::Sse { name, url, headers, .. } => {
                assert_eq!(name, "sse-test");
                assert_eq!(url, "https://example.com/sse");
                assert!(headers.is_empty());
            }
            _ => panic!("expected Sse"),
        }
    }

    #[test]
    fn convert_skips_unsupported_transport() {
        let stdio_only = AcpMcpCapabilities {
            stdio: true,
            http: false,
            sse: false,
        };
        let http_server = make_server("http-test", true, http_transport("https://example.com/mcp"));
        assert!(convert_server(&http_server, &stdio_only).is_none());

        let sse_server = make_server("sse-test", true, sse_transport("https://example.com/sse"));
        assert!(convert_server(&sse_server, &stdio_only).is_none());
    }

    // -- build_session_mcp_servers --------------------------------------------

    #[test]
    fn build_filters_disabled_servers() {
        let servers = vec![
            make_server("enabled", true, stdio_transport("npx")),
            make_server("disabled", false, stdio_transport("node")),
        ];
        let result = build_session_mcp_servers(&servers, &all_caps());
        assert_eq!(result.len(), 1);
        match &result[0] {
            AcpSessionMcpServer::Stdio { name, .. } => assert_eq!(name, "enabled"),
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn build_filters_by_capabilities() {
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: false,
            sse: true,
        };
        let servers = vec![
            make_server("s1", true, stdio_transport("npx")),
            make_server("s2", true, http_transport("https://example.com/mcp")),
            make_server("s3", true, sse_transport("https://example.com/sse")),
        ];
        let result = build_session_mcp_servers(&servers, &caps);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn build_empty_servers_returns_empty() {
        let result = build_session_mcp_servers(&[], &all_caps());
        assert!(result.is_empty());
    }

    #[test]
    fn build_no_capabilities_returns_empty() {
        let no_caps = AcpMcpCapabilities {
            stdio: false,
            http: false,
            sse: false,
        };
        let servers = vec![
            make_server("s1", true, stdio_transport("npx")),
            make_server("s2", true, http_transport("https://example.com")),
        ];
        let result = build_session_mcp_servers(&servers, &no_caps);
        assert!(result.is_empty());
    }

    // -- build_builtin_image_gen_server ---------------------------------------

    #[test]
    fn builtin_image_gen_with_full_config() {
        let caps = all_caps();
        let config = ImageGenConfig {
            model: Some("dall-e-3".into()),
            api_url: Some("https://api.openai.com".into()),
            api_key: Some("sk-test".into()),
            size: Some("1024x1024".into()),
            quality: Some("hd".into()),
            style: Some("natural".into()),
        };
        let result = build_builtin_image_gen_server(&caps, "/usr/bin/img-gen", &config);
        assert!(result.is_some());
        match result.unwrap() {
            AcpSessionMcpServer::Stdio {
                name,
                command,
                args,
                env,
            } => {
                assert_eq!(name, "nomifun-image-generation");
                assert_eq!(command, "/usr/bin/img-gen");
                assert!(args.is_empty());
                assert_eq!(env.len(), 6);
                assert_eq!(env[0].name, "NOMIFUN_IMG_API_KEY");
                assert_eq!(env[0].value, "sk-test");
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn builtin_image_gen_with_partial_config() {
        let caps = all_caps();
        let config = ImageGenConfig {
            model: Some("dall-e-3".into()),
            api_url: Some("https://api.openai.com".into()),
            ..Default::default()
        };
        let result = build_builtin_image_gen_server(&caps, "img-gen", &config);
        assert!(result.is_some());
        match result.unwrap() {
            AcpSessionMcpServer::Stdio { env, .. } => {
                assert_eq!(env.len(), 2);
                // Sorted alphabetically: API_URL before MODEL
                assert_eq!(env[0].name, "NOMIFUN_IMG_API_URL");
                assert_eq!(env[0].value, "https://api.openai.com");
                assert_eq!(env[1].name, "NOMIFUN_IMG_MODEL");
                assert_eq!(env[1].value, "dall-e-3");
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn builtin_image_gen_no_stdio_returns_none() {
        let caps = AcpMcpCapabilities {
            stdio: false,
            http: true,
            sse: true,
        };
        let config = ImageGenConfig {
            model: Some("dall-e-3".into()),
            ..Default::default()
        };
        assert!(build_builtin_image_gen_server(&caps, "img-gen", &config).is_none());
    }

    #[test]
    fn builtin_image_gen_empty_command_returns_none() {
        let caps = all_caps();
        let config = ImageGenConfig::default();
        assert!(build_builtin_image_gen_server(&caps, "", &config).is_none());
    }

    #[test]
    fn builtin_image_gen_empty_config() {
        let caps = all_caps();
        let config = ImageGenConfig::default();
        let result = build_builtin_image_gen_server(&caps, "img-gen", &config);
        assert!(result.is_some());
        match result.unwrap() {
            AcpSessionMcpServer::Stdio { env, .. } => {
                assert!(env.is_empty());
            }
            _ => panic!("expected Stdio"),
        }
    }

    // -- hashmap_to_pairs -----------------------------------------------------

    #[test]
    fn hashmap_to_pairs_sorted() {
        let map = HashMap::from([
            ("Z_KEY".into(), "z_val".into()),
            ("A_KEY".into(), "a_val".into()),
            ("M_KEY".into(), "m_val".into()),
        ]);
        let pairs = hashmap_to_pairs(&map);
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0].name, "A_KEY");
        assert_eq!(pairs[1].name, "M_KEY");
        assert_eq!(pairs[2].name, "Z_KEY");
    }

    #[test]
    fn hashmap_to_pairs_empty() {
        let map = HashMap::new();
        let pairs = hashmap_to_pairs(&map);
        assert!(pairs.is_empty());
    }

    // -- Serialization roundtrip ----------------------------------------------

    #[test]
    fn stdio_serialization_roundtrip() {
        let server = AcpSessionMcpServer::Stdio {
            name: "test".into(),
            command: "npx".into(),
            args: vec!["-y".into()],
            env: vec![NameValuePair {
                name: "K".into(),
                value: "V".into(),
            }],
        };
        let json = serde_json::to_string(&server).unwrap();
        let parsed: AcpSessionMcpServer = serde_json::from_str(&json).unwrap();
        assert_eq!(server, parsed);
    }

    #[test]
    fn http_serialization_roundtrip() {
        let server = AcpSessionMcpServer::Http {
            name: "http-test".into(),
            url: "https://example.com/mcp".into(),
            headers: vec![NameValuePair {
                name: "Auth".into(),
                value: "Bearer tok".into(),
            }],
        };
        let json = serde_json::to_string(&server).unwrap();
        let parsed: AcpSessionMcpServer = serde_json::from_str(&json).unwrap();
        assert_eq!(server, parsed);
    }

    #[test]
    fn sse_serialization_roundtrip() {
        let server = AcpSessionMcpServer::Sse {
            name: "sse-test".into(),
            url: "https://example.com/sse".into(),
            headers: vec![],
        };
        let json = serde_json::to_string(&server).unwrap();
        assert!(!json.contains("headers")); // skip_serializing_if
        let parsed: AcpSessionMcpServer = serde_json::from_str(&json).unwrap();
        assert_eq!(server, parsed);
    }

    #[test]
    fn stdio_json_has_type_field() {
        let server = AcpSessionMcpServer::Stdio {
            name: "test".into(),
            command: "npx".into(),
            args: vec![],
            env: vec![],
        };
        let value: serde_json::Value = serde_json::to_value(&server).unwrap();
        assert_eq!(value["type"], "stdio");
        assert_eq!(value["name"], "test");
        assert_eq!(value["command"], "npx");
    }

    #[test]
    fn http_json_has_type_field() {
        let server = AcpSessionMcpServer::Http {
            name: "h".into(),
            url: "https://example.com".into(),
            headers: vec![],
        };
        let value: serde_json::Value = serde_json::to_value(&server).unwrap();
        assert_eq!(value["type"], "http");
    }

    #[test]
    fn sse_json_has_type_field() {
        let server = AcpSessionMcpServer::Sse {
            name: "s".into(),
            url: "https://example.com".into(),
            headers: vec![],
        };
        let value: serde_json::Value = serde_json::to_value(&server).unwrap();
        assert_eq!(value["type"], "sse");
    }
}
