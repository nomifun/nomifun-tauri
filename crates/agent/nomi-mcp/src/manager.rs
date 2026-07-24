use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use nomi_tools::ToolExecutionContext;
use serde_json::json;

use super::config::{McpServerConfig, TransportType};
use super::protocol::{
    ClientCapabilities, ClientInfo, InitializeParams, InitializeResult, JsonRpcRequest,
    McpResource, McpToolDef, McpToolResult, ResourcesListResult, ResourcesReadResult,
    ToolsListResult,
};
use super::transport::sse::SseTransport;
use super::transport::stdio::{ConnectionCleanupRegistry, StdioTransport};
use super::transport::streamable_http::StreamableHttpTransport;
use super::transport::{McpError, McpTransport};

/// Structured result of an MCP tool call. Inline artifacts remain separate
/// from model-facing text so the backend can validate and persist them before
/// publishing a successful tool receipt.
#[derive(Debug, Default, Clone)]
pub struct McpCallOutput {
    /// All text content joined with `\n` (preserves the pre-existing behaviour).
    pub text: String,
    /// Image/audio/file/resource content in order of appearance.
    pub artifacts: Vec<McpArtifactOut>,
    /// Protocol-level MCP `CallToolResult.isError` marker.
    pub is_error: bool,
}

/// A single inline artifact returned by an MCP tool call.
#[derive(Debug, Clone)]
pub struct McpArtifactOut {
    /// Raw base64 (straight from the server — not re-encoded).
    pub data: String,
    /// MIME type, e.g. "image/png", "audio/mpeg" or "application/pdf".
    pub mime_type: String,
    /// Original MCP locator for embedded resources/resource links.
    pub source_uri: Option<String>,
}

/// A connected MCP server with its discovered tools and capabilities
struct McpServer {
    #[allow(dead_code)]
    name: String,
    transport: Box<dyn McpTransport>,
    tools: Vec<McpToolDef>,
    /// Whether the server declared resources capability in its initialize response
    supports_resources: bool,
}

/// Manages connections to multiple MCP servers
pub struct McpManager {
    servers: HashMap<String, McpServer>,
    /// Includes registries from failed and timed-out stdio construction
    /// attempts. They remain joinable by exact manager shutdown even when no
    /// `McpServer` was published for the attempt.
    stdio_cleanup_registries: Vec<Arc<ConnectionCleanupRegistry>>,
    /// Monotonically increasing request ID counter for all JSON-RPC calls
    next_id: AtomicU64,
}

#[cfg(any(test, feature = "test-utils"))]
pub type TestMcpServerWithTools<'a> = (
    &'a str,
    bool,
    Vec<McpToolDef>,
    Box<dyn super::transport::McpTransport>,
);

/// Timeout for connecting + initializing a single MCP server (transport spawn,
/// `initialize` handshake, `tools/list`). Without it, a server that starts but
/// never answers the handshake would hang the entire Agent bootstrap — and thus
/// any Conversation that injects that server — indefinitely, with no error
/// surfaced to the user.
const MCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MCP_MAX_RESOURCE_URI_LEN: usize = 4096;
const MCP_MAX_DATA_RESOURCE_URI_LEN: usize = (20 * 1024 * 1024 * 4 / 3) + 1024;
const MCP_MAX_RESOURCE_NAME_LEN: usize = 512;

enum ValidatedResourceUri<'a> {
    Locator(&'a str),
    Inline { mime_type: String, data: &'a str },
}

fn is_valid_uri_scheme(scheme: &str) -> bool {
    scheme.bytes().enumerate().all(|(index, byte)| {
        byte.is_ascii_alphabetic()
            || (index > 0 && (byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')))
    })
}

/// Validate an MCP resource locator before it can enter tool output/history.
/// `data:` links are decoded later by the verified artifact store and therefore
/// never echoed into text. Ephemeral `blob:` URLs are not durable locators and
/// are rejected explicitly.
fn validate_resource_uri(uri: &str) -> Result<ValidatedResourceUri<'_>, McpError> {
    if uri.is_empty() || uri.trim() != uri || uri.chars().any(char::is_control) {
        return Err(McpError::Transport("MCP resource URI is empty or malformed".into()));
    }
    let (scheme, rest) = uri
        .split_once(':')
        .ok_or_else(|| McpError::Transport("MCP resource URI has no scheme".into()))?;
    if scheme.is_empty() || !is_valid_uri_scheme(scheme) {
        return Err(McpError::Transport("MCP resource URI has an invalid scheme".into()));
    }
    if scheme.eq_ignore_ascii_case("blob") {
        return Err(McpError::Transport(
            "MCP resource link uses an ephemeral blob: URI and cannot be delivered durably".into(),
        ));
    }
    if !scheme.eq_ignore_ascii_case("data") {
        if uri.len() > MCP_MAX_RESOURCE_URI_LEN {
            return Err(McpError::Transport(format!(
                "MCP resource URI exceeds the {MCP_MAX_RESOURCE_URI_LEN} byte limit"
            )));
        }
        if rest.is_empty() {
            return Err(McpError::Transport("MCP resource URI has an empty locator".into()));
        }
        return Ok(ValidatedResourceUri::Locator(uri));
    }

    if uri.len() > MCP_MAX_DATA_RESOURCE_URI_LEN {
        return Err(McpError::Transport(format!(
            "MCP data: resource URI exceeds the {MCP_MAX_DATA_RESOURCE_URI_LEN} byte limit"
        )));
    }

    let (header, data) = rest
        .split_once(',')
        .ok_or_else(|| McpError::Transport("MCP data: resource URI has no payload separator".into()))?;
    let header = header
        .strip_suffix(";base64")
        .ok_or_else(|| McpError::Transport("MCP data: resource URI must use base64 encoding".into()))?;
    if data.is_empty() {
        return Err(McpError::Transport("MCP data: resource URI has an empty payload".into()));
    }
    let mime_type = if header.trim().is_empty() {
        "text/plain".to_owned()
    } else {
        header.to_owned()
    };
    Ok(ValidatedResourceUri::Inline { mime_type, data })
}

impl McpManager {
    /// Connect to all configured MCP servers
    pub async fn connect_all(configs: &HashMap<String, McpServerConfig>) -> Result<Self, McpError> {
        let mut servers = HashMap::new();
        let mut stdio_cleanup_registries = Vec::new();

        for (name, config) in configs {
            let cleanup_registry = ConnectionCleanupRegistry::new();
            if matches!(config.transport, TransportType::Stdio) {
                stdio_cleanup_registries.push(Arc::clone(&cleanup_registry));
            }
            match tokio::time::timeout(
                MCP_CONNECT_TIMEOUT,
                Self::connect_server(name, config, cleanup_registry.clone()),
            )
            .await
            {
                Ok(Ok(server)) => {
                    tracing::info!(target: "nomi_mcp", server = %name, tools = server.tools.len(), resources = server.supports_resources, "mcp server connected");
                    servers.insert(name.clone(), server);
                }
                Ok(Err(e)) => {
                    // Non-fatal: continue with other servers
                    tracing::warn!(target: "nomi_mcp", server = %name, error = %e, "mcp server connection failed");
                    if let Err(cleanup_error) = cleanup_registry.wait_all().await {
                        tracing::error!(target: "nomi_mcp", server = %name, error = %cleanup_error, "failed MCP construction did not close exactly");
                    }
                }
                Err(_) => {
                    // Non-fatal: a hung handshake must not block the other
                    // servers or the agent bootstrap. Skip this server.
                    tracing::warn!(target: "nomi_mcp", server = %name, timeout_secs = MCP_CONNECT_TIMEOUT.as_secs(), "mcp server connection timed out");
                    if let Err(cleanup_error) = cleanup_registry.wait_all().await {
                        tracing::error!(target: "nomi_mcp", server = %name, error = %cleanup_error, "timed-out MCP construction did not close exactly");
                    }
                }
            }
        }

        Ok(Self {
            servers,
            stdio_cleanup_registries,
            next_id: AtomicU64::new(10),
        })
    }

    /// Connect to a single MCP server: create transport, initialize, discover tools
    async fn connect_server(
        name: &str,
        config: &McpServerConfig,
        cleanup_registry: Arc<ConnectionCleanupRegistry>,
    ) -> Result<McpServer, McpError> {
        let empty_map = HashMap::new();

        // 1. Create transport
        let transport: Box<dyn McpTransport> = match config.transport {
            TransportType::Stdio => {
                let command = config.command.as_deref().ok_or_else(|| {
                    McpError::InitFailed("stdio transport requires 'command'".into())
                })?;
                let args = config.args.as_deref().unwrap_or(&[]);
                let env = config.env.as_ref().unwrap_or(&empty_map);
                let init_params = InitializeParams {
                    protocol_version: "2025-03-26".to_string(),
                    capabilities: ClientCapabilities {
                        tools: Some(json!({})),
                    },
                    client_info: ClientInfo {
                        name: "nomi".to_string(),
                        version: "0.3.0".to_string(),
                    },
                };
                Box::new(
                    StdioTransport::spawn_with_cleanup_registry(
                        command,
                        args,
                        env,
                        init_params,
                        cleanup_registry,
                    )
                    .await?,
                )
            }
            TransportType::Sse => {
                let url = config
                    .url
                    .as_deref()
                    .ok_or_else(|| McpError::InitFailed("SSE transport requires 'url'".into()))?;
                let headers = config.headers.as_ref().unwrap_or(&empty_map);
                Box::new(SseTransport::connect(url, headers).await?)
            }
            TransportType::StreamableHttp => {
                let url = config.url.as_deref().ok_or_else(|| {
                    McpError::InitFailed("streamable-http transport requires 'url'".into())
                })?;
                let headers = config.headers.as_ref().unwrap_or(&empty_map);
                Box::new(StreamableHttpTransport::connect(url, headers).await?)
            }
        };

        // 2. Initialize handshake
        let init_params = InitializeParams {
            protocol_version: "2025-03-26".to_string(),
            capabilities: ClientCapabilities {
                tools: Some(json!({})),
            },
            client_info: ClientInfo {
                name: "nomi".to_string(),
                version: "0.3.0".to_string(),
            },
        };

        let init_req = JsonRpcRequest::new(
            1,
            "initialize",
            Some(serde_json::to_value(&init_params).map_err(|e| {
                McpError::InitFailed(format!("Failed to serialize init params: {}", e))
            })?),
        );

        let init_response = transport.request(&init_req).await?;
        let init_result: InitializeResult = serde_json::from_value(
            init_response
                .result
                .ok_or_else(|| McpError::InitFailed("No result in initialize response".into()))?,
        )
        .map_err(|e| McpError::InitFailed(format!("Failed to parse init result: {}", e)))?;

        // Check whether server declared resources capability
        let supports_resources = init_result
            .capabilities
            .get("resources")
            .map(|v| !v.is_null())
            .unwrap_or(false);

        // 3. Send initialized notification
        let initialized_notification =
            JsonRpcRequest::notification("notifications/initialized", None);
        transport.notify(&initialized_notification).await?;

        // 4. List tools
        let list_req = JsonRpcRequest::new(2, "tools/list", None);
        let list_response = transport.request(&list_req).await?;
        let tools_result: ToolsListResult = serde_json::from_value(
            list_response
                .result
                .ok_or_else(|| McpError::InitFailed("No result in tools/list response".into()))?,
        )
        .map_err(|e| McpError::InitFailed(format!("Failed to parse tools list: {}", e)))?;

        Ok(McpServer {
            name: name.to_string(),
            transport,
            tools: tools_result.tools,
            supports_resources,
        })
    }

    /// Get all discovered tools with their server names
    pub fn all_tools(&self) -> Vec<(&str, &McpToolDef)> {
        let mut result = Vec::new();
        for (server_name, server) in &self.servers {
            for tool in &server.tools {
                result.push((server_name.as_str(), tool));
            }
        }
        result
    }

    /// Execute a tool on a specific server.
    ///
    /// Returns a structured [`McpCallOutput`] keeping text and artifact content
    /// separate. Binary bytes are preserved until the workspace delivery
    /// boundary; resource locators are also retained as readable text.
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpCallOutput, McpError> {
        self.call_tool_inner(server_name, tool_name, arguments, None)
            .await
    }

    /// Execute a tool while carrying the engine-owned invocation identity in
    /// MCP `_meta`. The operation id is never added to model-visible arguments.
    pub async fn call_tool_with_context(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: serde_json::Value,
        context: &ToolExecutionContext,
    ) -> Result<McpCallOutput, McpError> {
        self.call_tool_inner(server_name, tool_name, arguments, Some(context))
            .await
    }

    async fn call_tool_inner(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: serde_json::Value,
        context: Option<&ToolExecutionContext>,
    ) -> Result<McpCallOutput, McpError> {
        let server = self
            .servers
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotFound(server_name.to_string()))?;

        // JSON-RPC ids correlate protocol responses only. They are unique per
        // manager but never serve as a durable business idempotency key.
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut request = JsonRpcRequest::new(
            request_id,
            "tools/call",
            Some(json!({
                "name": tool_name,
                "arguments": arguments
            })),
        );
        if let Some(context) = context {
            request = request.with_execution_operation_id(context.operation_id());
        }

        let response = server.transport.request(&request).await?;

        let result_value = response
            .result
            .ok_or_else(|| McpError::Transport("No result in tool call response".into()))?;

        // Parse result, keeping text and image content separate.
        let tool_result: McpToolResult = serde_json::from_value(result_value)
            .map_err(|e| McpError::Transport(format!("Failed to parse tool result: {}", e)))?;

        let mut out = McpCallOutput::default();
        out.is_error = tool_result.is_error;
        let mut text_parts: Vec<String> = Vec::new();
        for content in &tool_result.content {
            match content {
                super::protocol::McpContent::Text { text } => text_parts.push(text.clone()),
                super::protocol::McpContent::Image { data, mime_type } => {
                    out.artifacts.push(McpArtifactOut {
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                        source_uri: None,
                    });
                }
                super::protocol::McpContent::Audio { data, mime_type } => {
                    out.artifacts.push(McpArtifactOut {
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                        source_uri: None,
                    });
                }
                super::protocol::McpContent::Resource { resource } => {
                    let locator = match validate_resource_uri(&resource.uri)? {
                        ValidatedResourceUri::Locator(locator) => locator,
                        ValidatedResourceUri::Inline { .. } => {
                            return Err(McpError::Transport(
                                "embedded MCP resources must carry bytes in text/blob, not duplicate them in a data: URI"
                                    .into(),
                            ));
                        }
                    };
                    let payload = match (&resource.text, &resource.blob) {
                        (Some(text), None) => base64::engine::general_purpose::STANDARD
                            .encode(text.as_bytes()),
                        (None, Some(blob)) => blob.clone(),
                        (Some(_), Some(_)) => {
                            return Err(McpError::Transport(format!(
                                "MCP resource '{}' contains both text and blob payloads",
                                resource.uri
                            )));
                        }
                        (None, None) => {
                            return Err(McpError::Transport(format!(
                                "MCP resource '{}' contains no text or blob payload",
                                resource.uri
                            )));
                        }
                    };
                    let mime_type = resource.mime_type.clone().unwrap_or_else(|| {
                        if resource.text.is_some() {
                            "text/plain".to_owned()
                        } else {
                            "application/octet-stream".to_owned()
                        }
                    });
                    text_parts.push(format!("Embedded resource: {locator}"));
                    out.artifacts.push(McpArtifactOut {
                        data: payload,
                        mime_type,
                        source_uri: Some(locator.to_owned()),
                    });
                }
                super::protocol::McpContent::ResourceLink {
                    uri,
                    name,
                    title: _,
                    description: _,
                    mime_type,
                    size: _,
                } => {
                    if name.trim().is_empty()
                        || name.len() > MCP_MAX_RESOURCE_NAME_LEN
                        || name.chars().any(char::is_control)
                    {
                        return Err(McpError::Transport(format!(
                            "MCP resource link name is empty or exceeds the {MCP_MAX_RESOURCE_NAME_LEN} byte limit"
                        )));
                    }
                    match validate_resource_uri(uri)? {
                        ValidatedResourceUri::Inline {
                            mime_type: inline_mime,
                            data,
                        } => {
                            if let Some(declared) = mime_type.as_deref()
                                && declared
                                    .split(';')
                                    .next()
                                    .unwrap_or_default()
                                    .trim()
                                    .to_ascii_lowercase()
                                    != inline_mime
                                        .split(';')
                                        .next()
                                        .unwrap_or_default()
                                        .trim()
                                        .to_ascii_lowercase()
                            {
                                return Err(McpError::Transport(
                                    "MCP data: resource link MIME does not match mimeType".into(),
                                ));
                            }
                            text_parts.push(format!(
                                "Inline resource materialized: {name} ({inline_mime})"
                            ));
                            out.artifacts.push(McpArtifactOut {
                                data: data.to_owned(),
                                mime_type: inline_mime,
                                // Never retain/echo a data URI containing base64.
                                source_uri: None,
                            });
                        }
                        ValidatedResourceUri::Locator(locator) => {
                            // A JSON descriptor of a URL is not the remote
                            // artifact. Counting it as a verified receipt would
                            // recreate false success. Do not blindly GET here,
                            // either: MCP links can require server-local auth and
                            // untrusted URLs would introduce SSRF. Require the
                            // server to return embedded bytes or a data: URI.
                            return Err(McpError::Transport(format!(
                                "MCP resource link '{name}' was not materialized by the server: {locator}. Return embedded bytes or a data: URI before reporting success"
                            )));
                        }
                    }
                }
            }
        }
        out.text = text_parts.join("\n");

        Ok(out)
    }

    /// Get names of all connected servers.
    pub fn server_names(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    /// Check if a connected server declared the resources capability.
    pub fn server_supports_resources(&self, server_name: &str) -> bool {
        self.servers
            .get(server_name)
            .map(|s| s.supports_resources)
            .unwrap_or(false)
    }

    /// List all resources from a server.
    pub async fn list_resources(&self, server_name: &str) -> Result<Vec<McpResource>, McpError> {
        let server = self
            .servers
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotFound(server_name.to_string()))?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest::new(id, "resources/list", None);
        let response = server.transport.request(&request).await?;

        let result_value = response
            .result
            .ok_or_else(|| McpError::Transport("No result in resources/list response".into()))?;

        let list_result: ResourcesListResult = serde_json::from_value(result_value)
            .map_err(|e| McpError::Transport(format!("Failed to parse resources/list: {}", e)))?;

        Ok(list_result.resources)
    }

    /// Read a single resource by URI from a server. Returns the text content.
    pub async fn read_resource(&self, server_name: &str, uri: &str) -> Result<String, McpError> {
        let server = self
            .servers
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotFound(server_name.to_string()))?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest::new(id, "resources/read", Some(json!({ "uri": uri })));
        let response = server.transport.request(&request).await?;

        let result_value = response
            .result
            .ok_or_else(|| McpError::Transport("No result in resources/read response".into()))?;

        let read_result: ResourcesReadResult = serde_json::from_value(result_value)
            .map_err(|e| McpError::Transport(format!("Failed to parse resources/read: {}", e)))?;

        // Return the first text content found
        read_result
            .contents
            .into_iter()
            .find_map(|c| c.text)
            .ok_or_else(|| McpError::Transport(format!("No text content in resource '{}'", uri)))
    }

    /// Gracefully shutdown all servers
    pub async fn shutdown(&self) -> Result<(), McpError> {
        let mut failures = Vec::new();
        for (name, server) in &self.servers {
            if let Err(e) = server.transport.close().await {
                tracing::warn!(target: "nomi_mcp", server = %name, error = %e, "error closing mcp server");
                failures.push(format!("{name}: {e}"));
            }
        }
        for (index, cleanup_registry) in self.stdio_cleanup_registries.iter().enumerate() {
            if let Err(error) = cleanup_registry.wait_all().await {
                failures.push(format!("stdio cleanup registry {index}: {error}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(McpError::Transport(format!(
                "one or more MCP servers failed exact shutdown: {}",
                failures.join(" | ")
            )))
        }
    }

    /// Test-only constructor: build a manager from pre-configured servers.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn new_for_test(
        entries: Vec<(&str, bool, Box<dyn super::transport::McpTransport>)>,
    ) -> Self {
        let mut servers = HashMap::new();
        for (name, supports_resources, transport) in entries {
            servers.insert(
                name.to_string(),
                McpServer {
                    name: name.to_string(),
                    transport,
                    tools: vec![],
                    supports_resources,
                },
            );
        }
        Self {
            servers,
            stdio_cleanup_registries: Vec::new(),
            next_id: AtomicU64::new(10),
        }
    }

    /// Test-only constructor with an already-discovered tool catalog.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn new_for_test_with_tools(
        entries: Vec<TestMcpServerWithTools<'_>>,
    ) -> Self {
        let mut servers = HashMap::new();
        for (name, supports_resources, tools, transport) in entries {
            servers.insert(
                name.to_string(),
                McpServer {
                    name: name.to_string(),
                    transport,
                    tools,
                    supports_resources,
                },
            );
        }
        Self {
            servers,
            stdio_cleanup_registries: Vec::new(),
            next_id: AtomicU64::new(10),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::JsonRpcResponse;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    // -----------------------------------------------------------------------
    // MockTransport: returns pre-configured JSON-RPC responses
    // -----------------------------------------------------------------------

    struct MockTransport {
        /// Responses returned in order for each request call
        responses: Mutex<Vec<serde_json::Value>>,
    }

    impl MockTransport {
        fn new(responses: Vec<serde_json::Value>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl McpTransport for MockTransport {
        async fn request(&self, _req: &JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
            let mut guard = self.responses.lock().unwrap();
            let value = if guard.is_empty() {
                json!(null)
            } else {
                guard.remove(0)
            };
            Ok(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: Some(1),
                result: Some(value),
                error: None,
            })
        }

        async fn notify(&self, _req: &JsonRpcRequest) -> Result<(), McpError> {
            Ok(())
        }

        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
    }

    struct RecordingTransport {
        requests: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    #[async_trait]
    impl McpTransport for RecordingTransport {
        async fn request(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
            self.requests
                .lock()
                .unwrap()
                .push(serde_json::to_value(req).unwrap());
            Ok(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: req.id,
                result: Some(json!({"content": []})),
                error: None,
            })
        }

        async fn notify(&self, _req: &JsonRpcRequest) -> Result<(), McpError> {
            Ok(())
        }

        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
    }

    struct ErrorTransport;

    #[async_trait]
    impl McpTransport for ErrorTransport {
        async fn request(&self, _req: &JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
            Err(McpError::Transport("mock transport error".into()))
        }

        async fn notify(&self, _req: &JsonRpcRequest) -> Result<(), McpError> {
            Ok(())
        }

        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // Test helpers: build McpManager with pre-configured servers
    // -----------------------------------------------------------------------

    fn make_manager_with_servers(entries: Vec<(&str, bool, Box<dyn McpTransport>)>) -> McpManager {
        McpManager::new_for_test(entries)
    }

    #[tokio::test]
    async fn call_tool_uses_protocol_ids_only_for_correlation_and_propagates_context() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let manager = make_manager_with_servers(vec![(
            "srv",
            false,
            Box::new(RecordingTransport {
                requests: Arc::clone(&requests),
            }),
        )]);
        let context =
            ToolExecutionContext::from_scoped_tool_call("conversation-a:turn-1", "call_0");

        manager
            .call_tool_with_context(
                "srv",
                "nomi_send_to_conversation",
                json!({
                    "message": "first",
                    "_meta": {
                        crate::protocol::NOMIFUN_EXECUTION_OPERATION_META_KEY: "forged"
                    }
                }),
                &context,
            )
            .await
            .unwrap();
        manager
            .call_tool_with_context(
                "srv",
                "nomi_send_to_conversation",
                json!({"message": "retry"}),
                &context,
            )
            .await
            .unwrap();

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["id"], 10);
        assert_eq!(requests[1]["id"], 11);
        assert_ne!(requests[0]["id"], 0);
        assert_ne!(requests[1]["id"], 0);
        for request in requests.iter() {
            assert_eq!(
                request["params"]["_meta"]
                    [crate::protocol::NOMIFUN_EXECUTION_OPERATION_META_KEY],
                context.operation_id()
            );
        }
        assert_eq!(
            requests[0]["params"]["arguments"]["_meta"]
                [crate::protocol::NOMIFUN_EXECUTION_OPERATION_META_KEY],
            "forged"
        );
    }

    // -----------------------------------------------------------------------
    // TC-2.x: server_supports_resources [黑盒 + 白盒]
    // -----------------------------------------------------------------------

    #[test]
    fn tc_2_1_server_supports_resources_true() {
        // [黑盒] TC-2.1: server with resources capability returns true
        let manager = make_manager_with_servers(vec![(
            "test-server",
            true,
            Box::new(MockTransport::new(vec![])),
        )]);

        assert!(manager.server_supports_resources("test-server"));
    }

    #[test]
    fn tc_2_2_server_supports_resources_false() {
        // [黑盒] TC-2.2: server without resources capability returns false
        let manager = make_manager_with_servers(vec![(
            "no-resources-server",
            false,
            Box::new(MockTransport::new(vec![])),
        )]);

        assert!(!manager.server_supports_resources("no-resources-server"));
    }

    #[test]
    fn tc_2_3_server_supports_resources_unknown_server() {
        // [黑盒] TC-2.3: unknown server name returns false (not error)
        let manager = make_manager_with_servers(vec![]);

        assert!(!manager.server_supports_resources("unknown-server"));
    }

    #[test]
    fn tc_2_wb_supports_resources_from_capabilities_null_value() {
        // [白盒] capabilities.get("resources") = null → supports_resources = false
        // This is tested via the parsed field; we verify via make_manager helper
        let manager = make_manager_with_servers(vec![(
            "server",
            false, // null resources → false per impl: !v.is_null() = false
            Box::new(MockTransport::new(vec![])),
        )]);

        assert!(!manager.server_supports_resources("server"));
    }

    // -----------------------------------------------------------------------
    // TC-2.10/2.11: server_names [黑盒]
    // -----------------------------------------------------------------------

    #[test]
    fn tc_2_10_server_names_returns_all() {
        // [黑盒] TC-2.10: server_names returns all connected server names
        let manager = make_manager_with_servers(vec![
            ("server-a", false, Box::new(MockTransport::new(vec![]))),
            ("server-b", true, Box::new(MockTransport::new(vec![]))),
        ]);

        let mut names = manager.server_names();
        names.sort();
        assert_eq!(names, vec!["server-a", "server-b"]);
    }

    #[test]
    fn tc_2_11_server_names_empty_manager() {
        // [黑盒] TC-2.11: no connected servers → empty vec
        let manager = make_manager_with_servers(vec![]);

        assert!(manager.server_names().is_empty());
    }

    #[test]
    fn tc_2_wb_server_names_returns_owned_strings() {
        // [白盒] Decision 1: server_names() returns Vec<String> not Vec<&str>
        let manager = make_manager_with_servers(vec![(
            "my-server",
            false,
            Box::new(MockTransport::new(vec![])),
        )]);

        let names: Vec<String> = manager.server_names();
        assert_eq!(names, vec!["my-server"]);
    }

    // -----------------------------------------------------------------------
    // TC-2.4/2.5: list_resources [黑盒]
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn tc_2_4_list_resources_normal() {
        // [黑盒] TC-2.4: list_resources returns resources from server
        let resources_response = json!({
            "resources": [
                {"uri": "skill://skill-a"},
                {"uri": "skill://skill-b", "name": "Skill B"}
            ]
        });

        let manager = make_manager_with_servers(vec![(
            "test-server",
            true,
            Box::new(MockTransport::new(vec![resources_response])),
        )]);

        let result = manager.list_resources("test-server").await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].uri, "skill://skill-a");
        assert_eq!(result[1].uri, "skill://skill-b");
    }

    #[tokio::test]
    async fn tc_2_5_list_resources_empty() {
        // [黑盒] TC-2.5: list_resources returns empty list when server has no resources
        let resources_response = json!({"resources": []});

        let manager = make_manager_with_servers(vec![(
            "test-server",
            true,
            Box::new(MockTransport::new(vec![resources_response])),
        )]);

        let result = manager.list_resources("test-server").await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn tc_2_6_list_resources_server_not_found() {
        // [黑盒] TC-2.6: list_resources returns error when server does not exist
        let manager = make_manager_with_servers(vec![]);

        let result = manager.list_resources("nonexistent").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            McpError::ServerNotFound(name) => assert_eq!(name, "nonexistent"),
            e => panic!("expected ServerNotFound, got {:?}", e),
        }
    }

    // -----------------------------------------------------------------------
    // TC-2.7/2.8/2.9: read_resource [黑盒 + 白盒]
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn tc_2_7_read_resource_returns_text() {
        // [黑盒] TC-2.7: read_resource returns text content
        let read_response = json!({
            "contents": [{"uri": "skill://my-skill", "mimeType": "text/plain", "text": "---\ndescription: A skill\n---\n# My Skill\n"}]
        });

        let manager = make_manager_with_servers(vec![(
            "test-server",
            true,
            Box::new(MockTransport::new(vec![read_response])),
        )]);

        let result = manager
            .read_resource("test-server", "skill://my-skill")
            .await
            .unwrap();
        assert!(result.contains("description: A skill"));
    }

    #[tokio::test]
    async fn tc_2_8_read_resource_transport_error() {
        // [黑盒] TC-2.8: read_resource returns error when server returns transport error
        let manager =
            make_manager_with_servers(vec![("test-server", true, Box::new(ErrorTransport))]);

        let result = manager
            .read_resource("test-server", "skill://nonexistent")
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn tc_2_9_read_resource_server_not_found() {
        // [黑盒] TC-2.9: read_resource returns error when server does not exist
        let manager = make_manager_with_servers(vec![]);

        let result = manager
            .read_resource("nonexistent", "skill://my-skill")
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            McpError::ServerNotFound(name) => assert_eq!(name, "nonexistent"),
            e => panic!("expected ServerNotFound, got {:?}", e),
        }
    }

    #[tokio::test]
    async fn tc_2_wb_read_resource_no_text_content_returns_error() {
        // [白盒] Decision 3: find_map returns None when all contents have text=None → error
        let read_response = json!({
            "contents": [{"uri": "skill://binary", "mimeType": "application/octet-stream"}]
        });

        let manager = make_manager_with_servers(vec![(
            "test-server",
            true,
            Box::new(MockTransport::new(vec![read_response])),
        )]);

        let result = manager.read_resource("test-server", "skill://binary").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn tc_2_wb_read_resource_find_map_first_text() {
        // [白盒] Decision 3: find_map returns first content with non-None text
        let read_response = json!({
            "contents": [
                {"uri": "skill://x"},
                {"uri": "skill://x", "text": "actual content"}
            ]
        });

        let manager = make_manager_with_servers(vec![(
            "test-server",
            true,
            Box::new(MockTransport::new(vec![read_response])),
        )]);

        let result = manager
            .read_resource("test-server", "skill://x")
            .await
            .unwrap();
        assert_eq!(result, "actual content");
    }

    #[test]
    fn tc_2_wb_next_id_starts_at_10() {
        // [白盒] Decision 4: AtomicU64 counter starts at 10 to avoid conflict with connect_server IDs 1/2
        let manager = make_manager_with_servers(vec![]);
        // next_id is private — we verify by doing two fetch_adds and checking values are 10 and 11
        let id1 = manager
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id2 = manager
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(id1, 10, "first ID should be 10");
        assert_eq!(id2, 11, "second ID should be 11");
    }

    // -----------------------------------------------------------------------
    // call_tool: image content passthrough into McpCallOutput
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn call_tool_preserves_image_data_and_mime() {
        // tools/call response: one text block + one png image
        let resp = json!({ "content": [
            {"type":"text","text":"done"},
            {"type":"image","data":"AAAAbase64==","mimeType":"image/png"}
        ]});
        let mgr = make_manager_with_servers(vec![(
            "srv",
            false,
            Box::new(MockTransport::new(vec![resp])),
        )]);
        let out = mgr.call_tool("srv", "shot", json!({})).await.unwrap();
        assert_eq!(out.text, "done");
        assert_eq!(out.artifacts.len(), 1);
        assert_eq!(out.artifacts[0].data, "AAAAbase64==");
        assert_eq!(out.artifacts[0].mime_type, "image/png");
    }

    #[tokio::test]
    async fn call_tool_text_only_has_no_images() {
        // Pure-text result must not regress: text preserved, no images.
        let resp = json!({ "content": [{"type":"text","text":"hello"}] });
        let mgr = make_manager_with_servers(vec![(
            "srv",
            false,
            Box::new(MockTransport::new(vec![resp])),
        )]);
        let out = mgr.call_tool("srv", "echo", json!({})).await.unwrap();
        assert_eq!(out.text, "hello");
        assert!(out.artifacts.is_empty());
    }

    #[tokio::test]
    async fn call_tool_preserves_mcp_is_error_flag() {
        let resp = json!({
            "content": [{"type":"text","text":"invalid arguments: missing kb_id"}],
            "isError": true
        });
        let mgr = make_manager_with_servers(vec![(
            "srv",
            false,
            Box::new(MockTransport::new(vec![resp])),
        )]);

        let out = mgr.call_tool("srv", "update_base", json!({})).await.unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("missing kb_id"));
    }

    #[tokio::test]
    async fn call_tool_defaults_missing_is_error_to_success() {
        let resp = json!({
            "content": [{"type":"text","text":"Error: ordinary text"}]
        });
        let mgr = make_manager_with_servers(vec![(
            "srv",
            false,
            Box::new(MockTransport::new(vec![resp])),
        )]);

        let out = mgr.call_tool("srv", "echo", json!({})).await.unwrap();
        assert!(!out.is_error);
        assert_eq!(out.text, "Error: ordinary text");
    }

    #[tokio::test]
    async fn call_tool_preserves_images_and_embedded_resource_without_placeholder() {
        // Multiple images interleaved with text + a real embedded resource.
        let resp = json!({ "content": [
            {"type":"image","data":"img1","mimeType":"image/png"},
            {"type":"text","text":"between"},
            {"type":"image","data":"img2","mimeType":"image/jpeg"},
            {"type":"resource","resource":{"uri":"x://y","mimeType":"text/plain","text":"report body"}}
        ]});
        let mgr = make_manager_with_servers(vec![(
            "srv",
            false,
            Box::new(MockTransport::new(vec![resp])),
        )]);
        let out = mgr.call_tool("srv", "t", json!({})).await.unwrap();
        assert_eq!(out.artifacts.len(), 3);
        assert_eq!(out.artifacts[0].mime_type, "image/png");
        assert_eq!(out.artifacts[1].mime_type, "image/jpeg");
        assert_eq!(out.artifacts[2].mime_type, "text/plain");
        assert_eq!(out.artifacts[2].source_uri.as_deref(), Some("x://y"));
        assert!(out.text.contains("between"));
        assert!(out.text.contains("Embedded resource: x://y"));
        assert!(!out.text.contains("[resource]"));
        assert!(!out.text.contains("[image"));
    }

    #[tokio::test]
    async fn call_tool_rejects_unmaterialized_remote_resource_link() {
        let resp = json!({ "content": [
            {"type":"audio","data":"SUQzYXVkaW8=","mimeType":"audio/mpeg"},
            {"type":"resource_link","uri":"https://example.test/report.pdf","name":"report.pdf","mimeType":"application/pdf","size":42}
        ]});
        let mgr = make_manager_with_servers(vec![(
            "srv",
            false,
            Box::new(MockTransport::new(vec![resp])),
        )]);

        let error = mgr.call_tool("srv", "export", json!({})).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("was not materialized"));
        assert!(message.contains("https://example.test/report.pdf"));
    }

    #[tokio::test]
    async fn malformed_embedded_resource_is_an_explicit_error() {
        let resp = json!({ "content": [
            {"type":"resource","resource":{"uri":"x://missing"}}
        ]});
        let mgr = make_manager_with_servers(vec![(
            "srv",
            false,
            Box::new(MockTransport::new(vec![resp])),
        )]);

        let error = mgr.call_tool("srv", "export", json!({})).await.unwrap_err();
        assert!(error.to_string().contains("contains no text or blob payload"));
    }

    #[tokio::test]
    async fn data_resource_link_is_materialized_without_echoing_base64_uri() {
        let resp = json!({ "content": [
            {"type":"resource_link","uri":"data:text/plain;base64,aGVsbG8=","name":"hello.txt","mimeType":"text/plain"}
        ]});
        let mgr = make_manager_with_servers(vec![(
            "srv",
            false,
            Box::new(MockTransport::new(vec![resp])),
        )]);

        let out = mgr.call_tool("srv", "export", json!({})).await.unwrap();

        assert_eq!(out.artifacts.len(), 1);
        assert_eq!(out.artifacts[0].mime_type, "text/plain");
        assert_eq!(out.artifacts[0].data, "aGVsbG8=");
        assert!(out.artifacts[0].source_uri.is_none());
        assert!(out.text.contains("Inline resource materialized"));
        assert!(!out.text.contains("data:"));
        assert!(!out.text.contains("aGVsbG8="));
    }

    #[tokio::test]
    async fn empty_ephemeral_and_oversized_resource_links_are_rejected() {
        for uri in [
            String::new(),
            "blob:https://example.test/temporary".to_owned(),
            format!("custom:{}", "x".repeat(MCP_MAX_RESOURCE_URI_LEN)),
        ] {
            let resp = json!({ "content": [
                {"type":"resource_link","uri":uri,"name":"report"}
            ]});
            let mgr = make_manager_with_servers(vec![(
                "srv",
                false,
                Box::new(MockTransport::new(vec![resp])),
            )]);

            assert!(mgr.call_tool("srv", "export", json!({})).await.is_err());
        }
    }
}
