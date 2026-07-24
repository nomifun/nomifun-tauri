//! `nomicore mcp-gateway-stdio` subcommand: MCP stdio server for the Desktop
//! Gateway capabilities (`nomi_*` — the whole platform control surface).
//!
//! Spawned by Agent sessions holding a Platform Gateway capability. Uses `rmcp`
//! crate for protocol handling so it is byte-compatible with each CLI's MCP
//! client (claude / codex / gemini advertise stdio-only MCP capabilities) and
//! with the nomi engine's MCP manager.
//!
//! This bridge is now FULLY REGISTRY-DRIVEN: it declares no per-tool parameter
//! struct or `#[tool]` method. `tools/list` is projected from the capability
//! registry (`nomifun_gateway::Registry`) filtered by this session's permission
//! surface, and `tools/call` forwards the raw arguments to the in-process
//! `GatewayMcpServer` (the loopback port inside `NOMI_GW_MCP_CAPABILITY`), which
//! deserializes them into the capability's single typed request and enforces the
//! danger-tier × surface gate. Adding/renaming a capability in `nomifun-gateway`
//! updates this bridge automatically — there is nothing to keep in sync here.

use std::process::ExitCode;
use std::sync::Arc;

use nomifun_api_types::{
    GATEWAY_CALL_TOOL_OPERATION, GATEWAY_LIST_TOOLS_OPERATION,
    GATEWAY_CAPABILITY_DOMAIN, GatewayCapabilityClaims,
    GatewayCapabilityScope, GatewayMcpConfig,
};
use nomifun_gateway::{Registry, Surface};
use nomifun_common::{LoopbackCapabilityError, LoopbackSessionKind};
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams, Tool,
};
use rmcp::service::{RequestContext, RoleServer, ServiceExt};
use rmcp::transport;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::stdio_common::{ForwardToolOutcome, into_mcp_tool_result};

const EXECUTION_OPERATION_META_KEY: &str =
    "com.nomifun.execution.operation_id";

pub async fn run_gateway_stdio() -> ExitCode {
    let client = match super::stdio_common::ScopedBridgeClient::from_env(
        GatewayMcpConfig::ENV_CAPABILITY,
        GATEWAY_CAPABILITY_DOMAIN,
        "mcp-gateway-stdio",
        validate_gateway_claims,
    )
    .await
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("[mcp-gateway-stdio] ERROR: {error}");
            return ExitCode::from(1);
        }
    };
    let claims = client.access().await.expect("startup renewal succeeded").claims;

    eprintln!(
        "[mcp-gateway-stdio] Started OK. PORT={}, CONV_ID={}",
        client.port(),
        claims.session.session_id
    );

    let lifecycle = client.clone();
    let server = GatewayStdioServer { client };

    let transport = transport::io::stdio();
    let exit = match server.serve(transport).await {
        Ok(peer) => {
            eprintln!("[mcp-gateway-stdio] MCP session started, waiting for completion...");
            if let Err(e) = peer.waiting().await {
                eprintln!("[mcp-gateway-stdio] Session ended with error: {e}");
            } else {
                eprintln!("[mcp-gateway-stdio] Session ended normally");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[mcp-gateway-stdio] Failed to start MCP server: {e}");
            ExitCode::from(1)
        }
    };
    lifecycle.revoke().await;
    exit
}

#[derive(Clone)]
struct GatewayStdioServer {
    client: super::stdio_common::ScopedBridgeClient<GatewayCapabilityScope>,
}

fn validate_gateway_claims(
    claims: &GatewayCapabilityClaims,
) -> Result<(), LoopbackCapabilityError> {
    claims.validate_renewable_shape()?;
    claims.scope.validate()?;
    if claims.session.kind != LoopbackSessionKind::Conversation {
        return Err(LoopbackCapabilityError::InvalidIdentity);
    }
    Ok(())
}

impl GatewayStdioServer {
    fn execution_operation_hint(
        request: &CallToolRequestParams,
    ) -> Result<Option<&str>, rmcp::ErrorData> {
        let Some(value) = request
            .meta
            .as_ref()
            .and_then(|meta| meta.0.get(EXECUTION_OPERATION_META_KEY))
        else {
            return Ok(None);
        };
        let Some(value) = value.as_str() else {
            return Err(rmcp::ErrorData::invalid_request(
                format!(
                    "MCP _meta.{EXECUTION_OPERATION_META_KEY} must be a string"
                ),
                None,
            ));
        };
        if value.is_empty()
            || value.len() > 128
            || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(rmcp::ErrorData::invalid_request(
                format!(
                    "MCP _meta.{EXECUTION_OPERATION_META_KEY} must contain 1..=128 visible ASCII bytes"
                ),
                None,
            ));
        }
        Ok(Some(value))
    }

    /// Produce the loopback HTTP operation key. Engine-provided MCP metadata
    /// survives an outer JSON-RPC replay; clients without that extension still
    /// receive a fresh per-call nonce that protects the bridge's own HTTP
    /// response-loss retries. Session and tool fields prevent cross-
    /// conversation/cross-capability aliasing.
    fn loopback_operation_key(
        claims: &GatewayCapabilityClaims,
        tool_name: &str,
        execution_hint: Option<&str>,
    ) -> String {
        fn hash_field(hasher: &mut Sha256, value: &str) {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value.as_bytes());
        }

        let generated;
        let execution_hint = match execution_hint {
            Some(value) => value,
            None => {
                generated = Uuid::new_v4().to_string();
                &generated
            }
        };
        let mut hasher = Sha256::new();
        hasher.update(b"nomifun-gateway-stdio-tool:v1\0");
        hash_field(&mut hasher, &claims.session.session_id);
        hash_field(&mut hasher, tool_name);
        hash_field(&mut hasher, execution_hint);
        format!("gateway-tool-v1-{:x}", hasher.finalize())
    }

    /// The permission surface this bridge session acts on (mirrors
    /// `nomifun_gateway::CallerCtx::surface`): an IM channel platform marks an
    /// external session, otherwise it is a local desktop session.
    fn surface(claims: &GatewayCapabilityClaims) -> Surface {
        if claims.scope.channel_platform.is_some() {
            Surface::Channel
        } else {
            Surface::Desktop
        }
    }

    fn visible_tool_specs(
        claims: &GatewayCapabilityClaims,
    ) -> Vec<nomifun_gateway::ToolSpec> {
        let domains = GatewayMcpConfig::domains_for_profile(&claims.scope.profile);
        let mut specs = Registry::global().tool_specs_for_caller(
            Self::surface(claims),
            domains,
            claims.scope.instance_owner,
        );
        specs.retain(|spec| !claims.scope.excludes(spec.name));
        specs
    }

    fn is_tool_visible(claims: &GatewayCapabilityClaims, tool_name: &str) -> bool {
        if claims.scope.excludes(tool_name) {
            return false;
        }
        let domains = GatewayMcpConfig::domains_for_profile(&claims.scope.profile);
        Registry::global().tool_visible_for_caller(
            Self::surface(claims),
            domains,
            claims.scope.instance_owner,
            tool_name,
        )
    }

    fn blocked_tool_message(
        claims: &GatewayCapabilityClaims,
        tool_name: &str,
    ) -> Option<String> {
        if Self::is_tool_visible(claims, tool_name) {
            return None;
        }
        Some(
            serde_json::json!({
                "error": format!("tool '{tool_name}' is not enabled in the Platform Gateway MCP '{}' profile", claims.scope.profile),
                "tool": tool_name,
                "profile": claims.scope.profile,
                "hint": "Start a session with a broader gateway profile if this capability is needed."
            })
            .to_string(),
        )
    }

    /// Forward a tool call to the in-process gateway server over authenticated
    /// HTTP, carrying this session's identity. Preserves the gateway's
    /// structured success/error envelope for MCP `CallToolResult.isError`.
    async fn forward_tool(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        idempotency_key: &str,
    ) -> ForwardToolOutcome {
        eprintln!("[mcp-gateway-stdio] tools/call: {tool_name}");
        let body = serde_json::json!({
            "tool": tool_name,
            "args": args,
        });
        self.client
            .forward_tool_outcome_idempotent(
                GATEWAY_CALL_TOOL_OPERATION,
                body,
                true,
                idempotency_key,
            )
            .await
    }

    async fn require_operation(
        &self,
        operation: &str,
    ) -> Result<GatewayCapabilityClaims, rmcp::ErrorData> {
        self.client
            .access_for(operation)
            .await
            .map(|access| access.claims)
            .map_err(|error| {
                rmcp::ErrorData::invalid_request(
                    format!("gateway capability is no longer valid: {error}"),
                    None,
                )
            })
    }
}

impl ServerHandler for GatewayStdioServer {
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let claims = self.require_operation(GATEWAY_LIST_TOOLS_OPERATION).await?;
        let tools: Vec<Tool> = Self::visible_tool_specs(&claims)
            .into_iter()
            .map(|spec| Tool::new(spec.name, spec.description, Arc::new(spec.input_schema)))
            .collect();
        Ok(ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let claims = self.require_operation(GATEWAY_CALL_TOOL_OPERATION).await?;
        if let Some(blocked) = Self::blocked_tool_message(&claims, &request.name) {
            return Ok(build_tool_result(ForwardToolOutcome::Error(blocked)));
        }
        let execution_hint = Self::execution_operation_hint(&request)?;
        let idempotency_key =
            Self::loopback_operation_key(&claims, &request.name, execution_hint);
        let args = serde_json::Value::Object(request.arguments.unwrap_or_default());
        let result = self
            .forward_tool(&request.name, &args, &idempotency_key)
            .await;
        Ok(build_tool_result(result))
    }
}

/// Build the MCP tool result from the forwarded JSON text.
///
/// **Extensibility convention (forward-looking seam):** a capability that needs
/// to return images/binary attaches an `_mcp_images` array to its result JSON —
/// `[{"mime_type":"image/png","data":"<base64>"}]`. The bridge then emits those
/// as proper MCP `image` content parts (so a screenshot renders as an image, not
/// base64 tokens) and strips the array from the text payload to avoid duplicating
/// the bytes. Capabilities that don't set it are unaffected (fast-path: we only
/// parse when the marker is present). No registry/handler signature change is
/// needed for a future image/binary-returning capability — it just sets the key.
fn build_tool_result(outcome: ForwardToolOutcome) -> CallToolResult {
    into_mcp_tool_result(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::GatewayCapabilityScope;
    use nomifun_common::LoopbackSessionBinding;

    const TEST_OWNER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    fn test_claims() -> GatewayCapabilityClaims {
        GatewayCapabilityClaims::issue(
            TEST_OWNER_ID,
            LoopbackSessionBinding::conversation(
                "0190f5fe-7c00-7a00-8000-000000000001",
            ),
            [
                GATEWAY_LIST_TOOLS_OPERATION,
                GATEWAY_CALL_TOOL_OPERATION,
            ],
            GatewayCapabilityScope {
                companion_id: None,
                channel_platform: None,
                session_mode: None,
                profile: GatewayMcpConfig::PROFILE_FULL.into(),
                excluded_tools: Vec::new(),
                instance_owner: true,
            },
        )
        .expect("valid test capability")
    }

    #[test]
    fn structured_forward_error_becomes_mcp_tool_error() {
        let result = build_tool_result(ForwardToolOutcome::Error(
            r#"{"error":"invalid arguments for this tool: missing field `kb_id`"}"#.into(),
        ));
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn loopback_operation_key_is_stable_scoped_and_transport_safe() {
        let first_claims = test_claims();
        let mut second_claims = test_claims();
        second_claims.session =
            LoopbackSessionBinding::conversation("0190f5fe-7c00-7a00-8000-000000000002");

        let first = GatewayStdioServer::loopback_operation_key(
            &first_claims,
            "nomi_send_to_conversation",
            Some("tool-call-v1-abc"),
        );
        let retry = GatewayStdioServer::loopback_operation_key(
            &first_claims,
            "nomi_send_to_conversation",
            Some("tool-call-v1-abc"),
        );
        let other_session = GatewayStdioServer::loopback_operation_key(
            &second_claims,
            "nomi_send_to_conversation",
            Some("tool-call-v1-abc"),
        );
        let other_tool = GatewayStdioServer::loopback_operation_key(
            &first_claims,
            "nomi_conversation_status",
            Some("tool-call-v1-abc"),
        );

        assert_eq!(first, retry);
        assert_ne!(first, other_session);
        assert_ne!(first, other_tool);
        assert!(first.len() <= 128);
        assert!(
            first
                .bytes()
                .all(|byte| (0x21..=0x7e).contains(&byte))
        );
    }

    #[test]
    fn execution_operation_hint_rejects_non_string_or_illegal_metadata() {
        use rmcp::model::Meta;

        for value in [
            serde_json::Value::Bool(true),
            serde_json::Value::String(String::new()),
            serde_json::Value::String("contains space".into()),
            serde_json::Value::String("x".repeat(129)),
        ] {
            let mut meta = serde_json::Map::new();
            meta.insert(EXECUTION_OPERATION_META_KEY.to_owned(), value);
            let mut request =
                CallToolRequestParams::new("nomi_send_to_conversation");
            request.meta = Some(Meta(meta));
            assert!(GatewayStdioServer::execution_operation_hint(&request).is_err());
        }
    }

    #[test]
    fn successful_text_is_not_reclassified_by_error_words() {
        let result = build_tool_result(ForwardToolOutcome::Success(
            "Error: this is ordinary successful output".into(),
        ));
        assert_ne!(result.is_error, Some(true));
    }

    /// The bridge lists exactly what the registry exposes to a desktop session,
    /// every schema carries `properties`, and every wire name fits the Anthropic
    /// 64-char limit (`mcp__<server>__<tool>`).
    #[test]
    fn desktop_surface_lists_valid_tools() {
        let specs = Registry::global().tool_specs(Surface::Desktop);
        assert!(
            !specs.is_empty(),
            "registry must expose tools to a desktop session"
        );
        for spec in &specs {
            assert!(
                spec.input_schema.contains_key("properties"),
                "tool '{}' schema missing 'properties' (OpenAI rejects such schemas)",
                spec.name
            );
            let wire = format!("mcp__{}__{}", GatewayMcpConfig::SERVER_NAME, spec.name);
            assert!(
                wire.len() <= 64,
                "Anthropic 64-char tool-name limit exceeded: {wire} ({} chars)",
                wire.len()
            );
        }
    }

    /// External IM channels hard-deny destructive ops, so the channel surface is
    /// a strict subset that hides e.g. conversation deletion.
    #[test]
    fn channel_surface_hides_hard_denied_tools() {
        let desktop: Vec<&str> = Registry::global()
            .tool_specs(Surface::Desktop)
            .iter()
            .map(|s| s.name)
            .collect();
        let channel: Vec<&str> = Registry::global()
            .tool_specs(Surface::Channel)
            .iter()
            .map(|s| s.name)
            .collect();
        assert!(
            channel.len() < desktop.len(),
            "channel must hide at least the hard-denied destructive tools"
        );
        assert!(desktop.contains(&"nomi_delete_conversation"));
        assert!(
            !channel.contains(&"nomi_delete_conversation"),
            "destructive conversation deletion must be hidden on external channels"
        );
    }

    #[test]
    fn surface_derives_from_channel_platform() {
        let mut claims = test_claims();
        assert_eq!(GatewayStdioServer::surface(&claims), Surface::Desktop);
        claims.scope.channel_platform = Some("lark".into());
        assert_eq!(GatewayStdioServer::surface(&claims), Surface::Channel);
    }

    #[test]
    fn profile_filter_hides_domains_outside_allow_list() {
        let mut claims = test_claims();
        claims.scope.profile = GatewayMcpConfig::PROFILE_WORK.into();

        let names: Vec<&str> = GatewayStdioServer::visible_tool_specs(&claims)
            .iter()
            .map(|spec| spec.name)
            .collect();
        assert!(names.contains(&"nomi_cron_create"));
        assert!(names.contains(&"nomi_requirement_create"));
        assert!(names.contains(&"nomi_knowledge_list_bases"));
        assert!(
            names.contains(&"nomi_remote_agent_list"),
            "the work profile must let a trusted Nomi session discover saved OpenClaw gateways"
        );
        assert!(
            names.contains(&"nomi_remote_agent_handshake"),
            "the work profile must let a trusted desktop Nomi session verify a saved gateway"
        );
        // The desktop default (work) profile exposes the unified collaboration
        // surface so the lead Agent can delegate or create persistent executions.
        assert!(names.contains(&"nomi_delegate"));
        assert!(names.contains(&"nomi_execution_get"));
        assert!(names.contains(&"nomi_execution_update"));
        assert!(!names.contains(&"nomi_workshop_generate"));
        assert!(!names.contains(&"nomi_system_update_settings"));
        assert!(!names.contains(&"nomi_mcp_add_server"));
    }

    #[test]
    fn work_profile_blocks_workshop_for_conversations_and_companions() {
        let mut claims = test_claims();
        claims.scope.profile = GatewayMcpConfig::PROFILE_WORK.into();

        for companion_id in [
            None,
            Some(
                nomifun_common::CompanionId::parse(
                    "0190f5fe-7c00-7a00-8000-000000000001",
                )
                .unwrap(),
            ),
        ] {
            claims.scope.companion_id = companion_id;
            let names: Vec<&str> = GatewayStdioServer::visible_tool_specs(&claims)
                .iter()
                .map(|spec| spec.name)
                .collect();
            assert!(!names.iter().any(|name| name.starts_with("nomi_workshop_")));
            assert!(
                GatewayStdioServer::blocked_tool_message(
                    &claims,
                    "nomi_workshop_generate",
                )
                .is_some()
            );
        }
    }

    #[test]
    fn profile_filter_blocks_direct_call_to_hidden_tool() {
        let mut claims = test_claims();
        claims.scope.profile = GatewayMcpConfig::PROFILE_WORK.into();

        assert!(GatewayStdioServer::blocked_tool_message(&claims, "nomi_cron_create").is_none());
        let blocked = GatewayStdioServer::blocked_tool_message(
            &claims,
            "nomi_system_update_settings",
        )
            .expect("system tool must be blocked by work profile");
        assert!(blocked.contains("not enabled"));
        assert!(blocked.contains(GatewayMcpConfig::PROFILE_WORK));
    }

    #[test]
    fn exact_exclusion_removes_delegate_but_keeps_execution_inspection() {
        let mut claims = test_claims();
        claims.scope.profile = GatewayMcpConfig::PROFILE_WORK.into();
        claims.scope.excluded_tools = vec!["nomi_delegate".to_owned()];

        let names: Vec<&str> = GatewayStdioServer::visible_tool_specs(&claims)
            .iter()
            .map(|spec| spec.name)
            .collect();
        assert!(!names.contains(&"nomi_delegate"));
        assert!(names.contains(&"nomi_execution_get"));
        assert!(names.contains(&"nomi_execution_update"));
        assert!(GatewayStdioServer::blocked_tool_message(&claims, "nomi_delegate").is_some());
    }

    #[test]
    fn ordinary_conversation_hides_top_level_creation_but_companion_keeps_it() {
        let mut claims = test_claims();
        claims.scope.profile = GatewayMcpConfig::PROFILE_WORK.into();

        let plain_names: Vec<&str> = GatewayStdioServer::visible_tool_specs(&claims)
            .iter()
            .map(|spec| spec.name)
            .collect();
        assert!(!plain_names.contains(&"nomi_create_conversation"));
        assert!(plain_names.contains(&"nomi_delegate"));
        assert!(
            GatewayStdioServer::blocked_tool_message(&claims, "nomi_create_conversation")
                .is_some()
        );

        claims.scope.companion_id = Some(
            nomifun_common::CompanionId::parse(
                "0190f5fe-7c00-7a00-8000-000000000001",
            )
            .unwrap(),
        );
        let companion_names: Vec<&str> = GatewayStdioServer::visible_tool_specs(&claims)
            .iter()
            .map(|spec| spec.name)
            .collect();
        assert!(companion_names.contains(&"nomi_create_conversation"));
        assert!(
            GatewayStdioServer::blocked_tool_message(&claims, "nomi_create_conversation")
                .is_none()
        );
    }

    #[test]
    fn secondary_session_keeps_user_tools_and_never_sees_owner_tools() {
        let mut claims = test_claims();
        claims.user_id = nomifun_common::UserId::parse(
            "0190f5fe-7c00-7a00-8000-000000000002",
        )
        .unwrap();
        claims.scope.instance_owner = false;
        claims.scope.profile = GatewayMcpConfig::PROFILE_WORK.into();

        let names: Vec<&str> = GatewayStdioServer::visible_tool_specs(&claims)
            .iter()
            .map(|spec| spec.name)
            .collect();
        assert!(names.contains(&"nomi_cron_create"));
        assert!(!names.contains(&"nomi_delegate"));
        assert!(!names.contains(&"nomi_execution_get"));
        assert!(!names.contains(&"nomi_execution_update"));
        assert!(!names.contains(&"nomi_requirement_create"));
        assert!(!names.contains(&"nomi_knowledge_list_bases"));
        assert!(GatewayStdioServer::blocked_tool_message(&claims, "nomi_requirement_create").is_some());
        assert!(GatewayStdioServer::blocked_tool_message(&claims, "nomi_delegate").is_some());
    }

    #[test]
    fn bridge_revalidates_operation_scope_and_expiry_for_every_request() {
        let mut claims = test_claims();
        claims.allowed_tools = vec![GATEWAY_CALL_TOOL_OPERATION.into()];
        assert!(!claims.allows(GATEWAY_LIST_TOOLS_OPERATION));
        assert!(claims.allows(GATEWAY_CALL_TOOL_OPERATION));

        let now = nomifun_common::unix_time_secs();
        claims.issued_at_unix_secs = now.saturating_sub(61);
        claims.expires_at_unix_secs = now.saturating_sub(31);
        assert!(claims.validate_at(now).is_err());

        let mut claims = test_claims();
        claims.session = LoopbackSessionBinding::terminal("terminal-1");
        assert!(validate_gateway_claims(&claims).is_err());
    }
}
