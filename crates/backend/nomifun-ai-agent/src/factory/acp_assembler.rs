use crate::session::PersistedSessionState;
use agent_client_protocol::schema::{EnvVariable, McpServer, McpServerStdio, NewSessionRequest};
use nomifun_api_types::AgentMetadata;
use nomifun_api_types::{
    AcpBuildExtra, BrowserMcpConfig, ComputerMcpConfig, GatewayMcpConfig, OpenMcpConfig,
    RequirementMcpConfig,
};
use nomifun_common::{
    CommandSpec, LoopbackCapabilityLease, LoopbackCapabilityLeaseSet,
};
use nomifun_knowledge::context::{
    KnowledgeContextFormat, KnowledgeContextOptions, build_knowledge_context,
};
use std::path::PathBuf;

/// Pre-computed workspace information.
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub path: String,
    pub is_custom: bool,
}

/// All pre-computed parameters needed to create and drive an ACP session.
///
/// Assembled once by `assemble_acp_params` in the factory layer; the
/// `AcpAgentManager` reads from this but never mutates it. By front-loading
/// the decision logic (which MCP servers to inject, what preset context to
/// compose) we keep the manager focused on execution + state.
#[derive(Debug, Clone)]
pub struct AcpSessionParams {
    pub conversation_id: String,
    pub workspace: WorkspaceInfo,
    pub metadata: AgentMetadata,
    pub command_spec: CommandSpec,
    pub config: AcpBuildExtra,
    pub mcp_servers: Vec<McpServer>,
    /// Process-local capability guards for the scoped built-in MCP bridges.
    /// The manager's `Arc<AcpSessionParams>` owns these for exactly the ACP
    /// runtime lifetime; final drop revokes every renewable lease.
    pub loopback_capability_leases: LoopbackCapabilityLeaseSet,
    pub preset_context: Option<String>,
    /// The knowledge-base retrieval-protocol section, kept SEPARATE from
    /// `preset_context`. Delivered on the FIRST prompt of every session open
    /// (new AND resume) by `KnowledgeContextHook`, not folded into the
    /// new-session-only `[Assistant Rules]` prelude. This is what lets a
    /// resumed/restarted ACP session — or a session whose `挂载知识库` binding
    /// changed mid-conversation (after a rebuild) — still receive the
    /// retrieval protocol. `None`/empty means no bases are mounted.
    pub knowledge_context: Option<String>,
    pub session_snapshot: Option<PersistedSessionState>,
    /// Backend data directory (`AppConfig.data_dir`). Passed through to
    /// `CliAgentProcess::spawn_for_sdk` so bun cache / tmp directories
    /// land under the operator-chosen path rather than the OS default.
    pub data_dir: PathBuf,
}

impl AcpSessionParams {
    /// Build a `NewSessionRequest` using the pre-computed MCP servers.
    pub fn new_session_request(&self) -> NewSessionRequest {
        let req = NewSessionRequest::new(&self.workspace.path);
        if self.mcp_servers.is_empty() {
            req
        } else {
            req.mcp_servers(self.mcp_servers.clone())
        }
    }
}

/// Assemble fully-resolved ACP session params from factory inputs.
///
/// This front-loads all decision logic that was previously scattered across
/// `build_new_session_request`, preset-context composition, and the factory's
/// ACP match arm.
///
/// `user_mcp_servers` are operator-configured MCP servers loaded from the DB
/// by the factory layer; they are appended after built-in bridges so the agent
/// gets *all* the user's tools on `session/new` (ELECTRON-1JG fix).
#[allow(clippy::too_many_arguments)]
pub async fn assemble_acp_params(
    conversation_id: String,
    workspace: WorkspaceInfo,
    metadata: AgentMetadata,
    command_spec: CommandSpec,
    config: AcpBuildExtra,
    user_mcp_servers: Vec<McpServer>,
    session_snapshot: Option<PersistedSessionState>,
    data_dir: PathBuf,
) -> AcpSessionParams {
    let (mcp_servers, loopback_capability_leases) = resolve_mcp_servers(
        &config,
        &conversation_id,
        &workspace.path,
        user_mcp_servers,
    );
    let preset_context = append_launch_nudge(
        compose_preset_context(
            config.preset_context.as_deref(),
            config.backend.as_deref(),
        ),
        config.open_mcp_config.is_some(),
        config.computer_mcp_config.is_some(),
        config.browser_mcp_config.is_some(),
    );
    // Knowledge is delivered separately from preset_context (see
    // `AcpSessionParams::knowledge_context` and `KnowledgeContextHook`), so it
    // reaches resumed sessions too — not only `session/new`.
    let knowledge_context = build_knowledge_context_section(&config, &conversation_id);

    AcpSessionParams {
        conversation_id,
        workspace,
        metadata,
        command_spec,
        config,
        mcp_servers,
        loopback_capability_leases,
        preset_context,
        knowledge_context,
        session_snapshot,
        data_dir,
    }
}

/// Determine which MCP servers to inject into `session/new`.
///
/// Layout: `[requirement?, ...user_mcp_servers]`. The requirement MCP server is
/// injected independently so that
/// AutoWork can drive any ACP session; it is harmless when the session is not an
/// AutoWork target because its tools are simply never called. The user's own
/// enabled MCP servers are always appended last.
fn resolve_mcp_servers(
    config: &AcpBuildExtra,
    conversation_id: &str,
    workspace_path: &str,
    user_mcp_servers: Vec<McpServer>,
) -> (Vec<McpServer>, LoopbackCapabilityLeaseSet) {
    let mut servers: Vec<McpServer> = Vec::new();
    let mut leases = LoopbackCapabilityLeaseSet::new();
    if let (Some(req_cfg), Some(user_id)) = (
        config.requirement_mcp_config.as_ref(),
        config.user_id.as_deref(),
    ) {
        if let Some((server, lease)) = requirement_mcp_server(
            req_cfg,
            user_id,
            conversation_id,
        ) {
            servers.push(server);
            leases.push(lease);
        }
    }
    // Scoped knowledge-search MCP: injected ONLY when the session has bound
    // bases (independent of the platform Gateway). The bound base ids are baked
    // into the server's env here, so the model-facing tool stays query-only.
    if let Some(cfg) = config.knowledge_mcp_config.as_ref()
        && let Some(user_id) = config.user_id.as_deref()
        && !config.knowledge_mounts.is_empty()
    {
        let kb_ids: Vec<nomifun_common::KnowledgeBaseId> = config
            .knowledge_mounts
            .iter()
            .map(|m| m.knowledge_base_id.clone())
            .collect();
        if let Some((server, lease)) = knowledge_mcp_server(
            cfg,
            user_id,
            conversation_id,
            workspace_path,
            &kb_ids,
            config.knowledge_writeback,
        ) {
            servers.push(server);
            leases.push(lease);
        }
    }
    // Reliable-launch `open` tool, injected unconditionally like requirement
    // (config is `Some` only on Windows, so this is a no-op on macOS/Linux).
    if let Some(open_cfg) = config.open_mcp_config.as_ref() {
        servers.push(open_mcp_server(open_cfg));
    }
    // Computer-use discrete tools, injected unconditionally like the open bridge
    // (config is `Some` on every desktop OS built with the `computer-use`
    // feature; `None` on web/headless builds, so this is a no-op there).
    if let Some(computer_cfg) = config.computer_mcp_config.as_ref() {
        servers.push(computer_mcp_server(computer_cfg));
    }
    // Browser-use discrete tools, injected unconditionally and symmetric with the
    // computer bridge (P4-2, 裁决①). `browser_mcp_config` is `Some` on every
    // desktop OS built with the `browser-use` feature; `None` on web/headless, so
    // this is a no-op there. Pushed after computer so the wire layout stays
    // deterministic.
    if let Some(browser_cfg) = config.browser_mcp_config.as_ref() {
        servers.push(browser_mcp_server(browser_cfg));
    }
    if let Some(gw_cfg) = config.gateway_mcp_config.as_ref() {
        if let Some((server, lease)) = gateway_mcp_server(gw_cfg, config, conversation_id) {
            servers.push(server);
            leases.push(lease);
        }
    }
    servers.extend(user_mcp_servers);
    (servers, leases)
}

/// Compose first-message preset context.
fn compose_preset_context(
    base_preset_context: Option<&str>,
    _backend: Option<&str>,
) -> Option<String> {
    base_preset_context
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Prepend forceful `[Assistant Rules]` to the session preset context when the
/// desktop bridges are injected. Two INDEPENDENT pieces, each data-driven on its
/// own bridge's presence (both OS-gated upstream in `services.rs`):
///
///   - Reliable-launch rule — emitted ONLY when the `open` bridge is present
///     (`open_mcp_config` is `Some` only on Windows). On that console-less
///     Windows host, launching GUI apps/URLs via the shell (`cmd /c start`,
///     `Start-Process`, an `.exe`) FAILS and pops a blocking "Windows cannot
///     find" dialog; the `open` tool's `ShellExecuteExW` works. macOS/Linux
///     launch fine via the shell (`open`/`xdg-open`), so this Windows-specific
///     rule is deliberately NOT emitted there.
///   - Desktop-control rule — emitted whenever the computer bridge is present
///     (`computer_mcp_config` is `Some` on EVERY desktop OS with the
///     `computer-use` build). Platform-neutral.
///   - Browser-control rule — emitted whenever the browser bridge is present
///     (`browser_mcp_config` is `Some` on EVERY desktop OS with the
///     `browser-use` build). Platform-neutral; describes the CORE LOOP
///     (navigate → observe → act by `[ref]` → observe to verify).
///
/// Prepended (not appended) so the rules lead the preset context.
fn append_launch_nudge(
    ctx: Option<String>,
    open_injected: bool,
    computer_injected: bool,
    browser_injected: bool,
) -> Option<String> {
    if !open_injected && !computer_injected && !browser_injected {
        return ctx;
    }
    let mut rule = String::new();

    // Windows-only: steer the model off the failing shell-launch path. Gated on
    // `open_injected`, which is `Some` only on Windows (services.rs), so this
    // never reaches macOS/Linux sessions.
    if open_injected {
        rule.push_str(
            "[Launching apps/URLs — MANDATORY on this Windows host] To open ANY URL, file, folder, or \
            application on the user's desktop, you MUST call the `open` tool (MCP server `nomifun-open`). \
            Pass `target` = a URL (https://…), a file/folder path, or an app name (\"msedge\", \"notepad\"); \
            optionally pass `app` to open a URL in a specific browser (e.g. target=URL, app=\"msedge\"). \
            NEVER launch apps/URLs by running `cmd /c start`, `start`, `Start-Process`, `explorer`, or an \
            `.exe` path in the shell (Bash/exec_command) — on this host those FAIL (the shell has no \
            console) and pop a blocking \"Windows 找不到\" / \"cannot find\" modal dialog at the user. Use \
            the shell only for non-launch work (file ops, `taskkill` to close apps, queries).",
        );
    }

    // Cross-platform: describe the discrete desktop-control tools. Emitted on
    // every desktop OS where the computer bridge is injected.
    if computer_injected {
        if !rule.is_empty() {
            rule.push_str("\n\n");
        }
        rule.push_str(
            "[Controlling the desktop] You can SEE and operate the desktop with the \
            `nomifun-computer` MCP tools: call `snapshot` to get a numbered [ref] tree of windows \
            and controls (+ a screenshot), then act with `click`/`right_click`/`double_click`/\
            `set_value` by [ref], or `type`/`key`/`scroll`/`click_xy` for raw input. To open an \
            application, URL, file, or folder, use the `launch` tool. Re-run `snapshot` after any \
            UI change — a [ref] is only valid for the latest snapshot. Prefer these over guessing \
            pixel coordinates.",
        );
    }

    // Cross-platform: describe the browser-control tools. Emitted on every
    // desktop OS where the browser bridge is injected. Symmetric with the
    // computer rule; teaches the CORE LOOP (BrowserTool::DESCRIPTION).
    if browser_injected {
        if !rule.is_empty() {
            rule.push_str("\n\n");
        }
        rule.push_str(
            "[Driving the web] You can drive a managed Chromium browser with the `nomifun-browser` \
            MCP tools. THE CORE LOOP: `navigate` → `observe` → act by `[ref]` → `observe` again to \
            verify. `navigate` loads a URL; `observe` returns the page's accessibility tree as an \
            aria snapshot plus numbered `[ref=f<seq>e<n>]` handles. Every interactive action \
            (`click`/`type`/`select`/…) targets an element by its `[ref]` from the MOST RECENT \
            `observe` — a ref goes stale after any navigation or page change, so re-`observe` for \
            fresh refs and to confirm the result of each write action.",
        );
    }

    Some(match ctx {
        Some(c) => format!("{rule}\n\n{c}"),
        None => rule,
    })
}

/// Build the knowledge-base section for the conversation's mounted bases, if
/// any. Rendering is delegated to the shared builder
/// (`nomifun_knowledge::context::build_knowledge_context`, `PromptSection`
/// format) — the single source of truth for the retrieval protocol, per-base
/// sections, and the write-back ("回血") contract: read-only, staged
/// (`_inbox/{conversation_id}/`), or direct.
///
/// The section is kept SEPARATE from `preset_context` (it is NOT part of the
/// new-session `[Assistant Rules]` prelude) so it can be delivered on the
/// first prompt of EVERY session open — including `session/load` / claude
/// resume — via `KnowledgeContextHook`. Returns `None` when nothing is mounted.
fn build_knowledge_context_section(
    config: &AcpBuildExtra,
    conversation_id: &str,
) -> Option<String> {
    build_knowledge_context(
        &config.knowledge_mounts,
        &KnowledgeContextOptions {
            format: KnowledgeContextFormat::PromptSection,
            writeback: config.knowledge_writeback,
            writeback_mode: config.knowledge_writeback_mode.as_deref(),
            writeback_eagerness: config.knowledge_writeback_eagerness.as_deref(),
            target_id: conversation_id,
            has_search_tool: config.knowledge_mcp_config.is_some()
                && !config.knowledge_mounts.is_empty(),
            // ACP/terminal sessions now have a real knowledge_write tool via the
            // scoped MCP bridge (P2): when write-back is enabled and the MCP is
            // injected with mounted bases, point the model at knowledge_write
            // (handle/base+rel_path) instead of the file-write prose. The server
            // resolves staged/direct placement from the workpath binding.
            has_write_tool: config.knowledge_writeback
                && config.knowledge_mcp_config.is_some()
                && !config.knowledge_mounts.is_empty(),
        },
    )
}

/// Build the requirement MCP stdio bridge server for an ACP session. The bridge
/// (`nomicore mcp-requirement-stdio`) forwards `requirement_complete` /
/// `requirement_update_status` calls back to the in-process
/// `RequirementMcpServer`. `conversation_id` is passed so mutations can be
/// scoped to the calling session.
fn requirement_mcp_server(
    cfg: &RequirementMcpConfig,
    user_id: &str,
    conversation_id: &str,
) -> Option<(McpServer, LoopbackCapabilityLease)> {
    let child = match cfg.issue_for_conversation(user_id, conversation_id) {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, conversation_id, "requirement MCP capability issuance failed closed");
            return None;
        }
    };
    let env = vec![EnvVariable::new(
        RequirementMcpConfig::ENV_CAPABILITY.to_owned(),
        child
            .bootstrap_json()
            .expect("validated requirement bootstrap serializes"),
    )];
    let stdio = McpServerStdio::new(
        RequirementMcpConfig::SERVER_NAME,
        &child.binary_path,
    )
        .args(vec!["mcp-requirement-stdio".to_owned()])
        .env(env);
    Some((McpServer::Stdio(stdio), child.lease))
}

/// Build the scoped knowledge-search MCP stdio bridge server for an ACP session
/// that has bound knowledge bases. The bridge (`nomicore mcp-knowledge-stdio`)
/// forwards `knowledge_search` calls back to the in-process retrieval server.
///
/// SECURITY: workspace, bound `kb_ids`, write authority, user and session are
/// signed into one short-lived claims document. The child receives only the
/// scoped token, never this issuer's root secret, and the model-facing tool
/// cannot widen the searchable base set.
fn knowledge_mcp_server(
    cfg: &nomifun_api_types::KnowledgeMcpConfig,
    user_id: &str,
    conversation_id: &str,
    workspace_path: &str,
    kb_ids: &[nomifun_common::KnowledgeBaseId],
    allow_write: bool,
) -> Option<(McpServer, LoopbackCapabilityLease)> {
    let child = match cfg.issue_for_conversation(
        user_id,
        conversation_id,
        workspace_path,
        kb_ids,
        allow_write,
    ) {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, conversation_id, "knowledge MCP capability issuance failed closed");
            return None;
        }
    };
    let env = vec![EnvVariable::new(
        nomifun_api_types::KnowledgeMcpConfig::ENV_CAPABILITY.to_owned(),
        child
            .bootstrap_json()
            .expect("validated knowledge bootstrap serializes"),
    )];
    let stdio = McpServerStdio::new(
        nomifun_api_types::KnowledgeMcpConfig::SERVER_NAME,
        &child.binary_path,
    )
    .args(vec!["mcp-knowledge-stdio".to_owned()])
    .env(env);
    Some((McpServer::Stdio(stdio), child.lease))
}

/// Build the reliable-launch (`open`) MCP stdio bridge server. The bridge
/// (`nomicore mcp-open-stdio`) is stateless — its single `open` tool
/// ShellExecutes the target locally — so it needs no env (no port/token/conv id).
fn open_mcp_server(cfg: &OpenMcpConfig) -> McpServer {
    let stdio = McpServerStdio::new(OpenMcpConfig::SERVER_NAME, &cfg.binary_path)
        .args(vec!["mcp-open-stdio".to_owned()])
        .env(Vec::new());
    McpServer::Stdio(stdio)
}

/// Build the computer-use discrete-tool MCP stdio bridge server. The bridge
/// (`nomicore mcp-computer-stdio`) drives the local desktop directly (a facade
/// over the in-tree ComputerTool), so it needs no env (no port/token/conv id).
fn computer_mcp_server(cfg: &ComputerMcpConfig) -> McpServer {
    let stdio = McpServerStdio::new(ComputerMcpConfig::SERVER_NAME, &cfg.binary_path)
        .args(vec!["mcp-computer-stdio".to_owned()])
        .env(Vec::new());
    McpServer::Stdio(stdio)
}

/// Build the browser-use discrete-tool MCP stdio bridge server. The bridge
/// (`nomicore mcp-browser-stdio`) drives a managed Chromium directly (a facade
/// over the in-tree BrowserTool), so it needs no env (no port/token/conv id) —
/// stateless fail-safe, symmetric with the open/computer bridges. R2: carrying
/// NO env-borne session context is deliberate (secret:NAME fails closed, downloads
/// land in the data-dir sandbox; per-pet context stays on the nomi engine path).
fn browser_mcp_server(cfg: &BrowserMcpConfig) -> McpServer {
    let stdio = McpServerStdio::new(BrowserMcpConfig::SERVER_NAME, &cfg.binary_path)
        .args(vec!["mcp-browser-stdio".to_owned()])
        .env(Vec::new());
    McpServer::Stdio(stdio)
}

/// Build the Platform Gateway MCP stdio bridge for an ACP session. The
/// process-owned issuer config is the authority; no Conversation JSON flag
/// participates. The bridge receives only one short-lived child capability.
/// (`nomicore mcp-gateway-stdio`) forwards every `nomi_*` desktop tool call
/// back to the in-process `GatewayMcpServer`. Caller conversation + user ids
/// are passed for self-protection and data scoping; the companion binding (when
/// present) rides along for attribution.
fn gateway_mcp_server(
    cfg: &GatewayMcpConfig,
    extra: &AcpBuildExtra,
    conversation_id: &str,
) -> Option<(McpServer, LoopbackCapabilityLease)> {
    let Some(user_id) = extra.user_id.as_deref() else {
        tracing::warn!(conversation_id, "gateway MCP capability issuance requires a user ID");
        return None;
    };
    let child = match cfg.issue_for_conversation(
        user_id,
        conversation_id,
        extra.companion_id.as_deref(),
        extra.channel_platform.as_deref(),
        extra.session_mode.as_deref(),
        &extra.gateway_excluded_tools,
    ) {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, conversation_id, "gateway MCP capability issuance failed closed");
            return None;
        }
    };
    let env = vec![EnvVariable::new(
        GatewayMcpConfig::ENV_CAPABILITY.to_owned(),
        child
            .bootstrap_json()
            .expect("validated gateway bootstrap serializes"),
    )];
    let stdio = McpServerStdio::new(GatewayMcpConfig::SERVER_NAME, &child.binary_path)
        .args(vec!["mcp-gateway-stdio".to_owned()])
        .env(env);
    Some((McpServer::Stdio(stdio), child.lease))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_issuer() -> std::sync::Arc<nomifun_common::LoopbackCapabilityIssuer> {
        std::sync::Arc::new(nomifun_common::LoopbackCapabilityIssuer::random().unwrap())
    }

    fn requirement_config(port: u16, binary: &str) -> RequirementMcpConfig {
        RequirementMcpConfig::from_issuer(port, test_issuer(), binary.into())
    }

    fn knowledge_config(port: u16, binary: &str) -> nomifun_api_types::KnowledgeMcpConfig {
        nomifun_api_types::KnowledgeMcpConfig::from_issuer(port, test_issuer(), binary.into())
    }

    fn gateway_config(port: u16, binary: &str, owner: &str) -> GatewayMcpConfig {
        GatewayMcpConfig::from_issuer(port, test_issuer(), binary.into(), std::sync::Arc::<str>::from(owner))
    }

    #[test]
    fn compose_preset_context_passes_through_base_context() {
        let result = compose_preset_context(Some("hello"), None);
        assert_eq!(result, Some("hello".to_owned()));
    }

    #[test]
    fn compose_preset_context_ignores_backend() {
        let result = compose_preset_context(Some("hello"), Some("claude"));
        assert_eq!(result, Some("hello".to_owned()));
    }

    #[test]
    fn compose_preset_context_unknown_backend() {
        let result = compose_preset_context(Some("hello"), Some("unknown"));
        assert_eq!(result, Some("hello".to_owned()));
    }

    #[test]
    fn compose_preset_context_none_base_stays_none() {
        let result = compose_preset_context(None, Some("claude"));
        assert_eq!(result, None);
    }

    #[test]
    fn compose_preset_context_empty_string_treated_as_none() {
        let result = compose_preset_context(Some("  "), Some("unknown"));
        assert_eq!(result, None);
    }

    #[test]
    fn launch_nudge_only_when_open_or_computer_injected() {
        // Neither injected → context passes through untouched.
        assert_eq!(
            append_launch_nudge(Some("hi".to_owned()), false, false, false),
            Some("hi".to_owned())
        );
        assert_eq!(append_launch_nudge(None, false, false, false), None);
        // open injected → rule PREPENDED (leads), original context preserved after it.
        let appended = append_launch_nudge(Some("hi".to_owned()), true, false, false).unwrap();
        assert!(
            appended.ends_with("\n\nhi"),
            "original context must follow the rule: {appended}"
        );
        assert!(appended.contains("`open` tool"));
        assert!(appended.contains("cmd /c start"));
        let solo = append_launch_nudge(None, true, false, false).unwrap();
        assert!(solo.contains("`open` tool"));
        // Computer injected WITHOUT open (the macOS/Linux desktop case) → ONLY the
        // platform-neutral desktop-control rule. The Windows-specific launch rule
        // must NOT leak in (it tells the model the shell launch path "FAILS",
        // which is false on macOS/Linux).
        let comp = append_launch_nudge(None, false, true, false).unwrap();
        assert!(comp.contains("nomifun-computer"), "{comp}");
        assert!(comp.contains("snapshot"), "{comp}");
        assert!(comp.contains("`launch` tool"), "{comp}");
        assert!(
            !comp.contains("MANDATORY on this Windows host"),
            "no Windows launch rule off-Windows: {comp}"
        );
        assert!(
            !comp.contains("cmd /c start"),
            "no Windows shell warning off-Windows: {comp}"
        );
        // Windows desktop case (both bridges) → Windows launch rule AND the
        // desktop-control rule, in that order.
        let both = append_launch_nudge(None, true, true, false).unwrap();
        assert!(both.contains("MANDATORY on this Windows host"), "{both}");
        assert!(both.contains("`open` tool"), "{both}");
        assert!(both.contains("[Controlling the desktop]"), "{both}");
        let (win_idx, ctrl_idx) = (
            both.find("MANDATORY on this Windows host").unwrap(),
            both.find("[Controlling the desktop]").unwrap(),
        );
        assert!(
            win_idx < ctrl_idx,
            "launch rule must lead the desktop-control rule: {both}"
        );
    }

    /// P4-2: the browser bridge contributes its own independent nudge piece,
    /// symmetric with the computer rule. Injected alone (no open/computer) it
    /// yields ONLY the browser-control rule (no Windows launch warning, no
    /// desktop-control rule).
    #[test]
    fn launch_nudge_emits_browser_rule_when_browser_injected() {
        // Nothing injected (incl. browser) → passthrough.
        assert_eq!(
            append_launch_nudge(Some("hi".to_owned()), false, false, false),
            Some("hi".to_owned())
        );
        // Browser-only → only the browser rule + the CORE LOOP, nothing else.
        let br = append_launch_nudge(None, false, false, true).unwrap();
        assert!(br.contains("nomifun-browser"), "{br}");
        assert!(br.contains("[Driving the web]"), "{br}");
        assert!(br.contains("navigate"), "{br}");
        assert!(br.contains("observe"), "{br}");
        assert!(br.contains("[ref"), "{br}");
        assert!(
            !br.contains("MANDATORY on this Windows host"),
            "no Windows launch rule: {br}"
        );
        assert!(
            !br.contains("[Controlling the desktop]"),
            "no desktop-control rule: {br}"
        );
        // Browser injected → original context preserved after the rule.
        let with_ctx = append_launch_nudge(Some("hi".to_owned()), false, false, true).unwrap();
        assert!(
            with_ctx.ends_with("\n\nhi"),
            "context must follow the rule: {with_ctx}"
        );
        // Computer + browser (desktop with both desktop bridges) → both rules,
        // computer leading (it is pushed first), browser following.
        let cb = append_launch_nudge(None, false, true, true).unwrap();
        assert!(cb.contains("[Controlling the desktop]"), "{cb}");
        assert!(cb.contains("[Driving the web]"), "{cb}");
        let (ctrl_idx, web_idx) = (
            cb.find("[Controlling the desktop]").unwrap(),
            cb.find("[Driving the web]").unwrap(),
        );
        assert!(
            ctrl_idx < web_idx,
            "desktop-control rule must lead the browser rule: {cb}"
        );
    }

    #[test]
    fn knowledge_context_section_is_none_without_mounts() {
        let config = AcpBuildExtra::default();
        assert_eq!(build_knowledge_context_section(&config, "0190f5fe-7c00-7a00-8abc-012345678963"), None);
    }

    #[test]
    fn process_owned_gateway_config_injects_gateway_mcp_server() {
        let config = AcpBuildExtra {
            gateway_mcp_config: Some(gateway_config(41236, "/usr/bin/nomicore", "owner")),
            user_id: Some("0190f5fe-7c00-7a00-8abc-012345678961".into()),
            companion_id: Some("0190f5fe-7c00-7a00-8abc-012345678965".into()),
            session_mode: Some("yolo".into()),
            ..Default::default()
        };
        let (servers, _leases) = resolve_mcp_servers(&config, "0190f5fe-7c00-7a00-8abc-012345678963", "/workspace", vec![]);
        let rendered = serde_json::to_string(&servers).expect("McpServer serializes");
        assert!(rendered.contains("mcp-gateway-stdio"), "got {rendered}");
        assert!(
            rendered.contains(GatewayMcpConfig::SERVER_NAME),
            "got {rendered}"
        );
        assert!(
            rendered.contains(GatewayMcpConfig::ENV_CAPABILITY),
            "got {rendered}"
        );
        assert!(rendered.contains("yolo"), "got {rendered}");
        assert!(
            rendered.contains(GatewayMcpConfig::PROFILE_WORK),
            "got {rendered}"
        );
        assert!(rendered.contains("0190f5fe-7c00-7a00-8abc-012345678963"), "got {rendered}");
        assert!(rendered.contains("0190f5fe-7c00-7a00-8abc-012345678961"), "got {rendered}");
        assert!(rendered.contains("0190f5fe-7c00-7a00-8abc-012345678965"), "got {rendered}");
        assert!(
            !rendered.contains("gw-root-secret"),
            "root Gateway issuer secret must never leave the backend: {rendered}"
        );
    }

    #[test]
    fn gateway_env_omits_companion_id_when_unbound() {
        let config = AcpBuildExtra {
            gateway_mcp_config: Some(gateway_config(41236, "/usr/bin/nomicore", "owner")),
            user_id: Some("0190f5fe-7c00-7a00-8abc-012345678961".into()),
            companion_id: None,
            channel_platform: None,
            ..Default::default()
        };
        let (servers, _leases) = resolve_mcp_servers(&config, "0190f5fe-7c00-7a00-8abc-012345678963", "/workspace", vec![]);
        let rendered = serde_json::to_string(&servers).expect("McpServer serializes");
        assert!(
            !rendered.contains("companion_id"),
            "no binding → signed claims omit companion_id, got {rendered}"
        );
    }

    #[test]
    fn gateway_env_uses_lite_profile_for_channel_sessions() {
        let config = AcpBuildExtra {
            gateway_mcp_config: Some(gateway_config(41236, "/usr/bin/nomicore", "owner")),
            user_id: Some("0190f5fe-7c00-7a00-8abc-012345678961".into()),
            channel_platform: Some("lark".into()),
            ..Default::default()
        };
        let (servers, _leases) = resolve_mcp_servers(&config, "0190f5fe-7c00-7a00-8abc-012345678963", "/workspace", vec![]);
        let rendered = serde_json::to_string(&servers).expect("McpServer serializes");
        assert!(
            rendered.contains(GatewayMcpConfig::PROFILE_LITE),
            "got {rendered}"
        );
    }

    #[test]
    fn gateway_mcp_is_absent_without_process_owned_config() {
        let config = AcpBuildExtra {
            gateway_mcp_config: None,
            ..Default::default()
        };
        let (servers, _leases) = resolve_mcp_servers(&config, "0190f5fe-7c00-7a00-8abc-012345678963", "/workspace", vec![]);
        let rendered = serde_json::to_string(&servers).expect("McpServer serializes");
        assert!(!rendered.contains("mcp-gateway-stdio"), "got {rendered}");
    }

    #[test]
    fn knowledge_context_section_renders_mounts_and_writeback() {
        let conversation_id = "0190f5fe-7c00-7a00-8abc-012345678963";
        let mut config = AcpBuildExtra {
            knowledge_mounts: vec![nomifun_api_types::KnowledgeMountInfo {
                knowledge_base_id: nomifun_common::KnowledgeBaseId::new(),
                name: "领域知识".into(),
                description: "团队约定".into(),
                rel_path: ".nomi/knowledge/领域知识".into(),
                toc: vec!["concepts/术语.md — 术语表".into(), "(+3 more files)".into()],
                summary: Some("Covers team conventions and domain terms.".into()),
                live_sources: vec![],
            }],
            knowledge_writeback: false,
            knowledge_writeback_mode: None,
            ..Default::default()
        };

        // The section is standalone (no preset prefix) — it is delivered by its
        // own hook, not folded into the [Assistant Rules] prelude.
        let readonly = build_knowledge_context_section(&config, conversation_id).unwrap();
        assert!(readonly.starts_with("## Knowledge bases (extended knowledge source)"));
        assert!(readonly.contains("领域知识"));
        assert!(readonly.contains(".nomi/knowledge/领域知识"));
        assert!(readonly.contains("团队约定"));
        assert!(readonly.contains("concepts/术语.md — 术语表"));
        assert!(readonly.contains("(+3 more files)"));
        assert!(readonly.contains("READ-ONLY"));
        // Hit-rate contract: retrieval protocol (once), per-base summary and
        // when-to-consult guidance.
        assert_eq!(readonly.matches("Retrieval protocol").count(), 1);
        assert!(readonly.contains("Covers team conventions and domain terms."));
        assert!(readonly.contains("When to consult"));

        // writeback on + default (staged) mode → inbox path scoped to the session.
        config.knowledge_writeback = true;
        let staged = build_knowledge_context_section(&config, conversation_id).unwrap();
        assert!(staged.contains("STAGED mode"));
        assert!(staged.contains(&format!("_inbox/{conversation_id}/")));

        config.knowledge_writeback_mode = Some("direct".into());
        let direct = build_knowledge_context_section(&config, conversation_id).unwrap();
        assert!(direct.contains("DIRECT mode"));
        assert!(!direct.contains("_inbox/"));
        // Disposition (回写意识) threads from build-extra → contract; defaults
        // to conservative, flips to aggressive when set.
        assert!(direct.contains("Disposition — CONSERVATIVE"));
        config.knowledge_writeback_eagerness = Some("aggressive".into());
        let eager = build_knowledge_context_section(&config, conversation_id).unwrap();
        assert!(eager.contains("Disposition — AGGRESSIVE"));
    }

    fn user_stdio(name: &str) -> McpServer {
        McpServer::Stdio(McpServerStdio::new(name, "/bin/sh"))
    }

    /// Core ELECTRON-1JG regression contract: when the operator has
    /// configured user MCP servers (e.g. via Settings → MCP), they must
    /// reach the `session/new` payload even without built-in bridge injection.
    /// Pre-fix: this returned an empty Vec because
    /// the factory only knew about internal MCP config fields.
    #[test]
    fn resolve_mcp_servers_appends_user_servers_in_solo_session() {
        let config = AcpBuildExtra {
            agent_id: None,
            backend: Some("unknown-backend".into()),
            cli_path: None,
            agent_name: None,
            custom_agent_id: None,
            preset_context: None,
            skills: vec![],
            preset_id: None,
            session_mode: None,
            current_model_id: None,
            cron_job_id: None,
            requirement_mcp_config: None,
            knowledge_mcp_config: None,
            gateway_mcp_config: None,
            gateway_excluded_tools: vec![],
            open_mcp_config: None,
            computer_mcp_config: None,
            browser_mcp_config: None,
            mcp_server_ids: None,
            session_mcp_servers: vec![],
            user_id: None,
            companion_id: None,
            channel_platform: None,
            knowledge_mounts: vec![],
            knowledge_writeback: false,
            knowledge_writeback_mode: None,
            knowledge_writeback_eagerness: None,
        };
        let user = vec![user_stdio("ctx7"), user_stdio("playwright")];
        let (servers, _leases) = resolve_mcp_servers(&config, "0190f5fe-7c00-7a00-8abc-012345678963", "/workspace", user);
        assert_eq!(servers.len(), 2);
        let names: Vec<_> = servers
            .iter()
            .map(|s| match s {
                McpServer::Stdio(s) => s.name.as_str(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(names, vec!["ctx7", "playwright"]);
    }

    /// The pre-fix bug: with no internal MCP configured and an empty
    /// user-server list, the payload is empty. This is the *no-fix*
    /// scenario and remains valid (no MCP configured anywhere).
    #[test]
    fn resolve_mcp_servers_empty_when_nothing_configured() {
        let config = AcpBuildExtra {
            agent_id: None,
            backend: Some("claude".into()),
            cli_path: None,
            agent_name: None,
            custom_agent_id: None,
            preset_context: None,
            skills: vec![],
            preset_id: None,
            session_mode: None,
            current_model_id: None,
            cron_job_id: None,
            requirement_mcp_config: None,
            knowledge_mcp_config: None,
            gateway_mcp_config: None,
            gateway_excluded_tools: vec![],
            open_mcp_config: None,
            computer_mcp_config: None,
            browser_mcp_config: None,
            mcp_server_ids: None,
            session_mcp_servers: vec![],
            user_id: None,
            companion_id: None,
            channel_platform: None,
            knowledge_mounts: vec![],
            knowledge_writeback: false,
            knowledge_writeback_mode: None,
            knowledge_writeback_eagerness: None,
        };
        let (servers, _leases) =
            resolve_mcp_servers(&config, "0190f5fe-7c00-7a00-8abc-012345678963", "/workspace", Vec::new());
        assert!(servers.is_empty());
    }

    #[test]
    fn resolve_mcp_servers_appends_requirement_when_configured() {
        let config = AcpBuildExtra {
            backend: Some("claude".into()),
            requirement_mcp_config: Some(requirement_config(41000, "/bin/backend")),
            user_id: Some("0190f5fe-7c00-7a00-8abc-012345678962".into()),
            ..Default::default()
        };
        let (servers, leases) = resolve_mcp_servers(
            &config,
            "0190f5fe-7c00-7a00-8000-000000000009",
            "/workspace",
            Vec::new(),
        );
        assert_eq!(leases.len(), 1);
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, RequirementMcpConfig::SERVER_NAME);
                assert!(
                    s.args.iter().any(|a| a == "mcp-requirement-stdio"),
                    "must spawn the requirement stdio bridge"
                );
                // One indivisible capability bootstrap is set.
                assert_eq!(s.env.len(), 1, "expected one capability env var");
                let rendered = serde_json::to_string(&s.env).unwrap();
                assert!(rendered.contains(RequirementMcpConfig::ENV_CAPABILITY));
                assert!(!rendered.contains("root-secret"));
            }
            _ => panic!("expected stdio server"),
        }
    }

    #[test]
    fn resolve_mcp_servers_appends_open_when_configured() {
        let config = AcpBuildExtra {
            backend: Some("codex".into()),
            open_mcp_config: Some(OpenMcpConfig {
                binary_path: "/bin/nomicore".into(),
            }),
            ..Default::default()
        };
        let (servers, _leases) =
            resolve_mcp_servers(&config, "conv-open", "/workspace", Vec::new());
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, OpenMcpConfig::SERVER_NAME);
                assert!(
                    s.args.iter().any(|a| a == "mcp-open-stdio"),
                    "must spawn the open stdio bridge"
                );
                // Stateless: no env vars (no port/token/conversation id).
                assert!(s.env.is_empty(), "open bridge needs no env");
            }
            _ => panic!("expected stdio server"),
        }
    }

    #[test]
    fn resolve_mcp_servers_appends_computer_when_configured() {
        let config = AcpBuildExtra {
            backend: Some("codex".into()),
            computer_mcp_config: Some(ComputerMcpConfig {
                binary_path: "/bin/nomicore".into(),
            }),
            ..Default::default()
        };
        let (servers, _leases) =
            resolve_mcp_servers(&config, "conv-computer", "/workspace", Vec::new());
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, ComputerMcpConfig::SERVER_NAME);
                assert!(
                    s.args.iter().any(|a| a == "mcp-computer-stdio"),
                    "must spawn the computer stdio bridge"
                );
                assert!(s.env.is_empty(), "computer bridge needs no env");
            }
            _ => panic!("expected stdio server"),
        }
    }

    /// P4-2: symmetric with the computer test — `browser_mcp_config` Some →
    /// the assembler injects the `nomifun-browser` stdio bridge spawning
    /// `mcp-browser-stdio`, stateless (no env).
    #[test]
    fn resolve_mcp_servers_appends_browser_when_configured() {
        let config = AcpBuildExtra {
            backend: Some("codex".into()),
            browser_mcp_config: Some(BrowserMcpConfig {
                binary_path: "/bin/nomicore".into(),
            }),
            ..Default::default()
        };
        let (servers, _leases) =
            resolve_mcp_servers(&config, "conv-browser", "/workspace", Vec::new());
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, BrowserMcpConfig::SERVER_NAME);
                assert!(
                    s.args.iter().any(|a| a == "mcp-browser-stdio"),
                    "must spawn the browser stdio bridge"
                );
                assert!(
                    s.env.is_empty(),
                    "browser bridge needs no env (stateless fail-safe)"
                );
            }
            _ => panic!("expected stdio server"),
        }
    }

    /// 裁决⑦ (double-bridge non-conflict): when BOTH the computer and browser
    /// bridges are injected, they occupy DISTINCT server-name slots
    /// (`nomifun-computer` ⊥ `nomifun-browser`) and spawn distinct subcommands,
    /// so their tool namespaces never collide.
    #[test]
    fn resolve_mcp_servers_browser_and_computer_coexist_distinct_slots() {
        let config = AcpBuildExtra {
            backend: Some("codex".into()),
            computer_mcp_config: Some(ComputerMcpConfig {
                binary_path: "/bin/nomicore".into(),
            }),
            browser_mcp_config: Some(BrowserMcpConfig {
                binary_path: "/bin/nomicore".into(),
            }),
            ..Default::default()
        };
        let (servers, _leases) =
            resolve_mcp_servers(&config, "conv-both", "/workspace", Vec::new());
        assert_eq!(servers.len(), 2, "both bridges injected");
        let names: Vec<&str> = servers
            .iter()
            .map(|s| match s {
                McpServer::Stdio(stdio) => stdio.name.as_str(),
                _ => panic!("expected stdio server"),
            })
            .collect();
        assert!(
            names.contains(&ComputerMcpConfig::SERVER_NAME),
            "got {names:?}"
        );
        assert!(
            names.contains(&BrowserMcpConfig::SERVER_NAME),
            "got {names:?}"
        );
        assert_ne!(
            ComputerMcpConfig::SERVER_NAME,
            BrowserMcpConfig::SERVER_NAME,
            "server names must be distinct so tool namespaces never collide"
        );
        // Computer is pushed before browser (deterministic wire layout).
        assert_eq!(
            names[0],
            ComputerMcpConfig::SERVER_NAME,
            "computer leads: {names:?}"
        );
        assert_eq!(
            names[1],
            BrowserMcpConfig::SERVER_NAME,
            "browser follows: {names:?}"
        );
    }

    /// Requirement and user servers are emitted in deterministic order.
    #[test]
    fn resolve_mcp_servers_orders_requirement_before_user_servers() {
        let config = AcpBuildExtra {
            backend: Some("claude".into()),
            requirement_mcp_config: Some(requirement_config(41000, "/bin/backend")),
            user_id: Some("0190f5fe-7c00-7a00-8abc-012345678962".into()),
            ..Default::default()
        };
        let user = vec![user_stdio("ctx7")];
        let (servers, _leases) = resolve_mcp_servers(
            &config,
            "0190f5fe-7c00-7a00-8000-000000000001",
            "/workspace",
            user,
        );
        assert_eq!(servers.len(), 2);
        let names: Vec<&str> = servers
            .iter()
            .map(|s| match s {
                McpServer::Stdio(s) => s.name.as_str(),
                _ => panic!("expected stdio"),
            })
            .collect();
        assert_eq!(names, vec![RequirementMcpConfig::SERVER_NAME, "ctx7"]);
    }

    fn knowledge_mount(id: &str) -> nomifun_api_types::KnowledgeMountInfo {
        nomifun_api_types::KnowledgeMountInfo {
            knowledge_base_id: nomifun_common::KnowledgeBaseId::parse(id).expect("canonical knowledge base test id"),
            name: "领域知识".into(),
            description: "团队约定".into(),
            rel_path: format!(".nomi/knowledge/{id}"),
            toc: vec![],
            summary: None,
            live_sources: vec![],
        }
    }

    /// SECURITY contract for the scoped knowledge MCP injection (Task B5):
    ///   - Invariant 1: injected ONLY when config present AND bound bases exist
    ///     (and independent of the platform Gateway).
    ///   - Invariant 2: bound ids live inside signed claims, not loose env/body.
    ///   - Invariant 3: the root issuer secret never reaches the child.
    #[test]
    fn knowledge_mcp_injected_only_with_config_and_bound_bases() {
        let cfg = knowledge_config(41555, "/bin/nomicore");

        // Case A: Some(config) + 1 mount → the "nomifun-knowledge" server is
        // injected, spawns the right bridge, and signs the mount id into claims.
        // No platform Gateway config is present, proving independence.
        let config_a = AcpBuildExtra {
            backend: Some("claude".into()),
            knowledge_mcp_config: Some(cfg.clone()),
            knowledge_mounts: vec![knowledge_mount("0190f5fe-7c00-7a00-8abc-012345678969")],
            user_id: Some("0190f5fe-7c00-7a00-8abc-012345678962".into()),
            ..Default::default()
        };
        let (servers, leases) =
            resolve_mcp_servers(&config_a, "0190f5fe-7c00-7a00-8abc-012345678964", "/workspace", Vec::new());
        assert_eq!(leases.len(), 1);
        let kb_server = servers
            .iter()
            .find(|s| match s {
                McpServer::Stdio(s) => s.name == nomifun_api_types::KnowledgeMcpConfig::SERVER_NAME,
                _ => false,
            })
            .expect("knowledge server must be injected with config + bound bases");
        let McpServer::Stdio(stdio) = kb_server else {
            panic!("expected stdio");
        };
        // Invariant 1: present, and it spawns the knowledge stdio bridge.
        assert!(
            stdio.args.iter().any(|a| a == "mcp-knowledge-stdio"),
            "must spawn the knowledge stdio bridge"
        );
        let env_val = |key: &str| {
            stdio
                .env
                .iter()
                .find(|e| e.name == key)
                .map(|e| e.value.clone())
        };
        // Invariant 2: the bound kb ids live only inside the capability bootstrap.
        let baked = env_val(nomifun_api_types::KnowledgeMcpConfig::ENV_CAPABILITY)
            .expect("knowledge capability bootstrap env must be set");
        assert!(
            baked.contains("0190f5fe-7c00-7a00-8abc-012345678969"),
            "baked kb_ids must carry the mount id, got {baked}"
        );
        // Invariant 3: port + access + renewal proof are one immutable value.
        assert!(baked.contains("41555"));
        assert!(!baked.contains("root-kb-secret"));
        // Proof of gateway independence: no gateway server was injected, yet
        // the knowledge server still is.
        let rendered = serde_json::to_string(&servers).expect("serializes");
        assert!(!rendered.contains("root-kb-secret"));
        assert!(
            !rendered.contains("mcp-gateway-stdio"),
            "no gateway must be present"
        );

        // Case B: Some(config) + 0 mounts → NO knowledge server (invariant 1).
        let config_b = AcpBuildExtra {
            backend: Some("claude".into()),
            knowledge_mcp_config: Some(cfg.clone()),
            knowledge_mounts: vec![],
            ..Default::default()
        };
        let (servers_b, _leases) =
            resolve_mcp_servers(&config_b, "0190f5fe-7c00-7a00-8abc-012345678964", "/workspace", Vec::new());
        assert!(
            !servers_b.iter().any(|s| matches!(
                s,
                McpServer::Stdio(s) if s.name == nomifun_api_types::KnowledgeMcpConfig::SERVER_NAME
            )),
            "no bound bases → no knowledge server"
        );

        // Case C: None config + 1 mount → NO knowledge server (invariant 1).
        let config_c = AcpBuildExtra {
            backend: Some("claude".into()),
            knowledge_mcp_config: None,
            knowledge_mounts: vec![knowledge_mount("0190f5fe-7c00-7a00-8abc-012345678969")],
            ..Default::default()
        };
        let (servers_c, _leases) =
            resolve_mcp_servers(&config_c, "0190f5fe-7c00-7a00-8abc-012345678964", "/workspace", Vec::new());
        assert!(
            !servers_c.iter().any(|s| matches!(
                s,
                McpServer::Stdio(s) if s.name == nomifun_api_types::KnowledgeMcpConfig::SERVER_NAME
            )),
            "no config → no knowledge server"
        );
    }

    /// The ACP protocol section must promise `knowledge_search` ONLY when the
    /// knowledge MCP is actually injected (config present AND bound bases).
    #[test]
    fn knowledge_context_has_search_tool_tracks_injection() {
        let cfg = knowledge_config(41555, "/bin/nomicore");
        // config + mount → section promises the search tool.
        let with = AcpBuildExtra {
            knowledge_mcp_config: Some(cfg.clone()),
            knowledge_mounts: vec![knowledge_mount("0190f5fe-7c00-7a00-8abc-012345678969")],
            ..Default::default()
        };
        let section = build_knowledge_context_section(&with, "0190f5fe-7c00-7a00-8abc-012345678964").expect("section renders");
        assert!(
            section.contains("knowledge_search"),
            "section must advertise the search tool when injected, got {section}"
        );
        // mount but NO config → no search tool promised (it is not injected).
        let without = AcpBuildExtra {
            knowledge_mcp_config: None,
            knowledge_mounts: vec![knowledge_mount("0190f5fe-7c00-7a00-8abc-012345678969")],
            ..Default::default()
        };
        let section_no =
            build_knowledge_context_section(&without, "0190f5fe-7c00-7a00-8abc-012345678964").expect("section renders");
        assert!(
            !section_no.contains("knowledge_search"),
            "section must NOT advertise an uninjected search tool, got {section_no}"
        );
    }
}
