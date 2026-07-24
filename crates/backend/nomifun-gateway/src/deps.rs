//! Late-bound dependency bundle for the gateway tool implementations.

use std::sync::Arc;

use nomifun_ai_agent::AgentRuntimeRegistry;
use nomifun_preset::PresetService;
use nomifun_companion::CompanionService;
use nomifun_conversation::ConversationService;
use nomifun_cron::service::CronService;
use nomifun_db::IProviderRepository;
use nomifun_idmm::IdmmService;
use nomifun_knowledge::KnowledgeService;
use nomifun_requirement::{AutoWorkRunner, RequirementService};
use nomifun_system::{ClientPrefService, ModelFetchService, ProviderService, SettingsService};
use nomifun_terminal::TerminalService;
use nomifun_common::{CompanionId, ConversationId, UserId};

/// Everything the gateway tools need to operate the desktop.
///
/// Constructed by `nomifun-app` AFTER `build_module_states` (the
/// `ConversationService` / `CronService` instances live there) and wired into
/// the already-running [`crate::GatewayMcpServer`] via `set_deps` — the same
/// late-wire choreography as the guide / requirement MCP servers, which is
/// what lets the server start before the agent factory while the factory
/// still receives the server's connection config.
///
/// NEW FIELD? A capability needing a new service adds it here, then wires it in
/// `nomifun-app/src/router/routes.rs::inject_gateway_deps` (clone from the
/// matching `states.*` / `services.*`). The struct is just an Arc bundle —
/// growth is O(1) pointers, negligible.
pub struct GatewayDeps {
    /// Canonical installation owner. Every installation-scoped capability is
    /// gated against this same immutable identity before its handler runs.
    pub authoritative_user_id: Arc<str>,
    pub conversation_service: ConversationService,
    pub runtime_registry: Arc<dyn AgentRuntimeRegistry>,
    pub cron_service: Arc<CronService>,
    /// MUST be the router-state instance (the singleton clone that had
    /// `with_conversation_service` / `with_terminal_driver` attached in
    /// `build_requirement_state`) — the AutoWork config tools need those
    /// attachments; the bare singleton would error "not attached".
    pub requirement_service: Arc<RequirementService>,
    pub companion_service: Arc<CompanionService>,
    /// Singleton terminal service (owns the live PTY map shared with the
    /// terminal routes + AutoWork runner).
    pub terminal_service: Arc<TerminalService>,
    /// Main-db provider rows: model listing + the nomi model resolution chain.
    pub provider_repo: Arc<dyn IProviderRepository>,
    /// IDMM supervision config (same instance as `/api/idmm` so save also
    /// arms/stops the live supervisor).
    pub idmm_service: Arc<IdmmService>,
    /// 创意工坊 (Creative Workshop) canvas index — backs the read-only
    /// `nomi_workshop_list_canvases` capability. Main-db rows only (the canvas
    /// bodies live on disk in the workshop service); the same table the
    /// `/api/workshop/*` routes read.
    pub workshop_repo: Arc<dyn nomifun_db::IWorkshopRepository>,
    /// 创意工坊 canvas/asset service — the SAME singleton the `/api/workshop/*`
    /// routes use (so the 画布助手 agent-op queue is shared: the gateway enqueues
    /// while an open frontend polls/acks the SAME in-memory queue). Backs
    /// `nomi_workshop_get_canvas` / `nomi_workshop_list_assets` /
    /// `nomi_workshop_apply_ops`.
    pub workshop_service: Arc<nomifun_workshop::WorkshopService>,
    /// 生成引擎 (creation) media task queue — the SAME singleton the
    /// `/api/creation/*` routes use. Backs `nomi_workshop_generate` /
    /// `nomi_workshop_get_task` (submit + inspect generation tasks).
    pub creation_service: Arc<nomifun_creation::CreationService>,
    /// Knowledge base registry + bindings (same instance the conversation
    /// service mounts from at task start).
    pub knowledge_service: Arc<KnowledgeService>,
    /// AutoWork live-loop control. The REST `POST /api/requirements/autowork`
    /// starts/stops this runner alongside persisting the config; the
    /// gateway autowork tools must mirror that or an "enabled" toggle would
    /// only take effect after the next desktop boot (boot-resume).
    pub auto_work_runner: Arc<AutoWorkRunner>,
    /// System domain services (same instances the `/api/settings`,
    /// `/api/settings/client`, `/api/providers` routes use — so a gateway theme /
    /// toggle / provider change and a UI change act on identical state).
    pub settings_service: SettingsService,
    pub client_pref_service: ClientPrefService,
    pub provider_service: ProviderService,
    pub model_fetch_service: ModelFetchService,
    /// Channel domain state (plugin manager + pairing + sessions + settings),
    /// the same instances the `/api/channels` routes use. `Clone` (all Arc).
    pub channel_state: nomifun_channel::ChannelRouterState,
    /// Filesystem service (path-scoped to the configured allowed roots).
    pub file_service: nomifun_file::FileServiceRef,
    /// Shell-open service (OS ShellExecute / `open`).
    pub shell_service: std::sync::Arc<nomifun_shell::ShellService>,
    /// MCP server CRUD (same instance as the `/api/mcp` routes).
    pub mcp_config_service: nomifun_mcp::McpConfigService,
    /// Extension registry + hub + skills (same instances as the extension routes).
    pub extension_registry: nomifun_extension::ExtensionRegistry,
    pub hub_index_manager: nomifun_extension::HubIndexManager,
    pub hub_installer: nomifun_extension::HubInstaller,
    pub skill_paths: nomifun_extension::SkillPaths,
    /// Agent catalog + remote agents (same instances as the agent routes).
    pub agent_service: std::sync::Arc<nomifun_ai_agent::AgentService>,
    pub remote_agent_service: std::sync::Arc<nomifun_ai_agent::RemoteAgentService>,
    /// Client-preference repo backing the global model-failover config.
    pub client_pref_repo: std::sync::Arc<dyn nomifun_db::IClientPreferenceRepository>,
    /// One shared persistent collaboration facade. REST, gateway tools, boot
    /// recovery and scheduling all use this exact instance.
    pub agent_execution_engine: Arc<nomifun_agent_execution::AgentExecutionEngine>,
    /// Preset service — the same singleton used by `/api/presets`.
    pub preset_service: Arc<PresetService>,
    /// **P3-GW1 (route A)**: per-companion browser tool registry, living in the
    /// main process. `Some` only when the `browser-use` feature is on and the
    /// app wired it; `None` (or the field absent without the feature) → the
    /// gateway exposes no `nomi_browser_*` tools. Each companion gets its own
    /// lazily-engined `BrowserTool` + a serialization mutex (X5). See
    /// [`crate::browser_registry`].
    #[cfg(feature = "browser-use")]
    pub browser_registry: Option<crate::browser_registry::BrowserRegistry>,
    /// Shared desktop `ComputerTool` (one screen → one serialized instance).
    /// `Some` only when the `computer-use` feature is on and the app wired it;
    /// otherwise the gateway exposes no `nomi_computer_*` tools. See
    /// [`crate::computer_registry`].
    #[cfg(feature = "computer-use")]
    pub computer_registry: Option<crate::computer_registry::ComputerRegistry>,
}

/// Identity of the calling Agent session, reconstructed only from the validated
/// signed Gateway child capability forwarded by the stdio bridge.
#[derive(Debug, Clone)]
pub struct CallerCtx {
    /// The conversation the calling agent lives in. Used for self-protection
    /// (a session may not message or delete itself) and as the default cron
    /// binding target.
    pub conversation_id: Option<ConversationId>,
    /// The desktop user every tool scopes its data access to.
    pub user_id: UserId,
    /// The companion the calling session is bound to (multi-companion upgrade). `None`
    /// for sessions without a companion binding — memory/requirement tools are
    /// deliberately companion-agnostic (memory is shared), so this is attribution
    /// context, not an access scope.
    pub companion_id: Option<CompanionId>,
    /// IM platform when this is a Channel Agent session (e.g. "lark").
    /// `None` for plain companion/desktop sessions. Used to resolve the write
    /// surface (channel → write-disabled in P1).
    pub channel_platform: Option<String>,
    /// Resolved approval mode for the calling agent session.
    pub session_mode: Option<String>,
    /// `true` when the caller is an external network consumer reaching the
    /// platform through the Remote front door (the "外部伙伴" surface). Takes
    /// precedence over `channel_platform` in [`CallerCtx::surface`]. Defaults
    /// `false` so every existing (desktop/channel) construction site is
    /// unaffected.
    pub remote: bool,
    /// Authenticated, transport-derived identity for the current mutating
    /// operation. Conversation send capabilities fail closed when absent; it
    /// is never read from model-visible tool arguments or a JSON-RPC request id.
    pub operation_id: Option<String>,
}

impl Default for CallerCtx {
    fn default() -> Self {
        Self {
            conversation_id: None,
            user_id: UserId::new(),
            companion_id: None,
            channel_platform: None,
            session_mode: None,
            remote: false,
            operation_id: None,
        }
    }
}

impl CallerCtx {
    /// Build the identity context for an authenticated Remote caller.
    ///
    /// Both values cross process/network boundaries as strings, so they are
    /// validated here before any capability can observe them.
    pub fn try_remote(user_id: &str, companion_id: &str) -> Result<Self, String> {
        Ok(Self {
            conversation_id: None,
            user_id: UserId::parse(user_id).map_err(|error| error.to_string())?,
            companion_id: Some(
                CompanionId::parse(companion_id).map_err(|error| error.to_string())?,
            ),
            channel_platform: None,
            session_mode: None,
            remote: true,
            operation_id: None,
        })
    }
}
