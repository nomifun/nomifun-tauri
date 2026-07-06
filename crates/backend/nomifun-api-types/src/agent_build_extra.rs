use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{
    BrowserMcpConfig, GatewayMcpConfig, GuideMcpConfig, KnowledgeMcpConfig, KnowledgeMountInfo,
    OpenMcpConfig, RequirementMcpConfig, ComputerMcpConfig,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionMcpTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    Sse {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    StreamableHttp {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMcpServer {
    pub id: String,
    pub name: String,
    pub transport: SessionMcpTransport,
}

/// ACP-specific fields extracted from `extra` in build task options.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AcpBuildExtra {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub cli_path: Option<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub custom_agent_id: Option<String>,
    #[serde(default)]
    pub preset_context: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub preset_assistant_id: Option<String>,
    #[serde(default)]
    pub session_mode: Option<String>,
    #[serde(default)]
    pub current_model_id: Option<String>,
    #[serde(default)]
    pub cron_job_id: Option<String>,
    #[serde(default)]
    pub guide_mcp_config: Option<GuideMcpConfig>,
    /// Requirement MCP stdio bridge config. When `Some`, the ACP assembler
    /// injects `nomicore mcp-requirement-stdio` so the agent gets the
    /// `requirement_complete` / `requirement_update_status` declaration tools.
    /// Injected from `AgentFactoryDeps::requirement_mcp_config` at build time.
    #[serde(default)]
    pub requirement_mcp_config: Option<RequirementMcpConfig>,
    /// Knowledge-search MCP stdio bridge config. When `Some`, the ACP assembler
    /// injects `nomicore mcp-knowledge-stdio` so the agent gets a scoped
    /// knowledge-search tool over the session's bound knowledge bases. The bound
    /// `kb_ids` ride the bridge env (`NOMI_KB_MCP_KB_IDS`), not this struct, so
    /// the agent tool takes only a query and cannot widen the bound base set.
    /// Injected at build time; the nomi engine has `knowledge_search` natively
    /// and so carries no equivalent field.
    #[serde(default)]
    pub knowledge_mcp_config: Option<KnowledgeMcpConfig>,
    /// Marks a session entitled to the Desktop Gateway MCP (`nomi_*` tools —
    /// full desktop control: conversations, cron, memory, requirements).
    /// Backend-set only (channel master-agent sessions, companion companion
    /// threads); the HTTP conversation routes strip it from client-supplied
    /// extra so a session cannot self-authorize. Accepts both camelCase
    /// (`desktopGateway`, the extra-JSON spelling) and snake_case.
    #[serde(default, alias = "desktopGateway")]
    pub desktop_gateway: bool,
    /// Desktop Gateway MCP stdio bridge config. Injected from
    /// `AgentFactoryDeps::gateway_mcp_config` at build time when
    /// `desktop_gateway` is set.
    #[serde(default)]
    pub gateway_mcp_config: Option<GatewayMcpConfig>,
    /// Reliable-launch (`open`) MCP stdio bridge config. When `Some`, the ACP
    /// assembler injects `nomicore mcp-open-stdio` so the agent gets the `open`
    /// tool (ShellExecute a URL/file/app). Injected from
    /// `AgentFactoryDeps::open_mcp_config` at build time — populated on Windows
    /// only (macOS/Linux already launch reliably), independent of any flag.
    #[serde(default)]
    pub open_mcp_config: Option<OpenMcpConfig>,
    /// Computer-use discrete-tool MCP stdio bridge config. When `Some`, the ACP
    /// assembler injects `nomicore mcp-computer-stdio` so the agent gets discrete
    /// desktop tools (snapshot / click / type / launch / …). Injected from
    /// `AgentFactoryDeps::computer_mcp_config` at build time — populated on every
    /// desktop OS (macOS / Windows / Linux) when the host binary has the
    /// `computer-use` feature.
    #[serde(default)]
    pub computer_mcp_config: Option<ComputerMcpConfig>,
    /// Browser-use discrete-tool MCP stdio bridge config. When `Some`, the ACP
    /// assembler injects `nomicore mcp-browser-stdio` so the agent gets discrete
    /// browser tools (navigate / observe / click / type / …). Injected from
    /// `AgentFactoryDeps::browser_mcp_config` at build time — populated on every
    /// desktop OS when the host binary has the `browser-use` feature. Symmetric
    /// with `computer_mcp_config`; the bridge is stateless fail-safe (R2: no
    /// per-pet context over the env boundary — see `BrowserMcpConfig`).
    #[serde(default)]
    pub browser_mcp_config: Option<BrowserMcpConfig>,
    /// The companion this session is bound to (multi-companion upgrade). Set by the
    /// channel layer on master-agent sessions (platform binding > default
    /// companion); rides the gateway MCP env (`NOMI_GW_MCP_COMPANION_ID`) so desktop
    /// tools can attribute the caller. Accepts both camelCase (`companionId`, the
    /// extra-JSON spelling) and snake_case.
    #[serde(default, alias = "companionId")]
    pub companion_id: Option<String>,
    /// IM platform this session serves (e.g. "lark") when it is a channel
    /// master-agent. Set by the channel layer; rides the gateway MCP env
    /// (`NOMI_GW_MCP_CHANNEL_PLATFORM`) so the gateway resolves the write surface
    /// (channel → write-disabled unless re-enabled). Mirrors `NomiBuildExtra`.
    #[serde(default, alias = "channelPlatform")]
    pub channel_platform: Option<String>,
    #[serde(default)]
    pub mcp_server_ids: Option<Vec<String>>,
    #[serde(default)]
    pub session_mcp_servers: Vec<SessionMcpServer>,
    #[serde(default)]
    pub user_id: Option<String>,
    /// Knowledge bases mounted into this session's workspace, computed at
    /// task start by the conversation service. The ACP assembler renders
    /// these into a preset-context section so the agent knows what extended
    /// knowledge is available and where it lives.
    #[serde(default)]
    pub knowledge_mounts: Vec<KnowledgeMountInfo>,
    /// Write-back ("回血") switch: `true` invites the agent to persist new
    /// knowledge as markdown into the mounted directories; `false` declares
    /// them read-only. Prompt-level contract — the mounts themselves stay
    /// writable on disk.
    #[serde(default)]
    pub knowledge_writeback: bool,
    /// Write-back mode while `knowledge_writeback` is true: `staged` confines
    /// writes to `_inbox/{conversation_id}/` (conflict-free across sessions,
    /// the default), `direct` allows editing the base body.
    #[serde(default)]
    pub knowledge_writeback_mode: Option<String>,
    /// Write-back disposition ("回写意识") while `knowledge_writeback` is true:
    /// `conservative` (restrained, the default) only persists clearly-useful
    /// knowledge, `aggressive` captures anything plausibly relevant. Orthogonal
    /// to `knowledge_writeback_mode`.
    #[serde(default)]
    pub knowledge_writeback_eagerness: Option<String>,
}

/// OpenClaw gateway configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenClawGatewayConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub token: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub use_external_gateway: bool,
    pub cli_path: Option<String>,
}

/// OpenClaw-specific fields extracted from `extra` in build task options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenClawBuildExtra {
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub gateway: OpenClawGatewayConfig,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub preset_assistant_id: Option<String>,
    #[serde(default)]
    pub cron_job_id: Option<String>,
    #[serde(default, rename = "sessionKey")]
    pub session_key: Option<String>,
}

/// Remote agent-specific fields extracted from `extra` in build task options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteBuildExtra {
    /// Primary key of the `remote_agents` row (i64 since the primary-key
    /// rework). The frontend carries it as a JSON number.
    pub remote_agent_id: i64,
}

/// Opt-in goal-driven continuation for a session. When present, the engine
/// keeps working toward `objective` across turns (with a completion audit)
/// until the model proves completion, hits `max_auto_continuations`, or
/// `max_turns`. Absent (the default) = normal one-shot turn behavior.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NomiGoalSpec {
    pub objective: String,
    /// Cap on automatic continuations (anti-runaway). Defaults to 8 when unset.
    #[serde(default)]
    pub max_auto_continuations: Option<usize>,
}

/// Nomi-specific fields extracted from `extra` in build task options.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NomiBuildExtra {
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub preset_rules: Option<String>,
    #[serde(default = "default_nomi_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub max_turns: Option<usize>,
    /// Opt-in goal-driven continuation (see [`NomiGoalSpec`]).
    #[serde(default)]
    pub goal: Option<NomiGoalSpec>,
    #[serde(default)]
    pub session_mode: Option<String>,
    #[serde(default)]
    pub guide_mcp_config: Option<GuideMcpConfig>,
    #[serde(default)]
    pub mcp_server_ids: Option<Vec<String>>,
    #[serde(default)]
    pub session_mcp_servers: Vec<SessionMcpServer>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    /// Marks a companion-companion conversation: the factory registers the
    /// companion memory tools (recall/save memory, recent events) and skips
    /// the guide MCP injection (the companion talks, it doesn't create teams).
    /// Accepts both camelCase (frontend extra) and snake_case.
    #[serde(default, alias = "companionSession")]
    pub companion: bool,
    /// Opt-in to the Computer tool (screen/mouse/keyboard control) for this
    /// session. Falls back to host config / NOMIFUN_COMPUTER_USE when None.
    #[serde(default, alias = "computerUse")]
    pub computer_use: Option<bool>,
    /// Opt-in to the Browser tool (CDP automation) for this session.
    /// Falls back to host config / NOMIFUN_BROWSER_USE when None.
    #[serde(default, alias = "browserUse")]
    pub browser_use: Option<bool>,
    /// Marks a session entitled to the Desktop Gateway MCP (`nomi_*` tools).
    /// Backend-set only; stripped from client-supplied extra by the HTTP
    /// conversation routes. See `AcpBuildExtra::desktop_gateway`.
    #[serde(default, alias = "desktopGateway")]
    pub desktop_gateway: bool,
    /// Desktop Gateway MCP stdio bridge config, injected from
    /// `AgentFactoryDeps::gateway_mcp_config` when `desktop_gateway` is set.
    #[serde(default)]
    pub gateway_mcp_config: Option<GatewayMcpConfig>,
    /// IM platform this conversation serves (e.g. "telegram", "lark"), set by
    /// the channel layer on master-agent sessions. Consumed by the companion
    /// prompt provider so the persona can acknowledge the remote context.
    #[serde(default, alias = "channelPlatform")]
    pub channel_platform: Option<String>,
    /// The companion this session is bound to (multi-companion upgrade). Set by the
    /// channel layer on master-agent sessions (platform binding > default
    /// companion) and consumed by the companion prompt provider to pick the
    /// persona; also rides the gateway MCP env (`NOMI_GW_MCP_COMPANION_ID`).
    /// `None` resolves to the host's default companion. Accepts both camelCase
    /// (`companionId`, the extra-JSON spelling) and snake_case.
    #[serde(default, alias = "companionId")]
    pub companion_id: Option<String>,
    /// Knowledge bases mounted into this session's workspace, computed at
    /// task start by the conversation service. The nomi factory renders
    /// these into a system-prompt section so the agent knows what extended
    /// knowledge is available and where it lives. Same serde shape as
    /// `AcpBuildExtra::knowledge_mounts`.
    #[serde(default)]
    pub knowledge_mounts: Vec<KnowledgeMountInfo>,
    /// Write-back ("回血") switch: `true` invites the agent to persist new
    /// knowledge as markdown into the mounted directories; `false` declares
    /// them read-only. Prompt-level contract — the mounts themselves stay
    /// writable on disk. Same shape as `AcpBuildExtra::knowledge_writeback`.
    #[serde(default)]
    pub knowledge_writeback: bool,
    /// Write-back mode while `knowledge_writeback` is true: `staged` confines
    /// writes to `_inbox/{conversation_id}/` (conflict-free across sessions,
    /// the default), `direct` allows editing the base body. Same shape as
    /// `AcpBuildExtra::knowledge_writeback_mode`.
    #[serde(default)]
    pub knowledge_writeback_mode: Option<String>,
    /// Write-back disposition ("回写意识") while `knowledge_writeback` is true:
    /// `conservative` (the default) or `aggressive`. Orthogonal to
    /// `knowledge_writeback_mode`; same shape as
    /// `AcpBuildExtra::knowledge_writeback_eagerness`.
    #[serde(default)]
    pub knowledge_writeback_eagerness: Option<String>,
    /// Opt-in for unattended IM-channel (bot) sessions to write back. Off by
    /// default; channel writes are always staged. The nomi factory reconstructs
    /// the knowledge binding from this build-extra to resolve the per-surface
    /// write policy, so this MUST be threaded through — otherwise the
    /// reconstructed binding defaults it to `false` and `WriteSurface::ExternalChannel`
    /// is permanently `Disabled` on the nomi engine. (The ACP path doesn't need
    /// a mirror: it resolves channel writes at write time from the live binding
    /// via the scoped knowledge MCP bridge.)
    #[serde(default)]
    pub knowledge_channel_write_enabled: bool,
    /// Orchestration role marker. When `"lead"`, the conversation was created
    /// from the 会话 entry with "auto/range" models selected: the nomi factory
    /// injects a server-authored 编排主管 (orchestration lead) system prompt so
    /// the conversation knows to decompose complex requirements via the
    /// `nomi_run_create` tool. Client-supplied (the WebUI sets it on the new
    /// conversation's extra); it ONLY composes a system prompt and never
    /// self-authorizes tool access — the orchestration tools ride the
    /// independently-gated desktop gateway. Any other value (or `None`, the
    /// default for every existing conversation) is a no-op.
    #[serde(default)]
    pub orchestrator_role: Option<String>,
    /// Per-session 工具白名单（受限角色的编排 worker 用）。非空时引擎只保留
    /// 名单内的工具（bootstrap `retain_named`）。后端（orchestrator worker 的
    /// `build_worker_extra`）设置；普通会话恒空 = 不限制。
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// 对外服务信任档（正交于 Surface）。后端设定；`PublicService` 令 nomi 工厂把
    /// 会话硬钳到安全白名单（关网关 / computer / browser / spawn），覆盖任何上游
    /// 传入的工具授予——execution-time 后端权威闸。缺省 `Private` = 今日行为，零回归。
    #[serde(default)]
    pub exposure: crate::ExposureMode,
    /// 对外伙伴（public agent / 对外服务）绑定 id。置位即标记本会话为对外服务：
    /// nomi 工厂据此把 `exposure` 升到 `PublicService`（硬钳，安全边界），并从
    /// `PublicAgentConfig` LIVE 解析人格 / 服务守则 / grounded / 知识库范围。后端
    /// 设定 only —— HTTP 会话路由像 `desktop_gateway` 一样从 client extra 中剥离，
    /// 防止自授权。接受驼峰 (`publicAgentId`) 与蛇形。缺省 `None` = 非对外会话。
    #[serde(default, alias = "publicAgentId")]
    pub public_agent_id: Option<String>,
    /// 「agent 集群」意图标记（需求1）。用户在 composer 显式点选后写到会话 extra；
    /// nomi 工厂据此在常驻 subagent 提示之上追加更强的 `CLUSTER_MODE_HINT`（必须
    /// 刻意评估是否开集群、太简单须先说明原因再作答）。仅塑形提示、不授予能力——
    /// 编排工具仍随独立门控的桌面网关走。缺省 `false` = 既有会话零变化。
    #[serde(default, alias = "agentClusterMode")]
    pub agent_cluster_mode: bool,
}

fn default_nomi_max_tokens() -> u32 {
    8192
}

/// ACP model information returned by the ACP backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpModelInfo {
    pub model_id: String,
    pub model_name: Option<String>,
    pub provider: Option<String>,
}

/// A slash command item available in a conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandItem {
    pub command: String,
    pub description: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P4-2: the `browser_mcp_config` field on `AcpBuildExtra` round-trips
    /// symmetrically with `computer_mcp_config` (both `Some`, both preserved).
    #[test]
    fn acp_build_extra_browser_mcp_config_json_roundtrip() {
        let extra = AcpBuildExtra {
            browser_mcp_config: Some(BrowserMcpConfig {
                binary_path: "/usr/bin/nomicore".into(),
            }),
            computer_mcp_config: Some(ComputerMcpConfig {
                binary_path: "/usr/bin/nomicore".into(),
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&extra).unwrap();
        let parsed: AcpBuildExtra = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.browser_mcp_config.as_ref().map(|c| c.binary_path.as_str()),
            Some("/usr/bin/nomicore"),
            "browser_mcp_config must survive the round-trip"
        );
        // Symmetry: the sibling computer field still round-trips alongside it.
        assert_eq!(
            parsed.computer_mcp_config.as_ref().map(|c| c.binary_path.as_str()),
            Some("/usr/bin/nomicore"),
        );
    }

    /// Default `AcpBuildExtra` (feature OFF / no injection) leaves
    /// `browser_mcp_config` `None`, so the assembler injects nothing (no-op).
    #[test]
    fn acp_build_extra_browser_mcp_config_defaults_none() {
        let extra = AcpBuildExtra::default();
        assert!(extra.browser_mcp_config.is_none());
    }

    /// Task 4: a conversation marked as the orchestration lead carries
    /// `extra.orchestrator_role = "lead"`, which must deserialize onto
    /// `NomiBuildExtra` so the nomi factory can inject the 主管 system prompt.
    #[test]
    fn nomi_build_extra_deserializes_orchestrator_role_lead() {
        let extra: NomiBuildExtra =
            serde_json::from_value(serde_json::json!({ "orchestrator_role": "lead" })).unwrap();
        assert_eq!(extra.orchestrator_role.as_deref(), Some("lead"));
    }

    /// Absence of the marker (every existing conversation) leaves the field
    /// `None`, so no 主管 prompt is injected — the field is purely additive.
    #[test]
    fn nomi_build_extra_orchestrator_role_defaults_none() {
        let extra: NomiBuildExtra = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(extra.orchestrator_role.is_none());
        assert!(NomiBuildExtra::default().orchestrator_role.is_none());
    }
}
