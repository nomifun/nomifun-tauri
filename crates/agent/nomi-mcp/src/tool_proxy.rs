use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::config::McpServerConfig;
use super::manager::{McpCallOutput, McpManager};
use super::protocol::ToolAnnotations;
use super::transport::McpError;
use nomi_protocol::events::ToolCategory;
use nomi_tools::{Tool, ToolExecutionContext};
use nomi_types::tool::{JsonSchema, ToolArtifact, ToolImage, ToolResult};

/// Upper bound on a single MCP image's decoded byte size before it is dropped.
///
/// Browser/Playwright screenshots routinely run 1–5 MB; routing one verbatim
/// into the message history would balloon the context. Aligned with
/// `nomi-tools` `read.rs` `MAX_IMAGE_BYTES` (5 MiB). We estimate the decoded
/// size from the base64 length (decoded ≈ len * 3 / 4) to avoid decoding the
/// whole payload just to measure it.
const MCP_MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Non-image inline resources are never sent to a provider as visual context,
/// but the backend artifact store accepts them up to this bounded size.
const MCP_MAX_ARTIFACT_BYTES: usize = 20 * 1024 * 1024;
const MCP_PROVIDER_NAME_PREFIX: &str = "mcp__";
const MAX_PROVIDER_TOOL_NAME_LEN: usize = 64;
const MCP_DISPLAY_SEPARATOR: &str = "__";
/// 80 bits is ample origin disambiguation while leaving most of the provider
/// name available for a human-readable `server__tool` slug.
const MCP_DISPLAY_HASH_LEN: usize = 16;
const MCP_DISPLAY_SLUG_LEN: usize = MAX_PROVIDER_TOOL_NAME_LEN
    - MCP_PROVIDER_NAME_PREFIX.len()
    - MCP_DISPLAY_SEPARATOR.len()
    - MCP_DISPLAY_HASH_LEN;

/// Estimate the decoded byte length of a (possibly padded) base64 string
/// without allocating/decoding it. Standard base64 encodes every 3 bytes as 4
/// characters; trailing `=` padding marks 1–2 missing bytes. Whitespace and a
/// `data:...;base64,` prefix (if any) are ignored so we measure the payload.
fn decoded_base64_len(data: &str) -> usize {
    // Strip a data-URL prefix if a server happened to send one.
    let payload = match data.split_once(";base64,") {
        Some((_, b64)) => b64,
        None => data,
    };
    let mut chars = 0usize;
    let mut padding = 0usize;
    for b in payload.bytes() {
        match b {
            b'=' => padding += 1,
            b if b.is_ascii_whitespace() => {}
            _ => chars += 1,
        }
    }
    let total_units = chars + padding;
    // Each 4-char group decodes to 3 bytes; subtract for padding.
    let bytes = total_units / 4 * 3;
    bytes.saturating_sub(padding.min(2))
}

/// Wraps an MCP server tool as a local Tool trait implementation. Every MCP
/// tool uses an origin-derived canonical provider name, so transcripts remain
/// routable even when server registration order changes between sessions.
pub struct McpToolProxy {
    /// Canonical origin-derived name exposed to the model/provider.
    display_name: String,
    /// Stable deferred-activation identity, independent of display collisions.
    activation_identity: String,
    /// Complete origin identity used for artifact contract classification. This
    /// must never use the bounded provider display alias because a long server
    /// name can truncate the semantic tool suffix.
    artifact_identity: String,
    /// Original tool name on the MCP server
    tool_name: String,
    /// Server this tool belongs to
    server_name: String,
    description: String,
    input_schema: JsonSchema,
    manager: Arc<McpManager>,
    /// Whether this tool's schema should be deferred (sent as name-only stub).
    deferred: bool,
    /// MCP behaviour hints used to derive the approval category. `None` means
    /// the server declared no annotations → safe default (`Exec`, needs approval).
    annotations: Option<ToolAnnotations>,
}

impl McpToolProxy {
    pub fn new(
        tool_name: String,
        server_name: String,
        description: String,
        input_schema: JsonSchema,
        manager: Arc<McpManager>,
        deferred: bool,
        annotations: Option<ToolAnnotations>,
    ) -> Self {
        let display_name = canonical_mcp_display_name(&server_name, &tool_name);
        let activation_identity = canonical_mcp_tool_identity(&server_name, &tool_name);
        let artifact_identity = format!("{server_name}__{tool_name}");
        Self {
            display_name,
            activation_identity,
            artifact_identity,
            tool_name,
            server_name,
            description,
            input_schema,
            manager,
            deferred,
            annotations,
        }
    }

    /// Map MCP annotations to an approval [`ToolCategory`].
    ///
    /// Rule (mirrors codex `requires_mcp_tool_approval`, collapsed onto nomi's
    /// Info/Exec axis):
    /// - `readOnlyHint == Some(true)` → [`ToolCategory::Info`] (approval-free).
    /// - everything else — `destructiveHint`, no hints, or an old server with no
    ///   `annotations` block at all — → [`ToolCategory::Exec`] (needs approval).
    ///
    /// The from-strict default is deliberate: an unannotated tool could mutate
    /// the world, so we never silently auto-approve it.
    fn category_from_annotations(&self) -> ToolCategory {
        if self.is_read_only() {
            ToolCategory::Info
        } else {
            ToolCategory::Exec
        }
    }

    /// Whether the tool declared `readOnlyHint == true` (no side effects).
    /// Drives both the approval category and concurrency-safety.
    fn is_read_only(&self) -> bool {
        self.annotations
            .as_ref()
            .and_then(|a| a.read_only_hint)
            .unwrap_or(false)
    }

    /// Image-generation tools have a stronger success contract than ordinary
    /// MCP calls: a text acknowledgement without an image is a failed result.
    fn expects_image_output(&self) -> bool {
        let server = self.server_name.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        let tool = self.tool_name.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        server == "nomifun_image_generation"
            || matches!(
                tool.as_str(),
                "image_gen" | "image_generation" | "generate_image" | "create_image"
            )
    }

    fn render_call_result(
        &self,
        call: Result<McpCallOutput, McpError>,
    ) -> ToolResult {
        match call {
            Ok(out) => {
                let mut text = out.text;
                let is_error = out.is_error;
                let mut artifacts: Vec<ToolArtifact> =
                    Vec::with_capacity(out.artifacts.len());
                let mut delivery_error = None;

                for artifact in out.artifacts {
                    let decoded_len = decoded_base64_len(&artifact.data);
                    let limit = if artifact.mime_type.starts_with("image/") {
                        MCP_MAX_IMAGE_BYTES
                    } else {
                        MCP_MAX_ARTIFACT_BYTES
                    };
                    if decoded_len > limit {
                        tracing::warn!(
                            target: "nomi_mcp",
                            server = %self.server_name,
                            tool = %self.tool_name,
                            mime_type = %artifact.mime_type,
                            bytes = decoded_len,
                            limit,
                            "rejecting oversized MCP artifact"
                        );
                        delivery_error = Some(format!(
                            "MCP artifact {} is {decoded_len} bytes, above the {limit} byte limit",
                            artifact.mime_type
                        ));
                        break;
                    }

                    artifacts.push(ToolImage {
                        media_type: artifact.mime_type,
                        data: artifact.data,
                    });
                }

                if let Some(error) = delivery_error {
                    if !text.trim().is_empty() {
                        text.push('\n');
                    }
                    text.push_str("Artifact delivery rejected: ");
                    text.push_str(&error);
                    return ToolResult::error(text);
                }

                let contains_image = artifacts
                    .iter()
                    .any(|artifact| artifact.media_type.starts_with("image/"));
                let result = if is_error {
                    ToolResult::error(text)
                } else if self.expects_image_output() && !contains_image {
                    let detail = if text.trim().is_empty() {
                        "the MCP server returned no content".to_owned()
                    } else {
                        format!("server response: {text}")
                    };
                    ToolResult::error(format!(
                        "Image generation failed: no image artifact was returned ({detail})."
                    ))
                } else {
                    ToolResult::text(text)
                };
                if artifacts.is_empty() {
                    result
                } else {
                    result.with_artifacts(artifacts)
                }
            }
            Err(error) => ToolResult::error(format!("MCP tool error: {error}")),
        }
    }
}

#[async_trait]
impl Tool for McpToolProxy {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn activation_identity(&self) -> &str {
        &self.activation_identity
    }

    fn artifact_identity(&self) -> &str {
        &self.artifact_identity
    }

    fn reserved_provider_name_prefix(&self) -> Option<&'static str> {
        Some(MCP_PROVIDER_NAME_PREFIX)
    }

    fn deferred_search_aliases(&self) -> Vec<String> {
        vec![
            self.tool_name.clone(),
            self.server_name.clone(),
            format!("{}/{}", self.server_name, self.tool_name),
        ]
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> JsonSchema {
        self.input_schema.clone()
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        // Read-only MCP tools have no side effects → safe to run in parallel with
        // other read-only calls (mirrors built-in Read/Grep/Glob). Mutating or
        // unannotated tools stay serial.
        self.is_read_only()
    }

    fn is_deferred(&self) -> bool {
        self.deferred
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let call = self
            .manager
            .call_tool(&self.server_name, &self.tool_name, input)
            .await;
        self.render_call_result(call)
    }

    async fn execute_with_context(
        &self,
        input: Value,
        context: &ToolExecutionContext,
    ) -> ToolResult {
        let call = self
            .manager
            .call_tool_with_context(
                &self.server_name,
                &self.tool_name,
                input,
                context,
            )
            .await;
        self.render_call_result(call)
    }

    fn category(&self) -> ToolCategory {
        // Annotation-driven: readOnly tools are approval-free Info, everything
        // else (destructive or unannotated) is Exec → needs approval. We no
        // longer collapse every MCP tool into the single `Mcp` bucket, which
        // forced even read-only snapshots through the approval gate.
        self.category_from_annotations()
    }

    fn describe(&self, input: &Value) -> String {
        format!(
            "MCP {}/{}: {}",
            self.server_name,
            self.tool_name,
            serde_json::to_string(input).unwrap_or_default()
        )
    }
}

/// Register all MCP tools under origin-stable canonical provider names.
///
/// Each tool's deferred flag is read from the server's config:
/// `McpServerConfig::deferred` — defaults to `true` when absent.
pub fn register_mcp_tools(
    registry: &mut nomi_tools::registry::ToolRegistry,
    manager: &Arc<McpManager>,
    server_configs: &HashMap<String, McpServerConfig>,
) {
    let mut registrations: BTreeMap<String, Vec<_>> = BTreeMap::new();
    for (server_name, tool_def) in manager.all_tools() {
        registrations
            .entry(server_name.to_owned())
            .or_default()
            .push(tool_def);
    }

    for (server_name, mut tool_defs) in registrations {
        tool_defs.sort_by(|left, right| left.name.cmp(&right.name));
        let deferred = server_configs
            .get(&server_name)
            .and_then(|c| c.deferred)
            .unwrap_or(true);
        let proxies: Vec<Box<dyn Tool>> = tool_defs
            .into_iter()
            .map(|tool_def| {
                Box::new(McpToolProxy::new(
                    tool_def.name.clone(),
                    server_name.clone(),
                    tool_def.description.clone().unwrap_or_default(),
                    tool_def.input_schema.clone(),
                    Arc::clone(manager),
                    deferred,
                    tool_def.annotations.clone(),
                )) as Box<dyn Tool>
            })
            .collect();
        if registry.register_batch(proxies).is_empty() {
            tracing::warn!(
                target: "nomi_mcp",
                server = %server_name,
                "rejecting every tool from MCP server because its registration batch conflicts"
            );
        }
    }
}

/// One dynamic MCP tool that survived registry policy and is provider-visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredMcpTool {
    pub original_name: String,
    pub provider_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpToolRegistrationError {
    #[error("MCP server '{server_name}' advertised no tools")]
    NoTools { server_name: String },
    #[error(
        "MCP server '{server_name}' tool set conflicts with an existing route or registry policy"
    )]
    Rejected { server_name: String },
}

/// Register tools from a single newly-connected MCP server and return the
/// accepted original-to-provider name mapping for `McpReady` metadata.
/// Uses the same origin-stable naming as `register_mcp_tools`.
pub fn register_single_server_tools(
    registry: &mut nomi_tools::registry::ToolRegistry,
    manager: &Arc<McpManager>,
    server_name: &str,
    deferred: bool,
) -> Result<Vec<RegisteredMcpTool>, McpToolRegistrationError> {
    let mut server_tools: Vec<_> = manager
        .all_tools()
        .into_iter()
        .filter(|(sn, _)| *sn == server_name)
        .collect();
    server_tools.sort_by(|left, right| left.1.name.cmp(&right.1.name));
    if server_tools.is_empty() {
        return Err(McpToolRegistrationError::NoTools {
            server_name: server_name.to_owned(),
        });
    }
    let mut registered = Vec::with_capacity(server_tools.len());
    let mut proxies: Vec<Box<dyn Tool>> = Vec::with_capacity(server_tools.len());

    for (server_name, tool_def) in server_tools {
        let original_name = &tool_def.name;

        let proxy = McpToolProxy::new(
            original_name.clone(),
            server_name.to_string(),
            tool_def.description.clone().unwrap_or_default(),
            tool_def.input_schema.clone(),
            Arc::clone(manager),
            deferred,
            tool_def.annotations.clone(),
        );
        let provider_name = proxy.name().to_owned();

        registered.push(RegisteredMcpTool {
            original_name: original_name.clone(),
            provider_name,
        });
        proxies.push(Box::new(proxy));
    }

    let inserted_names = registry.register_batch(proxies);
    if inserted_names.is_empty() {
        return Err(McpToolRegistrationError::Rejected {
            server_name: server_name.to_owned(),
        });
    }
    let inserted_names: std::collections::BTreeSet<String> =
        inserted_names.into_iter().collect();
    registered.retain(|tool| inserted_names.contains(&tool.provider_name));

    Ok(registered)
}

/// Provider-visible origin name. Keep as much of the sanitized
/// `server__tool` origin as the strictest 64-character provider limit permits,
/// then append a fixed 80-bit SHA-256 prefix to disambiguate origins whose
/// readable slugs collide. The result never depends on registration order.
pub fn canonical_mcp_display_name(server_name: &str, original_name: &str) -> String {
    let identity = canonical_mcp_tool_identity(server_name, original_name);
    let digest = Sha256::digest(identity.as_bytes());
    let digest = base32_no_pad(&digest);
    let digest = &digest[..MCP_DISPLAY_HASH_LEN];
    let mut slug = sanitize_display_slug(&format!("{server_name}__{original_name}"));
    slug.truncate(MCP_DISPLAY_SLUG_LEN);
    let display_name =
        format!("{MCP_PROVIDER_NAME_PREFIX}{slug}{MCP_DISPLAY_SEPARATOR}{digest}");
    debug_assert!(display_name.len() <= MAX_PROVIDER_TOOL_NAME_LEN);
    display_name
}

fn sanitize_display_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_separator = false;
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') {
            slug.push(char::from(byte));
            last_was_separator = false;
        } else if !last_was_separator {
            slug.push('_');
            last_was_separator = true;
        }
    }
    let slug = slug.trim_matches('_');
    if slug.is_empty() {
        "tool".to_owned()
    } else {
        slug.to_owned()
    }
}

fn base32_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut encoded = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let mut buffer = 0_u16;
    let mut bits = 0_u8;
    for byte in bytes {
        buffer = (buffer << 8) | u16::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = usize::from((buffer >> bits) & 0x1f);
            encoded.push(char::from(ALPHABET[index]));
            buffer &= (1_u16 << bits).saturating_sub(1);
        }
    }
    if bits > 0 {
        let index = usize::from((buffer << (5 - bits)) & 0x1f);
        encoded.push(char::from(ALPHABET[index]));
    }
    encoded
}

/// Canonical origin identity for an MCP tool. This is persisted for deferred
/// activation independently from its bounded provider-visible hash alias.
fn canonical_mcp_tool_identity(server_name: &str, original_name: &str) -> String {
    format!(
        "mcp:{}:{server_name}:{}:{original_name}",
        server_name.len(),
        original_name.len()
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ToolAnnotations;
    use nomi_config::config::TransportType;
    use serde_json::json;

    // Minimal MockTransport local to this test module (the one in manager.rs's
    // test mod is not cross-module visible). McpTransport is crate-visible with
    // 3 methods, so duplicating it keeps the modules decoupled.
    use crate::protocol::{JsonRpcRequest, JsonRpcResponse, McpToolDef};
    use crate::transport::{McpError, McpTransport};
    use async_trait::async_trait;
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    struct MockTransport {
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

    fn make_proxy(deferred: bool) -> McpToolProxy {
        // manager is only used during execute(), which we don't call in these
        // tests, so we can construct one with no servers.
        let manager = Arc::new(McpManager::new_for_test(vec![]));
        McpToolProxy::new(
            "test_tool".into(),
            "test_server".into(),
            "A test tool".into(),
            json!({"type": "object"}),
            manager,
            deferred,
            None,
        )
    }

    /// Build a proxy with the given annotations (no real transport needed —
    /// category() does not touch the manager).
    fn make_proxy_with_annotations(annotations: Option<ToolAnnotations>) -> McpToolProxy {
        let manager = Arc::new(McpManager::new_for_test(vec![]));
        McpToolProxy::new(
            "test_tool".into(),
            "test_server".into(),
            "A test tool".into(),
            json!({"type": "object"}),
            manager,
            true,
            annotations,
        )
    }

    #[test]
    fn proxy_deferred_true_returns_true() {
        let proxy = make_proxy(true);
        assert!(proxy.is_deferred());
    }

    #[test]
    fn proxy_deferred_false_returns_false() {
        let proxy = make_proxy(false);
        assert!(!proxy.is_deferred());
    }

    // -----------------------------------------------------------------------
    // category(): annotations → approval class (readOnly→Info, else→Exec)
    // -----------------------------------------------------------------------

    #[test]
    fn category_read_only_hint_true_is_info() {
        let proxy = make_proxy_with_annotations(Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        }));
        assert_eq!(proxy.category(), ToolCategory::Info);
        // category_for delegates to category() for tools that don't override it.
        assert_eq!(proxy.category_for(&json!({})), ToolCategory::Info);
    }

    #[test]
    fn category_destructive_hint_is_exec() {
        // destructive (and not read-only) must require approval.
        let proxy = make_proxy_with_annotations(Some(ToolAnnotations {
            destructive_hint: Some(true),
            ..Default::default()
        }));
        assert_eq!(proxy.category(), ToolCategory::Exec);
    }

    #[test]
    fn category_read_only_false_is_exec() {
        // Explicit readOnlyHint=false → still needs approval.
        let proxy = make_proxy_with_annotations(Some(ToolAnnotations {
            read_only_hint: Some(false),
            ..Default::default()
        }));
        assert_eq!(proxy.category(), ToolCategory::Exec);
    }

    #[test]
    fn read_only_hint_true_is_concurrency_safe() {
        // A read-only MCP tool has no side effects, so concurrent execution with
        // other read-only calls is safe (mirrors built-in Read/Grep/Glob).
        let proxy = make_proxy_with_annotations(Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        }));
        assert!(proxy.is_concurrency_safe(&json!({})));
    }

    #[test]
    fn non_read_only_is_not_concurrency_safe() {
        let explicit_false = make_proxy_with_annotations(Some(ToolAnnotations {
            read_only_hint: Some(false),
            ..Default::default()
        }));
        assert!(!explicit_false.is_concurrency_safe(&json!({})));
        // No annotations → safe default: assume side effects, run serially.
        let none = make_proxy_with_annotations(None);
        assert!(!none.is_concurrency_safe(&json!({})));
    }

    #[test]
    fn category_no_annotations_defaults_to_exec() {
        // Old server with no annotations block → safe default: Exec.
        let proxy = make_proxy_with_annotations(None);
        assert_eq!(proxy.category(), ToolCategory::Exec);
    }

    #[test]
    fn category_empty_annotations_defaults_to_exec() {
        // annotations present but no readOnlyHint → from-strict default: Exec.
        let proxy = make_proxy_with_annotations(Some(ToolAnnotations::default()));
        assert_eq!(proxy.category(), ToolCategory::Exec);
    }

    #[test]
    fn category_read_only_wins_when_no_destructive() {
        // openWorld + idempotent set but readOnly true → still Info.
        let proxy = make_proxy_with_annotations(Some(ToolAnnotations {
            read_only_hint: Some(true),
            open_world_hint: Some(true),
            idempotent_hint: Some(true),
            ..Default::default()
        }));
        assert_eq!(proxy.category(), ToolCategory::Info);
    }

    fn make_server_config(deferred: Option<bool>) -> McpServerConfig {
        McpServerConfig {
            transport: TransportType::Stdio,
            command: Some("echo".into()),
            args: None,
            env: None,
            url: None,
            headers: None,
            deferred,
        }
    }

    fn manager_with_tools(entries: &[(&str, &str)]) -> Arc<McpManager> {
        let servers: Vec<crate::manager::TestMcpServerWithTools<'_>> = entries
            .iter()
            .map(|(server_name, tool_name)| {
                (
                    *server_name,
                    false,
                    vec![McpToolDef {
                        name: (*tool_name).to_owned(),
                        description: Some(format!("remote {server_name}/{tool_name}")),
                        input_schema: json!({"type": "object"}),
                        annotations: None,
                    }],
                    Box::new(MockTransport::new(vec![])) as Box<dyn McpTransport>,
                )
            })
            .collect();
        Arc::new(McpManager::new_for_test_with_tools(servers))
    }

    fn manager_with_tool(server_name: &str, tool_name: &str) -> Arc<McpManager> {
        manager_with_tools(&[(server_name, tool_name)])
    }

    fn manager_with_server_tool_names(
        server_name: &str,
        tool_names: &[&str],
    ) -> Arc<McpManager> {
        let tools = tool_names
            .iter()
            .map(|tool_name| McpToolDef {
                name: (*tool_name).to_owned(),
                description: Some(format!("remote {server_name}/{tool_name}")),
                input_schema: json!({"type": "object"}),
                annotations: None,
            })
            .collect();
        Arc::new(McpManager::new_for_test_with_tools(vec![(
            server_name,
            false,
            tools,
            Box::new(MockTransport::new(vec![])),
        )]))
    }

    fn manager_with_tool_response(
        server_name: &str,
        tool_name: &str,
        response_text: &str,
    ) -> Arc<McpManager> {
        Arc::new(McpManager::new_for_test_with_tools(vec![(
            server_name,
            false,
            vec![McpToolDef {
                name: tool_name.to_owned(),
                description: Some(format!("remote {server_name}/{tool_name}")),
                input_schema: json!({"type": "object"}),
                annotations: None,
            }],
            Box::new(MockTransport::new(vec![json!({
                "content": [{"type": "text", "text": response_text}]
            })])),
        )]))
    }

    struct NamedNativeTool {
        name: String,
    }

    #[async_trait]
    impl Tool for NamedNativeTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "native collision fixture"
        }

        fn input_schema(&self) -> JsonSchema {
            json!({"type": "object"})
        }

        fn is_concurrency_safe(&self, _input: &Value) -> bool {
            true
        }

        async fn execute(&self, _input: Value) -> ToolResult {
            ToolResult::text("native")
        }

        fn category(&self) -> ToolCategory {
            ToolCategory::Info
        }
    }

    #[test]
    fn register_defaults_to_deferred_when_config_omits_field() {
        let manager = Arc::new(McpManager::new_for_test(vec![]));
        let mut registry = nomi_tools::registry::ToolRegistry::new();
        // Empty server configs — deferred field absent
        let configs = HashMap::new();

        register_mcp_tools(&mut registry, &manager, &configs);

        // No tools registered because manager has no tools, but the logic
        // is tested via the deferred default path. Test with a real config below.
        assert!(registry.tool_names().is_empty());
    }

    #[test]
    fn static_mcp_always_uses_origin_stable_canonical_namespace() {
        let manager = manager_with_tool("static_server", "ToolSearch");
        let mut registry = nomi_tools::registry::ToolRegistry::new();
        let state = registry.deferred_state();
        registry.register(Box::new(
            nomi_tools::tool_search::ToolSearchTool::new(state),
        ));
        let mut configs = HashMap::new();
        configs.insert("static_server".to_owned(), make_server_config(Some(true)));

        register_mcp_tools(&mut registry, &manager, &configs);

        let alias = canonical_mcp_display_name("static_server", "ToolSearch");
        assert_eq!(
            registry.tool_names(),
            vec!["ToolSearch".to_owned(), alias]
        );
        assert_eq!(registry.to_tool_defs().len(), 2);
    }

    #[test]
    fn dynamic_mcp_uses_the_same_origin_stable_canonical_namespace() {
        let manager = manager_with_tool("late_server", "ToolSearch");
        let mut registry = nomi_tools::registry::ToolRegistry::new();
        let state = registry.deferred_state();
        registry.register(Box::new(
            nomi_tools::tool_search::ToolSearchTool::new(state),
        ));
        let registrations =
            register_single_server_tools(&mut registry, &manager, "late_server", true).unwrap();

        let alias = canonical_mcp_display_name("late_server", "ToolSearch");
        assert_eq!(
            registrations,
            vec![RegisteredMcpTool {
                original_name: "ToolSearch".to_owned(),
                provider_name: alias.clone(),
            }]
        );
        assert_eq!(
            registry.tool_names(),
            vec!["ToolSearch".to_owned(), alias.clone()]
        );
        assert!(alias.starts_with("mcp__late_server__ToolSearch__"));
        assert!(alias.len() <= MAX_PROVIDER_TOOL_NAME_LEN);
    }

    #[test]
    fn reserved_mcp_namespace_prevents_native_transcript_route_capture() {
        let manager = manager_with_tool("A", "search");
        let mut registry = nomi_tools::registry::ToolRegistry::new();
        registry.register(Box::new(NamedNativeTool {
            name: "search".to_owned(),
        }));
        let alias = canonical_mcp_display_name("A", "search");
        registry.register(Box::new(NamedNativeTool {
            name: alias.clone(),
        }));
        assert!(registry.get(&alias).is_none());

        register_single_server_tools(&mut registry, &manager, "A", true).unwrap();

        assert_eq!(registry.tool_names(), vec!["search".to_owned(), alias.clone()]);
        assert_eq!(
            registry.get(&alias).unwrap().activation_identity(),
            canonical_mcp_tool_identity("A", "search")
        );
    }

    #[test]
    fn static_mcp_alias_allocation_is_unique_for_ambiguous_display_segments() {
        let manager = manager_with_tools(&[("a__b", "c"), ("a", "b__c")]);
        let mut registry = nomi_tools::registry::ToolRegistry::new();
        registry.register(Box::new(NamedNativeTool {
            name: "c".to_owned(),
        }));
        registry.register(Box::new(NamedNativeTool {
            name: "b__c".to_owned(),
        }));
        let mut configs = HashMap::new();
        configs.insert("a__b".to_owned(), make_server_config(Some(true)));
        configs.insert("a".to_owned(), make_server_config(Some(true)));

        register_mcp_tools(&mut registry, &manager, &configs);

        let a_alias = canonical_mcp_display_name("a", "b__c");
        let ab_alias = canonical_mcp_display_name("a__b", "c");
        assert!(a_alias.starts_with("mcp__a__b__c__"));
        assert!(ab_alias.starts_with("mcp__a__b__c__"));
        assert_ne!(a_alias, ab_alias);
        assert_eq!(
            registry.tool_names(),
            vec![
                "c".to_owned(),
                "b__c".to_owned(),
                a_alias.clone(),
                ab_alias.clone()
            ]
        );
        assert_eq!(
            registry.get(&a_alias).unwrap().activation_identity(),
            canonical_mcp_tool_identity("a", "b__c")
        );
        assert_eq!(
            registry.get(&ab_alias).unwrap().activation_identity(),
            canonical_mcp_tool_identity("a__b", "c")
        );
        let provider_names: BTreeSet<_> = registry
            .to_tool_defs()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        assert_eq!(provider_names.len(), registry.tool_names().len());
    }

    #[test]
    fn activation_identity_is_unambiguous_when_segments_contain_display_separator() {
        assert_ne!(
            canonical_mcp_tool_identity("a__b", "c"),
            canonical_mcp_tool_identity("a", "b__c")
        );
        assert_ne!(
            canonical_mcp_tool_identity("alpha__beta", "gamma__delta"),
            canonical_mcp_tool_identity("alpha", "beta__gamma__delta")
        );
    }

    #[test]
    fn canonical_provider_name_is_bounded_safe_and_origin_unique() {
        let long_server = "知识库-server-with-a-very-long-name-and spaces";
        let long_tool = "检索/tool-with-an-equally-long-name-and symbols!?";
        let alias = canonical_mcp_display_name(long_server, long_tool);
        let readable_alias =
            canonical_mcp_display_name("knowledge_gateway", "search_documents");

        assert!(alias.starts_with(MCP_PROVIDER_NAME_PREFIX));
        assert_eq!(alias.len(), MAX_PROVIDER_TOOL_NAME_LEN);
        assert!(
            alias
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        );
        assert!(readable_alias.starts_with("mcp__knowledge_gateway__search_documents__"));
        let (_, hash) = readable_alias.rsplit_once(MCP_DISPLAY_SEPARATOR).unwrap();
        assert_eq!(hash.len(), MCP_DISPLAY_HASH_LEN);
        assert!(
            hash.bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        );
        assert_eq!(alias, canonical_mcp_display_name(long_server, long_tool));
        assert_ne!(alias, canonical_mcp_display_name(long_tool, long_server));
        assert_ne!(
            canonical_mcp_display_name("a__b", "c"),
            canonical_mcp_display_name("a", "b__c")
        );
    }

    #[test]
    fn artifact_identity_retains_tool_suffix_hidden_by_bounded_provider_name() {
        let server_name = "server-with-an-extremely-long-origin-name-that-consumes-the-readable-provider-slug";
        for tool_name in ["export_pdf", "render_video"] {
            let manager = manager_with_tool_response(server_name, tool_name, "done");
            let proxy = McpToolProxy::new(
                tool_name.to_owned(),
                server_name.to_owned(),
                "produce a durable artifact".to_owned(),
                json!({"type": "object"}),
                manager,
                false,
                None,
            );

            assert_eq!(proxy.name().len(), MAX_PROVIDER_TOOL_NAME_LEN);
            assert!(!proxy.name().contains(tool_name));
            assert_eq!(
                proxy.artifact_identity(),
                format!("{server_name}__{tool_name}")
            );
        }
    }

    #[tokio::test]
    async fn tool_search_finds_mcp_by_original_tool_name_and_server_alias() {
        // Pure Unicode origins sanitize to an opaque `tool` slug, so these
        // matches prove ToolSearch uses explicit origin metadata rather than
        // accidentally finding readable text in the canonical provider name.
        let manager = manager_with_tool("知识库", "精准检索");
        let alias = canonical_mcp_display_name("知识库", "精准检索");
        assert!(alias.starts_with("mcp__tool__"));

        let mut registry = nomi_tools::registry::ToolRegistry::new();
        let state = registry.deferred_state();
        let search = nomi_tools::tool_search::ToolSearchTool::new(state.clone());
        let registrations =
            register_single_server_tools(&mut registry, &manager, "知识库", true).unwrap();
        assert_eq!(registrations[0].provider_name, alias);

        for query in ["精准检索", "知识库"] {
            let result = search.execute(json!({"query": query})).await;
            assert!(!result.is_error, "query {query:?} should be accepted");
            let matches: Vec<Value> = serde_json::from_str(&result.content).unwrap();
            assert_eq!(matches.len(), 1);
            assert_eq!(matches[0]["name"], alias);
        }
        assert_eq!(
            state.activated_identities(),
            vec![canonical_mcp_tool_identity("知识库", "精准检索")]
        );
    }

    #[tokio::test]
    async fn restored_mcp_activation_and_display_routing_follow_the_same_origin() {
        let manager_a = manager_with_tool("A", "search");
        let a_alias = canonical_mcp_display_name("A", "search");

        let mut original = nomi_tools::registry::ToolRegistry::new();
        register_single_server_tools(&mut original, &manager_a, "A", true).unwrap();
        assert_eq!(original.tool_names(), vec![a_alias.clone()]);
        let search = nomi_tools::tool_search::ToolSearchTool::new(original.deferred_state());
        let search_result = search.execute(json!({"query": a_alias})).await;
        assert!(!search_result.is_error);
        let persisted = original.session_deferred_tool_identities();
        assert_eq!(persisted, vec![canonical_mcp_tool_identity("A", "search")]);

        // B registers first after resume. Both display routing and deferred
        // activation remain bound to origin rather than registration order.
        let manager_b = manager_with_tool("B", "search");
        let b_alias = canonical_mcp_display_name("B", "search");
        let mut resumed = nomi_tools::registry::ToolRegistry::new();
        for identity in persisted {
            resumed.restore_deferred_tool_activation(&identity);
        }
        register_single_server_tools(&mut resumed, &manager_b, "B", true).unwrap();
        let b_definition = resumed
            .to_tool_defs()
            .into_iter()
            .find(|definition| definition.name == b_alias)
            .unwrap();
        assert!(b_definition.deferred, "B must not consume A's activation");

        register_single_server_tools(&mut resumed, &manager_a, "A", true).unwrap();
        let definitions = resumed.to_tool_defs();
        assert!(
            definitions
                .iter()
                .find(|definition| definition.name == b_alias)
                .unwrap()
                .deferred
        );
        assert!(
            !definitions
                .iter()
                .find(|definition| definition.name == a_alias)
                .unwrap()
                .deferred,
            "A must restore independently from B's registration order"
        );
    }

    #[test]
    fn provider_display_names_do_not_change_when_dynamic_registration_order_reverses() {
        let manager_a = manager_with_tool("A", "search");
        let manager_b = manager_with_tool("B", "search");
        let a_alias = canonical_mcp_display_name("A", "search");
        let b_alias = canonical_mcp_display_name("B", "search");

        let mut a_then_b = nomi_tools::registry::ToolRegistry::new();
        register_single_server_tools(&mut a_then_b, &manager_a, "A", true).unwrap();
        register_single_server_tools(&mut a_then_b, &manager_b, "B", true).unwrap();

        let mut b_then_a = nomi_tools::registry::ToolRegistry::new();
        register_single_server_tools(&mut b_then_a, &manager_b, "B", true).unwrap();
        register_single_server_tools(&mut b_then_a, &manager_a, "A", true).unwrap();

        for registry in [&a_then_b, &b_then_a] {
            assert_eq!(
                registry.get(&a_alias).unwrap().activation_identity(),
                canonical_mcp_tool_identity("A", "search")
            );
            assert_eq!(
                registry.get(&b_alias).unwrap().activation_identity(),
                canonical_mcp_tool_identity("B", "search")
            );
        }
    }

    #[tokio::test]
    async fn repeated_dynamic_origin_is_rejected_and_keeps_the_old_manager_route() {
        let old_manager = manager_with_tool_response("gateway", "search", "old-manager");
        let new_manager = manager_with_tool_response("gateway", "search", "new-manager");
        let alias = canonical_mcp_display_name("gateway", "search");
        let mut registry = nomi_tools::registry::ToolRegistry::new();

        let first =
            register_single_server_tools(&mut registry, &old_manager, "gateway", false).unwrap();
        let second = register_single_server_tools(
            &mut registry,
            &new_manager,
            "gateway",
            false,
        )
        .unwrap_err();

        assert_eq!(first.len(), 1);
        assert!(matches!(
            second,
            McpToolRegistrationError::Rejected { .. }
        ));
        assert_eq!(registry.tool_names(), vec![alias.clone()]);
        let result = registry.get(&alias).unwrap().execute(json!({})).await;
        assert_eq!(result.content, "old-manager");
    }

    #[test]
    fn dynamic_server_registration_is_atomic_when_only_one_tool_conflicts() {
        let old_manager = manager_with_tool("gateway", "search");
        let mixed_manager =
            manager_with_server_tool_names("gateway", &["fresh_tool", "search"]);
        let old_alias = canonical_mcp_display_name("gateway", "search");
        let fresh_alias = canonical_mcp_display_name("gateway", "fresh_tool");
        let mut registry = nomi_tools::registry::ToolRegistry::new();
        register_single_server_tools(&mut registry, &old_manager, "gateway", true).unwrap();

        let error = register_single_server_tools(
            &mut registry,
            &mixed_manager,
            "gateway",
            true,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            McpToolRegistrationError::Rejected { .. }
        ));
        assert_eq!(registry.tool_names(), vec![old_alias]);
        assert!(registry.get(&fresh_alias).is_none());
    }

    #[test]
    fn static_duplicate_tool_catalog_is_rejected_as_one_atomic_server_batch() {
        let manager = manager_with_server_tool_names("duplicate", &["same", "same"]);
        let mut registry = nomi_tools::registry::ToolRegistry::new();
        let mut configs = HashMap::new();
        configs.insert("duplicate".to_owned(), make_server_config(Some(true)));

        register_mcp_tools(&mut registry, &manager, &configs);

        assert!(registry.tool_names().is_empty());
        assert!(registry.to_tool_defs().is_empty());
    }

    #[test]
    fn dynamic_mcp_registration_cannot_bypass_persisted_registry_policy() {
        let manager = manager_with_tool("late", "remote_tool");
        let alias = canonical_mcp_display_name("late", "remote_tool");
        let mut registry = nomi_tools::registry::ToolRegistry::new();
        let state = registry.deferred_state();
        registry.register(Box::new(
            nomi_tools::tool_search::ToolSearchTool::new(state),
        ));
        registry.retain_named(&["ToolSearch".to_string()]);
        let error =
            register_single_server_tools(&mut registry, &manager, "late", true).unwrap_err();

        assert_eq!(registry.tool_names(), vec!["ToolSearch"]);
        assert!(registry.get(&alias).is_none());
        assert!(matches!(error, McpToolRegistrationError::Rejected { .. }));
    }

    #[test]
    fn dynamic_mcp_allowlist_requires_the_stable_canonical_provider_name() {
        let manager = manager_with_tool("late", "remote_tool");
        let alias = canonical_mcp_display_name("late", "remote_tool");

        let mut raw_name_policy = nomi_tools::registry::ToolRegistry::new();
        raw_name_policy.retain_named(&["remote_tool".to_owned()]);
        let raw_error =
            register_single_server_tools(&mut raw_name_policy, &manager, "late", true)
                .unwrap_err();
        assert!(raw_name_policy.tool_names().is_empty());
        assert!(matches!(
            raw_error,
            McpToolRegistrationError::Rejected { .. }
        ));

        let mut canonical_policy = nomi_tools::registry::ToolRegistry::new();
        canonical_policy.retain_named(std::slice::from_ref(&alias));
        let canonical_registrations =
            register_single_server_tools(&mut canonical_policy, &manager, "late", true).unwrap();
        assert_eq!(canonical_policy.tool_names(), vec![alias.clone()]);
        assert_eq!(
            canonical_registrations,
            vec![RegisteredMcpTool {
                original_name: "remote_tool".to_owned(),
                provider_name: alias.clone(),
            }]
        );
        assert!(canonical_policy.get("remote_tool").is_none());
        assert_eq!(
            canonical_policy.get(&alias).unwrap().category(),
            ToolCategory::Exec,
            "approval classification must resolve through the unique canonical route"
        );
    }

    #[test]
    fn dynamic_mcp_allowlist_registers_only_the_canonical_allowed_subset() {
        let manager = manager_with_server_tool_names(
            "late",
            &["allowed_tool", "raw_name_must_not_authorize"],
        );
        let allowed_alias = canonical_mcp_display_name("late", "allowed_tool");
        let denied_alias = canonical_mcp_display_name("late", "raw_name_must_not_authorize");
        let mut registry = nomi_tools::registry::ToolRegistry::new();
        registry.retain_named(&[
            allowed_alias.clone(),
            "raw_name_must_not_authorize".to_owned(),
        ]);

        let registrations =
            register_single_server_tools(&mut registry, &manager, "late", true).unwrap();

        assert_eq!(
            registrations,
            vec![RegisteredMcpTool {
                original_name: "allowed_tool".to_owned(),
                provider_name: allowed_alias.clone(),
            }]
        );
        assert_eq!(registry.tool_names(), vec![allowed_alias.clone()]);
        assert!(registry.get(&allowed_alias).is_some());
        assert!(registry.get(&denied_alias).is_none());
        assert!(registry.get("raw_name_must_not_authorize").is_none());
    }

    #[test]
    fn dynamic_mcp_registration_cannot_bypass_clear_deny_all() {
        let manager = manager_with_tool("late", "remote_tool");
        let mut registry = nomi_tools::registry::ToolRegistry::new();
        registry.clear();

        let error =
            register_single_server_tools(&mut registry, &manager, "late", true).unwrap_err();

        assert!(registry.tool_names().is_empty());
        assert!(matches!(error, McpToolRegistrationError::Rejected { .. }));
    }

    #[test]
    fn server_config_deferred_none_defaults_true() {
        let config = make_server_config(None);
        let deferred = config.deferred.unwrap_or(true);
        assert!(deferred, "deferred should default to true when None");
    }

    #[test]
    fn server_config_deferred_explicit_false() {
        let config = make_server_config(Some(false));
        let deferred = config.deferred.unwrap_or(true);
        assert!(!deferred, "deferred should be false when explicitly set");
    }

    #[test]
    fn server_config_deferred_explicit_true() {
        let config = make_server_config(Some(true));
        let deferred = config.deferred.unwrap_or(true);
        assert!(deferred, "deferred should be true when explicitly set");
    }

    // -----------------------------------------------------------------------
    // execute: image content → ToolResult.images (end-to-end mapping)
    // -----------------------------------------------------------------------

    fn proxy_with_response(
        tool: &str,
        resp: serde_json::Value,
    ) -> McpToolProxy {
        let mgr = Arc::new(McpManager::new_for_test(vec![(
            "srv",
            false,
            Box::new(MockTransport::new(vec![resp])),
        )]));
        McpToolProxy::new(
            tool.into(),
            "srv".into(),
            "desc".into(),
            json!({"type":"object"}),
            mgr,
            true,
            None,
        )
    }

    fn proxy_with_server_response(
        server: &str,
        tool: &str,
        resp: serde_json::Value,
    ) -> McpToolProxy {
        let mgr = Arc::new(McpManager::new_for_test(vec![(
            server,
            false,
            Box::new(MockTransport::new(vec![resp])),
        )]));
        McpToolProxy::new(
            tool.into(),
            server.into(),
            "desc".into(),
            json!({"type":"object"}),
            mgr,
            true,
            None,
        )
    }

    #[tokio::test]
    async fn proxy_execute_maps_image_to_tool_result_images() {
        let resp = json!({ "content": [
            {"type":"text","text":"ok"},
            {"type":"image","data":"ZZZ","mimeType":"image/png"}
        ]});
        let proxy = proxy_with_response("shot", resp);
        let r = proxy.execute(json!({})).await;
        assert!(!r.is_error);
        assert_eq!(r.content, "ok");
        assert_eq!(r.images.len(), 1);
        assert_eq!(r.images[0].media_type, "image/png");
        assert_eq!(r.images[0].data, "ZZZ");
    }

    #[tokio::test]
    async fn proxy_execute_rejects_unmaterialized_remote_link_without_partial_artifacts() {
        let resp = json!({ "content": [
            {"type":"audio","data":"SUQzYXVkaW8=","mimeType":"audio/mpeg"},
            {"type":"resource","resource":{"uri":"file:///report.txt","mimeType":"text/plain","text":"report"}},
            {"type":"resource_link","uri":"https://example.test/report.pdf","name":"report.pdf","mimeType":"application/pdf"}
        ]});
        let proxy = proxy_with_response("export", resp);

        let result = proxy.execute(json!({})).await;

        assert!(result.is_error);
        assert!(result.images.is_empty());
        assert!(result.content.contains("was not materialized"));
        assert!(result.content.contains("https://example.test/report.pdf"));
    }

    #[tokio::test]
    async fn image_generator_rejects_non_image_artifact_as_success() {
        let resp = json!({ "content": [
            {"type":"resource","resource":{"uri":"file:///claim.txt","mimeType":"text/plain","text":"generated"}}
        ]});
        let proxy = proxy_with_server_response("other", "image_gen", resp);

        let result = proxy.execute(json!({})).await;

        assert!(result.is_error);
        assert_eq!(result.images.len(), 1, "receipt remains available for error delivery");
        assert!(result.content.contains("no image artifact was returned"));
    }

    #[tokio::test]
    async fn proxy_execute_text_only_no_images() {
        let resp = json!({ "content": [{"type":"text","text":"plain"}] });
        let proxy = proxy_with_response("echo", resp);
        let r = proxy.execute(json!({})).await;
        assert!(!r.is_error);
        assert_eq!(r.content, "plain");
        assert!(r.images.is_empty());
    }

    #[tokio::test]
    async fn image_generation_text_only_response_is_an_error() {
        let resp = json!({ "content": [{"type":"text","text":"Image generated successfully."}] });
        let proxy = proxy_with_server_response("nomifun-image-generation", "generate", resp);
        let result = proxy.execute(json!({})).await;

        assert!(result.is_error);
        assert!(result.images.is_empty());
        assert!(result.content.contains("no image artifact was returned"));
    }

    #[tokio::test]
    async fn image_generation_empty_content_is_an_error() {
        let resp = json!({ "content": [] });
        let proxy = proxy_with_server_response("other", "image_gen", resp);
        let result = proxy.execute(json!({})).await;

        assert!(result.is_error);
        assert!(result.content.contains("no image artifact was returned"));
    }

    #[tokio::test]
    async fn proxy_execute_maps_mcp_is_error_to_tool_result() {
        let resp = json!({
            "content": [{
                "type":"text",
                "text":"Error: invalid arguments for this tool: missing field `kb_id`"
            }],
            "isError": true
        });
        let proxy = proxy_with_response("update_base", resp);
        let r = proxy.execute(json!({})).await;
        assert!(r.is_error);
        assert!(r.content.contains("kb_id"));
    }

    #[tokio::test]
    async fn proxy_execute_does_not_infer_error_from_text() {
        let resp = json!({
            "content": [{"type":"text","text":"Error: ordinary successful output"}]
        });
        let proxy = proxy_with_response("echo", resp);
        let r = proxy.execute(json!({})).await;
        assert!(!r.is_error);
        assert_eq!(r.content, "Error: ordinary successful output");
    }

    #[tokio::test]
    async fn proxy_execute_rejects_oversized_image_instead_of_false_success() {
        // Build a base64 string whose decoded size exceeds MCP_MAX_IMAGE_BYTES.
        // decoded ≈ len * 3/4, so we need > 5 MiB * 4/3 base64 chars.
        let huge = "A".repeat((MCP_MAX_IMAGE_BYTES + 1024) * 4 / 3 + 8);
        let resp = json!({ "content": [
            {"type":"text","text":"shot taken"},
            {"type":"image","data": huge,"mimeType":"image/png"}
        ]});
        let proxy = proxy_with_response("big_shot", resp);
        let r = proxy.execute(json!({})).await;
        assert!(r.is_error);
        assert!(r.images.is_empty());
        assert!(r.content.contains("shot taken"));
        assert!(
            r.content.contains("Artifact delivery rejected"),
            "expected explicit delivery error, got: {}",
            r.content
        );
        assert!(r.content.contains("above the"));
    }

    #[tokio::test]
    async fn proxy_execute_keeps_image_just_under_limit() {
        // A small image must survive the size guard untouched.
        let resp = json!({ "content": [
            {"type":"image","data":"c21hbGw=","mimeType":"image/jpeg"}
        ]});
        let proxy = proxy_with_response("small_shot", resp);
        let r = proxy.execute(json!({})).await;
        assert_eq!(r.images.len(), 1);
        assert_eq!(r.images[0].media_type, "image/jpeg");
        assert_eq!(r.images[0].data, "c21hbGw=");
    }

    #[test]
    fn decoded_base64_len_estimates_size() {
        // "QQ==" decodes to 1 byte; "QUI=" → 2 bytes; "QUJD" → 3 bytes.
        assert_eq!(decoded_base64_len("QQ=="), 1);
        assert_eq!(decoded_base64_len("QUI="), 2);
        assert_eq!(decoded_base64_len("QUJD"), 3);
        // Whitespace is ignored.
        assert_eq!(decoded_base64_len("QU\nJD"), 3);
        // data-URL prefix is stripped before measuring.
        assert_eq!(decoded_base64_len("data:image/png;base64,QUJD"), 3);
    }
}
