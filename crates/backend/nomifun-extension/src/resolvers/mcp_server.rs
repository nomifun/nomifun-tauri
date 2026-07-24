use tracing::warn;

use crate::error::ExtensionError;
use crate::resolvers::extension_source_key;
use crate::types::{ExtMcpServer, ResolvedMcpServer};

/// Resolve a single MCP server contribution.
///
/// MCP server config is passed through as-is (opaque JSON).
pub fn resolve_mcp_server(
    server: &ExtMcpServer,
    extension_name: &str,
) -> Result<ResolvedMcpServer, ExtensionError> {
    let source_key = extension_source_key(extension_name, &server.source_key)?;
    if server.name.trim().is_empty() {
        return Err(ExtensionError::ResolutionFailed {
            extension_name: extension_name.to_owned(),
            reason: format!("MCP contribution '{}' must have a non-empty name", server.source_key),
        });
    }

    Ok(ResolvedMcpServer {
        extension_name: extension_name.to_owned(),
        source_key,
        name: server.name.clone(),
        description: server.description.clone(),
        config: server.config.clone(),
    })
}

/// Resolve all MCP server contributions from an extension.
pub fn resolve_mcp_servers(servers: &[ExtMcpServer], extension_name: &str) -> Vec<ResolvedMcpServer> {
    if servers.is_empty() {
        return Vec::new();
    }
    tracing::debug!(
        extension = extension_name,
        count = servers.len(),
        "Resolving MCP servers"
    );
    servers
        .iter()
        .filter_map(|server| {
            resolve_mcp_server(server, extension_name)
                .map_err(|error| {
                    warn!(
                        extension = extension_name,
                        server_source_key = server.source_key,
                        "Failed to resolve MCP server: {error}"
                    );
                    error
                })
                .ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_server() -> ExtMcpServer {
        ExtMcpServer {
            source_key: "test-mcp".into(),
            name: "Test MCP".into(),
            description: Some("A test MCP server".into()),
            config: serde_json::json!({
                "command": "npx",
                "args": ["-y", "test-server"]
            }),
        }
    }

    #[test]
    fn test_resolve_basic_mcp_server() {
        let server = make_server();
        let result = resolve_mcp_server(&server, "my-ext").unwrap();

        assert_eq!(result.extension_name, "my-ext");
        assert_eq!(result.source_key, "my-ext:test-mcp");
        assert_eq!(result.name, "Test MCP");
        assert_eq!(result.config["command"], "npx");
    }

    #[test]
    fn test_resolve_mcp_servers_empty() {
        let result = resolve_mcp_servers(&[], "my-ext");
        assert!(result.is_empty());
    }

    #[test]
    fn test_resolve_mcp_servers_multiple() {
        let servers = vec![make_server(), make_server()];
        let result = resolve_mcp_servers(&servers, "my-ext");
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_resolve_mcp_servers_skips_invalid_source_key() {
        let mut invalid = make_server();
        invalid.source_key = "0190f5fe-7c00-7a00-8000-000000000003".into();

        let result = resolve_mcp_servers(&[make_server(), invalid], "my-ext");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source_key, "my-ext:test-mcp");
    }
}
