//! MCP stdio bridge connection configs shared across the backend.
//!
//! These config shapes (`GuideMcpConfig`, `RequirementMcpConfig`,
//! `KnowledgeMcpConfig`, `GatewayMcpConfig`, `OpenMcpConfig`,
//! `ComputerMcpConfig`, `BrowserMcpConfig`) live here so downstream crates
//! (`nomifun-ai-agent` deserializing `AcpBuildExtra`, etc.) can reference the
//! same shape from a leaf crate.

use serde::{Deserialize, Serialize};

/// Connection config for the Guide MCP stdio server in solo conversations.
///
/// Passed through `AcpBuildExtra::guide_mcp_config` by the factory so that
/// `build_new_session_request` can inject the Guide as a stdio MCP server
/// when a caller explicitly supplies Guide configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideMcpConfig {
    pub port: u16,
    pub token: String,
    pub binary_path: String,
}

/// Connection config for the requirement MCP stdio bridge injected into ACP
/// agent sessions.
///
/// Passed through `AcpBuildExtra::requirement_mcp_config` by the factory so the
/// session assembler can inject `nomicore mcp-requirement-stdio` as a stdio MCP
/// server. That bridge forwards `requirement_complete` /
/// `requirement_update_status` tool calls back to the in-process
/// `RequirementMcpServer` at `http://127.0.0.1:{port}/tool` using `token`.
///
/// stdio (not HTTP) because claude / codex / gemini advertise stdio-only MCP
/// capabilities — an HTTP server config would be dropped by the ACP capability
/// filter before reaching them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementMcpConfig {
    pub port: u16,
    pub token: String,
    pub binary_path: String,
}

impl RequirementMcpConfig {
    /// Wire-level MCP server name. Kept short so the longest wire-level tool
    /// name `mcp__nomifun-requirement__requirement_update_status` (51 chars)
    /// stays within Anthropic's 64-char tool-name limit (see ELECTRON-1JY).
    pub const SERVER_NAME: &'static str = "nomifun-requirement";
    /// env key the stdio bridge reads to learn the backend HTTP port.
    pub const ENV_PORT: &'static str = "NOMI_REQ_MCP_PORT";
    /// env key the stdio bridge reads to learn the auth token.
    pub const ENV_TOKEN: &'static str = "NOMI_REQ_MCP_TOKEN";
    /// env key the stdio bridge reads to learn the owning session id (numeric),
    /// used to scope mutations to the calling session. Carries the owner id for
    /// either conversations or terminals (the `ENV_OWNER_KIND` disambiguates).
    pub const ENV_CONVERSATION_ID: &'static str = "NOMI_REQ_MCP_CONVERSATION_ID";
    /// env key the stdio bridge reads to learn the owner domain of the calling
    /// session: `"conversation"` (default/back-compat) or `"terminal"`.
    /// `verify_scope` pairs this with the requirement's `owner_kind` to prevent
    /// cross-domain privilege escalation (conv#5 vs term#5).
    pub const ENV_OWNER_KIND: &'static str = "NOMI_REQ_MCP_OWNER_KIND";
}

/// Wiring for the per-session knowledge-search MCP (ACP external CLIs). Mirrors
/// `RequirementMcpConfig` but the bound `kb_ids` are baked at injection (passed
/// via env, not part of this struct): the agent tool takes only a query, so the
/// bound base set cannot be widened by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeMcpConfig {
    pub port: u16,
    pub token: String,
    pub binary_path: String,
}

impl KnowledgeMcpConfig {
    pub const SERVER_NAME: &'static str = "nomifun-knowledge";
    pub const ENV_PORT: &'static str = "NOMI_KB_MCP_PORT";
    pub const ENV_TOKEN: &'static str = "NOMI_KB_MCP_TOKEN";
    pub const ENV_KB_IDS: &'static str = "NOMI_KB_MCP_KB_IDS";
}

/// Connection config for the Desktop Gateway MCP stdio bridge.
///
/// Passed through `AcpBuildExtra::gateway_mcp_config` / `NomiBuildExtra::gateway_mcp_config`
/// by the factory when a conversation carries the backend-set `desktopGateway`
/// extra flag (channel master-agent sessions, companion companion threads). The
/// session assembler injects `nomicore mcp-gateway-stdio` as a stdio MCP
/// server; that bridge forwards every `nomi_*` desktop tool call back to the
/// in-process `GatewayMcpServer` at `http://127.0.0.1:{port}/tool` using
/// `token`.
///
/// stdio (not HTTP) for the same reason as the requirement bridge: claude /
/// codex / gemini advertise stdio-only MCP capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayMcpConfig {
    pub port: u16,
    pub token: String,
    pub binary_path: String,
}

impl GatewayMcpConfig {
    /// Wire-level MCP server name. Kept short so the longest wire-level tool
    /// name `mcp__nomifun-desktop__nomi_send_to_conversation` (47 chars) stays
    /// within Anthropic's 64-char tool-name limit (see ELECTRON-1JY).
    pub const SERVER_NAME: &'static str = "nomifun-desktop";
    /// env key the stdio bridge reads to learn the backend HTTP port.
    pub const ENV_PORT: &'static str = "NOMI_GW_MCP_PORT";
    /// env key the stdio bridge reads to learn the auth token.
    pub const ENV_TOKEN: &'static str = "NOMI_GW_MCP_TOKEN";
    /// env key carrying the calling conversation id (self-protection scope:
    /// a session may not delete itself or inject messages into itself).
    pub const ENV_CONVERSATION_ID: &'static str = "NOMI_GW_MCP_CONVERSATION_ID";
    /// env key carrying the owning user id; every gateway tool scopes its
    /// data access to this user.
    pub const ENV_USER_ID: &'static str = "NOMI_GW_MCP_USER_ID";
    /// env key carrying the companion the calling session is bound to (multi-companion
    /// upgrade). Optional — absent for sessions with no companion binding; the
    /// gateway treats a missing/empty value as "no specific companion".
    pub const ENV_COMPANION_ID: &'static str = "NOMI_GW_MCP_COMPANION_ID";
    /// env key carrying the IM platform when this is a channel master-agent
    /// session (e.g. "lark", "discord"). Optional — absent for plain
    /// companion/desktop sessions. The gateway uses it to resolve the write
    /// surface (channel → write-disabled in P1).
    pub const ENV_CHANNEL_PLATFORM: &'static str = "NOMI_GW_MCP_CHANNEL_PLATFORM";
    /// env key carrying the caller's resolved session approval mode.
    pub const ENV_SESSION_MODE: &'static str = "NOMI_GW_MCP_SESSION_MODE";
    /// Optional gateway tool profile. The stdio bridge maps this to a curated
    /// capability-domain list before answering `tools/list`.
    pub const ENV_PROFILE: &'static str = "NOMI_GW_MCP_PROFILE";
    /// Optional comma-separated capability-domain allow-list. When present it
    /// takes precedence over [`Self::ENV_PROFILE`].
    pub const ENV_DOMAINS: &'static str = "NOMI_GW_MCP_DOMAINS";

    pub const PROFILE_LITE: &'static str = "lite";
    pub const PROFILE_WORK: &'static str = "work";
    pub const PROFILE_DESKTOP: &'static str = "desktop";
    pub const PROFILE_ADMIN: &'static str = "admin";
    pub const PROFILE_FULL: &'static str = "full";

    pub const LITE_DOMAINS: &'static [&'static str] = &[
        "conversation",
        "provider",
        "cron",
        "requirement",
        "autowork",
        "confirmation",
    ];
    pub const WORK_DOMAINS: &'static [&'static str] = &[
        "conversation",
        "provider",
        "cron",
        "requirement",
        "autowork",
        "confirmation",
        "terminal",
        "files",
        "knowledge",
        "idmm",
        // The desktop default profile: the lead/main agent must be able to spin
        // up & track multi-agent orchestration runs (nomi_run_create/status/result).
        // Desktop-only domain (caps_orchestrator denies Remote), so safe here.
        "orchestrator",
    ];
    pub const DESKTOP_DOMAINS: &'static [&'static str] = &[
        "conversation",
        "provider",
        "confirmation",
        "terminal",
        "files",
        "browser",
        "computer",
        "orchestrator",
    ];
    pub const ADMIN_DOMAINS: &'static [&'static str] = &[
        "system",
        "mcp",
        "extension",
        "skill",
        "hub",
        "agent",
        "channel",
        "companion",
        "memory",
        "provider",
        "confirmation",
    ];
    const EMPTY_DOMAINS: &'static [&'static str] = &[];

    /// Map a profile to a capability-domain allow-list. `None` means full
    /// gateway exposure; unknown profiles intentionally resolve to an empty
    /// allow-list rather than widening access by typo.
    pub fn domains_for_profile(profile: &str) -> Option<&'static [&'static str]> {
        match profile.trim().to_ascii_lowercase().as_str() {
            "" | Self::PROFILE_FULL => None,
            Self::PROFILE_LITE => Some(Self::LITE_DOMAINS),
            Self::PROFILE_WORK => Some(Self::WORK_DOMAINS),
            Self::PROFILE_DESKTOP => Some(Self::DESKTOP_DOMAINS),
            Self::PROFILE_ADMIN => Some(Self::ADMIN_DOMAINS),
            _ => Some(Self::EMPTY_DOMAINS),
        }
    }

    pub fn default_profile_for_session(channel_platform: Option<&str>) -> &'static str {
        if channel_platform
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
        {
            Self::PROFILE_LITE
        } else {
            Self::PROFILE_WORK
        }
    }
}

/// Connection config for the reliable "open" MCP stdio bridge.
///
/// Passed through `AcpBuildExtra::open_mcp_config` by the factory on Windows
/// (only — macOS/Linux already have reliable `open`/`xdg-open` and need no
/// nudging away from `cmd /c start`). The session assembler injects
/// `nomicore mcp-open-stdio` as a stdio MCP server exposing a single `open`
/// tool that ShellExecutes a URL / file / folder / application — giving the
/// agent a dependable launch path instead of the fragile `cmd /c start`
/// window-title quirk.
///
/// Unlike the requirement/gateway bridges this is STATELESS: opening is a pure
/// local OS call, so the bridge needs no HTTP callback — hence no `port`/`token`,
/// only the `nomicore` binary path to re-spawn the subcommand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenMcpConfig {
    pub binary_path: String,
}

impl OpenMcpConfig {
    /// Wire-level MCP server name. Kept short so the wire-level tool name
    /// `mcp__nomifun-open__open` (23 chars) stays well within Anthropic's
    /// 64-char tool-name limit.
    pub const SERVER_NAME: &'static str = "nomifun-open";
}

/// Connection config for the computer-use discrete-tool MCP stdio bridge.
///
/// Passed through `AcpBuildExtra::computer_mcp_config` by the factory on every
/// desktop OS (macOS / Windows / Linux) when the host binary was built with the
/// `computer-use` feature. The session assembler injects `nomicore
/// mcp-computer-stdio` — an MCP server exposing the desktop computer-use
/// capability as discrete tools (snapshot / click / type / launch / …), a thin
/// facade over the in-tree `ComputerTool`, so codex/ACP get the same automation
/// the nomi engine has (macOS AX / Windows UIA / Linux AT-SPI via `nomi-a11y`).
///
/// Like the open bridge this is STATELESS at the protocol level (no HTTP
/// callback): it drives the local desktop directly, so it needs only the
/// `nomicore` binary path to re-spawn the subcommand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputerMcpConfig {
    pub binary_path: String,
}

impl ComputerMcpConfig {
    /// Wire-level MCP server name. Kept short so the longest wire-level tool name
    /// `mcp__nomifun-computer__cursor_position` (39 chars) stays within
    /// Anthropic's 64-char tool-name limit.
    pub const SERVER_NAME: &'static str = "nomifun-computer";
}

/// Connection config for the browser-use discrete-tool MCP stdio bridge.
///
/// Passed through `AcpBuildExtra::browser_mcp_config` by the factory on every
/// desktop OS when the host binary was built with the `browser-use` feature
/// (P4-2 wiring). The session assembler injects `nomicore mcp-browser-stdio` —
/// an MCP server exposing the browser-use capability as discrete tools
/// (navigate / observe / click / type / …), a thin facade over the in-tree
/// `BrowserTool`, so codex/ACP get the same self-hosted-CDP automation the nomi
/// engine has.
///
/// Like the open/computer bridges this is STATELESS at the protocol level (no
/// HTTP callback): it drives a private Chromium directly, so it needs only the
/// `nomicore` binary path to re-spawn the subcommand.
///
/// R2 (no per-pet context): the bridge carries NO env-borne session context —
/// it constructs `BrowserTool::new(&BrowserConfig::default())`, so `secret:NAME`
/// fails closed (empty store) and downloads land in the data-dir sandbox. Per-pet
/// credentials / workspace / persistent-login stay on the nomi engine path. See
/// `browser_stdio.rs` and the P4 plan decision D2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserMcpConfig {
    pub binary_path: String,
}

impl BrowserMcpConfig {
    /// Wire-level MCP server name. Kept short so the longest wire-level tool name
    /// `mcp__nomifun-browser__get_dropdown_options` (42 chars) stays within
    /// Anthropic's 64-char tool-name limit.
    pub const SERVER_NAME: &'static str = "nomifun-browser";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirement_mcp_config_json_roundtrip() {
        let cfg = RequirementMcpConfig {
            port: 41234,
            token: "rtok".into(),
            binary_path: "/usr/bin/nomicore".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: RequirementMcpConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, parsed);
    }

    /// Same Anthropic 64-char tool-name bound as the team server (ELECTRON-1JY).
    /// The longest requirement tool is `requirement_update_status`.
    #[test]
    fn requirement_mcp_tool_names_stay_within_anthropic_64_char_limit() {
        let longest_tool = "requirement_update_status";
        let wire_name = format!(
            "mcp__{}__{}",
            RequirementMcpConfig::SERVER_NAME,
            longest_tool
        );
        assert!(
            wire_name.len() <= 64,
            "Anthropic 64-char tool-name limit exceeded: {} ({} chars)",
            wire_name,
            wire_name.len()
        );
    }

    #[test]
    fn gateway_mcp_config_json_roundtrip() {
        let cfg = GatewayMcpConfig {
            port: 41235,
            token: "gtok".into(),
            binary_path: "/usr/bin/nomicore".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: GatewayMcpConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn gateway_profile_domains_are_curated_and_unknown_is_empty() {
        assert_eq!(
            GatewayMcpConfig::domains_for_profile(GatewayMcpConfig::PROFILE_FULL),
            None
        );
        assert!(
            GatewayMcpConfig::domains_for_profile(GatewayMcpConfig::PROFILE_WORK)
                .unwrap()
                .contains(&"requirement")
        );
        assert_eq!(
            GatewayMcpConfig::domains_for_profile("typo-profile"),
            Some(&[][..])
        );
        assert_eq!(
            GatewayMcpConfig::default_profile_for_session(Some("lark")),
            GatewayMcpConfig::PROFILE_LITE
        );
        assert_eq!(
            GatewayMcpConfig::default_profile_for_session(None),
            GatewayMcpConfig::PROFILE_WORK
        );
    }

    #[test]
    fn open_mcp_config_json_roundtrip() {
        let cfg = OpenMcpConfig {
            binary_path: "/usr/bin/nomicore".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: OpenMcpConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, parsed);
    }

    /// The open server's single tool `open` stays well within Anthropic's
    /// 64-char wire-level tool-name limit.
    #[test]
    fn open_mcp_tool_name_stays_within_anthropic_64_char_limit() {
        let wire_name = format!("mcp__{}__{}", OpenMcpConfig::SERVER_NAME, "open");
        assert!(
            wire_name.len() <= 64,
            "{wire_name} ({} chars)",
            wire_name.len()
        );
    }

    #[test]
    fn computer_mcp_config_json_roundtrip() {
        let cfg = ComputerMcpConfig {
            binary_path: "/usr/bin/nomicore".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: ComputerMcpConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, parsed);
    }

    /// The computer bridge's longest discrete tool name must stay within
    /// Anthropic's 64-char wire-level tool-name limit.
    #[test]
    fn computer_mcp_tool_name_stays_within_anthropic_64_char_limit() {
        let wire_name = format!(
            "mcp__{}__{}",
            ComputerMcpConfig::SERVER_NAME,
            "cursor_position"
        );
        assert!(
            wire_name.len() <= 64,
            "{wire_name} ({} chars)",
            wire_name.len()
        );
    }

    #[test]
    fn browser_mcp_config_json_roundtrip() {
        let cfg = BrowserMcpConfig {
            binary_path: "/usr/bin/nomicore".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: BrowserMcpConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, parsed);
    }

    /// The browser bridge's longest discrete tool name (`get_dropdown_options`)
    /// must stay within Anthropic's 64-char wire-level tool-name limit.
    #[test]
    fn browser_mcp_tool_name_stays_within_anthropic_64_char_limit() {
        let wire_name = format!(
            "mcp__{}__{}",
            BrowserMcpConfig::SERVER_NAME,
            "get_dropdown_options"
        );
        assert!(
            wire_name.len() <= 64,
            "{wire_name} ({} chars)",
            wire_name.len()
        );
    }

    /// The Anthropic 64-char wire-name bound (ELECTRON-1JY). The gateway
    /// advertises as `SERVER_NAME`, so a wire name is `mcp__{SERVER_NAME}__{tool}`.
    /// This asserts the server-name prefix leaves a workable budget for tool
    /// names (>= 42 chars). PER-TOOL enforcement — iterating every registered
    /// name against the real limit — lives in `nomifun-gateway`'s registry
    /// self-test (`registry_builds_and_names_fit_mcp_limit`); this avoids a
    /// stale hand-picked exemplar here.
    #[test]
    fn gateway_server_name_leaves_workable_tool_name_budget() {
        let prefix = format!("mcp__{}__", GatewayMcpConfig::SERVER_NAME).len();
        let budget = 64usize.saturating_sub(prefix);
        assert!(
            budget >= 42,
            "server name '{}' leaves only {budget} chars for tool names (need >= 42); pick a shorter SERVER_NAME",
            GatewayMcpConfig::SERVER_NAME,
        );
    }
}
