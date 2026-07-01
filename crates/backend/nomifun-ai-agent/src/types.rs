use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use nomifun_common::{AgentType, ProviderWithModel};

/// Data payload for sending a user message to an Agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageData {
    /// User message content.
    pub content: String,
    /// Client-generated message ID for correlation.
    pub msg_id: String,
    /// File paths attached to the message.
    #[serde(default)]
    pub files: Vec<String>,
    /// Skills to inject into this message turn.
    #[serde(default)]
    pub inject_skills: Vec<String>,
    /// Turn origin marker (companion/cron/autowork/idmm). `None`/empty = a human
    /// owner is speaking. Same semantics as the collector's `payload_origin`
    /// red line: non-empty origins are NOT human intent and must not be
    /// distilled into file-based memory.
    #[serde(default)]
    pub origin: Option<String>,
}

/// Options for building (creating or resuming) an Agent task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildTaskOptions {
    /// Type of agent to create.
    pub agent_type: AgentType,
    /// Working directory for the agent.
    pub workspace: String,
    /// Model selection config.
    pub model: ProviderWithModel,
    /// Conversation ID this task belongs to.
    pub conversation_id: String,
    /// Type-specific extra parameters (JSON object).
    #[serde(default)]
    pub extra: serde_json::Value,
    /// Owning conversation row's `created_at` (ms). Stable per conversation
    /// INSTANCE — used to stamp/validate the nomi session's `owner_token` so a
    /// reused integer id never resumes a stale session. `None` skips validation.
    #[serde(default)]
    pub conversation_created_at: Option<i64>,
}

/// Provider-specific compat overrides resolved in the factory.
#[derive(Debug, Clone, Default)]
pub struct NomiCompatOverrides {
    pub max_tokens_field: Option<String>,
    pub api_path: Option<String>,
    /// None = 默认支持图片;Some(false) = registry 已标记不支持,发送时剔图。
    pub supports_image: Option<bool>,
}

/// Fully resolved Nomi configuration passed to the agent manager.
#[derive(Debug, Clone)]
pub struct NomiResolvedConfig {
    /// LLM provider name (anthropic, openai, bedrock, vertex).
    pub provider: String,
    /// Decrypted API key.
    pub api_key: String,
    /// Model identifier.
    pub model: String,
    /// Provider base URL.
    pub base_url: Option<String>,
    /// System prompt override.
    pub system_prompt: Option<String>,
    /// Max tokens per response.
    pub max_tokens: u32,
    /// Max agentic turns.
    pub max_turns: Option<usize>,
    /// Provider's declared context window (tokens), if configured. Drives the
    /// engine's compaction window and the context-usage gauge denominator.
    pub context_limit: Option<u64>,
    /// Provider-specific compat overrides.
    pub compat_overrides: NomiCompatOverrides,
    /// Directory for nomi session persistence files.
    pub session_directory: PathBuf,
    /// Session mode (default, auto_edit, yolo).
    pub session_mode: Option<String>,
    /// Extra MCP servers to inject (team coordination or guide).
    pub extra_mcp_servers: HashMap<String, nomi_config::config::McpServerConfig>,
    /// AWS Bedrock credentials (region + access key or profile).
    pub bedrock_config: Option<nomi_config::config::BedrockConfig>,
    /// Enable the Computer tool (screen/mouse/keyboard control).
    pub computer_use: bool,
    /// Enable the Browser tool (CDP automation).
    pub browser_use: bool,
    /// **静默浏览器 LIVE 值**（「浏览器模式」的可见性维度）。`true`（**产品默认 ON**）→
    /// 引擎 headless（无可见窗口）；`false` → 弹出可见窗口。工厂经 `read_bool_pref` LIVE 读
    /// `agent.browserUse.silent`（host_default=**true**）。映射到 `config.tools.browser.headless`
    /// （silent→headless），facade 由 `!headless` 得 headful。无显示器时引擎本就强制 headless。
    pub browser_silent: bool,
    /// **浏览器来源 LIVE 值**（「浏览器模式」的来源维度，与 silent 正交）。`"managed"`（默认）=
    /// 内置/下载 CfT；`"system"` = 系统 Chrome/Edge 本体优先（未探到回退 managed）。工厂经
    /// `read_string_pref` LIVE 读 `agent.browserUse.source`（host_default=`"managed"`）。映射到
    /// `config.tools.browser.source`，facade 解析为 `ChromeSource`。红线不变：专属 user-data-dir。
    pub browser_source: String,
    /// **F1-sec: browser-use evaluate「全权模式」LIVE 值**（裁决⑨，default-deny）。`true` 当且仅当
    /// 用户在 System Settings 显式 opt-in（`client_preferences` `agent.browserUse.fullPower`，工厂经
    /// `read_bool_pref` 范式 LIVE 读）。`false`（默认）→ 引擎 `evaluate` 动作返 `Unsupported`。**绝不看
    /// session_mode**（yolo/companion 无从豁免，不变量⑧）。
    pub browser_full_power: bool,
    /// **SD-6: browser-use 持久登录 LIVE 值**（DESIGN §16/§27 互斥约束）。`true`（产品默认）→ 与全权
    /// 互斥（evaluate 在两者皆 true 时 Blocked）。工厂经 `read_bool_pref` 范式 LIVE 读
    /// `agent.browserUse.persistentLogin`（host_default=true）。`false` → 互斥不生效（evaluate 仅受
    /// full_power 开关控制）。代码级 Default = `false`（与 full_power 同范式 default-deny 基线）。
    pub browser_persistent_login: bool,
    /// **P7A site-memory LIVE 值**（opt-in，隐私相关）。`true` → bootstrap 给 `BrowserTool` 注入文件型
    /// `SiteMemorySink`（跨会话记住站点结构 + 向 observe 注入 hints）。工厂经 `read_bool_pref` 范式 LIVE
    /// 读 `agent.browserUse.siteMemory`（host_default=**false**=OFF）。`false`（默认）→ 不挂 sink，零行为变化。
    pub browser_site_memory: bool,
    /// **Phase D takeover/审批 LIVE 值**（opt-in，安全）。`true` → 桌面会话构造期注入
    /// `DesktopApprovalGate`：不可逆动作（bypass 会话）+ 被门控跨域 POST（SD-5）浮给用户审批后
    /// 才放行（否则 fail-closed 硬挡）。工厂经 `read_bool_pref` LIVE 读 `agent.browserUse.takeover`
    /// （host_default=**false**=OFF）。`false`（默认）→ 不注入 gate，维持 fail-closed 零回归。
    pub browser_takeover: bool,
    /// **P7B visual-fallback LIVE 值**（opt-in，有 token 成本）。`true` → bootstrap 给 `BrowserTool`
    /// 注入会话模型的 `VisualLocator`：DOM/aria 锚定失败（ref stale/detached）时截图交视觉模型按描述
    /// 定位再点。工厂经 `read_bool_pref` 范式 LIVE 读 `agent.browserUse.visualFallback`
    /// （host_default=**false**=OFF）。`false`（默认）→ 不注入 locator，facade 兜底保持 Unavailable（零行为变化）。
    pub browser_visual_fallback: bool,
    /// Opt-in goal-driven continuation (objective + auto-continuation cap).
    /// `None` (default) = normal one-shot turn behavior.
    pub goal: Option<nomi_agent::goal::runtime::GoalSpec>,
    /// **P3-X2: per-pet browser secret vault descriptor** (vault file path +
    /// machine-bound key). Threaded to the bootstrap so the native `BrowserTool`
    /// loads the user-registered credentials (`secret:NAME`, origin-gated) and
    /// derives the firewall domain allowlist from their `allowed_origins` (裁决⑤).
    /// `None` (no companion / browser-use off / probe sessions) → empty store +
    /// unrestricted egress (current behavior). The raw key is carried (not a
    /// `nomi_browser` type) so this crate needs no `nomi-browser` dependency.
    pub browser_secret_vault: Option<BrowserSecretVault>,
    /// Stable identity of the owning conversation INSTANCE (the conversation
    /// row's `created_at`, stringified). Threaded to the nomi manager so it can
    /// stamp/validate the session's `owner_token` and refuse to resume a stale
    /// session left by a prior conversation that reused this integer id. `None`
    /// = caller did not supply it (validation skipped — legacy/safe).
    pub owner_token: Option<String>,
}

/// **P3-X2**: the shared browser secret vault location + its machine-bound key
/// (去 per-pet 键化: browser identity globally shared — one vault for all companions).
/// Debug redacts the key so it never lands in a `NomiResolvedConfig` log line.
#[derive(Clone)]
pub struct BrowserSecretVault {
    /// The shared secret vault file path
    /// (`{data_dir}/browser-secrets/shared/secrets.json`).
    pub vault_path: std::path::PathBuf,
    /// The machine-bound AES-256-GCM `encryption_key` (32 bytes).
    pub key: [u8; 32],
}

impl std::fmt::Debug for BrowserSecretVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserSecretVault")
            .field("vault_path", &self.vault_path)
            .field("key", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::{AcpBuildExtra, AcpModelInfo, NomiBuildExtra, OpenClawGatewayConfig, SlashCommandItem};
    use serde_json::json;

    #[test]
    fn acp_build_extra_accepts_payload_without_skills() {
        let legacy = r#"{"backend":"claude"}"#;
        let parsed: AcpBuildExtra = serde_json::from_str(legacy).unwrap();
        assert!(parsed.skills.is_empty());
    }

    #[test]
    fn acp_build_extra_accepts_skills() {
        let with_field = r#"{"backend":"claude","skills":["cron","pdf"]}"#;
        let parsed: AcpBuildExtra = serde_json::from_str(with_field).unwrap();
        assert_eq!(parsed.skills, vec!["cron".to_owned(), "pdf".to_owned()]);
    }

    #[test]
    fn send_message_data_serde_roundtrip() {
        let data = SendMessageData {
            content: "Hello".into(),
            msg_id: "msg-001".into(),
            files: vec!["/tmp/a.txt".into()],
            inject_skills: vec!["review".into()],
            origin: None,
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["content"], "Hello");
        assert_eq!(json["msg_id"], "msg-001");
        assert_eq!(json["files"], json!(["/tmp/a.txt"]));
        assert_eq!(json["inject_skills"], json!(["review"]));

        let parsed: SendMessageData = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.content, "Hello");
        assert_eq!(parsed.msg_id, "msg-001");
    }

    #[test]
    fn send_message_data_defaults_optional_fields() {
        let json = json!({ "content": "Hi", "msg_id": "m1" });
        let data: SendMessageData = serde_json::from_value(json).unwrap();
        assert!(data.files.is_empty());
        assert!(data.inject_skills.is_empty());
        assert!(data.origin.is_none());
    }

    #[test]
    fn send_message_data_origin_roundtrips() {
        let json = json!({ "content": "Hi", "msg_id": "m1", "origin": "cron" });
        let data: SendMessageData = serde_json::from_value(json).unwrap();
        assert_eq!(data.origin.as_deref(), Some("cron"));
    }

    #[test]
    fn build_task_options_serde() {
        let opts = BuildTaskOptions {
            agent_type: AgentType::Acp,
            workspace: "/project".into(),
            model: ProviderWithModel {
                provider_id: "p1".into(),
                model: "claude-sonnet".into(),
                use_model: None,
            },
            conversation_id: "conv-1".into(),
            extra: json!({ "backend": "claude" }),
            conversation_created_at: None,
        };
        let json = serde_json::to_value(&opts).unwrap();
        assert_eq!(json["agent_type"], "acp");
        assert_eq!(json["workspace"], "/project");
        assert_eq!(json["conversation_id"], "conv-1");
    }

    #[test]
    fn acp_model_info_serde() {
        let info = AcpModelInfo {
            model_id: "claude-sonnet-4".into(),
            model_name: Some("Claude Sonnet 4".into()),
            provider: Some("anthropic".into()),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["model_id"], "claude-sonnet-4");
        assert_eq!(json["model_name"], "Claude Sonnet 4");
    }

    #[test]
    fn slash_command_item_serde() {
        let cmd = SlashCommandItem {
            command: "/review".into(),
            description: "Code review".into(),
        };
        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["command"], "/review");
    }

    #[test]
    fn openclaw_gateway_config_defaults() {
        let json = json!({});
        let config: OpenClawGatewayConfig = serde_json::from_value(json).unwrap();
        assert!(!config.use_external_gateway);
        assert!(config.host.is_none());
        assert!(config.port.is_none());
    }

    #[test]
    fn nomi_build_extra_serde_defaults() {
        let json = json!({});
        let extra: NomiBuildExtra = serde_json::from_value(json).unwrap();
        assert!(extra.system_prompt.is_none());
        assert!(extra.preset_rules.is_none());
        assert_eq!(extra.max_tokens, 8192);
        assert!(extra.max_turns.is_none());
    }

    #[test]
    fn nomi_build_extra_serde_with_overrides() {
        let json = json!({
            "system_prompt": "You are a helpful assistant.",
            "max_tokens": 4096,
            "max_turns": 10
        });
        let extra: NomiBuildExtra = serde_json::from_value(json).unwrap();
        assert_eq!(extra.system_prompt.unwrap(), "You are a helpful assistant.");
        assert_eq!(extra.max_tokens, 4096);
        assert_eq!(extra.max_turns.unwrap(), 10);
    }

    #[test]
    fn nomi_build_extra_serde_with_preset_rules() {
        let json = json!({
            "preset_rules": "You are a data analyst.",
            "max_tokens": 8192
        });
        let extra: NomiBuildExtra = serde_json::from_value(json).unwrap();
        assert!(extra.system_prompt.is_none());
        assert_eq!(extra.preset_rules.unwrap(), "You are a data analyst.");
    }
}
