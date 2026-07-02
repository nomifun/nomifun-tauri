pub mod acp_assembler;
pub mod provider_config;

mod acp;
mod context;
mod nanobot;
pub(crate) mod nomi;
mod openclaw;
mod remote;

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::FutureExt;
use nomi_agent::companion_tools::{CompanionMemorySink, CompanionSkillSink};
use nomi_agent::requirement_tools::RequirementSink;
use nomifun_api_types::{
    BrowserMcpConfig, ComputerMcpConfig, GatewayMcpConfig, GuideMcpConfig, OpenMcpConfig,
    RequirementMcpConfig,
};
use nomifun_common::{AgentType, AppError};
use nomifun_db::{
    IClientPreferenceRepository, IMcpServerRepository, IProviderRepository, IRemoteAgentRepository,
    ISettingsRepository,
};

use crate::agent_task::AgentInstance;
use crate::capability::skill_manager::AcpSkillManager;
use crate::factory::context::FactoryContext;
use crate::persistence::AcpSessionSyncService;
use crate::registry::AgentRegistry;
use crate::task_manager::AgentFactory;
use crate::types::BuildTaskOptions;

/// Builds the persona system prompt for companion-companion conversations that do
/// not carry one in their extra. Companion companion threads persist a prompt at
/// thread creation; channel master-agent sessions deliberately do NOT, so the
/// factory asks this provider at every agent build — the persona's memory
/// snapshot then refreshes whenever the agent restarts instead of being
/// frozen forever. Implemented by `nomifun-companion::CompanionService`.
#[async_trait::async_trait]
pub trait CompanionPromptProvider: Send + Sync {
    /// `companion_id` selects which companion's persona to build; `None` (or an unknown
    /// id) falls back to the host's default companion. `channel_platform` is the IM
    /// platform serving this session (e.g. "telegram"), `None` for local
    /// companion threads. Returns `None` when no companion exists.
    async fn build_system_prompt(
        &self,
        companion_id: Option<&str>,
        channel_platform: Option<&str>,
    ) -> Option<String>;

    /// The bound companion's exposure tier, read LIVE at every agent build so
    /// flipping a companion to/from public service takes effect on its next turn
    /// (no stale session extra). `None` / unknown id / non-companion sessions →
    /// `Private` (today's full-capability behavior). A `PublicService` companion
    /// causes the factory to hard-clamp the session to safe tools only.
    async fn exposure(
        &self,
        companion_id: Option<&str>,
    ) -> nomifun_api_types::ExposureMode {
        let _ = companion_id;
        nomifun_api_types::ExposureMode::Private
    }
}

/// Dependencies needed by the agent factory to construct agents.
pub struct AgentFactoryDeps {
    pub skill_manager: Arc<AcpSkillManager>,
    pub remote_agent_repo: Arc<dyn IRemoteAgentRepository>,
    pub provider_repo: Arc<dyn IProviderRepository>,
    pub encryption_key: [u8; 32],
    pub agent_registry: Arc<AgentRegistry>,
    pub acp_agent_service: Arc<AcpSessionSyncService>,
    pub data_dir: PathBuf,
    /// Root for auto-provisioned temp workspaces
    /// (`{work_dir}/conversations/{label}-temp-{id}`). Defaults to the data
    /// dir at composition; kept as its own field so the fallback in
    /// `FactoryContext::resolve` stays in sync with `ConversationService`,
    /// which provisions under `AppConfig.work_dir` — a `--work-dir` /
    /// `NOMIFUN_WORK_DIR` override must not split the two roots.
    pub work_dir: PathBuf,
    /// Absolute path to the backend binary, reused as the `command` of stdio MCP
    /// bridges injected into ACP `session/new`.
    /// Captured once at app startup (`std::env::current_exe()`).
    pub backend_binary_path: Arc<PathBuf>,
    /// Guide MCP server config. Retained for build-extra compatibility, but not
    /// injected while Team is not surfaced in the product.
    pub guide_mcp_config: Option<GuideMcpConfig>,
    /// Requirement MCP server config. When `Some`, injected into ACP agent
    /// sessions so the agent gets the `requirement_complete` /
    /// `requirement_update_status` declaration tools — the ACP soft-failure fix
    /// (a clean turn with no declaration becomes `needs_review`, not silent
    /// `done`). `None` when the requirement MCP server failed to start.
    pub requirement_mcp_config: Option<RequirementMcpConfig>,
    /// Wiring for the scoped knowledge-search MCP. Injected into ACP sessions
    /// ONLY when they have bound knowledge bases (`!knowledge_mounts.is_empty()`).
    /// Independent of `desktop_gateway`; its token reaches only the
    /// knowledge_search server, never the gateway. `None` disables ACP knowledge_search.
    pub knowledge_mcp_config: Option<nomifun_api_types::KnowledgeMcpConfig>,
    /// Desktop Gateway MCP server config. When `Some`, injected into sessions
    /// whose `extra.desktopGateway` is true (channel master-agent sessions,
    /// companion companion threads) so the agent gets the `nomi_*` desktop tools.
    /// `None` when the gateway server failed to start (graceful degradation).
    pub gateway_mcp_config: Option<GatewayMcpConfig>,
    /// Reliable-launch (`open`) MCP server config. When `Some`, injected
    /// UNCONDITIONALLY into every ACP session so the agent gets the `open` tool
    /// (ShellExecute a URL/file/app) instead of fragile `cmd /c start` shell
    /// commands. Populated on Windows only — `None` on macOS/Linux (which launch
    /// reliably already) and so never injected there.
    pub open_mcp_config: Option<OpenMcpConfig>,
    /// Computer-use discrete-tool MCP server config. When `Some`, injected
    /// UNCONDITIONALLY into every ACP session so the agent gets discrete desktop
    /// tools (snapshot / click / type / launch / …). Populated on Windows only and
    /// only when the host binary has the `computer-use` feature — `None`
    /// otherwise, and so never injected there.
    pub computer_mcp_config: Option<ComputerMcpConfig>,
    /// Browser-use discrete-tool MCP server config. When `Some`, injected
    /// UNCONDITIONALLY into every ACP session so the agent gets discrete browser
    /// tools (navigate / observe / click / type / …). Populated on every desktop
    /// OS only when the host binary has the `browser-use` feature — `None`
    /// otherwise (web/headless), and so never injected there. Symmetric with
    /// `computer_mcp_config`.
    pub browser_mcp_config: Option<BrowserMcpConfig>,
    /// Client-preferences repo for reading user-facing settings at session-build
    /// time — currently the `agent.computerUse` toggle that gates the nomi
    /// Computer tool. `Option` so tests can omit it (then the default applies).
    /// Read live per session so toggling the setting affects new sessions without
    /// a restart.
    pub client_prefs: Option<Arc<dyn IClientPreferenceRepository>>,
    /// System-settings repo for reading the app UI language at session-build
    /// time. Companion-owned sessions (local 桌面伙伴 chat + IM channel master)
    /// get a reply-language directive built from `SystemSettings.language` so the
    /// companion answers in the app's language instead of a hardcoded one.
    /// `Option` so tests can omit it (then the "en-US" default applies). Read live
    /// per build (mirrors `client_prefs`) so switching the language takes effect on
    /// the next agent (re)build.
    pub settings_repo: Option<Arc<dyn ISettingsRepository>>,
    /// User-configured MCP servers repository. Used by ACP factory to
    /// inject enabled servers into `session/new` (ELECTRON-1JG fix).
    /// `None` for tests/composition paths that do not need MCP injection.
    pub mcp_server_repo: Option<Arc<dyn IMcpServerRepository>>,
    /// Optional sink enabling nomi native requirement tools. When `Some`,
    /// `requirement_complete` / `requirement_update_status` are registered into
    /// the in-process engine. `None` (e.g. standalone) leaves them unregistered.
    pub requirement_sink: Option<Arc<dyn RequirementSink>>,
    /// Per-conversation factory for the agent's native cron tools. The app
    /// captures `CronService` here; the agent factory calls it with the
    /// conversation id to build a bound `CronSink`. `None` leaves the cron tools
    /// unregistered (e.g. standalone, or cron disabled).
    pub cron_sink_factory: Option<Arc<dyn Fn(&str) -> Arc<dyn crate::CronSink> + Send + Sync>>,
    /// Optional sink enabling the companion-companion memory tools
    /// (`recall_memories` / `save_memory` / `list_recent_events`). Only
    /// registered for conversations whose `extra.companionSession` is true.
    pub companion_sink: Option<Arc<dyn CompanionMemorySink>>,
    /// Optional sink enabling the companion's self-evolved skill auto-use
    /// (`companion_skill` tool + per-turn when_to_use ContextContributor). Only
    /// registered for companion sessions (`extra.companionSession` true).
    pub companion_skill_sink: Option<Arc<dyn CompanionSkillSink>>,
    /// Optional sink enabling the nomi native `knowledge_search` tool. When
    /// `Some` AND the session has bound knowledge bases, the tool is registered
    /// into the in-process engine. `None` (standalone) leaves it unregistered.
    pub knowledge_retrieval: Option<Arc<dyn nomi_agent::knowledge_tools::KnowledgeRetrievalSink>>,
    /// Optional sink enabling the nomi native `knowledge_write` (回血) tool. When
    /// `Some` AND the session has bound knowledge bases with write-back enabled,
    /// the tool is registered into the in-process engine and allow-listed past
    /// the approval gate. `None` (standalone) leaves it unregistered.
    pub knowledge_writeback: Option<Arc<dyn nomi_agent::knowledge_tools::KnowledgeWritebackSink>>,
    /// Optional persona prompt provider for companionSession conversations that
    /// carry no `extra.system_prompt` (channel master-agent sessions).
    pub companion_prompt: Option<Arc<dyn CompanionPromptProvider>>,
}

/// Build a production agent factory that dispatches to concrete agent types.
///
/// [`AgentFactory`] is async: the returned `BoxFuture` is driven by
/// [`crate::task_manager::IWorkerTaskManager::get_or_build_task`] on whatever
/// runtime is currently polling it. This lets us spawn CLI processes and
/// await ACP handshakes directly, without the scoped-thread + `block_on`
/// bridge the old sync-factory version needed.
pub fn build_agent_factory(deps: AgentFactoryDeps) -> AgentFactory {
    let deps = Arc::new(deps);

    Arc::new(move |options: BuildTaskOptions| {
        let deps = deps.clone();
        async move { build_agent(deps, options).await }.boxed()
    })
}

async fn build_agent(
    deps: Arc<AgentFactoryDeps>,
    options: BuildTaskOptions,
) -> Result<AgentInstance, AppError> {
    let ctx = FactoryContext::resolve(&deps, &options).await?;
    match options.agent_type {
        AgentType::Gemini => Err(AppError::ConversationArchived(
            "This conversation was created with the legacy Gemini runtime, which has been \
             removed. Please start a new conversation with the Gemini ACP backend to continue."
                .into(),
        )),
        AgentType::Acp => acp::build(deps, options, ctx).await,
        AgentType::OpenclawGateway => openclaw::build(deps, options, ctx).await,
        AgentType::Nanobot => nanobot::build(deps, options, ctx).await,
        AgentType::Remote => remote::build(deps, options, ctx).await,
        AgentType::Nomi => nomi::build(deps, options, ctx).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_deps_can_be_constructed() {
        // Verify types compile — actual construction requires DB
        let _: fn() -> AgentFactoryDeps = || {
            panic!("compile-time check only");
        };
    }
}
