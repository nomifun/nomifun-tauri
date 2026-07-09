//! Module-level router states + their builders.
//!
//! `ModuleStates` is the bundle returned by `build_module_states`; each
//! `build_*_state` constructs one `*RouterState` from `AppServices`.

use std::sync::Arc;
use std::time::Instant;

use nomifun_ai_agent::{
    AgentRouterState, AgentService, IWorkerTaskManager, RemoteAgentRouterState, RemoteAgentService,
};
use nomifun_assistant::{AssistantRouterState, AssistantService, BuiltinAssistantRegistry};
use nomifun_auth::extract_token_from_ws_headers;
use nomifun_channel::ChannelRouterState;
use nomifun_common::{OnConversationDelete, OnTerminalDelete};
use nomifun_conversation::{ConversationRouterState, ConversationService};
use nomifun_cron::{CronEventEmitter, CronRouterState};
use nomifun_db::{
    IAcpSessionRepository, IAgentMetadataRepository, IAssistantOverrideRepository, IAssistantRepository,
    IAssistantTagRepository, IIdmmInterventionRepository, IProviderRepository, SqliteAcpSessionRepository,
    SqliteAgentMetadataRepository, SqliteAssistantOverrideRepository, SqliteAssistantRepository,
    SqliteAssistantTagRepository, SqliteClientPreferenceRepository, SqliteConversationRepository,
    SqliteIdmmInterventionRepository, SqliteProviderRepository, SqliteRemoteAgentRepository, SqliteSettingsRepository,
};
use nomifun_extension::{
    AssistantRuleDispatcher, ExtensionRegistry, ExtensionRouterState, ExtensionStateStore, ExternalPathsManager,
    HubIndexManager, HubInstaller, HubRouterState, SkillRouterState, resolve_install_target_dir_for_data_dir,
    resolve_scan_paths_for_data_dir, resolve_state_file_path,
};
use nomifun_file::{FileRouterState, FileService, FileWatchService, SnapshotService};
use nomifun_idmm::{IdmmManager, IdmmRouterState};
use nomifun_knowledge::KnowledgeRouterState;
use nomifun_mcp::{
    ClaudeAdapter, CodeBuddyAdapter, CodexAdapter, GeminiAdapter, McpAgentAdapter, McpConfigService,
    McpConnectionTestService, McpRouterState, McpSyncService, NomiAdapter, NomifunAdapter, OpencodeAdapter,
    QwenAdapter,
};
use nomifun_office::{
    ConversionService, OfficeRouterState, OfficecliWatchManager, ProxyService,
    SnapshotService as OfficeSnapshotService, StarOfficeDetector,
};
use nomifun_orchestrator::{
    ConversationCanceller, ConversationSteerer, ConversationWorkerRunner, LeadReporter,
    LlmPlanProducer, OrchestratorRouterState, OrchestratorRunEventEmitter, PlanProducer, RunEngine,
    RunEngineDeps, RunOutcome, RunService, WorkerRunner,
};
use nomifun_companion::CompanionRouterState;
use nomifun_public_agent::PublicAgentRouterState;
use nomifun_workshop::WorkshopRouterState;
use nomifun_creation::CreationRouterState;
use nomifun_realtime::{NoopMessageRouter, WsHandlerState};
use nomifun_requirement::RequirementRouterState;
use nomifun_shell::ShellRouterState;
use nomifun_system::{
    ClientPrefService, ConnectionTestRouterState, ConnectionTestService, ModelFetchService, ProtocolDetectionService,
    ProviderService, SettingsService, SystemRouterState, VersionCheckService,
};
use nomifun_terminal::TerminalRouterState;
use nomifun_webhook::WebhookRouterState;

use nomifun_secret::SecretRouterState;

use crate::config::derive_encryption_key;
use crate::services::AppServices;

/// All module-level router states bundled into a single struct.
///
/// Reduces parameter bloat on router constructors and makes it easy for
/// tests to override individual modules.
pub struct ModuleStates {
    pub system: SystemRouterState,
    pub conversation: ConversationRouterState,
    pub remote_agent: RemoteAgentRouterState,
    pub agent: AgentRouterState,

    pub connection_test: ConnectionTestRouterState,
    pub file: FileRouterState,
    pub mcp: McpRouterState,
    pub extension: ExtensionRouterState,
    pub hub: HubRouterState,
    pub skill: SkillRouterState,
    pub channel: ChannelRouterState,
    pub cron: CronRouterState,
    pub requirement: RequirementRouterState,
    pub idmm: IdmmRouterState,
    pub knowledge: KnowledgeRouterState,
    pub companion: CompanionRouterState,
    pub public_agent: PublicAgentRouterState,
    /// 创意工坊 (Creative Workshop) canvas/asset domain.
    pub workshop: WorkshopRouterState,
    /// 生成引擎 (creation) media task queue.
    pub creation: CreationRouterState,
    pub webhook: WebhookRouterState,
    /// 智能编排 (orchestration): fleet + workspace CRUD state (P0).
    pub orchestrator: OrchestratorRouterState,
    /// P3-X2: per-pet browser-use credential secret CRUD state.
    pub secret: SecretRouterState,
    pub terminal: TerminalRouterState,
    pub office: OfficeRouterState,
    pub shell: ShellRouterState,
    pub assistant: AssistantRouterState,
}

fn default_allowed_roots(work_dir: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut roots = vec![
        std::env::temp_dir(),
        dirs::home_dir().unwrap_or_else(std::env::temp_dir),
    ];
    // Auto-provisioned per-conversation workspaces live under
    // `{work_dir}/conversations/{label}-temp-{token}/`. On Windows the
    // operator may put `work_dir` on a separate drive (e.g. `X:\Nomi`)
    // that's neither under `temp_dir` nor `home_dir`, which previously
    // caused `/api/fs/list` to 403 every Hermes-mode session
    // (ELECTRON-1BT). Including `work_dir` keeps temp + custom-on-drive
    // workspaces on the allowlist without widening the sandbox to
    // unrelated paths.
    if let Some(wd) = work_dir
        && !wd.as_os_str().is_empty()
        && !roots.iter().any(|r| r == wd)
    {
        roots.push(wd.to_path_buf());
    }
    roots
}

fn outbound_http_client() -> reqwest::Client {
    nomifun_net::http_client()
}

/// Components needed to start the channel orchestrator.
///
/// Returned alongside `ChannelRouterState` by `build_channel_state`.
/// The caller must spawn the orchestrator as a background task.
pub struct ChannelOrchestratorComponents {
    pub orchestrator: nomifun_channel::orchestrator::ChannelOrchestrator,
    pub message_rx: tokio::sync::mpsc::Receiver<nomifun_channel::types::ChannelIncoming>,
    pub confirm_rx: tokio::sync::mpsc::Receiver<(String, String)>,
    pub manager: Arc<nomifun_channel::manager::ChannelManager>,
    pub plugin_factory: Arc<nomifun_channel::manager::PluginFactory>,
}

/// Build all default `ModuleStates` from application services.
pub async fn build_module_states(services: &AppServices) -> (ModuleStates, ChannelOrchestratorComponents) {
    let boot = Instant::now();
    tracing::info!("startup: module state build started");

    let (ext_state, hub_state, mut skill_state) = build_extension_states(services).await;
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: extension states built"
    );

    let scan_paths = resolve_scan_paths_for_data_dir(&services.data_dir);
    if let Err(error) = ext_state.registry.initialize_with_scan_paths(scan_paths).await {
        tracing::warn!(error = %error, "extension registry initialize failed");
    }
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: extension registry initialized"
    );

    let assistant = build_assistant_state(services, ext_state.registry.clone());
    let cron = build_cron_state(services);
    cron.cron_service.init().await;
    // Register the process CronService so the agent's native cron tools (wired
    // via AgentFactoryDeps.cron_sink_factory) can reach it. (Phase 4)
    nomifun_cron::sink::set_process_cron_service(cron.cron_service.clone());
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: cron state initialized"
    );

    // The agent catalog already hydrated at startup (see `lib.rs`).
    // Extension-contributed rows will land in `agent_metadata` in a
    // later step; for now we rely on the builtin + internal seed rows.

    let dispatcher: Arc<dyn AssistantRuleDispatcher> = assistant.service.clone();
    skill_state.assistant_dispatcher = Some(dispatcher);

    let (channel_state, channel_components) = build_channel_state(services, ext_state.registry.clone()).await;
    tracing::info!(elapsed_ms = boot.elapsed().as_millis(), "startup: channel state built");

    let pool = services.database.pool().clone();
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool));
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);
    let agent_service = AgentService::new(
        services.agent_registry.clone(),
        provider_repo,
        services.model_profile_repo.clone(),
        encryption_key,
        services.data_dir.clone(),
    );
    tracing::info!(elapsed_ms = boot.elapsed().as_millis(), "startup: agent service built");

    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: module states bundle started"
    );
    let (requirement_state, idmm_state) = build_requirement_state(services);
    // P3b: clone the IDMM manager (as a `ConversationSupervisionHook`) BEFORE the
    // `ModuleStates` literal moves `idmm_state` into the `idmm:` field — used to
    // arm supervision on the orchestrator engine's dedicated ConversationService.
    let orchestrator_idmm_hook: Arc<dyn nomifun_conversation::ConversationSupervisionHook> =
        Arc::new(idmm_state.service.manager().clone());
    let companion_state = build_companion_state(services, channel_components.manager.clone());
    let states = ModuleStates {
        system: build_system_state(services),
        conversation: build_conversation_state(services, Some(cron.cron_service.clone())),
        remote_agent: build_remote_agent_state(services),
        agent: AgentRouterState {
            agent_registry: services.agent_registry.clone(),
            service: agent_service,
        },
        connection_test: build_connection_test_state(),
        file: build_file_state(services),
        mcp: build_mcp_state(services),
        extension: ext_state,
        hub: hub_state,
        skill: skill_state,
        channel: channel_state,
        cron,
        requirement: requirement_state,
        idmm: idmm_state,
        knowledge: KnowledgeRouterState::new(services.knowledge_service.clone()),
        companion: companion_state,
        public_agent: PublicAgentRouterState::new(services.public_agent_service.clone()),
        workshop: build_workshop_state(services),
        creation: build_creation_state(services),
        webhook: build_webhook_state(services),
        // P3b: arm IDMM supervision on the orchestrator engine's DEDICATED
        // ConversationService (the one worker turns run on) by handing it the
        // same IdmmManager (as a `ConversationSupervisionHook`) the route/terminal
        // instances use. `orchestrator_idmm_hook` was cloned above (before
        // `idmm_state` moved into the `idmm:` field) — autonomous worker (nomi
        // yolo) turns then arm 智能决策 per turn and auto-resolve
        // decisions/open-questions instead of stalling. The wiring lives entirely
        // in this app layer (no orchestrator→idmm dependency — the hook is a
        // `nomifun_conversation` trait the orchestrator's conv_service accepts).
        orchestrator: build_orchestrator_state(services, orchestrator_idmm_hook),
        secret: build_secret_state(services),
        terminal: build_terminal_state(services),
        office: build_office_state(services),
        shell: build_shell_state(services),
        assistant,
    };

    // RC1 fix — arm IDMM supervision on the instances that actually serve the
    // user-driven HTTP paths: `build_conversation_state`'s ConversationService
    // (the one behind `send_message`) and the terminal singleton. The previous
    // registration landed on `build_requirement_state`'s orchestrator-only
    // ConversationService — a separate `::new`, NOT a clone — so the route
    // instance's hook slot stayed `None` and an enabled 智能决策 never armed for a
    // manual chat/terminal turn. `with_*_hook` is `&self` interior-mutable, so a
    // post-construction registration on the singleton/route instance is enough
    // (AutoWork-driven turns arm independently via `OrchestratorDeps.idmm`).
    {
        let idmm_hook = Arc::new(states.idmm.service.manager().clone());
        states.conversation.service.with_supervision_hook(idmm_hook.clone());
        services.terminal_service.with_terminal_supervision_hook(idmm_hook);
    }

    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: module state build completed"
    );

    (states, channel_components)
}

/// Build the default `AssistantRouterState` from application services.
pub fn build_assistant_state(services: &AppServices, extension_registry: ExtensionRegistry) -> AssistantRouterState {
    let pool = services.database.pool().clone();
    let repo: Arc<dyn IAssistantRepository> = Arc::new(SqliteAssistantRepository::new(pool.clone()));
    let override_repo: Arc<dyn IAssistantOverrideRepository> =
        Arc::new(SqliteAssistantOverrideRepository::new(pool.clone()));
    let tag_repo: Arc<dyn IAssistantTagRepository> = Arc::new(SqliteAssistantTagRepository::new(pool.clone()));
    // Used by `AssistantService::resolve_default_agent_type` to infer a
    // working `preset_agent_type` from the configured provider list when
    // the caller does not supply one (ELECTRON-1J1 / 1KV).
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool));
    let builtin = Arc::new(BuiltinAssistantRegistry::load());
    // Pin user_data_dir to the runtime-resolved data directory so dev /
    // packaged / multi-instance launches all keep their assistant rule files
    // alongside the matching SQLite database (avoiding the historical bug
    // where dev wrote rules to the release `~/.nomifun/` while the db lived
    // under `~/.nomifun-dev/`).
    let service = Arc::new(AssistantService::new(
        repo,
        override_repo,
        tag_repo,
        provider_repo,
        builtin,
        extension_registry,
        services.data_dir.clone(),
    ));
    AssistantRouterState { service }
}

/// Build the default `SystemRouterState` from application services.
pub fn build_system_state(services: &AppServices) -> SystemRouterState {
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);
    let pool = services.database.pool().clone();
    let provider_repo = Arc::new(SqliteProviderRepository::new(pool.clone()));

    // Cross-subsystem provider-deletion guard: aggregate every hard-binding
    // in-use scan (companion / public-agent / idmm global backup / orchestrator
    // fleet) so `ProviderService::delete` refuses an in-use provider (409), and
    // strip the soft failover-queue reference after a successful delete. The
    // companion + public-agent singletons are reused from `services`; the client-pref
    // + fleet repos are freshly built from the pool (per-builder pattern).
    let client_pref_repo: Arc<dyn nomifun_db::IClientPreferenceRepository> =
        Arc::new(SqliteClientPreferenceRepository::new(pool.clone()));
    let fleet_repo: Arc<dyn nomifun_db::IFleetRepository> =
        Arc::new(nomifun_db::SqliteFleetRepository::new(pool.clone()));
    let deletion_coordinator = Arc::new(crate::provider_deletion::AppProviderDeletionCoordinator {
        companion: services.companion_service.clone(),
        public_agent: services.public_agent_service.clone(),
        client_prefs: client_pref_repo,
        fleet_repo,
    });

    SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(pool.clone()))),
        client_pref_service: ClientPrefService::new(Arc::new(SqliteClientPreferenceRepository::new(pool))),
        provider_service: ProviderService::new(provider_repo.clone(), encryption_key)
            .with_deletion_coordinator(deletion_coordinator),
        model_fetch_service: ModelFetchService::new_dynamic(provider_repo, encryption_key),
        model_profile_service: nomifun_system::ModelProfileService::new(
            services.model_profile_repo.clone(),
        ),
        protocol_detection_service: ProtocolDetectionService::new_dynamic(),
        version_check_service: VersionCheckService::new_dynamic(env!("CARGO_PKG_VERSION").to_owned()),
        data_dir: services.data_dir.clone(),
    }
}

/// Build the default `ConversationRouterState` from application services.
pub fn build_conversation_state(
    services: &AppServices,
    cron_service: Option<Arc<nomifun_cron::service::CronService>>,
) -> ConversationRouterState {
    let pool = services.database.pool().clone();
    let conversaion_repo = Arc::new(SqliteConversationRepository::new(pool.clone()));
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let acp_session_repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(pool));
    let skill_resolver = Arc::new(nomifun_conversation::skill_resolver::ExtensionSkillResolver::new(
        services.skill_paths.clone(),
    ));
    let conversation_service = ConversationService::new(
        services.work_dir.clone(),
        services.event_bus.clone(),
        skill_resolver,
        services.worker_task_manager.clone(),
        conversaion_repo,
        agent_metadata_repo,
        acp_session_repo,
    )
    .with_runtime_state(services.conversation_runtime_state.clone());
    conversation_service.with_mcp_server_repo(Arc::new(nomifun_db::SqliteMcpServerRepository::new(
        services.database.pool().clone(),
    )));
    conversation_service.with_knowledge_service(services.knowledge_service.clone());
    // Phase 3: wire the model-failover deps so a pre-response provider fault on a
    // nomi turn can switch to the next queued model (plan D5).
    conversation_service.with_failover_deps(
        Arc::new(SqliteProviderRepository::new(services.database.pool().clone())),
        Arc::new(SqliteClientPreferenceRepository::new(services.database.pool().clone())),
    );
    // Drop the conversation's knowledge binding when the conversation goes away.
    conversation_service.with_delete_hook(services.knowledge_service.clone());
    // Clear the dual-domain owner of any requirement this conversation owned —
    // requirements.owner_session_id has no FK to cascade (spec §9.B).
    conversation_service.with_delete_hook(
        services.requirement_service.clone() as Arc<dyn OnConversationDelete>,
    );
    // Drop this conversation's IDMM decision records (disposable audit trail,
    // polymorphic target_id with no FK — app-level cascade).
    conversation_service.with_delete_hook(Arc::new(IdmmRecordCascade {
        records: Arc::new(SqliteIdmmInterventionRepository::new(services.database.pool().clone())),
    }) as Arc<dyn OnConversationDelete>);
    // Remove the conversation's on-disk nomi session file + auto-provisioned
    // temp workspace so a future conversation reusing this integer id cannot
    // resume stale state (cross-conversation memory bleed). Pairs with the
    // per-session `owner_token` validation in the nomi factory.
    conversation_service.with_delete_hook(Arc::new(
        nomifun_ai_agent::task_manager::NomiSessionFilesCascade {
            data_dir: services.data_dir.clone(),
            work_dir: services.work_dir.clone(),
        },
    ) as Arc<dyn OnConversationDelete>);
    if let Some(hook) = services.task_manager_delete_hook.clone() {
        conversation_service.with_delete_hook(hook);
    }
    if let Some(cron_service) = cron_service {
        conversation_service.with_delete_hook(cron_service.clone());
        conversation_service.with_cron_service(Some(cron_service));
    }
    ConversationRouterState {
        service: conversation_service,
        task_manager: services.worker_task_manager.clone(),
    }
}

/// Build the default `RemoteAgentRouterState` from application services.
pub fn build_remote_agent_state(services: &AppServices) -> RemoteAgentRouterState {
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);
    let pool = services.database.pool().clone();
    let repo = Arc::new(SqliteRemoteAgentRepository::new(pool));
    RemoteAgentRouterState {
        service: Arc::new(RemoteAgentService::new(repo, encryption_key)),
    }
}

/// Build the default `ConnectionTestRouterState`.
pub fn build_connection_test_state() -> ConnectionTestRouterState {
    ConnectionTestRouterState {
        service: ConnectionTestService::new(outbound_http_client()),
    }
}

/// Build the default `FileRouterState` from application services.
pub fn build_file_state(services: &AppServices) -> FileRouterState {
    let broadcaster = services.event_bus.clone();
    let mut allowed_roots = default_allowed_roots(Some(services.work_dir.as_path()));
    // Requirement attachments live under the data dir; include it so the
    // image-base64 preview works when the data dir sits outside home/temp
    // (custom NOMIFUN_DATA_DIR on another drive).
    if !allowed_roots.iter().any(|r| r == &services.data_dir) {
        allowed_roots.push(services.data_dir.clone());
    }
    let browse_roots = nomifun_file::browse::default_browse_roots();
    let file_service = Arc::new(FileService::new(broadcaster.clone(), allowed_roots.clone()));
    let watch_service = Arc::new(FileWatchService::new(broadcaster).expect("file watch service initialization"));
    let snapshot_service = Arc::new(SnapshotService::new());
    FileRouterState {
        file_service,
        watch_service,
        snapshot_service,
        allowed_roots,
        browse_roots,
    }
}

/// Build the default `McpRouterState` from application services.
pub fn build_mcp_state(services: &AppServices) -> McpRouterState {
    let pool = services.database.pool().clone();
    let repo: Arc<dyn nomifun_db::IMcpServerRepository> = Arc::new(nomifun_db::SqliteMcpServerRepository::new(pool));

    let adapters: Vec<Arc<dyn McpAgentAdapter>> = vec![
        Arc::new(ClaudeAdapter),
        Arc::new(GeminiAdapter),
        Arc::new(QwenAdapter),
        Arc::new(CodexAdapter),
        Arc::new(CodeBuddyAdapter),
        Arc::new(OpencodeAdapter),
        Arc::new(NomiAdapter),
        Arc::new(NomifunAdapter::new(repo.clone())),
    ];

    let oauth_token_repo: Arc<dyn nomifun_db::IOAuthTokenRepository> = Arc::new(
        nomifun_db::SqliteOAuthTokenRepository::new(services.database.pool().clone()),
    );

    McpRouterState {
        config_service: McpConfigService::new(repo.clone()),
        sync_service: McpSyncService::new(repo, adapters),
        connection_test_service: McpConnectionTestService::new_dynamic(),
        oauth_service: nomifun_mcp::McpOAuthService::new_dynamic(oauth_token_repo),
    }
}

/// Adapter exposing the companion as the channel layer's master-agent profile.
///
/// The channel layer resolves a session's companion via the channel row's own
/// `companion_id` first; this profile supplies the legacy per-platform binding
/// (when present and alive) and the per-companion model lookup. There is **no
/// default-companion fallback** — an unbound channel is hosted by no companion.
/// Master-mode sessions with no per-platform model fall back to the bound
/// companion's configured model (the companion IS the master agent — its model
/// choice travels with it to remote sessions).
///
/// It ALSO backs the 对外伙伴 (public agent) side of the same trait: a platform
/// bound to a public agent (mutually exclusive with a companion binding) resolves
/// its live/enabled state + model here so the channel layer can serve strangers
/// via a `PublicService`-clamped per-chat session.
struct CompanionMasterAgentProfile {
    companion_service: Arc<nomifun_companion::CompanionService>,
    channel_settings: Arc<nomifun_channel::channel_settings::ChannelSettingsService>,
    public_agent_service: Arc<nomifun_public_agent::PublicAgentService>,
    /// Provider catalog, used to resolve the app's DEFAULT model when a public
    /// agent has no model of its own — so it answers as soon as ANY provider is
    /// configured (no per-agent model setup required).
    provider_repo: Arc<dyn IProviderRepository>,
}

#[async_trait::async_trait]
impl nomifun_channel::message_service::MasterAgentProfile for CompanionMasterAgentProfile {
    async fn master_companion_id(&self, platform: &str) -> Option<String> {
        // Per-companion binding is the ONLY way a channel becomes hosted by a companion:
        // each bot row carries its own `companion_id` (set when enabled from a companion's
        // 远程连接). The legacy per-platform binding still resolves here when present AND
        // alive, but there is **no default-companion fallback** — an unbound channel is
        // hosted by no companion (历史债「渠道与远程连接默认由默认伙伴接待」已废除；连接由
        // 用户为每个伙伴显式配置). A stale legacy binding (deleted companion) degrades to
        // None too, rather than pinning sessions to a ghost.
        if let Some(plugin) = nomifun_channel::types::PluginType::from_str_opt(platform)
            && let Ok(Some(bound)) = self.channel_settings.get_master_agent_companion_id(plugin).await
            && self.companion_service.get_companion(&bound).await.is_ok()
        {
            return Some(bound);
        }
        None
    }

    async fn companion_model(&self, companion_id: &str) -> Option<nomifun_common::ProviderWithModel> {
        let profile = self.companion_service.get_companion(companion_id).await.ok()?;
        if !profile.model.is_configured() {
            return None;
        }
        Some(nomifun_common::ProviderWithModel {
            provider_id: profile.model.provider_id.clone(),
            model: profile.model.model.clone(),
            use_model: Some(profile.model.model),
        })
    }

    async fn companion_exists(&self, companion_id: &str) -> bool {
        self.companion_service.get_companion(companion_id).await.is_ok()
    }

    async fn companion_name(&self, companion_id: &str) -> Option<String> {
        self.companion_service
            .get_companion(companion_id)
            .await
            .ok()
            .map(|c| c.name)
            .filter(|n| !n.trim().is_empty())
    }

    async fn ensure_companion_session(&self, companion_id: &str) -> Option<i64> {
        // Idempotent: returns the companion's existing single session or mints a
        // new one (requires the companion's chat model to be configured, else
        // AppError::BadRequest → None so the channel layer refuses the turn with
        // a notice). companion_threads stores the id as a String (the i64 bridged
        // at the boundary); parse it back here.
        match self.companion_service.create_companion_thread(companion_id, None).await {
            Ok(thread) => match thread.conversation_id.parse::<i64>() {
                Ok(id) => Some(id),
                Err(e) => {
                    tracing::warn!(
                        companion_id = %companion_id,
                        conversation_id = %thread.conversation_id,
                        error = %e,
                        "companion session conversation_id is not a valid i64"
                    );
                    None
                }
            },
            Err(e) => {
                tracing::warn!(companion_id = %companion_id, error = %e, "ensure_companion_session failed (likely no model configured)");
                None
            }
        }
    }

    async fn public_agent_servable(&self, id: &str) -> bool {
        // Servable = the public agent exists AND is enabled. A deleted agent or a
        // disabled/paused one is NOT servable, so the channel layer refuses the
        // turn rather than serving a dead agent. The bot→agent binding itself is
        // per-bot (the channel row's `public_agent_id`); this is a pure by-id
        // liveness check.
        matches!(self.public_agent_service.get(id).await, Ok(cfg) if cfg.enabled)
    }

    async fn public_agent_exists(&self, id: &str) -> bool {
        self.public_agent_service.exists(id).await
    }

    async fn public_agent_name(&self, id: &str) -> Option<String> {
        self.public_agent_service
            .get(id)
            .await
            .ok()
            .map(|a| a.name)
            .filter(|n| !n.trim().is_empty())
    }

    async fn public_agent_model(&self, id: &str) -> Option<nomifun_common::ProviderWithModel> {
        // The agent's OWN configured model wins.
        if let Ok(cfg) = self.public_agent_service.get(id).await {
            if cfg.model.is_configured() {
                return Some(nomifun_common::ProviderWithModel {
                    provider_id: cfg.model.provider_id.clone(),
                    model: cfg.model.model.clone(),
                    // Prefer the explicit concrete model id; fall back to the label so a
                    // usable id always reaches the provider.
                    use_model: cfg.model.use_model.clone().or_else(|| Some(cfg.model.model.clone())),
                });
            }
        }
        // Otherwise fall back to the app's DEFAULT model (first enabled provider +
        // model). This is what makes a public agent "just work" the moment any
        // provider (e.g. StepFun) is configured, without per-agent model setup —
        // the owner can still pin a specific model in the console. `None` only
        // when the machine has NO enabled provider/model at all.
        let (provider_id, model) = nomifun_ai_agent::resolve_default_model(&self.provider_repo).await?;
        Some(nomifun_common::ProviderWithModel {
            provider_id,
            model: model.clone(),
            use_model: Some(model),
        })
    }

    async fn record_public_agent_turn(&self, id: &str, platform: &str, text: &str) {
        // Best-effort audit into the public agent's own day-partitioned log
        // (never fails the turn).
        self.public_agent_service
            .record_turn(id, "channel", Some(platform), text)
            .await;
    }
}

/// Build the default `ChannelRouterState` and orchestrator components.
pub async fn build_channel_state(
    services: &AppServices,
    extension_registry: ExtensionRegistry,
) -> (ChannelRouterState, ChannelOrchestratorComponents) {
    let pool = services.database.pool().clone();
    let repo: Arc<dyn nomifun_db::IChannelRepository> = Arc::new(nomifun_db::SqliteChannelRepository::new(pool));
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);

    let (message_tx, message_rx) = tokio::sync::mpsc::channel(256);
    let (confirm_tx, confirm_rx) = tokio::sync::mpsc::channel(256);

    let manager = Arc::new(nomifun_channel::manager::ChannelManager::new(
        repo.clone(),
        services.event_bus.clone(),
        encryption_key,
        message_tx,
        confirm_tx,
    ));

    let pairing_service = Arc::new(nomifun_channel::pairing::PairingService::new(
        repo.clone(),
        services.event_bus.clone(),
    ));

    // Expired pairing codes are purged only by this background sweep — the
    // timer existed but had no caller, so stale codes lingered in the DB
    // indefinitely. Deliberately detached (handle dropped): like the channel
    // orchestrator and plugin restore tasks, it runs for the process lifetime.
    let _pairing_cleanup = nomifun_channel::pairing::PairingService::start_cleanup_timer(repo.clone());

    let session_manager = Arc::new(nomifun_channel::session::SessionManager::new(repo.clone()));

    let plugin_factory: Arc<nomifun_channel::manager::PluginFactory> =
        Arc::new(Box::new(nomifun_channel::plugins::create_plugin));

    // Build channel settings service for per-plugin agent/model configuration
    let pref_pool = services.database.pool().clone();
    let pref_repo: Arc<dyn nomifun_db::IClientPreferenceRepository> =
        Arc::new(SqliteClientPreferenceRepository::new(pref_pool));
    let channel_settings = Arc::new(nomifun_channel::channel_settings::ChannelSettingsService::new(
        pref_repo,
    ));

    // The owner/system identity: the primary WebUI user when one exists, else the
    // system fallback. Used as the conversation owner for channel turns AND as the
    // identity auto-served strangers run under on a public-agent-bound platform.
    let owner_user_id = services
        .user_repo
        .get_primary_webui_user()
        .await
        .ok()
        .flatten()
        .map(|u| u.id)
        .unwrap_or_else(|| "system_default_user".to_string());

    // Build orchestrator dependencies. The fallback agent type for the
    // `agent.select` action mirrors `ChannelSettingsService`'s default
    // ("nomi") so the two resolution paths cannot drift apart.
    let action_executor = Arc::new(
        nomifun_channel::action::ActionExecutor::new(
            Arc::clone(&pairing_service),
            Arc::clone(&session_manager),
            Arc::clone(&channel_settings),
            "nomi",
        )
        // Opt-in IM → requirement pipeline: the creator is always wired, but the
        // per-platform `routeToRequirement` setting (default off) gates it, so
        // behaviour is unchanged until a channel enables it.
        .with_requirement_creator(Some(
            nomifun_requirement::RequirementServiceSink::creator_arc(
                services.requirement_service.clone(),
            ),
        )),
    );

    let conv_repo: Arc<dyn nomifun_db::IConversationRepository> = Arc::new(
        nomifun_db::SqliteConversationRepository::new(services.database.pool().clone()),
    );
    let skill_resolver = Arc::new(nomifun_conversation::skill_resolver::ExtensionSkillResolver::new(
        services.skill_paths.clone(),
    ));
    let agent_metadata_repo: Arc<dyn nomifun_db::IAgentMetadataRepository> = Arc::new(
        nomifun_db::SqliteAgentMetadataRepository::new(services.database.pool().clone()),
    );
    let acp_session_repo: Arc<dyn nomifun_db::IAcpSessionRepository> = Arc::new(
        nomifun_db::SqliteAcpSessionRepository::new(services.database.pool().clone()),
    );
    let conversation_svc = Arc::new(
        ConversationService::new(
            services.work_dir.clone(),
            services.event_bus.clone(),
            skill_resolver,
            services.worker_task_manager.clone(),
            conv_repo,
            agent_metadata_repo,
            acp_session_repo,
        )
        .with_runtime_state(services.conversation_runtime_state.clone()),
    );
    conversation_svc.with_mcp_server_repo(Arc::new(nomifun_db::SqliteMcpServerRepository::new(
        services.database.pool().clone(),
    )));
    // Phase 3: channel master-agent turns run the nomi send loop too.
    conversation_svc.with_failover_deps(
        Arc::new(SqliteProviderRepository::new(services.database.pool().clone())),
        Arc::new(SqliteClientPreferenceRepository::new(services.database.pool().clone())),
    );
    if let Some(hook) = services.task_manager_delete_hook.clone() {
        conversation_svc.with_delete_hook(hook);
    }

    // Master-agent profile: per-platform companion binding + model resolution for
    // master-mode sessions and companion-id validation for the binding write
    // route, PLUS the 对外伙伴 (public agent) resolution/validation/audit for the
    // symmetric public-agent binding. One instance shared by the message service
    // and the router state.
    let master_profile: Arc<dyn nomifun_channel::message_service::MasterAgentProfile> =
        Arc::new(CompanionMasterAgentProfile {
            companion_service: services.companion_service.clone(),
            channel_settings: Arc::clone(&channel_settings),
            public_agent_service: services.public_agent_service.clone(),
            provider_repo: services.provider_repo.clone(),
        });

    let message_service = Arc::new(
        nomifun_channel::message_service::ChannelMessageService::new(
            conversation_svc,
            services.worker_task_manager.clone(),
            Arc::clone(&channel_settings),
            repo.clone(),
            owner_user_id,
        )
        // Master-agent mode: per-channel companion binding (with legacy platform
        // fallback) + model resolution fall back to the bound companion when the
        // platform has no config of its own.
        .with_master_profile(Arc::clone(&master_profile))
        // Outbound media: resolve `wsa_…` ids to bytes so channel replies can
        // send AI-generated images/files.
        .with_asset_resolver(Arc::new(crate::channel_asset_resolver::ChannelAssetResolver::new(
            services.workshop_service.clone(),
        ))),
    );

    let orchestrator = nomifun_channel::orchestrator::ChannelOrchestrator::new(
        action_executor,
        message_service,
        Arc::clone(&session_manager),
        manager.clone() as Arc<dyn nomifun_channel::stream_relay::ChannelSender>,
    );

    let state = ChannelRouterState {
        manager: Arc::clone(&manager),
        pairing_service,
        session_manager,
        repo,
        plugin_factory: Arc::clone(&plugin_factory),
        settings_service: channel_settings,
        master_profile: Some(master_profile),
        extension_registry,
    };

    let components = ChannelOrchestratorComponents {
        orchestrator,
        message_rx,
        confirm_rx,
        manager,
        plugin_factory,
    };

    (state, components)
}

/// Build the default `TerminalRouterState` from application services.
pub fn build_terminal_state(services: &AppServices) -> TerminalRouterState {
    // Late-wire the knowledge service into the terminal singleton (same
    // pattern as `ConversationService::with_knowledge_service`): terminal
    // create/relaunch then binds + mounts knowledge bases into the session
    // cwd. Interior mutability means every clone of the singleton (cron
    // executor, AutoWork driver) sees the wiring too.
    services
        .terminal_service
        .with_knowledge_service(services.knowledge_service.clone());
    // Clear the dual-domain owner of any requirement this terminal owned —
    // requirements.owner_session_id (owner_kind='terminal') has no FK to
    // cascade (spec §9.B). Mirror of the conversation delete hook.
    services
        .terminal_service
        .with_delete_hook(services.requirement_service.clone() as Arc<dyn OnTerminalDelete>);
    // Drop this terminal's IDMM decision records (disposable audit trail,
    // polymorphic target_id with no FK — app-level cascade, mirror of the
    // conversation delete hook).
    services
        .terminal_service
        .with_delete_hook(Arc::new(IdmmRecordCascade {
            records: Arc::new(SqliteIdmmInterventionRepository::new(services.database.pool().clone())),
        }) as Arc<dyn OnTerminalDelete>);
    // Reuse the singleton terminal service (owns the live PTY map), so the
    // terminal routes and the AutoWork orchestrator share the same PTYs.
    TerminalRouterState::new(services.terminal_service.clone())
}

/// Build the `RequirementRouterState` + `IdmmRouterState` from application
/// services.
///
/// Reuses the singleton `requirement_service` (which shares its repo + WS emitter
/// with the nomi native-tool sink), attaches a `ConversationService` + repo to a
/// clone for AutoWork config persistence, builds the AutoWork orchestrator, and
/// constructs the IDMM supervisor sharing the same live-session collaborators
/// (threaded back into the orchestrator as its `IdmmHandle`).
pub fn build_requirement_state(services: &AppServices) -> (RequirementRouterState, IdmmRouterState) {
    let pool = services.database.pool().clone();

    // Build a ConversationService exactly like build_cron_state does, for
    // injection into the orchestrator + AutoWork config reads/writes.
    let conv_repo: Arc<dyn nomifun_db::IConversationRepository> =
        Arc::new(SqliteConversationRepository::new(pool.clone()));
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let acp_session_repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(pool.clone()));
    let skill_resolver = Arc::new(nomifun_conversation::skill_resolver::ExtensionSkillResolver::new(
        services.skill_paths.clone(),
    ));
    let conv_service = ConversationService::new(
        services.work_dir.clone(),
        services.event_bus.clone(),
        skill_resolver,
        services.worker_task_manager.clone(),
        conv_repo.clone(),
        agent_metadata_repo,
        acp_session_repo,
    )
    .with_runtime_state(services.conversation_runtime_state.clone());
    // Phase 3: AutoWork-driven nomi turns run the send loop, and IDMM fault
    // supervision (Task 3) reuses `perform_model_failover` — wire the deps here too.
    conv_service.with_failover_deps(
        Arc::new(SqliteProviderRepository::new(pool.clone())),
        Arc::new(SqliteClientPreferenceRepository::new(pool.clone())),
    );

    // Router-state service: the singleton plus conversation service + repo for
    // AutoWork config, plus the terminal driver for terminal-target AutoWork. The
    // sink (built in AppServices) keeps using the plain singleton — it never needs
    // the autowork-config methods.
    let terminal_driver: Arc<dyn nomifun_terminal::TerminalDriver> = services.terminal_service.clone();
    let terminal_repo: Arc<dyn nomifun_db::ITerminalRepository> =
        Arc::new(nomifun_db::SqliteTerminalRepository::new(pool.clone()));
    // Shared AutoWork waker: the service fires it when a requirement becomes
    // claimable so idle orchestrator loops pick up new work without polling delay.
    let autowork_waker = Arc::new(tokio::sync::Notify::new());
    let requirement_service = Arc::new(
        (*services.requirement_service)
            .clone()
            .with_conversation_service(conv_service.clone(), conv_repo.clone())
            .with_terminal_driver(terminal_driver.clone())
            .with_terminal_repo(terminal_repo)
            .with_autowork_waker(autowork_waker.clone()),
    );

    // ── IDMM: build the supervisor manager + service, sharing the same
    // ConversationService / repo / terminal driver this orchestrator drives, so
    // IDMM observes the exact same live sessions. The manager is threaded into
    // OrchestratorDeps.idmm so AutoWork ensures supervision per turn.
    let idmm_state = build_idmm_state(
        services,
        conv_service.clone(),
        conv_repo.clone(),
        terminal_driver.clone(),
    );
    let idmm_handle: Arc<dyn nomifun_requirement::IdmmHandle> = Arc::new(idmm_state.service.manager().clone());

    // NOTE: the ConversationSupervisionHook for user-driven chat turns is
    // registered in `build_module_states` on the ROUTE ConversationService
    // (`build_conversation_state`'s instance), not here — this `conv_service` is
    // moved into `OrchestratorDeps` below and only drives AutoWork, which arms
    // IDMM per loop iteration via `OrchestratorDeps.idmm` (so a hook here was
    // dead code that fooled debugging into thinking arming was wired).

    let deps = Arc::new(nomifun_requirement::OrchestratorDeps {
        service: requirement_service.clone(),
        task_manager: services.worker_task_manager.clone(),
        conversation_service: conv_service,
        conversation_repo: conv_repo,
        agent_registry: services.agent_registry.clone(),
        terminal_driver: Some(terminal_driver),
        idmm: Some(idmm_handle),
        wake: autowork_waker,
        // ACP sessions expose the requirement declaration tools only when the
        // requirement MCP server started + was plumbed into the agent factory.
        requirement_mcp_enabled: services.requirement_mcp_config.is_some(),
    });
    let orchestrator = Arc::new(nomifun_requirement::Orchestrator::new(deps));
    // Start the periodic lease sweeper (re-pends stale claims from dead sessions).
    orchestrator.start_sweeper();
    // Resume every persisted-enabled binding so bound sessions work in the
    // background from boot — no foreground/session-page visit required.
    orchestrator.resume_persisted_bindings(services.user_repo.clone());

    (
        RequirementRouterState {
            requirement_service,
            orchestrator,
        },
        idmm_state,
    )
}

/// Build the `WebhookRouterState` (webhook CRUD + per-tag settings). Constructs
/// fresh repos + a platform-dispatching sender from the pool, matching the per-builder pattern.
/// Shares the same DB tables as the completion notifier wired in `AppServices`.
pub fn build_webhook_state(services: &AppServices) -> WebhookRouterState {
    let pool = services.database.pool().clone();
    let webhook_repo: Arc<dyn nomifun_db::IWebhookRepository> =
        Arc::new(nomifun_db::SqliteWebhookRepository::new(pool.clone()));
    let tag_setting_repo: Arc<dyn nomifun_db::ITagSettingRepository> =
        Arc::new(nomifun_db::SqliteTagSettingRepository::new(pool));
    let sender: Arc<dyn nomifun_webhook::WebhookSender> = Arc::new(nomifun_webhook::DefaultWebhookSender::new());
    let service = nomifun_webhook::WebhookService::new(webhook_repo, tag_setting_repo, sender);
    WebhookRouterState { service }
}

/// Build the 创意工坊 (Creative Workshop) router state, reusing the singleton
/// `workshop_service` (canvas/asset CRUD + on-disk docs/binaries).
pub fn build_workshop_state(services: &AppServices) -> WorkshopRouterState {
    WorkshopRouterState::new(services.workshop_service.clone())
}

/// Build the 生成引擎 (creation) router state, reusing the singleton
/// `creation_service`, and settle any task left live by a previous process
/// (boot reconciliation — "running ⟺ live worker" invariant). The reconcile is
/// detached + best-effort, mirroring the other boot-resume tasks.
pub fn build_creation_state(services: &AppServices) -> CreationRouterState {
    let service = services.creation_service.clone();
    {
        let service = service.clone();
        tokio::spawn(async move {
            service.reconcile_on_boot().await;
        });
    }
    CreationRouterState::new(service)
}

/// Wraps [`ConversationService::cancel`] so the [`RunEngine`] can end an
/// in-flight worker conversation when its run is cancelled (P2 Task 3). Holds a
/// (cloned) `ConversationService` + the shared `worker_task_manager` so a cancel
/// finds the live agent; runs as [`nomifun_auth::SYSTEM_USER_ID`] (the same user
/// the worker creates conversations as). Idempotent — a missing/finished
/// conversation or no live agent is a silent no-op (see `ConversationService::cancel`).
struct OrchestratorConversationCanceller {
    conv: ConversationService,
    task_manager: Arc<dyn IWorkerTaskManager>,
}

#[async_trait::async_trait]
impl ConversationCanceller for OrchestratorConversationCanceller {
    async fn cancel(&self, conversation_id: i64) {
        if let Err(e) = self
            .conv
            .cancel(
                nomifun_auth::SYSTEM_USER_ID,
                &conversation_id.to_string(),
                &self.task_manager,
            )
            .await
        {
            tracing::warn!(
                conversation_id,
                error = %e,
                "Orchestrator cancel: failed to cancel in-flight worker conversation"
            );
        }
    }
}

/// Wraps [`ConversationService::steer_message`] so the [`RunEngine`] can mid-turn
/// inject a supervisor message into an in-flight worker conversation (P3b). Holds
/// a (cloned) `ConversationService` + the shared `worker_task_manager`; runs as
/// [`nomifun_auth::SYSTEM_USER_ID`] (the user the worker creates conversations
/// as). `steer_message` injects into the live Nomi turn when one exists, else
/// falls back to a fresh send; a non-Nomi engine that cannot steer surfaces as an
/// error (the engine maps it to a 400). We discard the returned message id — the
/// caller only needs success/failure.
struct OrchestratorConversationSteerer {
    conv: ConversationService,
    task_manager: Arc<dyn IWorkerTaskManager>,
}

#[async_trait::async_trait]
impl ConversationSteerer for OrchestratorConversationSteerer {
    async fn steer(&self, conversation_id: i64, text: &str) -> Result<(), nomifun_common::AppError> {
        self.conv
            .steer_message(
                nomifun_auth::SYSTEM_USER_ID,
                &conversation_id.to_string(),
                nomifun_api_types::SendMessageRequest {
                    content: text.to_owned(),
                    files: vec![],
                    inject_skills: vec![],
                    hidden: false,
                    origin: Some("orchestrator".to_owned()),
                    channel_platform: None,
                },
                &self.task_manager,
            )
            .await
            .map(|_msg_id| ())
    }
}

/// Wraps [`ConversationService::steer_message`] so the [`RunEngine`] can report a
/// run's terminal / notable outcome back to the LEAD conversation that launched it
/// — re-engaging the master agent exactly once per notable event to summarize /
/// re-strategize / relay, instead of the old LLM busy-poll loop. Unlike the worker
/// steerer/canceller (which run as [`nomifun_auth::SYSTEM_USER_ID`] on
/// orchestrator-owned worker conversations), the LEAD conversation is owned by a
/// REAL user, so we resolve its owner from the row (mirroring cron's
/// `resolve_target_conversation_user_id`) — a `SYSTEM_USER_ID` steer would fail the
/// ownership check. The receipt PROMPT is `hidden:true` (it never shows as a user
/// bubble); the master agent's summary REPLY is the visible turn. A dedicated
/// `origin` keeps companion-memory / idmm from treating it as owner speech and lets
/// tool gating recognize it. Best-effort: a missing lead conversation or a read
/// error is a silent skip — a report must never fail the (already-finalized) run.
struct OrchestratorLeadReporter {
    conv: ConversationService,
    task_manager: Arc<dyn IWorkerTaskManager>,
    /// Read the run row (autonomy/goal/total_tokens) + its task list so an
    /// AUTONOMOUS run's receipt can be goal/budget-aware (Phase 3b Task 2) — the
    /// master self-assesses instead of the autonomy-blind "仅汇总" text. Best-effort:
    /// any read error falls back to the autonomy-blind receipt.
    run_repo: Arc<dyn nomifun_db::IRunRepository>,
    /// In-memory no-progress streak, `run_id → (last_done_count, streak)`. For an
    /// autonomous run, each report compares the current done-node count against the
    /// last one; a non-increase bumps the streak (else resets to 0). At
    /// [`nomifun_orchestrator::NO_PROGRESS_LIMIT`] the loop is judged stuck and the
    /// receipt tells the master to wrap up. Best-effort: a poisoned lock → no_progress=false.
    no_progress: Arc<std::sync::Mutex<std::collections::HashMap<String, (usize, u32)>>>,
}

/// Build the hidden receipt prompt injected into the lead conversation for a run's
/// outcome. It hands the master agent the per-node digest and tells it how to react.
///
/// For a NON-autonomous run (`autonomy != "autonomous"`) the behavior is unchanged:
/// summarize / re-strategize / relay — never spawn a fresh orchestration (the 会话9
/// loop guard). For an AUTONOMOUS run reaching a terminal outcome
/// (`Completed`/`Failed`) the receipt is instead a SELF-ASSESS prompt (Phase 3b Task
/// 2): it hands the master the goal + per-node brief + budget headroom
/// (`rounds_left`/`tokens_left`) and drives the emergent loop — 已达成→收尾不追加；
/// 未达成且有预算→用 `nomi_run_add_tasks` 往同一个 run 追加继续；预算耗尽/连续无进展→
/// 汇总进展交给用户。All other autonomous outcomes fall through to the shared arms.
fn compose_lead_receipt(
    run_id: &str,
    outcome: RunOutcome,
    brief: &str,
    autonomy: &str,
    goal: &str,
    task_count: usize,
    total_tokens: i64,
    no_progress: bool,
) -> String {
    // Autonomous terminal outcome → goal/budget-aware self-assess prompt.
    if autonomy == "autonomous" && matches!(outcome, RunOutcome::Completed | RunOutcome::Failed) {
        let rounds_left = nomifun_orchestrator::AUTONOMOUS_MAX_TASKS.saturating_sub(task_count);
        let tokens_left = nomifun_orchestrator::AUTONOMOUS_TOKEN_BUDGET - total_tokens;
        // Budget exhausted (no round OR no token headroom) or the loop stopped making
        // progress → the master must NOT append; a further append would be refused by
        // the gateway budget backstop anyway.
        let exhausted = no_progress || rounds_left == 0 || tokens_left <= 0;
        let phase = match outcome {
            RunOutcome::Failed => "本轮部分或全部子任务执行失败",
            _ => "本轮子任务已全部完成",
        };
        let head = format!(
            "[自主编排回执] 你正在自主推进这个 run（run {run_id}）。{phase}。\n\
             【目标】{goal}\n【本轮各节点产出】\n{brief}\n\
             【预算余量】剩余可追加任务数 {rounds_left}，剩余 token 约 {tokens_left}。\n"
        );
        if exhausted {
            return format!(
                "{head}\
                 预算已达上限或已连续多轮无实质进展：请**不要再用 nomi_run_add_tasks / \
                 nomi_run_adjust / nomi_task_rerun 继续推进本 run，也不要 nomi_run_create 新建 run**。\
                 用一段自然的话向用户汇总目前已完成的部分、说明尚未达成的目标，并把后续决定交给用户。"
            );
        }
        return format!(
            "{head}\
             请先自评是否已达成上述【目标】，再据下列规则行动：\n\
             - 已达成 → 用一段自然的话向用户汇总最终结论，本轮结束，**不要再追加**。\n\
             - 未达成且仍有预算（剩余可追加任务数 {rounds_left}）→ 用 `nomi_run_add_tasks` 向\
             **同一个 run（run {run_id}）** 追加下一批任务继续推进，然后结束本轮（勿轮询、勿新建 run）。\n\
             - 预算即将耗尽或已连续多轮无进展 → 汇总当前进展、说明未竟部分并交给用户，**不要再追加**。"
        );
    }
    match outcome {
        RunOutcome::Completed => format!(
            "[编排回执] 你之前派发的子任务（run {run_id}）已全部完成。各节点产出：\n{brief}\n\
             请据此用一段自然的话向用户汇总产出与结论。仅汇总，不要再发起新的编排。"
        ),
        RunOutcome::Failed => format!(
            "[编排回执] 你之前派发的子任务（run {run_id}）执行失败。各节点状态与产出：\n{brief}\n\
             请向用户说明哪些完成、哪些失败及可能原因；若可行，简述你将如何调整策略重试，\
             否则请询问用户如何处理。不要重复创建相同编排。"
        ),
        RunOutcome::Stalled => format!(
            "[编排回执] 你之前派发的子任务（run {run_id}）长时间无进展，已被系统判定为卡死并终止。\
             已知进度：\n{brief}\n请如实告知用户，并说明你打算如何处理（重试/换方式/请用户确认）。\
             不要重复创建相同编排。"
        ),
        RunOutcome::AwaitingApproval => format!(
            "[编排回执] 编排（run {run_id}）计划已拟好，正等待用户批准后执行。\
             请转达用户在编排面板点「批准执行」或直接回复批准。"
        ),
        RunOutcome::NodeFailed => format!(
            "[编排回执·中途] 你派发的编排（run {run_id}）中有一个节点永久失败，run 仍在进行：\n{brief}\n\
             你可以现在就介入：给该节点换更合适的模型后重跑、调整编排（保留已完成部分）、\
             采纳该节点当前产出，或放弃。若暂不处理，run 会按其它独立分支继续，终态时再给你完整回执。\
             不要重复创建相同编排。"
        ),
        RunOutcome::BatchProgress => format!(
            "[编排回执·进展] 你派发的编排（run {run_id}）刚完成了一批节点，run 仍在进行：\n{brief}\n\
             可据此判断是否需要追加工作：如有必要用 nomi_run_add_tasks 往同一个 run 追加任务；\
             否则继续等待，终态时会给你完整回执。不要新建编排。"
        ),
        RunOutcome::NodeQuestion => format!(
            "[编排回执·节点提问] 你派发的编排（run {run_id}）中有一个节点遇到决策问题、已挂起等待人工选择：\n{brief}\n\
             本 run 处于「审批模式」：请立即用一段话提醒用户——该节点已在画布与进度条上标出提问图标，\
             请用户点击进入该节点会话，直接查看问题并回复选择；回复后在该节点点「采用为该节点产出」即可继续。\
             你自己不要替用户作答，也不要重复创建编排。"
        ),
    }
}

#[async_trait::async_trait]
impl LeadReporter for OrchestratorLeadReporter {
    async fn report(
        &self,
        lead_conv_id: i64,
        run_id: &str,
        outcome: RunOutcome,
        brief: &str,
    ) -> Result<(), nomifun_common::AppError> {
        // Resolve the lead conversation's REAL owner (steer_message enforces
        // ownership). Missing conversation / read error → silent skip (best-effort).
        let row = match self.conv.conversation_repo().get(lead_conv_id).await {
            Ok(Some(row)) => row,
            Ok(None) => return Ok(()),
            Err(e) => {
                tracing::warn!(lead_conv_id, run_id, error = %e, "lead report: read owner failed — skipping");
                return Ok(());
            }
        };
        let owner = if row.user_id.trim().is_empty() {
            nomifun_auth::SYSTEM_USER_ID.to_owned()
        } else {
            row.user_id
        };
        // Autonomy/goal/budget context (Phase 3b Task 2): best-effort read the run
        // row (autonomy/goal/total_tokens) + its task list so an autonomous run's
        // receipt can be self-assess-driven. Any read error → autonomy-blind fallback
        // (empty `autonomy` selects the unchanged non-autonomous arms).
        let (autonomy, goal, total_tokens) = match self.run_repo.get_run(run_id).await {
            Ok(Some(run)) => (run.autonomy, run.goal, run.total_tokens.unwrap_or(0)),
            Ok(None) => (String::new(), String::new(), 0),
            Err(e) => {
                tracing::warn!(run_id, error = %e, "lead report: read run failed — autonomy-blind receipt");
                (String::new(), String::new(), 0)
            }
        };
        let tasks = match self.run_repo.list_tasks(run_id).await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(run_id, error = %e, "lead report: list tasks failed — budget headroom blind");
                Vec::new()
            }
        };
        let task_count = tasks.len();
        // No-progress streak (autonomous only): if the count of `done` nodes did NOT
        // increase since the last report, bump the streak, else reset to 0. At
        // NO_PROGRESS_LIMIT the receipt treats the loop as stuck. Best-effort — a
        // poisoned lock just treats no_progress=false.
        let no_progress = if autonomy == "autonomous" {
            let done = tasks.iter().filter(|t| t.status == "done").count();
            match self.no_progress.lock() {
                Ok(mut map) => {
                    let entry = map.entry(run_id.to_owned()).or_insert((0, 0));
                    if done > entry.0 {
                        entry.0 = done;
                        entry.1 = 0;
                    } else {
                        entry.1 = entry.1.saturating_add(1);
                    }
                    let np = entry.1 >= nomifun_orchestrator::NO_PROGRESS_LIMIT;
                    // Evict when the autonomous loop is ENDING — a terminal outcome that is
                    // budget/round/progress-exhausted (the gateway backstop will refuse any
                    // further re-arm) — so the map does not grow one entry per run forever.
                    let ending = matches!(outcome, RunOutcome::Completed | RunOutcome::Failed)
                        && (np
                            || task_count >= nomifun_orchestrator::AUTONOMOUS_MAX_TASKS
                            || total_tokens >= nomifun_orchestrator::AUTONOMOUS_TOKEN_BUDGET);
                    if ending {
                        map.remove(run_id);
                    }
                    np
                }
                Err(_) => false,
            }
        } else {
            false
        };
        self.conv
            .steer_message(
                &owner,
                &lead_conv_id.to_string(),
                nomifun_api_types::SendMessageRequest {
                    content: compose_lead_receipt(
                        run_id,
                        outcome,
                        brief,
                        &autonomy,
                        &goal,
                        task_count,
                        total_tokens,
                        no_progress,
                    ),
                    files: vec![],
                    inject_skills: vec![],
                    hidden: true,
                    origin: Some("orchestrator_report".to_owned()),
                    channel_platform: None,
                },
                &self.task_manager,
            )
            .await
            .map(|_msg_id| ())
    }
}
/// [`RunService`] (create/plan/inspect/cancel) + [`RunEngine`] (serial execution
/// loop). The fleet/workspace repos + the run repo are root-re-exported from
/// `nomifun_db`; the worker runs tasks on real nomi conversations via a
/// `ConversationWorkerRunner` built from a fresh `ConversationService` (the same
/// recipe `build_cron_state` / `build_requirement_state` use).
///
/// Task 9 (app integration): the [`PlanProducer`] is a real [`LlmPlanProducer`]
/// (lead resolved at plan time — see the LEAD CHOICE note inline), the worker is
/// the production [`ConversationWorkerRunner`], and every persisted `running` run
/// resumes its loop at boot via [`RunEngine::resume_persisted_runs`].
pub fn build_orchestrator_state(
    services: &AppServices,
    idmm_hook: Arc<dyn nomifun_conversation::ConversationSupervisionHook>,
) -> OrchestratorRouterState {
    let pool = services.database.pool().clone();
    let fleet_repo: Arc<dyn nomifun_db::IFleetRepository> =
        Arc::new(nomifun_db::SqliteFleetRepository::new(pool.clone()));
    let ws_repo: Arc<dyn nomifun_db::IOrchWorkspaceRepository> =
        Arc::new(nomifun_db::SqliteOrchWorkspaceRepository::new(pool.clone()));
    let run_repo: Arc<dyn nomifun_db::IRunRepository> =
        Arc::new(nomifun_db::SqliteRunRepository::new(pool.clone()));
    let fleet = nomifun_orchestrator::FleetService::new(fleet_repo.clone());
    let workspace = nomifun_orchestrator::WorkspaceService::new(ws_repo.clone());

    // Realtime emitter shares the app's broadcast bus.
    let emitter = OrchestratorRunEventEmitter::new(services.event_bus.clone());

    // Worker = a fresh nomi conversation. Build a ConversationService mirroring
    // build_cron_state's recipe, then a ConversationWorkerRunner over it.
    let conv_repo: Arc<dyn nomifun_db::IConversationRepository> =
        Arc::new(SqliteConversationRepository::new(pool.clone()));
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let acp_session_repo: Arc<dyn IAcpSessionRepository> =
        Arc::new(SqliteAcpSessionRepository::new(pool.clone()));
    let skill_resolver = Arc::new(nomifun_conversation::skill_resolver::ExtensionSkillResolver::new(
        services.skill_paths.clone(),
    ));
    let conv_service = ConversationService::new(
        services.work_dir.clone(),
        services.event_bus.clone(),
        skill_resolver,
        services.worker_task_manager.clone(),
        conv_repo,
        agent_metadata_repo,
        acp_session_repo,
    )
    .with_runtime_state(services.conversation_runtime_state.clone());
    // A worker turn runs the same nomi send loop as a plain conversation, so wire
    // the failover deps here too — parity with build_requirement_state's
    // ConversationService (the seam self-gates on AgentType::Nomi).
    conv_service.with_failover_deps(
        Arc::new(SqliteProviderRepository::new(pool.clone())),
        Arc::new(SqliteClientPreferenceRepository::new(pool.clone())),
    );
    // P3b: arm IDMM supervision on the worker's ConversationService so each worker
    // (autonomous nomi yolo) turn fires `on_turn_start` and IDMM auto-resolves
    // decisions/open-questions instead of stalling to timeout. `with_supervision_hook`
    // is `&self` interior-mutable, so the cancel/steer/worker clones below all share
    // this same armed hook slot. The hook is the same IdmmManager (as a
    // `ConversationSupervisionHook`) the route/terminal instances use — threaded in
    // from `build_module_states` (no orchestrator→idmm dependency).
    conv_service.with_supervision_hook(idmm_hook);
    // Cancel hook (P2 Task 3): clone the ConversationService for a canceller that
    // ends an in-flight worker conversation on run cancel. `ConversationService`
    // is `Clone` (Arc internals), so the clone shares the same runtime state /
    // failover deps as the worker's instance. Clone BEFORE moving the original
    // into the worker.
    let cancel_conversation: Arc<dyn ConversationCanceller> =
        Arc::new(OrchestratorConversationCanceller {
            conv: conv_service.clone(),
            task_manager: services.worker_task_manager.clone(),
        });
    // Steer hook (P3b): a second clone of the ConversationService lets the engine
    // mid-turn inject a supervisor message into an in-flight worker conversation
    // (`steer_message`). Clone BEFORE moving the original into the worker.
    let steer_conversation: Arc<dyn ConversationSteerer> =
        Arc::new(OrchestratorConversationSteerer {
            conv: conv_service.clone(),
            task_manager: services.worker_task_manager.clone(),
        });
    // Lead 回执 hook：run 到终态时，把结果回执给发起它的 LEAD 会话（解析真实 owner
    // 后 steer_message），触发主 agent 一次汇总/重策略——取代旧的 LLM busy-poll。
    // 第三个 ConversationService 克隆，须在 conv_service 移入 worker 之前克隆。
    let lead_reporter: Arc<dyn LeadReporter> = Arc::new(OrchestratorLeadReporter {
        conv: conv_service.clone(),
        task_manager: services.worker_task_manager.clone(),
        // Same run repo the RunService/engine use (Arc-internal clone, same DB) — the
        // reporter reads the live run row + tasks to make the receipt autonomy/goal/
        // budget-aware.
        run_repo: run_repo.clone(),
        no_progress: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    });
    // Path B (B3): the `create_adhoc_run` route associates the originating
    // conversation as the run's lead (`link_orchestrator_run`) when the request
    // carries a `lead_conv_id`. Hand it the SAME already-constructed
    // ConversationService the worker / canceller / steerer share (cheap
    // `Arc`-internal clone, same DB) — NOT a second instance. Clone BEFORE moving
    // the original into the worker.
    let route_conversation = conv_service.clone();
    let worker: Arc<dyn WorkerRunner> = Arc::new(ConversationWorkerRunner::new(
        conv_service,
        services.worker_task_manager.clone(),
        nomifun_auth::SYSTEM_USER_ID.to_string(),
    ));

    // Planner: a production LlmPlanProducer making one structured one-shot call
    // against a "lead" model to decompose the goal into a task DAG. The lead is
    // resolved against the live provider repo at `plan()` time via
    // `resolve_provider_config`, so the provider/encryption_key/workspace are
    // captured here (mirroring the IDMM sidecar's LiveCompleter recipe).
    //
    // LEAD CHOICE (P1): `build_orchestrator_state` is synchronous (it returns the
    // state, not a future), so we do not query the provider table here to pick a
    // concrete lead. Instead the lead is a deferred placeholder (empty
    // provider_id/model). On a machine with NO provider configured (CI / a fresh
    // install), `resolve_provider_config` errors at plan time → `RunService::plan`
    // surfaces it → `POST /runs` returns the error; the run row stays in
    // `planning` and is re-plannable once a provider exists. On a configured
    // machine the operator's provider drives planning. The full multi-task
    // planning path is validated by the real-machine acceptance run with a
    // configured provider; CI proves the route+engine seam with the mock stack in
    // `tests/orchestrator_run_e2e.rs`. Selecting a real lead from the run's fleet
    // snapshot (members already carry provider+model) is a natural follow-up.
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool));
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);
    let lead = nomifun_common::ProviderWithModel {
        provider_id: String::new(),
        model: String::new(),
        use_model: None,
    };
    let planner: Arc<dyn PlanProducer> = Arc::new(LlmPlanProducer::new(
        provider_repo,
        encryption_key,
        services.work_dir.clone(),
        lead,
    ));

    let run_service = Arc::new(RunService::new(
        run_repo.clone(),
        fleet_repo,
        ws_repo.clone(),
        planner.clone(),
        emitter.clone(),
    ));
    let mut engine_deps = RunEngineDeps::new(run_repo.clone(), worker, emitter, ws_repo);
    engine_deps.cancel_conversation = cancel_conversation;
    engine_deps.steer_conversation = steer_conversation;
    engine_deps.lead_reporter = lead_reporter;
    // Phase 1b: an ad-hoc/conversation-native run (no workspace, no work_dir) gets a
    // per-run SHARED working directory auto-allocated under
    // `{work_dir}/orchestrator/runs/{run_id}`, so all its nodes share one cwd (files
    // + a shared workpath KB binding). `services.work_dir` is the app data root.
    engine_deps.data_dir = services.work_dir.clone();
    // B2: the engine summarizes a COMPLETED run with the SAME LlmPlanProducer the
    // RunService plans with — a one-shot lead summary. Shared `Arc`, so the lead is
    // resolved the same way (off the run's fleet snapshot). Fail-soft in the engine:
    // a summary error falls back to the mechanical concat (never fails the run).
    engine_deps.summarizer = planner;
    let engine = RunEngine::new(Arc::new(engine_deps));
    // Boot-resume: every persisted `running` run resumes its execution loop from
    // boot (the running set is in-memory but run status is persisted), so a run
    // that was mid-flight when the process restarted keeps going without a
    // foreground visit. Detached + best-effort, mirroring the requirement
    // orchestrator's `resume_persisted_bindings`.
    engine.resume_persisted_runs(run_repo);
    // Run-level stall watchdog (safety net): periodically finalize any run stuck
    // non-terminal with no live loop (soft-strand) or a long-hung `planning`
    // (planner black hole) as failed(stalled) + report it to the lead — so no run is
    // ever a silent black hole the master agent waits on forever. Detached; the
    // engine's `Clone` shares the same deps/handles. A 120s startup delay lets
    // boot-resume restart persisted loops first, so a resuming run is never
    // mistaken for a strand (doubly guarded by `STRAND_GRACE_MS`).
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(120)).await;
            loop {
                engine.reap_stalled_runs().await;
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
    }
    OrchestratorRouterState::new(fleet, workspace, run_service, engine, route_conversation)
}

/// **P3-X2**: build the `SecretRouterState` (browser-use credential CRUD).
/// The service holds the app data dir (去 per-pet 键化: browser identity globally
/// shared — one vault under `{data_dir}/browser-secrets/shared`) + the machine-bound
/// `encryption_key` (`derive_encryption_key`, the same `[u8; 32]` the session/factory
/// side uses to build the `SecretStore`), so a secret registered here decrypts in a session.
pub fn build_secret_state(services: &AppServices) -> SecretRouterState {
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);
    let service = nomifun_secret::SecretService::new(services.data_dir.clone(), encryption_key);
    SecretRouterState::new(service)
}

/// Build the `IdmmRouterState` (the IDMM supervisor manager + service). Shares
/// the caller's `ConversationService` / conversation repo / terminal driver so
/// IDMM supervises the same live sessions AutoWork + the UI drive. Constructs a
/// fresh provider repo + client-preferences repo + encryption key from the pool
/// (matching the per-builder pattern used elsewhere in this module).
pub fn build_idmm_state(
    services: &AppServices,
    conv_service: ConversationService,
    conv_repo: Arc<dyn nomifun_db::IConversationRepository>,
    terminal_driver: Arc<dyn nomifun_terminal::TerminalDriver>,
) -> IdmmRouterState {
    let pool = services.database.pool().clone();
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool.clone()));
    let client_prefs: Arc<dyn nomifun_db::IClientPreferenceRepository> =
        Arc::new(SqliteClientPreferenceRepository::new(pool.clone()));
    let records: Arc<dyn IIdmmInterventionRepository> = Arc::new(SqliteIdmmInterventionRepository::new(pool));
    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);

    // The sidecar's one-shot completions run against a backup provider; use the
    // data dir as the (unused-for-supervision) workspace root.
    let completer: Arc<dyn nomifun_idmm::Completer> = Arc::new(nomifun_idmm::LiveCompleter {
        provider_repo,
        encryption_key,
        workspace: services.data_dir.clone(),
    });
    let sidecar = Arc::new(nomifun_idmm::SidecarClient::new(completer, client_prefs.clone()));

    let probe_deps = Arc::new(nomifun_idmm::ProbeDeps {
        conversation_service: conv_service,
        conversation_repo: conv_repo,
        terminal_driver,
        task_manager: services.worker_task_manager.clone(),
    });

    let loop_deps = Arc::new(nomifun_idmm::LoopDeps {
        sidecar: sidecar.clone(),
        emitter: nomifun_idmm::IdmmEventEmitter::new(services.event_bus.clone()),
        records: records.clone(),
    });
    let manager = IdmmManager::new(loop_deps, probe_deps.clone(), probe_deps.clone());
    let service = Arc::new(nomifun_idmm::IdmmService::new(
        probe_deps,
        client_prefs,
        sidecar,
        manager,
        records.clone(),
    ));

    // TTL janitor: IDMM records are deliberately disposable (per-target cap is
    // enforced on insert; this enforces the global TTL + hard backstop). Sweep
    // once at boot, then hourly. Best-effort — a failed sweep only warns and the
    // next tick retries.
    spawn_idmm_record_janitor(records);

    IdmmRouterState::new(service)
}

/// Spawn the IDMM record TTL janitor: sweeps rows older than `TTL_MS` and
/// enforces the global hard cap `GLOBAL_CAP`. Runs once immediately (boot
/// sweep) then on a ~1h interval. Best-effort — a sweep error only warns and
/// the next tick retries; the sweep is a backstop on top of the per-target cap
/// already enforced at insert time.
fn spawn_idmm_record_janitor(records: Arc<dyn IIdmmInterventionRepository>) {
    tokio::spawn(async move {
        // First missed tick fires immediately → boot sweep on the first
        // iteration, then hourly.
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
        loop {
            ticker.tick().await;
            let cutoff = nomifun_common::now_ms() - nomifun_db::TTL_MS;
            match records.sweep(cutoff, nomifun_db::GLOBAL_CAP).await {
                Ok(removed) if removed > 0 => {
                    tracing::debug!(removed, "IDMM record janitor swept expired/overflow rows");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "IDMM record janitor sweep failed (will retry)"),
            }
        }
    });
}

/// Build the `CompanionRouterState` (the "nomi" desktop companion: opt-in event
/// collection, scheduled learning, memories, companion chat). Reuses the
/// singleton `services.companion_service` (constructed in `AppServices::from_config`
/// before the agent factory, which holds its memory sink) and late-wires the
/// companion thread manager with a `ConversationService` so companion chats
/// run as real nomi conversations.
pub fn build_companion_state(
    services: &AppServices,
    channel_manager: Arc<nomifun_channel::manager::ChannelManager>,
) -> CompanionRouterState {
    let pool = services.database.pool().clone();
    let conv_repo: Arc<dyn nomifun_db::IConversationRepository> =
        Arc::new(SqliteConversationRepository::new(pool.clone()));
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let acp_session_repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(pool));
    let skill_resolver = Arc::new(nomifun_conversation::skill_resolver::ExtensionSkillResolver::new(
        services.skill_paths.clone(),
    ));
    let conv_service = ConversationService::new(
        services.work_dir.clone(),
        services.event_bus.clone(),
        skill_resolver,
        services.worker_task_manager.clone(),
        conv_repo,
        agent_metadata_repo,
        acp_session_repo,
    )
    .with_runtime_state(services.conversation_runtime_state.clone());
    conv_service.with_mcp_server_repo(Arc::new(nomifun_db::SqliteMcpServerRepository::new(
        services.database.pool().clone(),
    )));
    // Companion threads carry `extra.companionId`, so the conversation service
    // mounts the companion-level knowledge binding ('companion', companionId) at task start —
    // same injection as the main conversation assembly.
    conv_service.with_knowledge_service(services.knowledge_service.clone());
    // Phase 3: companion turns run the same nomi send loop, so wire failover too.
    conv_service.with_failover_deps(
        Arc::new(SqliteProviderRepository::new(services.database.pool().clone())),
        Arc::new(SqliteClientPreferenceRepository::new(services.database.pool().clone())),
    );
    if let Some(hook) = services.task_manager_delete_hook.clone() {
        conv_service.with_delete_hook(hook);
    }

    // Deleting a companion must also drop its ('companion', id) knowledge-binding row so
    // bindings don't orphan (T3.3). Switching a companion's chat model (single source
    // of truth) must clear its bound IM channel sessions so they recreate with
    // the new model; deleting a companion likewise clears them. Both via cleanup hooks.
    services.companion_service.set_cleanup_hooks(vec![
        Arc::new(CompanionKnowledgeCleanup {
            knowledge: services.knowledge_service.clone(),
        }),
        Arc::new(CompanionChannelModelSync {
            manager: channel_manager,
        }),
    ]);

    services
        .companion_service
        .attach_companion(Arc::new(conv_service), services.worker_task_manager.clone());
    CompanionRouterState::new(services.companion_service.clone())
}

/// Companion-delete cascade hook: drops the deleted companion's knowledge binding via
/// `KnowledgeService::delete_binding("companion", …)`. Failures are logged, never
/// propagated (hook contract — the companion is already gone).
struct CompanionKnowledgeCleanup {
    knowledge: Arc<nomifun_knowledge::KnowledgeService>,
}

#[async_trait::async_trait]
impl nomifun_companion::service::CompanionCleanupHook for CompanionKnowledgeCleanup {
    async fn on_companion_deleted(&self, companion_id: &str) {
        if let Err(e) = self.knowledge.delete_binding("companion", companion_id).await {
            tracing::warn!(companion_id, error = %e, "failed to delete companion knowledge binding");
        }
    }
}

/// Session-delete cascade for IDMM decision records: `idmm_interventions` has a
/// polymorphic `target_id` (no FK to cascade), so when a conversation or terminal
/// is deleted the app layer clears its disposable audit trail here. Best-effort:
/// failures are logged, never propagated (hook contract — the session is already
/// gone). Lives in `nomifun-app` so `nomifun-conversation` / `nomifun-terminal`
/// stay unaware of the IDMM record repo. The `target_id` string matches the
/// supervisor's probe target (`conversation_id` / `terminal_id` as decimal).
struct IdmmRecordCascade {
    records: Arc<dyn IIdmmInterventionRepository>,
}

#[async_trait::async_trait]
impl OnConversationDelete for IdmmRecordCascade {
    async fn on_conversation_deleted(&self, conversation_id: i64) {
        if let Err(e) = self
            .records
            .delete_for_target("conversation", &conversation_id.to_string())
            .await
        {
            tracing::warn!(conversation_id, error = %e, "failed to clear IDMM records on conversation delete");
        }
    }
}

#[async_trait::async_trait]
impl OnTerminalDelete for IdmmRecordCascade {
    async fn on_terminal_deleted(&self, terminal_id: i64) {
        if let Err(e) = self
            .records
            .delete_for_target("terminal", &terminal_id.to_string())
            .await
        {
            tracing::warn!(terminal_id, error = %e, "failed to clear IDMM records on terminal delete");
        }
    }
}

/// Companion model-switch / delete → IM channel session sync. The companion's chat model is
/// the single source of truth; when it changes (or the companion is deleted), the
/// channel sessions bound to that companion are cleared so the next inbound IM message
/// recreates the backing conversation with the current model. Best-effort.
struct CompanionChannelModelSync {
    manager: Arc<nomifun_channel::manager::ChannelManager>,
}

#[async_trait::async_trait]
impl nomifun_companion::service::CompanionCleanupHook for CompanionChannelModelSync {
    async fn on_companion_deleted(&self, companion_id: &str) {
        self.manager.unbind_channels_for_deleted_companion(companion_id).await;
    }
    async fn on_companion_model_changed(&self, companion_id: &str) {
        self.manager.clear_sessions_for_companion(companion_id).await;
    }
}

/// Build the default `CronRouterState` from application services.
pub fn build_cron_state(services: &AppServices) -> CronRouterState {
    let pool = services.database.pool().clone();
    let cron_repo: Arc<dyn nomifun_db::ICronRepository> = Arc::new(nomifun_db::SqliteCronRepository::new(pool.clone()));

    let conv_repo: Arc<dyn nomifun_db::IConversationRepository> =
        Arc::new(SqliteConversationRepository::new(pool.clone()));
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let acp_session_repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(pool));
    let skill_resolver = Arc::new(nomifun_conversation::skill_resolver::ExtensionSkillResolver::new(
        services.skill_paths.clone(),
    ));
    let conv_service = ConversationService::new(
        services.work_dir.clone(),
        services.event_bus.clone(),
        skill_resolver,
        services.worker_task_manager.clone(),
        conv_repo.clone(),
        agent_metadata_repo,
        acp_session_repo,
    )
    .with_runtime_state(services.conversation_runtime_state.clone());
    conv_service.with_mcp_server_repo(Arc::new(nomifun_db::SqliteMcpServerRepository::new(
        services.database.pool().clone(),
    )));
    // Cron-spawned conversations mount their bound knowledge bases too —
    // same injection as the main conversation assembly.
    conv_service.with_knowledge_service(services.knowledge_service.clone());
    // Phase 3: cron-spawned nomi conversations run the send loop too.
    conv_service.with_failover_deps(
        Arc::new(SqliteProviderRepository::new(services.database.pool().clone())),
        Arc::new(SqliteClientPreferenceRepository::new(services.database.pool().clone())),
    );

    let busy_guard = Arc::new(nomifun_cron::busy_guard::CronBusyGuard::new());
    let executor = Arc::new(nomifun_cron::executor::JobExecutor::new(
        services.worker_task_manager.clone(),
        conv_repo,
        Arc::new(conv_service.clone()),
        busy_guard,
        services.work_dir.clone(),
        services.data_dir.clone(),
        services.event_bus.clone(),
        services.agent_registry.clone(),
    ));

    let tick_service_ref: Arc<CronServiceTickRef> = Arc::new(CronServiceTickRef::default());
    let tick_ref = tick_service_ref.clone();
    let scheduler = Arc::new(nomifun_cron::scheduler::CronScheduler::new(Arc::new(
        move |job_id: String| {
            let svc = tick_ref.0.lock().unwrap().clone();
            tokio::spawn(async move {
                if let Some(svc) = svc {
                    svc.tick(&job_id).await;
                }
            });
        },
    )));

    let emitter = CronEventEmitter::new(services.event_bus.clone());
    let cron_service = Arc::new(nomifun_cron::service::CronService::new(
        cron_repo,
        scheduler,
        executor,
        emitter,
        services.data_dir.clone(),
    ));

    tick_service_ref.0.lock().unwrap().replace(cron_service.clone());

    CronRouterState {
        cron_service,
        conversation_service: conv_service,
    }
}

/// Build the default `OfficeRouterState` from application services.
pub fn build_office_state(services: &AppServices) -> OfficeRouterState {
    let data_dir = services.data_dir.as_path();
    let allowed_roots = default_allowed_roots(Some(services.work_dir.as_path()));

    let spawner: Arc<dyn nomifun_office::ProcessSpawner> = Arc::new(nomifun_office::DefaultProcessSpawner);
    let watch_manager = Arc::new(OfficecliWatchManager::new(spawner, services.event_bus.clone()));

    let snapshot_service = Arc::new(OfficeSnapshotService::new(data_dir));
    let star_office_detector = Arc::new(StarOfficeDetector::local());
    let conversion_service = Arc::new(ConversionService::new(None));
    let proxy_service = Arc::new(ProxyService::new(watch_manager.clone()));

    OfficeRouterState {
        watch_manager,
        snapshot_service,
        star_office_detector,
        conversion_service,
        proxy_service,
        allowed_roots,
    }
}

/// Build the default `ShellRouterState` from application services.
pub fn build_shell_state(services: &AppServices) -> ShellRouterState {
    let pool = services.database.pool().clone();
    let client_pref_repo = Arc::new(SqliteClientPreferenceRepository::new(pool));
    let client_pref_service = ClientPrefService::new(client_pref_repo);

    ShellRouterState {
        shell_service: Arc::new(nomifun_shell::ShellService::new(Arc::new(
            nomifun_shell::DefaultSystemOpener,
        ))),
        stt_service: Arc::new(nomifun_shell::SttService::new_dynamic()),
        client_pref_service,
    }
}

/// Helper to break the circular reference between CronScheduler and CronService.
#[derive(Default)]
struct CronServiceTickRef(std::sync::Mutex<Option<Arc<nomifun_cron::service::CronService>>>);

/// Build the default extension-related router states.
///
/// Returns `(ExtensionRouterState, HubRouterState, SkillRouterState)`.
pub async fn build_extension_states(
    services: &AppServices,
) -> (ExtensionRouterState, HubRouterState, SkillRouterState) {
    let skill_data_dir = services.data_dir.clone();

    let state_store = ExtensionStateStore::new(resolve_state_file_path(&skill_data_dir));
    let registry = ExtensionRegistry::new(state_store, services.event_bus.clone(), services.app_version.clone());

    let hub_dir = resolve_install_target_dir_for_data_dir(&skill_data_dir);
    let index_manager = HubIndexManager::new(hub_dir, registry.clone());
    let installer = HubInstaller::new(index_manager.clone(), registry.clone());

    let app_resource_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| p.parent().map(|pp| pp.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let skill_paths = nomifun_extension::resolve_skill_paths(&app_resource_dir, &skill_data_dir);

    let ext_paths_mgr = Arc::new(ExternalPathsManager::new(&skill_data_dir).await);

    let skill_tag_repo: Arc<dyn nomifun_db::ISkillTagRepository> =
        Arc::new(nomifun_db::SqliteSkillTagRepository::new(services.database.pool().clone()));
    let builtin_skill_tags = Arc::new(nomifun_extension::skill_service::load_builtin_skill_tags());

    let ext_state = ExtensionRouterState {
        registry: registry.clone(),
    };

    let hub_state = HubRouterState {
        index_manager,
        installer,
    };

    let skill_state = SkillRouterState {
        skill_paths,
        external_paths_manager: ext_paths_mgr,
        assistant_dispatcher: None,
        skill_tag_repo,
        builtin_skill_tags,
    };

    (ext_state, hub_state, skill_state)
}

/// Build the default `WsHandlerState` from application services.
pub fn build_ws_state(services: &AppServices) -> WsHandlerState {
    // NoAuth: every upgrade is accepted (dev / `--insecure-no-auth`).
    if services.auth_policy.is_no_auth() {
        return WsHandlerState {
            manager: services.ws_manager.clone(),
            router: Arc::new(NoopMessageRouter),
            token_validator: Arc::new(|_| true),
            token_extractor: Arc::new(|_| Some("local".into())),
        };
    }

    // Required / TrustLocalToken: accept either the per-boot local-trust secret
    // (the desktop webview presents it as a `Sec-WebSocket-Protocol` value,
    // since browsers cannot set custom headers on the WS handshake) or a valid
    // JWT (remote logged-in browser).
    let jwt_service = services.jwt_service.clone();
    let local_secret = services.local_trust_secret.clone();
    let token_validator = Arc::new(move |token: &str| {
        if let Some(secret) = local_secret.as_deref()
            && token == secret
        {
            return true;
        }
        jwt_service.verify(token).is_ok()
    });

    let token_extractor = Arc::new(|headers: &axum::http::HeaderMap| extract_token_from_ws_headers(headers));

    WsHandlerState {
        manager: services.ws_manager.clone(),
        router: Arc::new(NoopMessageRouter),
        token_validator,
        token_extractor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::AppConfig;
    use nomifun_extension::{ExtensionSource, ScanPath};

    #[tokio::test]
    async fn build_extension_states_uses_host_app_version_for_engine_filtering() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let ext_root = tmp.path().join("extensions");
        let ext_dir = ext_root.join("demo-ext");

        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("nomi-extension.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "name": "demo-ext",
                "version": "1.0.0",
                "engine": {
                    "nomifun": "^2.0.0"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let db = nomifun_db::init_database_memory().await.unwrap();
        let config = AppConfig {
            data_dir: data_dir.clone(),
            work_dir: data_dir,
            app_version: "2.1.0".to_string(),
            ..Default::default()
        };
        let services = AppServices::from_config(db, &config).await.unwrap();

        let (ext_state, _hub_state, _skill_state) = build_extension_states(&services).await;
        ext_state
            .registry
            .initialize_with_scan_paths(vec![ScanPath {
                path: ext_root,
                source: ExtensionSource::Local,
            }])
            .await
            .unwrap();

        let loaded = ext_state.registry.get_loaded_extensions().await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "demo-ext");

        services.database.close().await;
    }

    // 迁移 030: NodeQuestion 回执 arm——审批模式下节点提问的回执必须：携带 brief
    // （节点标题+问题原文）、指示 lead 提醒用户进入节点作答、禁止 lead 代答。
    // NodeQuestion 非终态（非 Completed/Failed），autonomous 自评分支天然不劫持它。
    #[test]
    fn compose_lead_receipt_node_question_directs_user_to_node() {
        let brief = "节点『竞品调研』：定价方案选 A（订阅）还是 B（买断）？";
        let receipt = compose_lead_receipt(
            "run_q1",
            RunOutcome::NodeQuestion,
            brief,
            "autonomous", // 即便 autonomous，非终态 outcome 也走共享 arm
            "做个定价页",
            3,
            1000,
            false,
        );
        assert!(receipt.contains("节点提问"), "回执须标记节点提问: {receipt}");
        assert!(receipt.contains("run_q1"), "回执须带 run id");
        assert!(receipt.contains(brief), "回执须携带节点标题与问题原文");
        assert!(receipt.contains("采用为该节点产出"), "回执须指出恢复路径");
        assert!(receipt.contains("不要替用户作答"), "审批模式下 lead 不得代答");
        assert!(
            !receipt.contains("[自主编排回执]"),
            "非终态 outcome 不得被 autonomous 自评分支劫持"
        );
    }
}
