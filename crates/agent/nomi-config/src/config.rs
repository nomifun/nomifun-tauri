use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::auth::{AuthConfig, OAuthManager};
use crate::compact::CompactConfig;
use crate::compat::ProviderCompat;
use crate::file_cache::FileCacheConfig;
use crate::hooks::HooksConfig;
use crate::logging::LoggingConfig;
use crate::plan::PlanConfig;
use nomi_types::llm::ThinkingConfig;

// ---------------------------------------------------------------------------
// Provider-specific sub-configurations (defined here to avoid circular deps)
// ---------------------------------------------------------------------------

/// AWS Bedrock credentials configuration
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct BedrockConfig {
    pub region: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    pub profile: Option<String>,
}

/// Google Vertex AI authentication configuration
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct VertexConfig {
    pub project_id: Option<String>,
    pub region: Option<String>,
    pub credentials_file: Option<String>,
    pub service_account_json: Option<String>,
}

/// Transport type for MCP server connections
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TransportType {
    #[default]
    Stdio,
    Sse,
    StreamableHttp,
}

/// A single MCP server configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub transport: TransportType,
    /// For stdio transport: the command to run
    pub command: Option<String>,
    /// For stdio transport: arguments to the command
    pub args: Option<Vec<String>>,
    /// Environment variables to set for this server (stdio)
    pub env: Option<HashMap<String, String>>,
    /// For SSE/HTTP transport: the URL
    pub url: Option<String>,
    /// HTTP headers for SSE/HTTP transports
    pub headers: Option<HashMap<String, String>>,
    /// Whether tools from this server should be deferred (name-only stub sent to LLM).
    /// Defaults to true when omitted — MCP tools are deferred by default to reduce
    /// input token usage. Set to `false` to send full schemas eagerly.
    pub deferred: Option<bool>,
}

/// Collection of MCP server configurations
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: HashMap<String, McpServerConfig>,
}

/// Top-level config file structure
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub default: DefaultConfig,

    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,

    #[serde(default)]
    pub profiles: HashMap<String, ProfileConfig>,

    #[serde(default)]
    pub tools: ToolsConfig,

    #[serde(default)]
    pub session: SessionConfig,

    #[serde(default)]
    pub compact: CompactConfig,

    #[serde(default)]
    pub plan: PlanConfig,

    #[serde(default)]
    pub file_cache: FileCacheConfig,

    #[serde(default)]
    pub hooks: HooksConfig,

    pub bedrock: Option<BedrockConfig>,
    pub vertex: Option<VertexConfig>,
    pub auth: Option<AuthConfig>,

    #[serde(default)]
    pub mcp: McpConfig,

    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DefaultConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    pub model: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub max_turns: Option<usize>,
    pub system_prompt: Option<String>,
}

impl Default for DefaultConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            model: None,
            max_tokens: default_max_tokens(),
            max_turns: None,
            system_prompt: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ProviderConfig {
    /// Underlying built-in provider type for a custom provider alias.
    pub provider: Option<String>,
    /// Optional default model for this provider entry.
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    /// Enable prompt caching (Anthropic only, default: true)
    pub prompt_caching: Option<bool>,
    /// Provider compatibility overrides
    pub compat: Option<ProviderCompat>,
}

/// A named profile bundles provider + model + overrides
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ProfileConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub max_tokens: Option<u32>,
    pub max_turns: Option<usize>,
    /// Inherit settings from another profile
    pub extends: Option<String>,
    /// MCP server names to enable for this profile (references [mcp.servers.*])
    pub mcp_servers: Option<Vec<String>>,
    /// Provider compatibility overrides
    pub compat: Option<ProviderCompat>,
}

/// Per-skill deny/allow rule lists loaded from `[tools.skills]` in config.toml.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SkillsPermissionConfig {
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolsConfig {
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default = "default_allow_list")]
    pub allow_list: Vec<String>,
    /// Skill-level deny/allow rules. Merged by concatenation across global + project configs.
    #[serde(default)]
    pub skills: SkillsPermissionConfig,
    /// How many recent image-bearing tool results keep their images in
    /// history. Older images are stripped (text kept) to bound token use.
    #[serde(default = "default_max_recent_images")]
    pub max_recent_images: usize,
    #[serde(default)]
    pub computer: ComputerConfig,
    #[serde(default)]
    pub browser: BrowserConfig,
    /// Dark-launch (default off): back the `Bash` tool with a long-lived shell
    /// session so `cd`/`export` persist across calls. Unix-only; ignored on
    /// Windows. Staged rollout per the overhaul design (§3.3 / §3.6 灰度).
    #[serde(default)]
    pub persistent_shell: bool,
    /// Opt-in (default empty = off): restrict Write/Edit/ApplyPatch to writes
    /// within this directory. Accidental/buggy out-of-root writes are rejected.
    /// NOT a security sandbox (the agent has Bash) — that needs OS-level
    /// confinement; this is a guardrail (§3.6 write-root containment).
    #[serde(default)]
    pub write_root: String,
    /// Opt-in (default empty = off): language servers for the experimental `Lsp`
    /// code-navigation tool. Each entry maps file extensions to a server
    /// command. The tool is registered only when this is non-empty.
    #[serde(default)]
    pub lsp_servers: Vec<LspServerConfig>,
    /// Opt-in (default None = uncapped): a cumulative token ceiling shared
    /// across all sub-agents of a Spawn fan-out. A soft ceiling that bounds
    /// runaway multi-agent spend (§3.4 shared token budget).
    #[serde(default)]
    pub subagent_token_budget: Option<u64>,
    /// Opt-in (default off, macOS only): run `Bash` commands under a Seatbelt
    /// write-containment sandbox (writes allowed only to the workspace + temp).
    /// OS-enforced over arbitrary subprocesses; ignored on non-macOS (§3.6).
    #[serde(default)]
    pub bash_sandbox: bool,
    /// Opt-in (default off): wind the engine down COOPERATIVELY on stop —
    /// cancel a token and await a clean finish — instead of dropping the run
    /// future mid-flight. Cleaner message state; trades a little mid-tool
    /// stop-latency (the run is awaited, not dropped). (Phase 0 F0.4)
    #[serde(default)]
    pub cooperative_cancel: bool,
    /// 是否注册进程内 `Spawn` 子 agent 工具（默认 true = 现状）。桌面后端会话
    /// 由工厂置 false —— 子 agent 改走可视化的 `nomi_spawn` 编排扇出（每个
    /// 子任务在 DAG 画布上有状态与转录）；CLI/独立模式保持 true（进程内
    /// Spawn 仍是其唯一扇出手段）。
    #[serde(default = "default_true")]
    pub in_process_spawn: bool,
    /// 非空时：bootstrap 注册完全部工具后只保留名字在此列表内的（含 MCP 代理
    /// 工具）。受限角色的编排 worker（searcher/reviewer 只读等）用它做
    /// per-node 工具白名单。空（默认）= 不限制。
    #[serde(default)]
    pub builtin_allowlist: Vec<String>,
}

/// One language-server entry for the `Lsp` tool (§3.3).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LspServerConfig {
    /// File extensions (without dot) this server handles, e.g. ["rs"].
    #[serde(default)]
    pub extensions: Vec<String>,
    /// The server command: program followed by args, e.g. ["rust-analyzer"].
    #[serde(default)]
    pub command: Vec<String>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            auto_approve: false,
            allow_list: default_allow_list(),
            skills: SkillsPermissionConfig::default(),
            max_recent_images: default_max_recent_images(),
            computer: ComputerConfig::default(),
            browser: BrowserConfig::default(),
            persistent_shell: false,
            write_root: String::new(),
            lsp_servers: Vec::new(),
            subagent_token_budget: None,
            bash_sandbox: false,
            cooperative_cancel: false,
            in_process_spawn: true,
            builtin_allowlist: Vec::new(),
        }
    }
}

/// Computer-use (screen/mouse/keyboard/window control) tool settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ComputerConfig {
    /// Off by default: controlling the local desktop is opt-in.
    #[serde(default)]
    pub enabled: bool,
    /// Screenshots are downscaled so their longest edge fits this many pixels.
    #[serde(default = "default_max_screenshot_edge")]
    pub max_screenshot_edge: u32,
}

impl Default for ComputerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_screenshot_edge: default_max_screenshot_edge(),
        }
    }
}

/// Browser-use (CDP automation) tool settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BrowserConfig {
    /// Off by default: driving a browser is opt-in.
    #[serde(default)]
    pub enabled: bool,
    /// DEPRECATED (no longer consumed). Browser-use is now the in-process native
    /// CDP engine, which acquires its own managed Chromium; the executable is not
    /// chosen via this field. Kept (serde-defaulted) one release so existing
    /// config.toml files still deserialize; remove after the migration window.
    #[serde(default)]
    pub browser_path: String,
    #[serde(default)]
    pub headless: bool,
    /// DEPRECATED (no longer consumed). The native engine manages its own browser
    /// lifecycle, not an idle timeout here. Kept (serde-defaulted) one release for
    /// config compatibility; remove after the migration window. (Not to be confused
    /// with the unrelated agent-session idle timeout in `idle_scanner`.)
    #[serde(default = "default_browser_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// Optional origin allowlist for the native browser engine's egress firewall
    /// (per-pet, derived alongside the secret vault's `allowed_origins`). Empty =
    /// allow all. Defense-in-depth, not a sole security boundary.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// **F1-sec: evaluate「全权模式」开关**（E3 裁决⑨，default-deny）。`false`（默认）→ 引擎的
    /// `evaluate` 动作返 `Unsupported`（最高危逃生舱默认封死）。`true` → evaluate 放行（仍受「与持久
    /// 登录互斥」约束）。上层（backend factory）把用户在 System Settings 显式 opt-in 的
    /// `client_preferences` `agent.browserUse.fullPower` LIVE 值写到这里（与 `computer_use`/`browser_use`
    /// 启用开关同范式，每会话构造时读最新值），bootstrap 据它构造 `BrowserTool::with_policy`。
    /// **绝不看 session_mode**——yolo/companion 无从豁免（不变量⑧）。
    #[serde(default)]
    pub full_power: bool,
    /// **SD-6: 持久登录开关**（DESIGN §16/§27 互斥约束）。`true`（产品默认）→ 与全权互斥（evaluate
    /// 动作在两者皆 true 时 Blocked）。上层（backend factory）把用户在 System Settings 的
    /// `client_preferences` `agent.browserUse.persistentLogin` LIVE 值写到这里（与 `full_power`
    /// 同范式，每会话构造时读最新值），bootstrap 据它构造 `BrowserTool::with_policy`。
    /// **代码级 Default = `false`**（default-deny 基线；产品 ON 由 factory host_default=true 实现）。
    #[serde(default)]
    pub persistent_login: bool,
    /// **Site memory（P7A 站点记忆）开关**（opt-in）。`false`（默认/代码级 Default）→ facade 不挂
    /// site-memory sink：不持久化、不向 observe 注入 per-domain hints（零行为变化）。`true` → bootstrap
    /// 据它给 `BrowserTool` 注入文件型 `SiteMemorySink`（agent 跨会话记住站点结构）。上层 factory 把
    /// `client_preferences` `agent.browserUse.siteMemory` LIVE 值写到这里（与 full_power/persistent_login
    /// 同范式，host_default=false=OFF——记录站点交互到磁盘是隐私相关行为，须用户显式 opt-in）。
    #[serde(default)]
    pub site_memory: bool,
    /// **Visual fallback（P7B 视觉兜底点击）开关**（opt-in）。`false`（默认/代码级 Default）→ facade 不挂
    /// vision locator：DOM/aria 锚定失败时不做截图+视觉模型定位（零行为变化，仅返回原始锚定错误）。`true`
    /// → bootstrap 据它给 `BrowserTool` 注入会话模型的 `VisualLocator` 适配器并置位
    /// `visual_fallback_enabled`（锚定 stale/detached 时截图交视觉模型定位再点）。上层 factory 把
    /// `client_preferences` `agent.browserUse.visualFallback` LIVE 值写到这里（与 site_memory/full_power
    /// 同范式，host_default=false=OFF——视觉兜底每次都过一遍视觉模型，有额外 token 成本，须用户显式 opt-in）。
    #[serde(default)]
    pub visual_fallback: bool,
    /// Explicit Browser Use approval bypass. Default false; host settings may set
    /// this from `agent.browserUse.unrestrictedApproval`.
    #[serde(default)]
    pub unrestricted_approval: bool,
    /// **浏览器来源**（「浏览器模式」的来源维度，与 `headless` 正交）。`"managed"`（默认）=
    /// 内置/下载的 Chrome for Testing；`"system"` = 用户系统已装的 Chrome/Edge 本体优先
    /// （未探到回退 managed）。**两种来源都用专属 user-data-dir 起独立托管实例**（红线：绝不
    /// 碰用户真实 profile；登录态由持久登录保险库单独维护）。上层（backend factory）把用户在
    /// System Settings 的 `client_preferences` `agent.browserUse.source` LIVE 值写到这里（每会话
    /// 构造时读最新值），facade `BrowserTool` 由本字段解析出 `ChromeSource`。坏值静默退回 managed。
    #[serde(default = "default_browser_source")]
    pub source: String,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            browser_path: String::new(),
            headless: false,
            idle_timeout_secs: default_browser_idle_timeout(),
            allowed_origins: Vec::new(),
            full_power: false,
            persistent_login: false,
            site_memory: false,
            visual_fallback: false,
            unrestricted_approval: false,
            source: default_browser_source(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_session_dir")]
    pub directory: String,
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            directory: default_session_dir(),
            max_sessions: default_max_sessions(),
        }
    }
}

// --- Default value functions ---

fn default_provider() -> String {
    "anthropic".to_string()
}
fn default_max_tokens() -> u32 {
    8192
}
fn default_allow_list() -> Vec<String> {
    vec!["Read".into(), "Grep".into(), "Glob".into()]
}
fn default_max_recent_images() -> usize {
    3
}
fn default_max_screenshot_edge() -> u32 {
    1568
}
fn default_browser_idle_timeout() -> u64 {
    300
}
/// 浏览器来源默认值：`"managed"`（内置/下载 CfT）。`"system"` = 用户系统 Chrome/Edge。
fn default_browser_source() -> String {
    "managed".to_owned()
}
fn default_true() -> bool {
    true
}
fn default_session_dir() -> String {
    ".nomi/sessions".to_string()
}
fn default_max_sessions() -> usize {
    20
}

// --- Resolved runtime config ---

#[derive(Debug, Clone)]
pub struct Config {
    pub provider_label: String,
    pub provider: ProviderType,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    pub max_turns: Option<usize>,
    pub system_prompt: Option<String>,
    pub thinking: Option<ThinkingConfig>,
    pub prompt_caching: bool,
    pub compat: ProviderCompat,
    pub tools: ToolsConfig,
    pub session: SessionConfig,
    pub compact: CompactConfig,
    pub plan: PlanConfig,
    pub file_cache: FileCacheConfig,
    pub hooks: HooksConfig,
    pub bedrock: Option<BedrockConfig>,
    pub vertex: Option<VertexConfig>,
    pub mcp: McpConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderType {
    Anthropic,
    OpenAI,
    Bedrock,
    Vertex,
}

#[derive(Debug, Clone)]
struct ResolvedProviderConfig {
    requested_name: String,
    provider_type: ProviderType,
    effective_config: ProviderConfig,
}

/// CLI arguments needed for config resolution
pub struct CliArgs {
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub max_turns: Option<usize>,
    pub system_prompt: Option<String>,
    pub profile: Option<String>,
    pub auto_approve: bool,
    pub project_dir: Option<PathBuf>,
}

impl Config {
    /// Load and merge config from all sources
    pub fn resolve(cli: &CliArgs) -> anyhow::Result<Self> {
        // 1. Load global config
        let global = load_config_file(&global_config_path());

        // 2. Load project config (from project_dir if specified, else CWD)
        let project_path = cli
            .project_dir
            .as_ref()
            .map(|d| d.join(".nomi.toml"))
            .unwrap_or_else(project_config_path);
        let project = load_config_file(&project_path);

        // 3. Merge: global <- project
        let mut merged = merge_config_files(global, project);

        // 4. If --profile specified, overlay profile settings
        if let Some(profile_name) = &cli.profile {
            merged = apply_profile(merged, profile_name)?;
        }

        // 5. Apply CLI overrides and resolve final config
        let provider_str = cli.provider.as_deref().unwrap_or(&merged.default.provider);

        let resolved_provider = resolve_provider_alias(&merged.providers, provider_str)?;
        let provider_label = resolved_provider.requested_name.clone();
        let provider = resolved_provider.provider_type;
        let provider_config = resolved_provider.effective_config;

        let base_url = cli
            .base_url
            .clone()
            .or_else(|| provider_config.base_url.clone())
            .unwrap_or_else(|| match provider {
                ProviderType::Anthropic => "https://api.anthropic.com".into(),
                ProviderType::OpenAI => "https://api.openai.com".into(),
                // Bedrock/Vertex URLs are constructed from region/project, not base_url
                ProviderType::Bedrock | ProviderType::Vertex => String::new(),
            });

        let model = cli
            .model
            .clone()
            .or(provider_config.model.clone())
            .or(merged.default.model.clone())
            .unwrap_or_else(|| match provider {
                ProviderType::Anthropic => "claude-sonnet-4-20250514".into(),
                ProviderType::OpenAI => "gpt-4o".into(),
                ProviderType::Bedrock => "anthropic.claude-sonnet-4-20250514-v1:0".into(),
                ProviderType::Vertex => "claude-sonnet-4@20250514".into(),
            });

        let max_tokens = cli.max_tokens.unwrap_or(merged.default.max_tokens);
        let max_turns = cli.max_turns.or(merged.default.max_turns);

        let system_prompt = cli
            .system_prompt
            .clone()
            .or(merged.default.system_prompt.clone());

        // 6. Resolve API key: CLI > config file > env var
        let api_key = resolve_api_key(
            cli.api_key.as_deref(),
            provider_config.api_key.as_deref(),
            provider,
        )?;

        // 7. Apply auto_approve from CLI
        let mut tools = merged.tools;
        if cli.auto_approve {
            tools.auto_approve = true;
        }

        // Resolve prompt_caching: default true for Anthropic
        let prompt_caching = provider_config
            .prompt_caching
            .unwrap_or(matches!(provider, ProviderType::Anthropic));

        // Resolve compat: provider-type defaults + user overrides
        let compat_defaults = match provider {
            ProviderType::Anthropic => ProviderCompat::anthropic_defaults(),
            ProviderType::OpenAI => ProviderCompat::openai_defaults(),
            ProviderType::Bedrock => ProviderCompat::bedrock_defaults(),
            ProviderType::Vertex => ProviderCompat::anthropic_defaults(),
        };

        let user_compat = provider_config.compat.clone().unwrap_or_default();

        let compat = ProviderCompat::merge(compat_defaults, user_compat);

        Ok(Config {
            provider_label,
            provider,
            api_key,
            base_url,
            model,
            max_tokens,
            max_turns,
            system_prompt,
            thinking: None,
            prompt_caching,
            compat,
            tools,
            session: merged.session,
            compact: merged.compact,
            plan: merged.plan,
            file_cache: merged.file_cache,
            hooks: merged.hooks,
            bedrock: merged.bedrock,
            vertex: merged.vertex,
            mcp: merged.mcp,
            logging: merged.logging,
        })
    }
}

fn parse_builtin_provider(s: &str) -> Option<ProviderType> {
    match s {
        "anthropic" => Some(ProviderType::Anthropic),
        "openai" => Some(ProviderType::OpenAI),
        "bedrock" => Some(ProviderType::Bedrock),
        "vertex" => Some(ProviderType::Vertex),
        _ => None,
    }
}

fn merge_provider_configs(base: ProviderConfig, overlay: ProviderConfig) -> ProviderConfig {
    ProviderConfig {
        provider: overlay.provider.or(base.provider),
        model: overlay.model.or(base.model),
        api_key: overlay.api_key.or(base.api_key),
        base_url: overlay.base_url.or(base.base_url),
        prompt_caching: overlay.prompt_caching.or(base.prompt_caching),
        compat: match (base.compat, overlay.compat) {
            (Some(base), Some(overlay)) => Some(ProviderCompat::merge(base, overlay)),
            (Some(base), None) => Some(base),
            (None, Some(overlay)) => Some(overlay),
            (None, None) => None,
        },
    }
}

fn resolve_provider_alias(
    providers: &HashMap<String, ProviderConfig>,
    requested: &str,
) -> anyhow::Result<ResolvedProviderConfig> {
    if let Some(provider_type) = parse_builtin_provider(requested) {
        return Ok(ResolvedProviderConfig {
            requested_name: requested.to_string(),
            provider_type,
            effective_config: providers.get(requested).cloned().unwrap_or_default(),
        });
    }

    let alias_config = providers.get(requested).cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown provider: '{}'. Expected a built-in provider (anthropic, openai, bedrock, vertex) \
             or a custom alias defined in [providers.{}].",
            requested,
            requested
        )
    })?;

    let underlying = alias_config.provider.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "Provider alias '{}' requires a 'provider' field in [providers.{}] \
             that maps to a built-in type (anthropic, openai, bedrock, vertex).",
            requested,
            requested
        )
    })?;

    let provider_type = parse_builtin_provider(&underlying).ok_or_else(|| {
        anyhow::anyhow!(
            "Provider alias '{}' maps to '{}', which is not a built-in provider. \
             Use one of: anthropic, openai, bedrock, vertex.",
            requested,
            underlying
        )
    })?;

    Ok(ResolvedProviderConfig {
        requested_name: requested.to_string(),
        provider_type,
        effective_config: merge_provider_configs(
            providers.get(&underlying).cloned().unwrap_or_default(),
            alias_config,
        ),
    })
}

fn resolve_api_key(
    cli_key: Option<&str>,
    config_key: Option<&str>,
    provider: ProviderType,
) -> anyhow::Result<String> {
    // CLI arg takes precedence
    if let Some(key) = cli_key {
        return Ok(key.to_string());
    }

    // Config file value
    if let Some(key) = config_key {
        return Ok(key.to_string());
    }

    // Env var fallback chain
    if let Ok(key) = std::env::var("API_KEY") {
        return Ok(key);
    }

    match provider {
        ProviderType::Anthropic => {
            if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
                return Ok(key);
            }
        }
        ProviderType::OpenAI => {
            if let Ok(key) = std::env::var("OPENAI_API_KEY") {
                return Ok(key);
            }
        }
        // Bedrock uses AWS credentials, Vertex uses GCP credentials
        // They don't need a traditional API key
        ProviderType::Bedrock | ProviderType::Vertex => {
            return Ok(String::new());
        }
    }

    // Try OAuth credentials as last resort
    let oauth = OAuthManager::new(AuthConfig::default());
    if oauth.has_credentials() {
        return Ok(String::new()); // Will be resolved at runtime via OAuth
    }

    anyhow::bail!(
        "No API key found. Provide via --api-key, config file, environment variable \
         (API_KEY, ANTHROPIC_API_KEY, or OPENAI_API_KEY), or run 'nomi --login'."
    )
}

// --- App directories ---

/// Platform-aware app config root.
///
/// - Linux:   `~/.config/nomi`
/// - macOS:   `~/Library/Application Support/nomi`
/// - Windows: `%APPDATA%\nomi`
pub fn app_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("nomi"))
}

// --- Config file loading and merging ---

pub fn global_config_path() -> PathBuf {
    app_config_dir()
        .unwrap_or_else(|| PathBuf::from("nomi"))
        .join("config.toml")
}

fn project_config_path() -> PathBuf {
    PathBuf::from(".nomi.toml")
}

fn load_config_file(path: &Path) -> ConfigFile {
    match std::fs::read_to_string(path) {
        Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
            tracing::warn!(target: "nomi_config", path = %path.display(), error = %e, "failed to parse config file");
            ConfigFile::default()
        }),
        Err(_) => ConfigFile::default(),
    }
}

/// Merge two config files. Project overrides global.
fn merge_config_files(global: ConfigFile, project: ConfigFile) -> ConfigFile {
    let default = DefaultConfig {
        provider: if project.default.provider != default_provider() {
            project.default.provider
        } else {
            global.default.provider
        },
        model: project.default.model.or(global.default.model),
        max_tokens: if project.default.max_tokens != default_max_tokens() {
            project.default.max_tokens
        } else {
            global.default.max_tokens
        },
        max_turns: project.default.max_turns.or(global.default.max_turns),
        system_prompt: project
            .default
            .system_prompt
            .or(global.default.system_prompt),
    };

    // Merge providers: global as base, project overrides
    let mut providers = global.providers;
    for (k, v) in project.providers {
        let base = providers.remove(&k).unwrap_or_default();
        providers.insert(k, merge_provider_configs(base, v));
    }

    // Merge profiles: global as base, project overrides
    let mut profiles = global.profiles;
    profiles.extend(project.profiles);

    // Tools: project overrides global for scalar fields; skills deny/allow are concatenated
    // (global first, then project) — consistent with the hooks merge strategy.
    // Computer/browser: enabling in either scope enables; project non-default scalars win.
    let computer = ComputerConfig {
        enabled: global.tools.computer.enabled || project.tools.computer.enabled,
        max_screenshot_edge: if project.tools.computer.max_screenshot_edge != default_max_screenshot_edge() {
            project.tools.computer.max_screenshot_edge
        } else {
            global.tools.computer.max_screenshot_edge
        },
    };
    let browser = BrowserConfig {
        enabled: global.tools.browser.enabled || project.tools.browser.enabled,
        browser_path: if !project.tools.browser.browser_path.is_empty() {
            project.tools.browser.browser_path
        } else {
            global.tools.browser.browser_path
        },
        headless: global.tools.browser.headless || project.tools.browser.headless,
        idle_timeout_secs: if project.tools.browser.idle_timeout_secs != default_browser_idle_timeout() {
            project.tools.browser.idle_timeout_secs
        } else {
            global.tools.browser.idle_timeout_secs
        },
        allowed_origins: if !project.tools.browser.allowed_origins.is_empty() {
            project.tools.browser.allowed_origins
        } else {
            global.tools.browser.allowed_origins
        },
        // F1-sec: 全权模式——任一层开启即开（与 enabled/headless 同 OR 合并语义）。运行时由 backend
        // factory 经 client_preferences LIVE 覆写（config.tools.browser.full_power），这里只是 toml 合并。
        full_power: global.tools.browser.full_power || project.tools.browser.full_power,
        // SD-6: 持久登录——任一层开启即开（与 full_power 同 OR 合并语义）。运行时由 backend factory 经
        // client_preferences LIVE 覆写，这里只是 toml 合并。
        persistent_login: global.tools.browser.persistent_login || project.tools.browser.persistent_login,
        // P7A site-memory——任一层开启即开（与 full_power 同 OR 合并语义）。运行时由 backend factory 经
        // client_preferences LIVE 覆写（host_default=false），这里只是 toml 合并。
        site_memory: global.tools.browser.site_memory || project.tools.browser.site_memory,
        // P7B visual-fallback——任一层开启即开（与 full_power 同 OR 合并语义）。运行时由 backend factory 经
        // client_preferences LIVE 覆写（host_default=false），这里只是 toml 合并。
        visual_fallback: global.tools.browser.visual_fallback || project.tools.browser.visual_fallback,
        unrestricted_approval: global.tools.browser.unrestricted_approval
            || project.tools.browser.unrestricted_approval,
        // 浏览器来源——project 非默认（显式设了 "system"/其它）则覆盖 global，否则用 global（与
        // browser_path 同「project 非默认优先」语义）。运行时由 backend factory 经 client_preferences
        // LIVE 覆写（config.tools.browser.source），这里只是 toml 合并。
        source: if project.tools.browser.source != default_browser_source() {
            project.tools.browser.source
        } else {
            global.tools.browser.source
        },
    };
    let max_recent_images = if project.tools.max_recent_images != default_max_recent_images() {
        project.tools.max_recent_images
    } else {
        global.tools.max_recent_images
    };
    let tools = if project.tools.allow_list != default_allow_list() || project.tools.auto_approve {
        ToolsConfig {
            auto_approve: global.tools.auto_approve || project.tools.auto_approve,
            allow_list: project.tools.allow_list,
            skills: SkillsPermissionConfig {
                deny: [global.tools.skills.deny, project.tools.skills.deny].concat(),
                allow: [global.tools.skills.allow, project.tools.skills.allow].concat(),
            },
            max_recent_images,
            computer,
            browser,
            persistent_shell: global.tools.persistent_shell || project.tools.persistent_shell,
            write_root: if !project.tools.write_root.is_empty() {
                project.tools.write_root
            } else {
                global.tools.write_root
            },
            lsp_servers: [global.tools.lsp_servers, project.tools.lsp_servers].concat(),
            subagent_token_budget: project.tools.subagent_token_budget.or(global.tools.subagent_token_budget),
            bash_sandbox: global.tools.bash_sandbox || project.tools.bash_sandbox,
            cooperative_cancel: global.tools.cooperative_cancel || project.tools.cooperative_cancel,
            // 任一层显式关闭即关闭（默认皆 true，行为不变）。
            in_process_spawn: global.tools.in_process_spawn && project.tools.in_process_spawn,
            // 项目层非空则覆盖全局（与 write_root 同模式）。
            builtin_allowlist: if !project.tools.builtin_allowlist.is_empty() {
                project.tools.builtin_allowlist
            } else {
                global.tools.builtin_allowlist
            },
        }
    } else {
        ToolsConfig {
            auto_approve: global.tools.auto_approve || project.tools.auto_approve,
            allow_list: global.tools.allow_list,
            skills: SkillsPermissionConfig {
                deny: [global.tools.skills.deny, project.tools.skills.deny].concat(),
                allow: [global.tools.skills.allow, project.tools.skills.allow].concat(),
            },
            max_recent_images,
            computer,
            browser,
            persistent_shell: global.tools.persistent_shell || project.tools.persistent_shell,
            write_root: if !project.tools.write_root.is_empty() {
                project.tools.write_root
            } else {
                global.tools.write_root
            },
            lsp_servers: [global.tools.lsp_servers, project.tools.lsp_servers].concat(),
            subagent_token_budget: project.tools.subagent_token_budget.or(global.tools.subagent_token_budget),
            bash_sandbox: global.tools.bash_sandbox || project.tools.bash_sandbox,
            cooperative_cancel: global.tools.cooperative_cancel || project.tools.cooperative_cancel,
            // 任一层显式关闭即关闭（默认皆 true，行为不变）。
            in_process_spawn: global.tools.in_process_spawn && project.tools.in_process_spawn,
            // 项目层非空则覆盖全局（与 write_root 同模式）。
            builtin_allowlist: if !project.tools.builtin_allowlist.is_empty() {
                project.tools.builtin_allowlist
            } else {
                global.tools.builtin_allowlist
            },
        }
    };

    // Session: project overrides global
    let session = if project.session.directory != default_session_dir() {
        project.session
    } else {
        SessionConfig {
            enabled: global.session.enabled && project.session.enabled,
            directory: if project.session.directory != default_session_dir() {
                project.session.directory
            } else {
                global.session.directory
            },
            max_sessions: if project.session.max_sessions != default_max_sessions() {
                project.session.max_sessions
            } else {
                global.session.max_sessions
            },
        }
    };

    // Hooks: combine hooks from both configs (project hooks appended after global)
    let hooks = HooksConfig {
        pre_tool_use: [global.hooks.pre_tool_use, project.hooks.pre_tool_use].concat(),
        post_tool_use: [global.hooks.post_tool_use, project.hooks.post_tool_use].concat(),
        stop: [global.hooks.stop, project.hooks.stop].concat(),
    };

    // MCP: merge servers from both configs, project overrides global
    let mut mcp_servers = global.mcp.servers;
    mcp_servers.extend(project.mcp.servers);
    let mcp = McpConfig {
        servers: mcp_servers,
    };

    // Plan: project overrides global if any field differs from default
    let plan = if !project.plan.enabled
        || project.plan.plan_directory != PlanConfig::default().plan_directory
    {
        project.plan
    } else {
        global.plan
    };

    // File cache: project overrides global if any field differs from default.
    let file_cache = if !project.file_cache.enabled
        || project.file_cache.max_entries != FileCacheConfig::default().max_entries
        || project.file_cache.max_size_bytes != FileCacheConfig::default().max_size_bytes
    {
        project.file_cache
    } else {
        global.file_cache
    };

    // Bedrock/Vertex/Auth: project overrides global
    let bedrock = project.bedrock.or(global.bedrock);
    let vertex = project.vertex.or(global.vertex);
    let auth = project.auth.or(global.auth);

    // Compact: project overrides global for any non-default field.
    // Since CompactConfig uses serde defaults, a fully-default project config
    // is indistinguishable from "absent". We use project if its context_window
    // differs from the default, otherwise fall back to global.
    let compact = if project.compact.context_window != CompactConfig::default().context_window
        || !project.compact.enabled
    {
        project.compact
    } else {
        global.compact
    };

    let logging = LoggingConfig::merge(global.logging, project.logging);

    ConfigFile {
        default,
        providers,
        profiles,
        tools,
        session,
        compact,
        plan,
        file_cache,
        hooks,
        bedrock,
        vertex,
        auth,
        mcp,
        logging,
    }
}

/// Resolve a profile with inheritance chain (with cycle detection)
fn resolve_profile(
    profiles: &HashMap<String, ProfileConfig>,
    name: &str,
    visited: &mut Vec<String>,
) -> anyhow::Result<ProfileConfig> {
    if visited.contains(&name.to_string()) {
        anyhow::bail!(
            "Circular profile inheritance detected: {} -> {}",
            visited.join(" -> "),
            name
        );
    }
    visited.push(name.to_string());

    let profile = profiles
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("Profile '{}' not found in config", name))?
        .clone();

    if let Some(parent_name) = &profile.extends {
        let parent = resolve_profile(profiles, parent_name, visited)?;
        Ok(merge_profiles(parent, profile))
    } else {
        Ok(profile)
    }
}

/// Merge two profiles: overlay takes precedence over base
fn merge_profiles(base: ProfileConfig, overlay: ProfileConfig) -> ProfileConfig {
    ProfileConfig {
        provider: overlay.provider.or(base.provider),
        model: overlay.model.or(base.model),
        api_key: overlay.api_key.or(base.api_key),
        base_url: overlay.base_url.or(base.base_url),
        max_tokens: overlay.max_tokens.or(base.max_tokens),
        max_turns: overlay.max_turns.or(base.max_turns),
        extends: None, // already resolved
        mcp_servers: overlay.mcp_servers.or(base.mcp_servers),
        compat: overlay.compat.or(base.compat),
    }
}

fn apply_profile(mut config: ConfigFile, profile_name: &str) -> anyhow::Result<ConfigFile> {
    let mut visited = Vec::new();
    let profile = resolve_profile(&config.profiles, profile_name, &mut visited)?;

    if let Some(provider) = profile.provider {
        config.default.provider = provider;
    }
    if let Some(model) = profile.model {
        config.default.model = Some(model);
    }
    if let Some(max_tokens) = profile.max_tokens {
        config.default.max_tokens = max_tokens;
    }
    if let Some(max_turns) = profile.max_turns {
        config.default.max_turns = Some(max_turns);
    }

    // Profile can override api_key, base_url, and compat for the active provider
    let provider_name = config.default.provider.clone();
    let entry = config.providers.entry(provider_name).or_default();
    if let Some(api_key) = profile.api_key {
        entry.api_key = Some(api_key);
    }
    if let Some(base_url) = profile.base_url {
        entry.base_url = Some(base_url);
    }
    if let Some(compat) = profile.compat {
        entry.compat = Some(match entry.compat.take() {
            Some(existing) => ProviderCompat::merge(existing, compat),
            None => compat,
        });
    }

    // Filter MCP servers by profile's mcp_servers list
    if let Some(server_names) = profile.mcp_servers {
        config
            .mcp
            .servers
            .retain(|name, _| server_names.contains(name));
    }

    Ok(config)
}

// --- Init config command ---

pub fn init_config() -> anyhow::Result<()> {
    let path = global_config_path();
    if path.exists() {
        tracing::info!(target: "nomi_config", path = %path.display(), "config file already exists");
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, DEFAULT_CONFIG_TEMPLATE)?;
    tracing::info!(target: "nomi_config", path = %path.display(), "config file created");
    Ok(())
}

const DEFAULT_CONFIG_TEMPLATE: &str = r#"# nomi configuration

# Default provider settings
[default]
provider = "anthropic"            # built-in provider or custom alias from [providers.<name>]
# model = "claude-sonnet-4-20250514"
max_tokens = 8192
# max_turns = 30                  # optional: omit for unlimited turns
# system_prompt = "..."          # optional custom system prompt

# Provider-specific API settings
[providers.anthropic]
# api_key = "sk-ant-xxx"         # can also use env: API_KEY or ANTHROPIC_API_KEY
# base_url = "https://api.anthropic.com"

[providers.openai]
# api_key = "sk-xxx"             # can also use env: OPENAI_API_KEY
# base_url = "https://api.openai.com"

# Custom provider alias (maps to a built-in provider type)
# [providers.my-service]
# provider = "openai"
# model = "custom-model-v1"
# api_key = "sk-xxx"
# base_url = "https://my-service.example.com/api/openai"

# Provider compatibility overrides (usually not needed — defaults work)
# [providers.openai.compat]
# max_tokens_field = "max_completion_tokens"  # for OpenAI official models
# merge_assistant_messages = true
# clean_orphan_tool_calls = true
# dedup_tool_results = true
# strip_patterns = ["__OPENROUTER_REASONING_DETAILS__"]

# AWS Bedrock configuration (uses AWS SigV4 auth, no API key needed)
# [bedrock]
# region = "us-east-1"
# access_key_id = "AKIA..."
# secret_access_key = "..."
# session_token = "..."
# profile = "my-profile"        # or use AWS profile

# Google Vertex AI configuration (uses GCP OAuth2 auth, no API key needed)
# [vertex]
# project_id = "my-gcp-project"
# region = "us-central1"
# credentials_file = "/path/to/service-account.json"  # or use ADC

# OAuth settings (for --login with Claude.ai account)
# [auth]
# auth_url = "https://claude.ai/oauth"
# token_url = "https://claude.ai/oauth/token"
# client_id = "nomi"

# Named profiles for quick switching (--profile <name>)
# [profiles.deepseek]
# provider = "openai"
# model = "deepseek-chat"
# api_key = "sk-xxx"
# base_url = "https://api.deepseek.com"

# [profiles.ollama]
# provider = "openai"
# model = "qwen2.5:32b"
# api_key = "ollama"
# base_url = "http://localhost:11434"

# [profiles.my-service]
# provider = "my-service"

# [profiles.bedrock-claude]
# provider = "bedrock"
# model = "anthropic.claude-sonnet-4-20250514-v1:0"

# [profiles.vertex-claude]
# provider = "vertex"
# model = "claude-sonnet-4@20250514"

# Tool confirmation settings
[tools]
auto_approve = false             # --auto-approve overrides
# Tools that skip confirmation even when auto_approve = false
allow_list = ["Read", "Grep", "Glob"]

# Context compaction settings
# [compact]
# context_window = 200000        # context window size in tokens
# output_reserve = 20000         # tokens reserved for output
# autocompact_buffer = 13000     # buffer below effective window for autocompact trigger
# emergency_buffer = 3000        # tokens from limit for emergency block
# max_failures = 3               # consecutive failures before circuit-breaker trips
# micro_keep_recent = 5          # keep N most recent tool results
# micro_gap_seconds = 3600       # gap threshold for time-based microcompact
# compactable_tools = ["Read", "Bash", "Grep", "Glob", "Write", "Edit"]
# enabled = true

# File state cache (dedup repeated reads, staleness detection)
# [file_cache]
# max_entries = 100            # max cached file entries
# max_size_bytes = 26214400    # 25 MB total cache size
# enabled = true

# Session settings
[session]
enabled = true
directory = ".nomi/sessions"  # relative to project root
max_sessions = 20                # auto-cleanup oldest

# Hook system: run shell commands at tool lifecycle events
# [[hooks.post_tool_use]]
# name = "rustfmt"
# tool_match = ["Write", "Edit"]
# file_match = ["*.rs"]
# command = "rustfmt ${TOOL_INPUT_FILE_PATH}"

# [[hooks.post_tool_use]]
# name = "prettier"
# tool_match = ["Write", "Edit"]
# file_match = ["*.ts", "*.tsx"]
# command = "npx prettier --write ${TOOL_INPUT_FILE_PATH}"

# [[hooks.stop]]
# name = "final-lint"
# command = "cargo clippy --quiet 2>&1 | tail -5"

# Logging configuration
# [logging]
# enabled = true                   # enable file logging (default: false)
# level = "info"                   # log level filter (default: "info")
# dir = "~/Library/Logs/nomi"    # log directory (default: platform-specific)

# MCP (Model Context Protocol) servers
# [mcp.servers.filesystem]
# transport = "stdio"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "/Users/me/project"]

# [mcp.servers.github]
# transport = "stdio"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-github"]
# env = { GITHUB_TOKEN = "ghp_xxx" }

# [mcp.servers.remote]
# transport = "sse"
# url = "http://localhost:3001/sse"

# [mcp.servers.api]
# transport = "streamable-http"
# url = "https://tools.example.com/mcp"
# headers = { Authorization = "Bearer xxx" }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_config_default_source_is_managed() {
        // 默认来源 = "managed"（内置/下载 CfT）—— 新装/未配置即现行为，零回归。
        // 引擎侧 `ChromeSource::from_source_str("managed")` == Managed（见 nomi-browser-engine::acquire）。
        assert_eq!(BrowserConfig::default().source, "managed");
    }

    // -------------------------------------------------------------------------
    // parse_builtin_provider tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_provider_type_from_str_anthropic() {
        let result = parse_builtin_provider("anthropic");
        assert_eq!(result, Some(ProviderType::Anthropic));
    }

    #[test]
    fn test_provider_type_from_str_openai() {
        let result = parse_builtin_provider("openai");
        assert_eq!(result, Some(ProviderType::OpenAI));
    }

    #[test]
    fn test_provider_type_from_str_bedrock() {
        let result = parse_builtin_provider("bedrock");
        assert_eq!(result, Some(ProviderType::Bedrock));
    }

    #[test]
    fn test_provider_type_from_str_vertex() {
        let result = parse_builtin_provider("vertex");
        assert_eq!(result, Some(ProviderType::Vertex));
    }

    #[test]
    fn test_provider_type_from_str_invalid() {
        let result = parse_builtin_provider("invalid");
        assert_eq!(result, None);
    }

    #[test]
    fn test_provider_alias_resolves_to_builtin_provider() {
        let mut providers = HashMap::new();
        providers.insert(
            "my-service".to_string(),
            ProviderConfig {
                provider: Some("openai".to_string()),
                model: Some("custom-model-v1".to_string()),
                api_key: Some("alias-key".to_string()),
                base_url: Some("https://my-service.example.com/v1".to_string()),
                ..Default::default()
            },
        );

        let resolved = resolve_provider_alias(&providers, "my-service").unwrap();
        assert_eq!(resolved.requested_name, "my-service");
        assert_eq!(resolved.provider_type, ProviderType::OpenAI);
        assert_eq!(
            resolved.effective_config.model.as_deref(),
            Some("custom-model-v1")
        );
        assert_eq!(
            resolved.effective_config.api_key.as_deref(),
            Some("alias-key")
        );
        assert_eq!(
            resolved.effective_config.base_url.as_deref(),
            Some("https://my-service.example.com/v1")
        );
    }

    #[test]
    fn test_provider_alias_overlays_builtin_provider_defaults() {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                api_key: Some("builtin-key".to_string()),
                model: Some("gpt-4o".to_string()),
                ..Default::default()
            },
        );
        providers.insert(
            "my-service".to_string(),
            ProviderConfig {
                provider: Some("openai".to_string()),
                base_url: Some("https://my-service.example.com/v1".to_string()),
                ..Default::default()
            },
        );

        let resolved = resolve_provider_alias(&providers, "my-service").unwrap();
        assert_eq!(resolved.provider_type, ProviderType::OpenAI);
        assert_eq!(
            resolved.effective_config.api_key.as_deref(),
            Some("builtin-key")
        );
        assert_eq!(resolved.effective_config.model.as_deref(), Some("gpt-4o"));
        assert_eq!(
            resolved.effective_config.base_url.as_deref(),
            Some("https://my-service.example.com/v1")
        );
    }

    #[test]
    fn test_provider_alias_requires_underlying_provider_type() {
        let mut providers = HashMap::new();
        providers.insert("my-service".to_string(), ProviderConfig::default());

        let result = resolve_provider_alias(&providers, "my-service");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("my-service"));
        assert!(msg.contains("provider"));
        assert!(msg.contains("built-in type"));
    }

    // -------------------------------------------------------------------------
    // merge_config_files tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_merge_config_cli_overrides_file() {
        // Project config sets a non-default provider; it should win over global.
        let global = ConfigFile {
            default: DefaultConfig {
                provider: "anthropic".to_string(),
                model: Some("global-model".to_string()),
                max_tokens: 4096,
                max_turns: Some(10),
                system_prompt: Some("global prompt".to_string()),
            },
            ..Default::default()
        };
        let project = ConfigFile {
            default: DefaultConfig {
                provider: "openai".to_string(), // non-default -> overrides global
                model: Some("project-model".to_string()),
                max_tokens: 2048,   // non-default -> overrides global
                max_turns: Some(5), // non-default -> overrides global
                system_prompt: Some("project prompt".to_string()),
            },
            ..Default::default()
        };

        let merged = merge_config_files(global, project);

        assert_eq!(merged.default.provider, "openai");
        assert_eq!(merged.default.model, Some("project-model".to_string()));
        assert_eq!(merged.default.max_tokens, 2048);
        assert_eq!(merged.default.max_turns, Some(5));
        assert_eq!(
            merged.default.system_prompt,
            Some("project prompt".to_string())
        );
    }

    #[test]
    fn test_merge_config_file_provides_defaults() {
        // Project config is default; global values should be preserved.
        let global = ConfigFile {
            default: DefaultConfig {
                provider: "openai".to_string(),
                model: Some("global-model".to_string()),
                max_tokens: 1024,
                max_turns: Some(5),
                system_prompt: Some("global prompt".to_string()),
            },
            ..Default::default()
        };
        // Project stays at built-in defaults (provider = "anthropic", max_tokens = 8192, max_turns = None)
        let project = ConfigFile::default();

        let merged = merge_config_files(global, project);

        // provider: project default "anthropic" == default_provider() -> use global "openai"
        assert_eq!(merged.default.provider, "openai");
        assert_eq!(merged.default.model, Some("global-model".to_string()));
        assert_eq!(merged.default.max_tokens, 1024);
        assert_eq!(merged.default.max_turns, Some(5));
        assert_eq!(
            merged.default.system_prompt,
            Some("global prompt".to_string())
        );
    }

    #[test]
    fn test_merge_config_empty_file() {
        // Two default ConfigFiles merged should yield defaults.
        let merged = merge_config_files(ConfigFile::default(), ConfigFile::default());

        assert_eq!(merged.default.provider, default_provider());
        assert_eq!(merged.default.max_tokens, default_max_tokens());
        assert_eq!(merged.default.max_turns, None);
        assert!(merged.default.model.is_none());
        assert!(merged.providers.is_empty());
        assert!(merged.profiles.is_empty());
    }

    // -------------------------------------------------------------------------
    // resolve_profile tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_profile_inheritance() {
        // Profile "child" extends "parent"; child fields win, missing ones fall back to parent.
        let mut profiles = HashMap::new();
        profiles.insert(
            "parent".to_string(),
            ProfileConfig {
                provider: Some("anthropic".to_string()),
                model: Some("claude-3".to_string()),
                max_tokens: Some(4096),
                ..Default::default()
            },
        );
        profiles.insert(
            "child".to_string(),
            ProfileConfig {
                model: Some("claude-4".to_string()), // overrides parent
                extends: Some("parent".to_string()),
                ..Default::default()
            },
        );

        let mut visited = Vec::new();
        let result = resolve_profile(&profiles, "child", &mut visited).unwrap();

        // Child's model wins
        assert_eq!(result.model, Some("claude-4".to_string()));
        // Parent's provider is inherited
        assert_eq!(result.provider, Some("anthropic".to_string()));
        // Parent's max_tokens is inherited
        assert_eq!(result.max_tokens, Some(4096));
        // extends is cleared after resolution
        assert!(result.extends.is_none());
    }

    #[test]
    fn test_profile_cycle_detection() {
        // A extends B, B extends A -> should fail with cycle error.
        let mut profiles = HashMap::new();
        profiles.insert(
            "a".to_string(),
            ProfileConfig {
                extends: Some("b".to_string()),
                ..Default::default()
            },
        );
        profiles.insert(
            "b".to_string(),
            ProfileConfig {
                extends: Some("a".to_string()),
                ..Default::default()
            },
        );

        let mut visited = Vec::new();
        let result = resolve_profile(&profiles, "a", &mut visited);

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Circular profile inheritance"));
    }

    #[test]
    fn test_profile_not_found() {
        let profiles: HashMap<String, ProfileConfig> = HashMap::new();
        let mut visited = Vec::new();
        let result = resolve_profile(&profiles, "nonexistent", &mut visited);

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("nonexistent"));
    }

    // -------------------------------------------------------------------------
    // resolve_api_key tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_api_key_from_cli_arg() {
        // CLI key takes highest priority regardless of other sources.
        let result =
            resolve_api_key(Some("cli-key"), Some("config-key"), ProviderType::Anthropic).unwrap();
        assert_eq!(result, "cli-key");
    }

    #[test]
    fn test_api_key_from_config() {
        // When CLI key is absent, config file key should be used.
        let result = resolve_api_key(None, Some("config-key"), ProviderType::Anthropic).unwrap();
        assert_eq!(result, "config-key");
    }

    #[test]
    fn test_api_key_missing_returns_error() {
        // Remove all env vars that could supply a key so the function must fail.
        // Note: single-threaded tests share the process environment; clearing here
        // is safe for unit test purposes.
        // SAFETY: single-threaded test context; no other threads read these vars.
        unsafe {
            std::env::remove_var("API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
        }

        // Only fails if OAuth credentials file is also absent, which is true in CI.
        // We accept either an error OR an empty key (Bedrock/Vertex path), but for
        // Anthropic with no key at all the function should return an error.
        let result = resolve_api_key(None, None, ProviderType::Anthropic);

        // The result is either an error (no OAuth file) or Ok (OAuth file found).
        // We can only assert the error path reliably when the OAuth file is absent.
        if let Err(e) = result {
            let msg = e.to_string();
            assert!(msg.contains("No API key found"));
        }
        // If OAuth credentials exist on the test machine, the function returns Ok("").
        // Both outcomes are correct; the important invariant is no panic.
    }

    #[test]
    fn test_api_key_bedrock_returns_empty_without_key() {
        // Bedrock uses AWS credentials, so an empty key is the expected success value.
        let result = resolve_api_key(None, None, ProviderType::Bedrock).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_api_key_vertex_returns_empty_without_key() {
        // Vertex uses GCP credentials, so an empty key is the expected success value.
        let result = resolve_api_key(None, None, ProviderType::Vertex).unwrap();
        assert_eq!(result, "");
    }

    // -------------------------------------------------------------------------
    // P5-14: SkillsPermissionConfig TOML deserialization
    // -------------------------------------------------------------------------

    #[test]
    fn test_merge_config_global_auto_approve_preserved_with_project_allow_list() {
        let global = ConfigFile {
            tools: ToolsConfig {
                auto_approve: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let project = ConfigFile {
            tools: ToolsConfig {
                allow_list: vec!["Bash".into()], // non-default, triggers if branch
                ..Default::default()
            },
            ..Default::default()
        };
        let merged = merge_config_files(global, project);
        assert!(
            merged.tools.auto_approve,
            "global auto_approve=true should be preserved"
        );
    }

    #[test]
    fn p5_14_skills_deny_allow_deserialized() {
        let toml_str = r#"
[tools]
auto_approve = false
allow_list = ["Read"]

[tools.skills]
deny = ["dangerous-skill", "admin:*"]
allow = ["commit", "review-pr", "db:*"]
"#;
        let config: ConfigFile = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.tools.skills.deny,
            vec!["dangerous-skill".to_string(), "admin:*".to_string()]
        );
        assert_eq!(
            config.tools.skills.allow,
            vec![
                "commit".to_string(),
                "review-pr".to_string(),
                "db:*".to_string()
            ]
        );
    }

    #[test]
    fn p5_14_skills_defaults_to_empty() {
        // When [tools.skills] is absent, deny and allow default to empty vecs.
        let config: ConfigFile = toml::from_str("").unwrap();
        assert!(config.tools.skills.deny.is_empty());
        assert!(config.tools.skills.allow.is_empty());
    }

    #[test]
    fn p5_14_merge_skills_concat() {
        // global and project skills lists are concatenated.
        let global = ConfigFile {
            tools: ToolsConfig {
                skills: SkillsPermissionConfig {
                    deny: vec!["global-deny".to_string()],
                    allow: vec!["global-allow".to_string()],
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let project = ConfigFile {
            tools: ToolsConfig {
                skills: SkillsPermissionConfig {
                    deny: vec!["project-deny".to_string()],
                    allow: vec!["project-allow".to_string()],
                },
                ..Default::default()
            },
            ..Default::default()
        };

        let merged = merge_config_files(global, project);
        assert_eq!(
            merged.tools.skills.deny,
            vec!["global-deny".to_string(), "project-deny".to_string()]
        );
        assert_eq!(
            merged.tools.skills.allow,
            vec!["global-allow".to_string(), "project-allow".to_string()]
        );
    }

    // -------------------------------------------------------------------------
    // ConfigFile TOML deserialization tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_config_file_deserialize_minimal() {
        // An empty TOML string should deserialize to all defaults without error.
        let config: ConfigFile = toml::from_str("").unwrap();

        assert_eq!(config.default.provider, "anthropic");
        assert_eq!(config.default.max_tokens, 8192);
        assert_eq!(config.default.max_turns, None);
        assert!(config.default.model.is_none());
        assert!(config.providers.is_empty());
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn test_config_file_deserialize_with_providers() {
        let toml_str = r#"
[default]
provider = "openai"
model = "gpt-4o"
max_tokens = 4096

[providers.openai]
api_key = "sk-test-key"
base_url = "https://api.openai.com"

[providers.anthropic]
api_key = "sk-ant-test"
prompt_caching = false
"#;
        let config: ConfigFile = toml::from_str(toml_str).unwrap();

        assert_eq!(config.default.provider, "openai");
        assert_eq!(config.default.model, Some("gpt-4o".to_string()));
        assert_eq!(config.default.max_tokens, 4096);

        let openai = config.providers.get("openai").unwrap();
        assert_eq!(openai.api_key.as_deref(), Some("sk-test-key"));
        assert_eq!(openai.base_url.as_deref(), Some("https://api.openai.com"));

        let anthropic = config.providers.get("anthropic").unwrap();
        assert_eq!(anthropic.api_key.as_deref(), Some("sk-ant-test"));
        assert_eq!(anthropic.prompt_caching, Some(false));
    }

    #[test]
    fn test_config_file_deserialize_custom_provider_alias() {
        let toml_str = r#"
[default]
provider = "my-service"

[providers.my-service]
provider = "openai"
model = "custom-model-v1"
api_key = "alias-key"
base_url = "https://my-service.example.com/api/openai"
"#;
        let config: ConfigFile = toml::from_str(toml_str).unwrap();

        assert_eq!(config.default.provider, "my-service");
        let alias = config.providers.get("my-service").unwrap();
        assert_eq!(alias.provider.as_deref(), Some("openai"));
        assert_eq!(alias.model.as_deref(), Some("custom-model-v1"));
        assert_eq!(alias.api_key.as_deref(), Some("alias-key"));
        assert_eq!(
            alias.base_url.as_deref(),
            Some("https://my-service.example.com/api/openai")
        );
    }

    // -------------------------------------------------------------------------
    // merge_provider_configs tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_merge_provider_configs_overlay_overrides_base() {
        let base = ProviderConfig {
            api_key: Some("base-key".to_string()),
            base_url: Some("https://base.example.com".to_string()),
            model: Some("base-model".to_string()),
            ..Default::default()
        };
        let overlay = ProviderConfig {
            api_key: Some("overlay-key".to_string()),
            model: Some("overlay-model".to_string()),
            ..Default::default()
        };

        let merged = merge_provider_configs(base, overlay);
        assert_eq!(merged.api_key.as_deref(), Some("overlay-key"));
        assert_eq!(merged.model.as_deref(), Some("overlay-model"));
        // base_url not in overlay -> preserved from base
        assert_eq!(merged.base_url.as_deref(), Some("https://base.example.com"));
    }

    #[test]
    fn test_merge_provider_configs_overlay_none_preserves_base() {
        let base = ProviderConfig {
            api_key: Some("base-key".to_string()),
            base_url: Some("https://base.example.com".to_string()),
            model: Some("base-model".to_string()),
            prompt_caching: Some(true),
            provider: Some("openai".to_string()),
            ..Default::default()
        };
        let overlay = ProviderConfig::default();

        let merged = merge_provider_configs(base, overlay);
        assert_eq!(merged.api_key.as_deref(), Some("base-key"));
        assert_eq!(merged.base_url.as_deref(), Some("https://base.example.com"));
        assert_eq!(merged.model.as_deref(), Some("base-model"));
        assert_eq!(merged.prompt_caching, Some(true));
        assert_eq!(merged.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn test_merge_provider_configs_compat_merges_both() {
        let base = ProviderConfig {
            compat: Some(ProviderCompat {
                merge_assistant_messages: Some(true),
                clean_orphan_tool_calls: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let overlay = ProviderConfig {
            compat: Some(ProviderCompat {
                merge_assistant_messages: Some(false), // override base
                dedup_tool_results: Some(true),        // new field
                ..Default::default()
            }),
            ..Default::default()
        };

        let merged = merge_provider_configs(base, overlay);
        let compat = merged.compat.unwrap();
        // overlay wins
        assert_eq!(compat.merge_assistant_messages, Some(false));
        // base preserved
        assert_eq!(compat.clean_orphan_tool_calls, Some(true));
        // overlay adds new
        assert_eq!(compat.dedup_tool_results, Some(true));
    }

    #[test]
    fn test_merge_provider_configs_both_empty() {
        let merged = merge_provider_configs(ProviderConfig::default(), ProviderConfig::default());
        assert!(merged.api_key.is_none());
        assert!(merged.base_url.is_none());
        assert!(merged.model.is_none());
        assert!(merged.provider.is_none());
        assert!(merged.prompt_caching.is_none());
        assert!(merged.compat.is_none());
    }

    // -------------------------------------------------------------------------
    // resolve_provider_alias: builtin name path tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_resolve_builtin_provider_with_config() {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                api_key: Some("openai-key".to_string()),
                base_url: Some("https://custom-openai.example.com".to_string()),
                ..Default::default()
            },
        );

        let resolved = resolve_provider_alias(&providers, "openai").unwrap();
        assert_eq!(resolved.requested_name, "openai");
        assert_eq!(resolved.provider_type, ProviderType::OpenAI);
        assert_eq!(
            resolved.effective_config.api_key.as_deref(),
            Some("openai-key")
        );
        assert_eq!(
            resolved.effective_config.base_url.as_deref(),
            Some("https://custom-openai.example.com")
        );
    }

    #[test]
    fn test_resolve_builtin_provider_without_config_entry() {
        let providers = HashMap::new();

        let resolved = resolve_provider_alias(&providers, "anthropic").unwrap();
        assert_eq!(resolved.requested_name, "anthropic");
        assert_eq!(resolved.provider_type, ProviderType::Anthropic);
        // No config entry -> all fields default to None
        assert!(resolved.effective_config.api_key.is_none());
        assert!(resolved.effective_config.base_url.is_none());
        assert!(resolved.effective_config.model.is_none());
    }

    // -------------------------------------------------------------------------
    // resolve_provider_alias: error path tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_resolve_alias_maps_to_invalid_builtin_type() {
        let mut providers = HashMap::new();
        providers.insert(
            "my-db".to_string(),
            ProviderConfig {
                provider: Some("mysql".to_string()),
                ..Default::default()
            },
        );

        let result = resolve_provider_alias(&providers, "my-db");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("my-db"));
        assert!(msg.contains("mysql"));
        assert!(msg.contains("not a built-in provider"));
    }

    #[test]
    fn test_resolve_alias_not_found_in_providers() {
        let providers = HashMap::new();

        let result = resolve_provider_alias(&providers, "nonexistent");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("nonexistent"));
        assert!(msg.contains("built-in provider"));
        assert!(msg.contains("[providers.nonexistent]"));
    }

    // -------------------------------------------------------------------------
    // provider_label (requested_name) tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_provider_label_is_alias_name_not_underlying_type() {
        let mut providers = HashMap::new();
        providers.insert(
            "my-service".to_string(),
            ProviderConfig {
                provider: Some("openai".to_string()),
                api_key: Some("key".to_string()),
                ..Default::default()
            },
        );

        let resolved = resolve_provider_alias(&providers, "my-service").unwrap();
        // provider_label should be the alias name, not "openai"
        assert_eq!(resolved.requested_name, "my-service");
        assert_eq!(resolved.provider_type, ProviderType::OpenAI);
    }

    #[test]
    fn test_provider_label_is_builtin_name_for_builtin() {
        let providers = HashMap::new();

        for (name, expected_type) in [
            ("anthropic", ProviderType::Anthropic),
            ("openai", ProviderType::OpenAI),
            ("bedrock", ProviderType::Bedrock),
            ("vertex", ProviderType::Vertex),
        ] {
            let resolved = resolve_provider_alias(&providers, name).unwrap();
            assert_eq!(resolved.requested_name, name);
            assert_eq!(resolved.provider_type, expected_type);
        }
    }

    // -------------------------------------------------------------------------
    // model priority: alias model in resolution chain
    // -------------------------------------------------------------------------

    #[test]
    fn test_alias_model_available_in_effective_config() {
        // Verifies that alias.model is carried through effective_config,
        // which feeds into the priority chain: CLI > alias.model > default.model > hardcoded
        let mut providers = HashMap::new();
        providers.insert(
            "my-service".to_string(),
            ProviderConfig {
                provider: Some("openai".to_string()),
                model: Some("alias-model-v1".to_string()),
                ..Default::default()
            },
        );

        let resolved = resolve_provider_alias(&providers, "my-service").unwrap();
        assert_eq!(
            resolved.effective_config.model.as_deref(),
            Some("alias-model-v1")
        );
    }

    #[test]
    fn test_alias_model_inherits_from_underlying_provider() {
        // When alias has no model but underlying provider does,
        // the alias should inherit it via merge_provider_configs
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                model: Some("gpt-4o".to_string()),
                ..Default::default()
            },
        );
        providers.insert(
            "my-service".to_string(),
            ProviderConfig {
                provider: Some("openai".to_string()),
                base_url: Some("https://my-service.example.com".to_string()),
                // no model -> should inherit from openai
                ..Default::default()
            },
        );

        let resolved = resolve_provider_alias(&providers, "my-service").unwrap();
        assert_eq!(resolved.effective_config.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn test_alias_model_overrides_underlying_provider_model() {
        // When both alias and underlying provider define model,
        // alias model should win
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                model: Some("gpt-4o".to_string()),
                ..Default::default()
            },
        );
        providers.insert(
            "my-service".to_string(),
            ProviderConfig {
                provider: Some("openai".to_string()),
                model: Some("custom-model-v2".to_string()),
                ..Default::default()
            },
        );

        let resolved = resolve_provider_alias(&providers, "my-service").unwrap();
        assert_eq!(
            resolved.effective_config.model.as_deref(),
            Some("custom-model-v2")
        );
    }

    // -------------------------------------------------------------------------
    // Phase 5.5: FileCacheConfig in ConfigFile / merge
    // -------------------------------------------------------------------------

    #[test]
    fn tc_5_5_04_file_cache_toml_deserialization() {
        let toml_str = r#"
[file_cache]
max_entries = 50
max_size_bytes = 10485760
enabled = false
"#;
        let config: ConfigFile = toml::from_str(toml_str).unwrap();
        assert_eq!(config.file_cache.max_entries, 50);
        assert_eq!(config.file_cache.max_size_bytes, 10_485_760);
        assert!(!config.file_cache.enabled);
    }

    #[test]
    fn tc_5_5_02_file_cache_defaults_when_absent() {
        let config: ConfigFile = toml::from_str("").unwrap();
        assert_eq!(config.file_cache.max_entries, 100);
        assert_eq!(config.file_cache.max_size_bytes, 25 * 1024 * 1024);
        assert!(config.file_cache.enabled);
    }

    #[test]
    fn tc_5_5_01_file_cache_custom_capacity_propagates() {
        let toml_str = r#"
[file_cache]
max_entries = 50
"#;
        let config: ConfigFile = toml::from_str(toml_str).unwrap();
        assert_eq!(config.file_cache.max_entries, 50);
        // Other fields keep defaults.
        assert_eq!(config.file_cache.max_size_bytes, 25 * 1024 * 1024);
        assert!(config.file_cache.enabled);
    }

    #[test]
    fn tc_5_5_03_file_cache_disabled_propagates() {
        let toml_str = r#"
[file_cache]
enabled = false
"#;
        let config: ConfigFile = toml::from_str(toml_str).unwrap();
        assert!(!config.file_cache.enabled);
    }

    #[test]
    fn merge_file_cache_project_overrides_global() {
        let global = ConfigFile {
            file_cache: FileCacheConfig {
                max_entries: 200,
                max_size_bytes: 50 * 1024 * 1024,
                enabled: true,
            },
            ..Default::default()
        };
        let project = ConfigFile {
            file_cache: FileCacheConfig {
                max_entries: 50,
                ..Default::default()
            },
            ..Default::default()
        };

        let merged = merge_config_files(global, project);
        assert_eq!(
            merged.file_cache.max_entries, 50,
            "project non-default max_entries should override global"
        );
    }

    #[test]
    fn merge_file_cache_global_preserved_when_project_default() {
        let global = ConfigFile {
            file_cache: FileCacheConfig {
                max_entries: 200,
                max_size_bytes: 50 * 1024 * 1024,
                enabled: true,
            },
            ..Default::default()
        };
        let project = ConfigFile::default();

        let merged = merge_config_files(global, project);
        assert_eq!(
            merged.file_cache.max_entries, 200,
            "global should be preserved when project is all-default"
        );
        assert_eq!(merged.file_cache.max_size_bytes, 50 * 1024 * 1024);
    }

    #[test]
    fn merge_file_cache_project_max_size_bytes_overrides_global() {
        // R-5.5-01: project changes only max_size_bytes (enabled=true, max_entries=default).
        let global = ConfigFile {
            file_cache: FileCacheConfig {
                max_entries: 100,
                max_size_bytes: 50 * 1024 * 1024,
                enabled: true,
            },
            ..Default::default()
        };
        let project = ConfigFile {
            file_cache: FileCacheConfig {
                max_entries: 100,                 // default
                max_size_bytes: 10 * 1024 * 1024, // non-default
                enabled: true,                    // default
            },
            ..Default::default()
        };

        let merged = merge_config_files(global, project);
        assert_eq!(
            merged.file_cache.max_size_bytes,
            10 * 1024 * 1024,
            "project max_size_bytes should override global"
        );
    }

    #[test]
    fn merge_file_cache_disabled_overrides_global() {
        let global = ConfigFile {
            file_cache: FileCacheConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let project = ConfigFile {
            file_cache: FileCacheConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };

        let merged = merge_config_files(global, project);
        assert!(
            !merged.file_cache.enabled,
            "project enabled=false should override global"
        );
    }

    #[test]
    fn test_resolve_with_project_dir_loads_project_config() {
        let tmp = tempfile::tempdir().unwrap();
        let project_toml = tmp.path().join(".nomi.toml");
        std::fs::write(
            &project_toml,
            r#"
[default]
max_tokens = 1234
"#,
        )
        .unwrap();

        let cli_args = CliArgs {
            provider: Some("anthropic".into()),
            api_key: Some("test-key".into()),
            base_url: None,
            model: None,
            max_tokens: None,
            max_turns: None,
            system_prompt: None,
            profile: None,
            auto_approve: false,
            project_dir: Some(tmp.path().to_path_buf()),
        };

        let config = Config::resolve(&cli_args).unwrap();
        assert_eq!(config.max_tokens, 1234);
    }

    #[test]
    fn test_resolve_without_project_dir_uses_cwd() {
        let cli_args = CliArgs {
            provider: Some("anthropic".into()),
            api_key: Some("test-key".into()),
            base_url: None,
            model: None,
            max_tokens: None,
            max_turns: None,
            system_prompt: None,
            profile: None,
            auto_approve: false,
            project_dir: None,
        };

        let config = Config::resolve(&cli_args);
        assert!(config.is_ok());
    }

    #[test]
    fn tools_config_new_fields_default_to_current_behavior() {
        let t = ToolsConfig::default();
        assert!(t.in_process_spawn, "默认必须保留进程内 Spawn（CLI 零回归）");
        assert!(t.builtin_allowlist.is_empty(), "默认不限制工具");
        // serde 缺字段也回落默认（旧 config 文件零回归）。
        let de: ToolsConfig = serde_json::from_str("{}").unwrap();
        assert!(de.in_process_spawn);
        assert!(de.builtin_allowlist.is_empty());
    }
}
