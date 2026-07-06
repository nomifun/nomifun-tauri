//! `nomicore mcp-gateway-stdio` subcommand: MCP stdio server for the Desktop
//! Gateway capabilities (`nomi_*` — the whole platform control surface).
//!
//! Spawned by agent sessions entitled to the Desktop Gateway. Uses the `rmcp`
//! crate for protocol handling so it is byte-compatible with each CLI's MCP
//! client (claude / codex / gemini advertise stdio-only MCP capabilities) and
//! with the nomi engine's MCP manager.
//!
//! This bridge is now FULLY REGISTRY-DRIVEN: it declares no per-tool parameter
//! struct or `#[tool]` method. `tools/list` is projected from the capability
//! registry (`nomifun_gateway::Registry`) filtered by this session's permission
//! surface, and `tools/call` forwards the raw arguments to the in-process
//! `GatewayMcpServer` (`http://127.0.0.1:{NOMI_GW_MCP_PORT}/tool`), which
//! deserializes them into the capability's single typed request and enforces the
//! danger-tier × surface gate. Adding/renaming a capability in `nomifun-gateway`
//! updates this bridge automatically — there is nothing to keep in sync here.

use std::process::ExitCode;
use std::sync::Arc;

use nomifun_api_types::GatewayMcpConfig;
use nomifun_gateway::{Registry, Surface};
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams, Tool,
};
use rmcp::service::{RequestContext, RoleServer, ServiceExt};
use rmcp::transport;

pub async fn run_gateway_stdio() -> ExitCode {
    let port = match std::env::var(GatewayMcpConfig::ENV_PORT) {
        Ok(p) => p,
        Err(_) => {
            eprintln!(
                "[mcp-gateway-stdio] ERROR: missing {}",
                GatewayMcpConfig::ENV_PORT
            );
            return ExitCode::from(1);
        }
    };
    let token = match std::env::var(GatewayMcpConfig::ENV_TOKEN) {
        Ok(t) => t,
        Err(_) => {
            eprintln!(
                "[mcp-gateway-stdio] ERROR: missing {}",
                GatewayMcpConfig::ENV_TOKEN
            );
            return ExitCode::from(1);
        }
    };
    let conversation_id = std::env::var(GatewayMcpConfig::ENV_CONVERSATION_ID).unwrap_or_default();
    let user_id = std::env::var(GatewayMcpConfig::ENV_USER_ID).unwrap_or_default();
    // Optional: only sessions with a companion binding carry it (multi-companion upgrade).
    let companion_id = std::env::var(GatewayMcpConfig::ENV_COMPANION_ID)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    // Optional: only channel master-agent sessions carry it.
    let channel_platform = std::env::var(GatewayMcpConfig::ENV_CHANNEL_PLATFORM)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let session_mode = std::env::var(GatewayMcpConfig::ENV_SESSION_MODE)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let tool_filter = GatewayToolFilter::from_env();

    eprintln!("[mcp-gateway-stdio] Started OK. PORT={port}, CONV_ID={conversation_id}");

    let http_client = super::stdio_common::build_bridge_http_client();

    let server = GatewayStdioServer {
        port: port.parse().unwrap_or(0),
        token,
        conversation_id,
        user_id,
        companion_id,
        channel_platform,
        session_mode,
        tool_filter,
        http_client,
    };

    let transport = transport::io::stdio();
    match server.serve(transport).await {
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
    }
}

#[derive(Clone)]
struct GatewayStdioServer {
    port: u16,
    token: String,
    conversation_id: String,
    user_id: String,
    /// The companion the calling session is bound to (from `NOMI_GW_MCP_COMPANION_ID`);
    /// `None` when the session has no companion binding.
    companion_id: Option<String>,
    /// IM platform when this is a channel master-agent session (from
    /// `NOMI_GW_MCP_CHANNEL_PLATFORM`); `None` for plain companion/desktop.
    channel_platform: Option<String>,
    /// Resolved approval mode for the calling session.
    session_mode: Option<String>,
    tool_filter: GatewayToolFilter,
    http_client: reqwest::Client,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct GatewayToolFilter {
    profile: Option<String>,
    domains: Vec<String>,
}

impl GatewayToolFilter {
    fn from_env() -> Self {
        let domains = std::env::var(GatewayMcpConfig::ENV_DOMAINS)
            .ok()
            .map(|raw| parse_domain_csv(&raw))
            .unwrap_or_default();
        let profile = std::env::var(GatewayMcpConfig::ENV_PROFILE)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        Self { profile, domains }
    }

    #[cfg(test)]
    fn from_profile(profile: &str) -> Self {
        Self {
            profile: Some(profile.to_owned()),
            domains: Vec::new(),
        }
    }

    #[cfg(test)]
    fn from_domains(domains: &[&str]) -> Self {
        Self {
            profile: Some(GatewayMcpConfig::PROFILE_FULL.to_owned()),
            domains: domains.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn label(&self) -> &str {
        if !self.domains.is_empty() {
            "custom"
        } else {
            self.profile
                .as_deref()
                .unwrap_or(GatewayMcpConfig::PROFILE_FULL)
        }
    }

    fn domain_refs(&self) -> Option<Vec<&str>> {
        if !self.domains.is_empty() {
            return Some(self.domains.iter().map(String::as_str).collect());
        }
        self.profile
            .as_deref()
            .and_then(GatewayMcpConfig::domains_for_profile)
            .map(|domains| domains.to_vec())
    }

    fn is_full(&self) -> bool {
        self.domain_refs().is_none()
    }
}

fn parse_domain_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

impl GatewayStdioServer {
    /// The permission surface this bridge session acts on (mirrors
    /// `nomifun_gateway::CallerCtx::surface`): an IM channel platform marks an
    /// external session, otherwise it is a local desktop session.
    fn surface(&self) -> Surface {
        if self.channel_platform.is_some() {
            Surface::Channel
        } else {
            Surface::Desktop
        }
    }

    fn visible_tool_specs(&self) -> Vec<nomifun_gateway::ToolSpec> {
        match self.tool_filter.domain_refs() {
            Some(domains) => Registry::global().tool_specs_for(self.surface(), &domains),
            None => Registry::global().tool_specs(self.surface()),
        }
    }

    fn is_tool_visible(&self, tool_name: &str) -> bool {
        match self.tool_filter.domain_refs() {
            Some(domains) => {
                Registry::global().tool_visible_for(self.surface(), &domains, tool_name)
            }
            None => Registry::global().tool_visible(self.surface(), tool_name),
        }
    }

    fn blocked_tool_message(&self, tool_name: &str) -> Option<String> {
        if self.tool_filter.is_full() || self.is_tool_visible(tool_name) {
            return None;
        }
        Some(
            serde_json::json!({
                "error": format!("tool '{tool_name}' is not enabled in the Desktop Gateway MCP '{}' profile", self.tool_filter.label()),
                "tool": tool_name,
                "profile": self.tool_filter.label(),
                "hint": "Start a session with a broader gateway profile if this capability is needed."
            })
            .to_string(),
        )
    }

    /// Forward a tool call to the in-process gateway server over authenticated
    /// HTTP, carrying this session's identity. Returns the tool result JSON text.
    async fn forward_tool(&self, tool_name: &str, args: &serde_json::Value) -> String {
        eprintln!("[mcp-gateway-stdio] tools/call: {tool_name}");
        let body = serde_json::json!({
            "tool": tool_name,
            "args": args,
            "conversation_id": self.conversation_id,
            "user_id": self.user_id,
            "companion_id": self.companion_id,
            "channel_platform": self.channel_platform,
            "session_mode": self.session_mode,
        });
        super::stdio_common::forward_tool_http(
            &self.http_client,
            self.port,
            &self.token,
            "mcp-gateway-stdio",
            &body,
            true,
        )
        .await
    }
}

impl ServerHandler for GatewayStdioServer {
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let tools: Vec<Tool> = self
            .visible_tool_specs()
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
        if let Some(blocked) = self.blocked_tool_message(&request.name) {
            return Ok(build_tool_result(blocked));
        }
        let args = serde_json::Value::Object(request.arguments.unwrap_or_default());
        let result = self.forward_tool(&request.name, &args).await;
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
fn build_tool_result(text: String) -> CallToolResult {
    if !text.contains("_mcp_images") {
        return CallToolResult::success(vec![Content::text(text)]);
    }
    let parsed: Option<serde_json::Value> = serde_json::from_str(&text).ok();
    let images: Vec<Content> = parsed
        .as_ref()
        .and_then(|v| v.get("_mcp_images"))
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|img| {
                    let data = img.get("data").and_then(serde_json::Value::as_str)?;
                    let mime = img.get("mime_type").and_then(serde_json::Value::as_str)?;
                    Some(Content::image(data.to_owned(), mime.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    if images.is_empty() {
        // Marker substring present but no valid images (e.g. it appeared inside a
        // normal string) — emit the original text unchanged.
        return CallToolResult::success(vec![Content::text(text)]);
    }
    // Strip `_mcp_images` from the text so the base64 isn't also sent as tokens.
    let text_out = match parsed {
        Some(serde_json::Value::Object(mut m)) => {
            m.remove("_mcp_images");
            serde_json::to_string(&serde_json::Value::Object(m)).unwrap_or(text)
        }
        _ => text,
    };
    let mut contents = vec![Content::text(text_out)];
    contents.extend(images);
    CallToolResult::success(contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_server() -> GatewayStdioServer {
        GatewayStdioServer {
            port: 0,
            token: String::new(),
            conversation_id: "1".into(),
            user_id: "u1".into(),
            companion_id: None,
            channel_platform: None,
            session_mode: None,
            tool_filter: GatewayToolFilter::default(),
            http_client: reqwest::Client::new(),
        }
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
        let mut s = test_server();
        assert_eq!(s.surface(), Surface::Desktop);
        s.channel_platform = Some("lark".into());
        assert_eq!(s.surface(), Surface::Channel);
    }

    #[test]
    fn profile_filter_hides_domains_outside_allow_list() {
        let mut s = test_server();
        s.tool_filter = GatewayToolFilter::from_profile(GatewayMcpConfig::PROFILE_WORK);

        let names: Vec<&str> = s
            .visible_tool_specs()
            .iter()
            .map(|spec| spec.name)
            .collect();
        assert!(names.contains(&"nomi_cron_create"));
        assert!(names.contains(&"nomi_requirement_create"));
        assert!(names.contains(&"nomi_knowledge_list_bases"));
        // The desktop default (work) profile must expose orchestration so the
        // lead/main agent can spin up multi-agent runs (Direction B: 智能编排).
        assert!(names.contains(&"nomi_run_create"));
        assert!(!names.contains(&"nomi_system_update_settings"));
        assert!(!names.contains(&"nomi_mcp_add_server"));
    }

    #[test]
    fn profile_filter_blocks_direct_call_to_hidden_tool() {
        let mut s = test_server();
        s.tool_filter = GatewayToolFilter::from_profile(GatewayMcpConfig::PROFILE_WORK);

        assert!(s.blocked_tool_message("nomi_cron_create").is_none());
        let blocked = s
            .blocked_tool_message("nomi_system_update_settings")
            .expect("system tool must be blocked by work profile");
        assert!(blocked.contains("not enabled"));
        assert!(blocked.contains(GatewayMcpConfig::PROFILE_WORK));
    }

    #[test]
    fn custom_domain_filter_takes_precedence_over_profile() {
        let mut s = test_server();
        s.tool_filter = GatewayToolFilter::from_domains(&["cron"]);

        let names: Vec<&str> = s
            .visible_tool_specs()
            .iter()
            .map(|spec| spec.name)
            .collect();
        assert!(names.contains(&"nomi_cron_create"));
        assert!(!names.contains(&"nomi_requirement_create"));
        assert!(!names.contains(&"nomi_system_update_settings"));
    }
}
