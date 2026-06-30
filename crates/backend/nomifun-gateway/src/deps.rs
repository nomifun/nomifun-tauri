//! Late-bound dependency bundle for the gateway tool implementations.

use std::sync::Arc;

use nomifun_ai_agent::IWorkerTaskManager;
use nomifun_assistant::AssistantService;
use nomifun_companion::CompanionService;
use nomifun_conversation::ConversationService;
use nomifun_cron::service::CronService;
use nomifun_db::IProviderRepository;
use nomifun_idmm::IdmmService;
use nomifun_knowledge::KnowledgeService;
use nomifun_requirement::{Orchestrator, RequirementService};
use nomifun_system::{ClientPrefService, ModelFetchService, ProviderService, SettingsService};
use nomifun_terminal::TerminalService;

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
    pub conversation_service: ConversationService,
    pub task_manager: Arc<dyn IWorkerTaskManager>,
    pub cron_service: Arc<CronService>,
    /// MUST be the router-state instance (the singleton clone that had
    /// `with_conversation_service` / `with_terminal_driver` attached in
    /// `build_requirement_state`) — the AutoWork config tools need those
    /// attachments; the bare singleton would error "not attached".
    pub requirement_service: Arc<RequirementService>,
    pub companion_service: Arc<CompanionService>,
    /// Singleton terminal service (owns the live PTY map shared with the
    /// terminal routes + AutoWork orchestrator).
    pub terminal_service: Arc<TerminalService>,
    /// Main-db provider rows: model listing + the nomi model resolution chain.
    pub provider_repo: Arc<dyn IProviderRepository>,
    /// IDMM supervision config (same instance as `/api/idmm` so save also
    /// arms/stops the live supervisor).
    pub idmm_service: Arc<IdmmService>,
    /// Knowledge base registry + bindings (same instance the conversation
    /// service mounts from at task start).
    pub knowledge_service: Arc<KnowledgeService>,
    /// AutoWork live-loop control. The REST `POST /api/requirements/autowork`
    /// starts/stops this orchestrator alongside persisting the config; the
    /// gateway autowork tools must mirror that or an "enabled" toggle would
    /// only take effect after the next desktop boot (boot-resume).
    pub autowork_orchestrator: Arc<Orchestrator>,
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
    /// 智能编排 Run control-plane: creates/plans/inspects orchestration runs.
    /// MUST be the router-state instance (`states.orchestrator.run_service` — the
    /// same `Arc<RunService>` the REST routes + the [`RunEngine`] loop share), so a
    /// gateway-created run and a UI-created run act on identical state.
    pub orchestrator_run_service: Arc<nomifun_orchestrator::RunService>,
    /// 智能编排 Run engine: the serial execution loop driver. MUST be the
    /// router-state instance (`states.orchestrator.engine` — `RunEngine` is itself
    /// `Clone` with `Arc` internals, so this `Arc` wraps that one live instance;
    /// `start()` must register against the SAME in-memory handle map the boot
    /// resume + REST cancel use, or a gateway-started run would not be cancellable).
    pub orchestrator_run_engine: Arc<nomifun_orchestrator::RunEngine>,
    /// 助手 (assistants) service — the SAME instance the `/api/assistants` routes
    /// use (`states.assistant.service`). The caps_orchestrator layer (P4 Task 2)
    /// reads the ENABLED assistants here and folds each one's persona/skills/model
    /// into an enriched [`nomifun_api_types::FleetMember`] when creating an ad-hoc
    /// run, so the orchestrator engine/worker can read a self-contained snapshot
    /// without an assistant-crate dependency. Dependency direction: gateway →
    /// nomifun-assistant (nomifun-assistant does NOT depend on gateway — no cycle).
    pub assistant_service: Arc<AssistantService>,
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

/// Identity of the calling agent session, forwarded by the stdio bridge from
/// the env the factory injected (`NOMI_GW_MCP_CONVERSATION_ID` /
/// `NOMI_GW_MCP_USER_ID` / `NOMI_GW_MCP_COMPANION_ID`).
#[derive(Debug, Clone, Default)]
pub struct CallerCtx {
    /// The conversation the calling agent lives in. Used for self-protection
    /// (a session may not message or delete itself) and as the default cron
    /// binding target.
    pub conversation_id: String,
    /// The desktop user every tool scopes its data access to.
    pub user_id: String,
    /// The companion the calling session is bound to (multi-companion upgrade). `None`
    /// for sessions without a companion binding — memory/requirement tools are
    /// deliberately companion-agnostic (memory is shared), so this is attribution
    /// context, not an access scope.
    pub companion_id: Option<String>,
    /// IM platform when this is a channel master-agent session (e.g. "lark").
    /// `None` for plain companion/desktop sessions. Used to resolve the write
    /// surface (channel → write-disabled in P1).
    pub channel_platform: Option<String>,
    /// `true` when the caller is an external network consumer reaching the
    /// platform through the Remote front door (the "外部伙伴" surface). Takes
    /// precedence over `channel_platform` in [`CallerCtx::surface`]. Defaults
    /// `false` so every existing (desktop/channel) construction site is
    /// unaffected.
    pub remote: bool,
}
