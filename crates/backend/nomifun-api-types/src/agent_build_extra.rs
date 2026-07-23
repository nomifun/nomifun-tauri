use std::collections::HashMap;

use nomifun_common::{
    CompanionId, CronJobId, DelegationPolicy, PublicAgentId, RemoteAgentId, UserId,
};
use serde::{Deserialize, Serialize};

use crate::{
    BrowserMcpConfig, ComputerMcpConfig, GatewayMcpConfig, KnowledgeMcpConfig,
    KnowledgeMountInfo, McpServerId, OpenMcpConfig, RequirementMcpConfig,
};

macro_rules! optional_id_deserializer {
    ($name:ident, $id:ty) => {
        fn $name<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let value = Option::<String>::deserialize(deserializer)?;
            value
                .map(|value| {
                    <$id>::parse(value.clone())
                        .map(|_| value)
                        .map_err(serde::de::Error::custom)
                })
                .transpose()
        }
    };
}

optional_id_deserializer!(deserialize_companion_id, CompanionId);
optional_id_deserializer!(deserialize_public_agent_id, PublicAgentId);
optional_id_deserializer!(deserialize_user_id, UserId);
optional_id_deserializer!(deserialize_cron_job_id, CronJobId);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SessionMcpServer {
    pub mcp_server_id: McpServerId,
    pub name: String,
    pub transport: SessionMcpTransport,
}


/// ACP-specific fields extracted from `extra` in build runtime options.
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
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_preset_id"
    )]
    pub preset_id: Option<String>,
    #[serde(default)]
    pub session_mode: Option<String>,
    #[serde(default)]
    pub current_model_id: Option<String>,
    /// Stable `cron_jobs.cron_job_id`.
    #[serde(default, deserialize_with = "deserialize_cron_job_id")]
    pub cron_job_id: Option<String>,
    /// Requirement MCP stdio bridge config. When `Some`, the ACP assembler
    /// injects `nomicore mcp-requirement-stdio` so the agent gets the
    /// `requirement_complete` / `requirement_update_status` declaration tools.
    /// Injected from `AgentFactoryDeps::requirement_mcp_config` at build time.
    #[serde(skip)]
    pub requirement_mcp_config: Option<RequirementMcpConfig>,
    /// Knowledge-search MCP stdio bridge config. When `Some`, the ACP assembler
    /// injects `nomicore mcp-knowledge-stdio` so the agent gets a scoped
    /// knowledge-search tool over the session's bound knowledge bases. The
    /// assembler signs user/session/workspace/base ids into a short-lived child
    /// capability; this non-serializable issuer is never persisted or sent to
    /// the child. The nomi engine has `knowledge_search` natively.
    #[serde(skip)]
    pub knowledge_mcp_config: Option<KnowledgeMcpConfig>,
    /// Platform Gateway MCP stdio bridge config. Process-owned and injected by
    /// the Agent factory only after it derives installation-owner authority.
    /// It is never deserialized from Conversation JSON.
    #[serde(skip)]
    pub gateway_mcp_config: Option<GatewayMcpConfig>,
    /// Exact Platform Gateway tools omitted from this session. This subtractive
    /// fence is signed into the session claims and enforced both by tools/list
    /// and the in-process dispatch boundary.
    #[serde(default)]
    pub gateway_excluded_tools: Vec<String>,
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
    /// channel layer on Channel Agent sessions (platform binding > default
    /// companion); the backend binds it into the signed Gateway child capability
    /// so desktop tools can attribute the caller.
    #[serde(default, deserialize_with = "deserialize_companion_id")]
    pub companion_id: Option<String>,
    /// IM platform this session serves (e.g. "lark") when it is a channel
    /// Channel Agent. Set by the channel layer and bound into the signed Gateway
    /// child capability so the gateway resolves the write surface (channel →
    /// write-disabled unless re-enabled). Mirrors `NomiBuildExtra`.
    #[serde(default)]
    pub channel_platform: Option<String>,
    #[serde(default)]
    /// Stable MCP server business IDs.
    pub mcp_server_ids: Option<Vec<McpServerId>>,
    #[serde(default)]
    pub session_mcp_servers: Vec<SessionMcpServer>,
    #[serde(default, deserialize_with = "deserialize_user_id")]
    pub user_id: Option<String>,
    /// Knowledge bases mounted into this session's workspace, computed when
    /// the Agent runtime is created. The ACP assembler renders
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

/// OpenClaw-specific fields extracted from `extra` in build runtime options.
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
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_preset_id"
    )]
    pub preset_id: Option<String>,
    /// Stable `cron_jobs.cron_job_id`.
    #[serde(default, deserialize_with = "deserialize_cron_job_id")]
    pub cron_job_id: Option<String>,
    #[serde(default)]
    pub session_key: Option<String>,
}

/// Remote agent-specific fields extracted from `extra` in build runtime options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteBuildExtra {
    /// Canonical global ID of the configured remote-agent entity.
    pub remote_agent_id: RemoteAgentId,
    /// Remote gateway session key persisted after a successful turn.
    #[serde(default)]
    pub session_key: Option<String>,
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

/// Nomi-specific fields extracted from `extra` in build runtime options.
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
    /// Stable MCP server business IDs.
    pub mcp_server_ids: Option<Vec<McpServerId>>,
    #[serde(default)]
    pub session_mcp_servers: Vec<SessionMcpServer>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default, deserialize_with = "deserialize_user_id")]
    pub user_id: Option<String>,
    /// Marks a companion conversation: the factory registers its memory tools
    /// (recall/save memory, recent events) and skips unrelated Guide capabilities.
    #[serde(default, rename = "companion_session")]
    pub companion: bool,
    /// Opt-in to the Computer tool (screen/mouse/keyboard control) for this
    /// session. Falls back to host config / NOMIFUN_COMPUTER_USE when None.
    #[serde(default)]
    pub computer_use: Option<bool>,
    /// Opt-in to the Browser tool (CDP automation) for this session.
    /// Falls back to host config / NOMIFUN_BROWSER_USE when None.
    #[serde(default)]
    pub browser_use: Option<bool>,
    /// Platform Gateway MCP stdio bridge config, injected only from
    /// process-owned factory dependencies after authority resolution.
    #[serde(skip)]
    pub gateway_mcp_config: Option<GatewayMcpConfig>,
    /// Exact Platform Gateway tools omitted from this session's MCP tools/list.
    /// This is a subtractive runtime capability fence, never a grant.
    #[serde(default)]
    pub gateway_excluded_tools: Vec<String>,
    /// IM platform this conversation serves (e.g. "telegram", "lark"), set by
    /// the channel layer on Channel Agent sessions. Consumed by the companion
    /// prompt provider so the persona can acknowledge the remote context.
    #[serde(default)]
    pub channel_platform: Option<String>,
    /// The companion this session is bound to (multi-companion upgrade). Set by the
    /// channel layer on Channel Agent sessions (platform binding > default
    /// companion) and consumed by the companion prompt provider to pick the
    /// persona; it is also bound into the signed Gateway child capability.
    /// `None` means there is no companion binding.
    #[serde(default, deserialize_with = "deserialize_companion_id")]
    pub companion_id: Option<String>,
    /// Knowledge bases mounted into this session's workspace, computed when
    /// the Agent runtime is created. The Nomi factory renders
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
    /// Per-session 工具白名单（受限的持久执行 Agent 使用）。非空时引擎只保留
    /// 名单内的工具（bootstrap `retain_named`）。执行层在创建 Agent attempt
    /// conversation 时设置；普通会话恒空 = 不限制。
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// 对外服务信任档（正交于 Surface）。后端设定；`PublicService` 令 nomi 工厂把
    /// 会话硬钳到安全白名单（关网关 / computer / browser / delegation），覆盖任何上游
    /// 传入的工具授予——execution-time 后端权威闸。缺省 `Private` = 今日行为，零回归。
    #[serde(default)]
    pub exposure: crate::ExposureMode,
    /// 对外伙伴（public agent / 对外服务）绑定 id。置位即标记本会话为对外服务：
    /// nomi 工厂据此把 `exposure` 升到 `PublicService`（硬钳，安全边界），并从
    /// `PublicAgentConfig` LIVE 解析人格 / 服务守则 / grounded / 知识库范围。后端
    /// 设定 only —— HTTP 会话路由从 client extra 中剥离，
    /// 防止自授权。缺省 `None` = 非对外会话。
    #[serde(default, deserialize_with = "deserialize_public_agent_id")]
    pub public_agent_id: Option<String>,
    /// Conversation-level delegation intent. This shapes when the Agent uses
    /// the unified persistent execution tools; it never grants tool authority.
    /// The factory always overwrites this from the typed runtime build option;
    /// a same-named value in open-ended JSON is never authoritative.
    #[serde(default = "default_delegation_policy")]
    pub delegation_policy: DelegationPolicy,
}

fn default_nomi_max_tokens() -> u32 {
    8192
}

fn default_delegation_policy() -> DelegationPolicy {
    DelegationPolicy::Automatic
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

    const MCP_SERVER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000123";

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

    #[test]
    fn nomi_build_extra_deserializes_delegation_policy() {
        let extra: NomiBuildExtra = serde_json::from_value(
            serde_json::json!({ "delegation_policy": "prefer_parallel" }),
        )
        .unwrap();
        assert_eq!(extra.delegation_policy, DelegationPolicy::PreferParallel);
    }

    #[test]
    fn nomi_build_extra_delegation_defaults_automatic() {
        let extra: NomiBuildExtra = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(extra.delegation_policy, DelegationPolicy::Automatic);
        assert_eq!(NomiBuildExtra::default().delegation_policy, DelegationPolicy::Automatic);
    }

    #[test]
    fn session_mcp_server_id_accepts_canonical_uuidv7() {
        let value = serde_json::json!({
            "mcp_server_id": MCP_SERVER_ID,
            "name": "temporary",
            "transport": { "type": "stdio", "command": "server" }
        });
        let parsed: SessionMcpServer = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.mcp_server_id.as_str(), MCP_SERVER_ID);
    }

    #[test]
    fn session_mcp_server_rejects_legacy_id() {
        let value = serde_json::json!({
            "id": 42,
            "name": "temporary",
            "transport": { "type": "stdio", "command": "server" }
        });
        assert!(serde_json::from_value::<SessionMcpServer>(value).is_err());
    }

    #[test]
    fn catalog_mcp_ids_require_canonical_uuidv7_strings() {
        let id = McpServerId::parse(MCP_SERVER_ID).unwrap();
        let parsed: NomiBuildExtra =
            serde_json::from_value(serde_json::json!({ "mcp_server_ids": [id.clone()] })).unwrap();
        assert_eq!(parsed.mcp_server_ids, Some(vec![id]));

        for invalid in [
            serde_json::json!([42]),
            serde_json::json!(["42"]),
            serde_json::json!(["550e8400-e29b-41d4-a716-446655440000"]),
            serde_json::json!([format!("mcp_{MCP_SERVER_ID}")]),
            serde_json::json!([true]),
        ] {
            assert!(
                serde_json::from_value::<NomiBuildExtra>(
                    serde_json::json!({ "mcp_server_ids": invalid })
                )
                .is_err()
            );
        }
    }
}
