//! Module-level router states + their builders.
//!
//! `ModuleStates` is the bundle returned by `build_module_states`; each
//! `build_*_state` constructs one `*RouterState` from `AppServices`.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nomifun_ai_agent::{
    AgentRouterState, AgentRuntimeRegistry, AgentService, RemoteAgentRouterState,
    RemoteAgentService,
};
use nomifun_api_types::TerminalExitEvent;
use nomifun_preset::{BuiltinPresetRegistry, PresetRouterState, PresetService};
use nomifun_auth::extract_token_from_ws_headers;
use nomifun_channel::ChannelRouterState;
use nomifun_common::{AppError, OnConversationDelete, OnTerminalDelete};
use nomifun_conversation::service::QuiescentOrphanReconciliation;
use nomifun_conversation::{ConversationRouterState, ConversationService};
use nomifun_cron::{CronEventEmitter, CronRouterState};
use nomifun_db::{
    IAcpSessionRepository, IAgentExecutionRepository, IAgentExecutionTemplateRepository,
    IAgentMetadataRepository,
    IIdmmInterventionRepository, IPresetRepository, IPresetStateRepository, IPresetTagRepository,
    IProviderRepository, SqliteAcpSessionRepository, SqliteAgentExecutionRepository,
    SqliteAgentExecutionTemplateRepository,
    SqliteAgentMetadataRepository, SqlitePresetRepository, SqlitePresetStateRepository,
    SqlitePresetTagRepository, SqliteClientPreferenceRepository, SqliteConversationRepository,
    SqliteIdmmInterventionRepository, SqliteProviderRepository, SqliteRemoteAgentRepository, SqliteSettingsRepository,
    MAX_UNSETTLED_TURN_ADMISSION_PAGE_SIZE,
};
use nomifun_extension::{
    PresetRuleDispatcher, ExtensionRegistry, ExtensionRouterState, ExtensionStateStore, ExternalPathsManager,
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
use nomifun_agent_execution::{AgentExecutionEngine, AgentExecutionEngineConfig};
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
    /// Persistent Agent collaboration and execution state.
    pub agent_execution: Arc<AgentExecutionEngine>,
    /// P3-X2: per-pet browser-use credential secret CRUD state.
    pub secret: SecretRouterState,
    pub terminal: TerminalRouterState,
    pub office: OfficeRouterState,
    pub shell: ShellRouterState,
    pub preset: PresetRouterState,
}

fn default_allowed_roots(work_dir: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut roots = vec![
        std::env::temp_dir(),
        dirs::home_dir().unwrap_or_else(std::env::temp_dir),
    ];
    // Auto-provisioned per-conversation workspaces live under
    // `{work_dir}/conversations/{uuidv7}/`. On Windows the
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

/// Components needed to start the channel message loop.
///
/// Returned alongside `ChannelRouterState` by `build_channel_state`.
/// The caller must spawn the message loop as a background task.
pub struct ChannelMessageLoopComponents {
    pub message_loop: nomifun_channel::message_loop::ChannelMessageLoop,
    pub message_rx: tokio::sync::mpsc::Receiver<nomifun_channel::types::ChannelIncoming>,
    pub confirm_rx: tokio::sync::mpsc::Receiver<(String, String)>,
    pub manager: Arc<nomifun_channel::manager::ChannelManager>,
    pub plugin_factory: Arc<nomifun_channel::manager::PluginFactory>,
}

#[derive(Debug, Default)]
struct BootConversationReconciliationSummary {
    reconciled: u64,
    already_terminal: u64,
    retained_execution_skipped: u64,
    quarantined: u64,
}

#[derive(Debug, Clone)]
struct BootConversationReconciliationCandidate {
    user_id: String,
    conversation_id: String,
    agent_type: String,
    status: Option<String>,
    admission_epoch: i64,
    operation_id: Option<String>,
}

fn boot_reconciliation_error_is_retryable(error: &AppError) -> bool {
    matches!(
        error,
        AppError::Internal(_)
            | AppError::BadGateway(_)
            | AppError::Timeout(_)
            | AppError::RateLimited
            | AppError::ProviderUnavailable(_)
    )
}

async fn reconcile_unsettled_conversation_turn_pages<
    ListPage,
    ListPageFuture,
    Reconcile,
    ReconcileFuture,
>(
    mut list_page: ListPage,
    mut reconcile: Reconcile,
) -> BootConversationReconciliationSummary
where
    ListPage: FnMut(Option<String>, u32) -> ListPageFuture,
    ListPageFuture:
        Future<Output = Result<Vec<BootConversationReconciliationCandidate>, AppError>>,
    Reconcile: FnMut(String, String) -> ReconcileFuture,
    ReconcileFuture:
        Future<Output = Result<QuiescentOrphanReconciliation, AppError>>,
{
    let mut summary = BootConversationReconciliationSummary::default();
    let mut after_conversation_id: Option<String> = None;
    loop {
        let mut retry_delay = Duration::from_millis(25);
        let page = loop {
            match list_page(
                after_conversation_id.clone(),
                MAX_UNSETTLED_TURN_ADMISSION_PAGE_SIZE,
            )
            .await
            {
                Ok(page) => break page,
                Err(error) => {
                    tracing::error!(
                        after_conversation_id = after_conversation_id.as_deref(),
                        error = %error,
                        "startup Conversation orphan enumeration failed; background work remains fenced"
                    );
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
                }
            }
        };
        if page.is_empty() {
            break;
        }

        for candidate in page {
            let BootConversationReconciliationCandidate {
                user_id,
                conversation_id,
                agent_type,
                status,
                admission_epoch,
                operation_id,
            } = candidate;
            let mut retry_delay = Duration::from_millis(25);
            loop {
                match reconcile(user_id.clone(), conversation_id.clone()).await {
                    Ok(QuiescentOrphanReconciliation::Reconciled) => {
                        summary.reconciled += 1;
                        tracing::info!(
                            user_id,
                            conversation_id,
                            agent_type,
                            admission_epoch,
                            operation_id,
                            "startup reconciled a terminal-proof-backed Conversation turn without re-execution"
                        );
                        break;
                    }
                    Ok(QuiescentOrphanReconciliation::AlreadyTerminal) => {
                        summary.already_terminal += 1;
                        break;
                    }
                    Ok(QuiescentOrphanReconciliation::RetainedExecutionSkipped) => {
                        summary.retained_execution_skipped += 1;
                        tracing::warn!(
                            user_id,
                            conversation_id,
                            agent_type,
                            admission_epoch,
                            operation_id,
                            "startup left retained Agent Execution Conversation authority to its owning engine"
                        );
                        break;
                    }
                    Err(error) if boot_reconciliation_error_is_retryable(&error) => {
                        tracing::error!(
                            user_id,
                            conversation_id,
                            agent_type,
                            admission_epoch,
                            operation_id,
                            error = %error,
                            "startup Conversation orphan reconciliation failed transiently; background work remains fenced"
                        );
                        tokio::time::sleep(retry_delay).await;
                        retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
                    }
                    Err(error) => {
                        summary.quarantined += 1;
                        tracing::warn!(
                            user_id,
                            conversation_id,
                            agent_type,
                            status,
                            admission_epoch,
                            operation_id,
                            error = %error,
                            "startup quarantined unresolved Conversation turn authority; no runtime was built"
                        );
                        break;
                    }
                }
            }
            after_conversation_id = Some(conversation_id);
        }
    }
    summary
}

/// Reconcile every durable Conversation turn authority before any subsystem
/// capable of producing work is started.
///
/// The repository enumeration is only a hint. `ConversationService` re-reads
/// the exact row/receipt/admission under its preparation gate. Every current
/// backend without durable, queryable process-tree termination proof is
/// deliberately quarantined. Retained Agent Execution transcripts stay with
/// their owning engine, while already-terminal rows are harmless no-ops.
async fn reconcile_unsettled_conversation_turns_before_background_work(
    services: &AppServices,
    conversation_service: &ConversationService,
) -> BootConversationReconciliationSummary {
    match services.has_valid_boot_reconciliation_authority().await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                "startup Conversation orphan reconciliation skipped: no matching retained server-lock authority"
            );
            return BootConversationReconciliationSummary::default();
        }
        Err(error) => {
            tracing::error!(
                error = %error,
                "startup Conversation orphan reconciliation skipped: retained server-lock authority could not be revalidated"
            );
            return BootConversationReconciliationSummary::default();
        }
    }

    let conversation_repo = services.conversation_repo.clone();
    let conversation_service = conversation_service.clone();
    let runtime_registry = services.agent_runtime_registry.clone();
    let summary = reconcile_unsettled_conversation_turn_pages(
        move |after_conversation_id, limit| {
            let conversation_repo = conversation_repo.clone();
            async move {
                conversation_repo
                    .list_unsettled_turn_admissions(
                        after_conversation_id.as_deref(),
                        limit,
                    )
                    .await
                    .map(|admissions| {
                        admissions
                            .into_iter()
                            .map(|admission| {
                                BootConversationReconciliationCandidate {
                                    user_id: admission.conversation.user_id,
                                    conversation_id:
                                        admission.conversation.conversation_id,
                                    agent_type: admission.conversation.r#type,
                                    status: admission.conversation.status,
                                    admission_epoch: admission.admission_epoch,
                                    operation_id: admission.active_operation_id,
                                }
                            })
                            .collect()
                    })
                    .map_err(AppError::from)
            }
        },
        move |user_id, conversation_id| {
            let conversation_service = conversation_service.clone();
            let runtime_registry = runtime_registry.clone();
            async move {
                conversation_service
                    .reconcile_locally_quiescent_orphan_on_boot(
                        &user_id,
                        &conversation_id,
                        &runtime_registry,
                    )
                    .await
            }
        },
    )
    .await;

    tracing::info!(
        reconciled = summary.reconciled,
        already_terminal = summary.already_terminal,
        retained_execution_skipped = summary.retained_execution_skipped,
        quarantined = summary.quarantined,
        "startup Conversation turn reconciliation completed before background work"
    );
    summary
}

/// Build all default `ModuleStates` from application services.
pub async fn build_module_states(services: &AppServices) -> (ModuleStates, ChannelMessageLoopComponents) {
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

    let preset = build_preset_state(services, ext_state.registry.clone());
    let cron = build_cron_state(services, preset.service.clone());
    cron.cron_service.with_preset_service(preset.service.clone());

    // Construct the route ConversationService before any producer starts, then
    // synchronously classify every unsettled generation while the exact
    // database-ownership lock is retained. The lock authorizes this sweep but
    // is not process-tree terminal proof, so unresolved current backends remain
    // quarantined. This awaited boundary must stay above cron.init, AutoWork
    // persisted resume, channel/plugin receive loops, and router publication.
    let conversation = build_conversation_state(services, Some(cron.cron_service.clone()));
    conversation.service.with_preset_service(preset.service.clone());
    reconcile_unsettled_conversation_turns_before_background_work(
        services,
        &conversation.service,
    )
    .await;

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

    let dispatcher: Arc<dyn PresetRuleDispatcher> = preset.service.clone();
    skill_state.preset_dispatcher = Some(dispatcher);

    let (channel_state, channel_components) = build_channel_state(services, ext_state.registry.clone()).await;
    tracing::info!(elapsed_ms = boot.elapsed().as_millis(), "startup: channel state built");

    let pool = services.database.pool().clone();
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool));
    let encryption_key = services.encryption_key;
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
    let companion_state = build_companion_state(
        services,
        channel_components.manager.clone(),
        preset.service.clone(),
    )
        .with_preset_service(preset.service.clone())
        .with_knowledge_service(services.knowledge_service.clone());
    // Arm the shared service before execution recovery can start. Every clone
    // shares this hook slot, so normal chat and Agent attempts observe the same
    // IDMM supervisor without a boot-time race.
    let idmm_hook = Arc::new(idmm_state.service.manager().clone());
    conversation.service.with_supervision_hook(idmm_hook.clone());
    services.terminal_service.with_terminal_supervision_hook(idmm_hook);
    let execution_conversation = conversation.service.clone();
    let states = ModuleStates {
        system: build_system_state(services),
        conversation,
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
        public_agent: PublicAgentRouterState::new(services.public_agent_service.clone())
            .with_preset_service(preset.service.clone()),
        workshop: build_workshop_state(services),
        creation: build_creation_state(services),
        webhook: build_webhook_state(services),
        // REST routes, model tools and attempt conversations share this one engine
        // and the same ConversationService/runtime registry as ordinary Nomi chat.
        agent_execution: build_agent_execution_engine(
            services,
            execution_conversation,
            preset.service.clone(),
        ),
        secret: build_secret_state(services),
        terminal: build_terminal_state(services),
        office: build_office_state(services),
        shell: build_shell_state(services),
        preset,
    };

    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: module state build completed"
    );

    (states, channel_components)
}

/// Build the process-wide preset catalog and resolver singleton.
pub fn build_preset_state(services: &AppServices, extension_registry: ExtensionRegistry) -> PresetRouterState {
    let pool = services.database.pool().clone();
    let repo: Arc<dyn IPresetRepository> = Arc::new(SqlitePresetRepository::new(pool.clone()));
    let state_repo: Arc<dyn IPresetStateRepository> = Arc::new(SqlitePresetStateRepository::new(pool.clone()));
    let tag_repo: Arc<dyn IPresetTagRepository> = Arc::new(SqlitePresetTagRepository::new(pool.clone()));
    let agent_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool));
    let builtin = Arc::new(BuiltinPresetRegistry::load());
    let service = Arc::new(PresetService::new(
        repo,
        state_repo,
        tag_repo,
        agent_repo,
        provider_repo,
        builtin,
        extension_registry,
        services.data_dir.clone(),
    ));
    PresetRouterState { service }
}

/// Build the default `SystemRouterState` from application services.
pub fn build_system_state(services: &AppServices) -> SystemRouterState {
    let encryption_key = services.encryption_key;
    let pool = services.database.pool().clone();
    let provider_repo = Arc::new(SqliteProviderRepository::new(pool.clone()));

    // Cross-subsystem provider-deletion guard: aggregate every hard binding
    // (companion, public Agent, IDMM backup, active Agent Execution) and strip
    // soft failover/model-pool references only after deletion is allowed.
    let client_pref_repo: Arc<dyn nomifun_db::IClientPreferenceRepository> =
        Arc::new(SqliteClientPreferenceRepository::new(pool.clone()));
    let execution_repo: Arc<dyn IAgentExecutionRepository> =
        Arc::new(SqliteAgentExecutionRepository::new(pool.clone()));
    let execution_template_repo: Arc<dyn IAgentExecutionTemplateRepository> =
        Arc::new(SqliteAgentExecutionTemplateRepository::new(pool.clone()));
    let deletion_coordinator = Arc::new(crate::provider_deletion::AppProviderDeletionCoordinator {
        provider_lifecycle: services.provider_lifecycle.clone(),
        companion: services.companion_service.clone(),
        public_agent: services.public_agent_service.clone(),
        workshop: services.workshop_service.clone(),
        client_prefs: client_pref_repo,
        execution_repo,
        execution_template_repo,
        conversation_repo: services.conversation_repo.clone(),
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
        managed_model_service: Some(services.managed_model_service.clone()),
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
        services.authoritative_user_id.clone(),
        services.work_dir.clone(),
        services.event_bus.clone(),
        skill_resolver,
        services.agent_runtime_registry.clone(),
        conversaion_repo,
        agent_metadata_repo,
        acp_session_repo,
        services.execution_conversation_boundary.clone(),
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
    // Clear the conversation-domain owner of any requirement this conversation
    // owned; the ownership boundary has no FK cascade (spec §9.B).
    conversation_service.with_delete_hook(
        services.requirement_service.clone() as Arc<dyn OnConversationDelete>,
    );
    // A terminal minted by a conversation is part of that conversation's
    // resource lifecycle, not a global sidebar session. Deleting the owner is
    // the final safety net if the agent or user did not close it earlier.
    conversation_service.with_delete_hook(Arc::new(ConversationTerminalCascade {
        terminals: services.terminal_service.clone(),
    }) as Arc<dyn OnConversationDelete>);
    // Drop this conversation's IDMM decision records (disposable audit trail,
    // polymorphic target_id with no FK —app-level cascade).
    conversation_service.with_delete_hook(Arc::new(IdmmRecordCascade {
        records: Arc::new(SqliteIdmmInterventionRepository::new(services.database.pool().clone())),
    }) as Arc<dyn OnConversationDelete>);
    // Remove the conversation's on-disk nomi session file + auto-provisioned
    // temp workspace so no future conversation can resume stale state
    // (cross-conversation memory bleed). The per-session `owner_token` binds
    // any surviving residue to the owning conversation UUIDv7.
    conversation_service.with_delete_hook(Arc::new(
        nomifun_ai_agent::runtime_registry::NomiSessionFilesCascade {
            data_dir: services.data_dir.clone(),
            work_dir: services.work_dir.clone(),
        },
    ) as Arc<dyn OnConversationDelete>);
    if let Some(hook) = services.runtime_registry_delete_hook.clone() {
        conversation_service.with_delete_hook(hook);
    }
    if let Some(cron_service) = cron_service {
        conversation_service.with_delete_hook(cron_service.clone());
        conversation_service.with_cron_service(Some(cron_service));
    }
    ConversationRouterState {
        service: conversation_service,
        runtime_registry: services.agent_runtime_registry.clone(),
    }
}

/// Build the default `RemoteAgentRouterState` from application services.
pub fn build_remote_agent_state(services: &AppServices) -> RemoteAgentRouterState {
    let encryption_key = services.encryption_key;
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

/// Adapter exposing companions and public agents to channel conversations.
///
/// The channel layer resolves a session's companion via the channel row's own
/// `companion_id` first; this profile supplies the legacy per-platform binding
/// (when present and alive) and the per-companion model lookup. There is **no
/// default-companion fallback** —an unbound channel is hosted by no companion.
/// Channel sessions with no per-platform model fall back to the bound
/// companion's configured model, so its model choice travels with it to remote
/// sessions.
///
/// It ALSO backs the 对外伙伴 (public agent) side of the same trait: a platform
/// bound to a public agent (mutually exclusive with a companion binding) resolves
/// its live/enabled state + model here so the channel layer can serve strangers
/// via a `PublicService`-clamped per-chat session.
struct CompanionChannelAgentProfile {
    companion_service: Arc<nomifun_companion::CompanionService>,
    channel_settings: Arc<nomifun_channel::channel_settings::ChannelSettingsService>,
    public_agent_service: Arc<nomifun_public_agent::PublicAgentService>,
    /// Provider catalog, used to resolve the app's DEFAULT model when a public
    /// agent has no model of its own —so it answers as soon as ANY provider is
    /// configured (no per-agent model setup required).
    provider_repo: Arc<dyn IProviderRepository>,
}

#[async_trait::async_trait]
impl nomifun_channel::message_service::ChannelAgentProfile for CompanionChannelAgentProfile {
    async fn channel_companion_id(&self, platform: &str) -> Option<String> {
        // Per-companion binding is the ONLY way a channel becomes hosted by a companion:
        // each bot row carries its own `companion_id` (set when enabled from a companion's
        // 远程连接). The legacy per-platform binding still resolves here when present AND
        // alive, but there is **no default-companion fallback** — an unbound channel is
        // hosted by no companion (历史债「渠道与远程连接默认由默认伙伴接待」已废除；连接由
        // 用户为每个伙伴显式配置. A stale legacy binding (deleted companion) degrades to
        // None too, rather than pinning sessions to a ghost.
        if let Some(plugin) = nomifun_channel::types::PluginType::from_str_opt(platform)
            && let Ok(Some(bound)) = self.channel_settings.get_channel_companion_id(plugin).await
            && self.companion_service.get_companion(&bound).await.is_ok()
        {
            return Some(bound);
        }
        None
    }

    async fn companion_model(&self, companion_id: &str) -> Option<nomifun_common::ProviderWithModel> {
        let profile = self.companion_service.get_companion(companion_id).await.ok()?;
        profile.model
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

    async fn ensure_companion_session(&self, companion_id: &str) -> Option<String> {
        match self.companion_service.create_companion_thread(companion_id, None).await {
            Ok(thread) => match nomifun_common::ConversationId::try_from(thread.conversation_id.as_str()) {
                Ok(_) => Some(thread.conversation_id),
                Err(error) => {
                    tracing::warn!(
                        companion_id = %companion_id,
                        conversation_id = %thread.conversation_id,
                        %error,
                        "companion session returned an invalid canonical conversation ID"
                    );
                    None
                }
            },
            Err(error) => {
                tracing::warn!(companion_id = %companion_id, %error, "ensure_companion_session failed (likely no model configured)");
                None
            }
        }
    }

    async fn public_agent_servable(&self, public_agent_id: &str) -> bool {
        // Servable = the public agent exists AND is enabled. A deleted agent or a
        // disabled/paused one is NOT servable, so the channel layer refuses the
        // turn rather than serving a dead agent. The bot→agent binding itself is
        // per-bot (the channel row's `public_agent_id`); this is a pure by-id
        // liveness check.
        matches!(
            self.public_agent_service.get(public_agent_id).await,
            Ok(cfg) if cfg.enabled
        )
    }

    async fn public_agent_exists(&self, public_agent_id: &str) -> bool {
        self.public_agent_service.exists(public_agent_id).await
    }

    async fn public_agent_name(&self, public_agent_id: &str) -> Option<String> {
        self.public_agent_service
            .get(public_agent_id)
            .await
            .ok()
            .map(|a| a.name)
            .filter(|n| !n.trim().is_empty())
    }

    async fn public_agent_model(
        &self,
        public_agent_id: &str,
    ) -> Option<nomifun_common::ProviderWithModel> {
        // The agent's OWN configured model wins.
        if let Ok(cfg) = self.public_agent_service.get(public_agent_id).await {
            if let Some(model) = cfg.model {
                return Some(nomifun_common::ProviderWithModel {
                    provider_id: model.provider_id.into_string(),
                    model: model.model.clone(),
                    use_model: Some(model.model),
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

    async fn record_public_agent_turn(
        &self,
        public_agent_id: &str,
        platform: &str,
        text: &str,
    ) {
        // Best-effort audit into the public agent's own day-partitioned log
        // (never fails the turn).
        self.public_agent_service
            .record_turn(public_agent_id, "channel", Some(platform), text)
            .await;
    }
}

/// Build the default `ChannelRouterState` and message-loop components.
pub async fn build_channel_state(
    services: &AppServices,
    extension_registry: ExtensionRegistry,
) -> (ChannelRouterState, ChannelMessageLoopComponents) {
    let pool = services.database.pool().clone();
    let repo: Arc<dyn nomifun_db::IChannelRepository> = Arc::new(nomifun_db::SqliteChannelRepository::new(pool));
    let encryption_key = services.encryption_key;

    let (message_tx, message_rx) = tokio::sync::mpsc::channel(256);
    let (confirm_tx, confirm_rx) = tokio::sync::mpsc::channel(256);

    // Channel configuration and pairing are personal control-plane state. Bind
    // their realtime audience to the authoritative primary WebUI user before
    // constructing any producer; never reconstruct or guess it from payloads.
    let owner_user_id = services.authoritative_user_id.to_string();

    let manager = Arc::new(nomifun_channel::manager::ChannelManager::new(
        repo.clone(),
        services.event_bus.clone(),
        owner_user_id.clone(),
        encryption_key,
        message_tx,
        confirm_tx,
    ));

    let pairing_service = Arc::new(nomifun_channel::pairing::PairingService::new(
        repo.clone(),
        services.event_bus.clone(),
        owner_user_id.clone(),
    ));

    // Expired pairing codes are purged only by this background sweep —the
    // timer existed but had no caller, so stale codes lingered in the DB
    // indefinitely. Deliberately detached (handle dropped): like the channel
    // message loop and plugin restore tasks, it runs for the process lifetime.
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

    // Build message-loop dependencies. The fallback agent type for the
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
            services.authoritative_user_id.clone(),
            services.work_dir.clone(),
            services.event_bus.clone(),
            skill_resolver,
            services.agent_runtime_registry.clone(),
            conv_repo,
            agent_metadata_repo,
            acp_session_repo,
            services.execution_conversation_boundary.clone(),
        )
        .with_runtime_state(services.conversation_runtime_state.clone()),
    );
    conversation_svc.with_mcp_server_repo(Arc::new(nomifun_db::SqliteMcpServerRepository::new(
        services.database.pool().clone(),
    )));
    // Channel turns run the same Nomi send loop as other conversations.
    conversation_svc.with_failover_deps(
        Arc::new(SqliteProviderRepository::new(services.database.pool().clone())),
        Arc::new(SqliteClientPreferenceRepository::new(services.database.pool().clone())),
    );
    if let Some(hook) = services.runtime_registry_delete_hook.clone() {
        conversation_svc.with_delete_hook(hook);
    }

    // Channel Agent profile: per-platform companion binding + model resolution
    // and companion-id validation for the binding write
    // route, PLUS the 对外伙伴 (public agent) resolution/validation/audit for the
    // symmetric public-agent binding. One instance shared by the message service
    // and the router state.
    let channel_agent_profile: Arc<dyn nomifun_channel::message_service::ChannelAgentProfile> =
        Arc::new(CompanionChannelAgentProfile {
            companion_service: services.companion_service.clone(),
            channel_settings: Arc::clone(&channel_settings),
            public_agent_service: services.public_agent_service.clone(),
            provider_repo: services.provider_repo.clone(),
        });

    let message_service = Arc::new(
        nomifun_channel::message_service::ChannelMessageService::new(
            conversation_svc,
            services.agent_runtime_registry.clone(),
            Arc::clone(&channel_settings),
            repo.clone(),
            owner_user_id,
        )
        // Per-channel companion binding (with platform fallback) + model
        // resolution falls back to the bound companion when the
        // platform has no config of its own.
        .with_channel_agent_profile(Arc::clone(&channel_agent_profile))
        // Outbound media: resolve bare Workshop asset UUIDv7 values to bytes so
        // channel replies can send AI-generated images/files.
        .with_asset_resolver(Arc::new(crate::channel_asset_resolver::ChannelAssetResolver::new(
            services.workshop_service.clone(),
        ))),
    );

    let message_loop = nomifun_channel::message_loop::ChannelMessageLoop::new(
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
        channel_agent_profile: Some(channel_agent_profile),
        extension_registry,
    };

    let components = ChannelMessageLoopComponents {
        message_loop,
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
    // Clear the terminal-domain owner of any requirement this terminal owned;
    // the ownership boundary has no FK cascade (spec §9.B). Mirror of the
    // conversation delete hook.
    services
        .terminal_service
        .with_delete_hook(services.requirement_service.clone() as Arc<dyn OnTerminalDelete>);
    // Drop this terminal's IDMM decision records (disposable audit trail,
    // polymorphic target_id with no FK —app-level cascade, mirror of the
    // conversation delete hook).
    services
        .terminal_service
        .with_delete_hook(Arc::new(IdmmRecordCascade {
            records: Arc::new(SqliteIdmmInterventionRepository::new(services.database.pool().clone())),
        }) as Arc<dyn OnTerminalDelete>);
    let lifecycle_notice = Arc::new(AgentTerminalLifecycleNotice {
        runtimes: services.agent_runtime_registry.clone(),
    });
    // `terminal.exit` is emitted only after the PTY exit status and final
    // scrollback have been persisted. Observe the internal owner-scoped event
    // so natural child exits (not just REST kill/delete requests) update the
    // owning Agent's trusted resource context.
    spawn_terminal_exit_agent_notice_forwarder(
        services,
        lifecycle_notice.clone(),
    );

    // Reuse the singleton terminal service (owns the live PTY map), so the
    // terminal routes and the AutoWork runner share the same PTYs.
    TerminalRouterState::new(services.terminal_service.clone())
        .with_conversation_notice_sink(lifecycle_notice)
}

/// Build the `RequirementRouterState` + `IdmmRouterState` from application
/// services.
///
/// Reuses the singleton `requirement_service` (which shares its repo + WS emitter
/// with the nomi native-tool sink), attaches a `ConversationService` + repo to a
/// clone for AutoWork config persistence, builds the AutoWork runner, and
/// constructs the IDMM supervisor sharing the same live-session collaborators
/// (threaded back into the runner as its `IdmmHandle`).
pub fn build_requirement_state(services: &AppServices) -> (RequirementRouterState, IdmmRouterState) {
    let pool = services.database.pool().clone();

    // Build a ConversationService exactly like build_cron_state does, for
    // injection into the AutoWork runner + config reads/writes.
    let conv_repo: Arc<dyn nomifun_db::IConversationRepository> =
        Arc::new(SqliteConversationRepository::new(pool.clone()));
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let acp_session_repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(pool.clone()));
    let skill_resolver = Arc::new(nomifun_conversation::skill_resolver::ExtensionSkillResolver::new(
        services.skill_paths.clone(),
    ));
    let conv_service = ConversationService::new(
        services.authoritative_user_id.clone(),
        services.work_dir.clone(),
        services.event_bus.clone(),
        skill_resolver,
        services.agent_runtime_registry.clone(),
        conv_repo.clone(),
        agent_metadata_repo,
        acp_session_repo,
        services.execution_conversation_boundary.clone(),
    )
    .with_runtime_state(services.conversation_runtime_state.clone());
    // Phase 3: AutoWork-driven nomi turns run the send loop, and IDMM fault
    // supervision (Task 3) reuses `perform_model_failover` —wire the deps here too.
    conv_service.with_failover_deps(
        Arc::new(SqliteProviderRepository::new(pool.clone())),
        Arc::new(SqliteClientPreferenceRepository::new(pool.clone())),
    );

    // Router-state service: the singleton plus conversation service + repo for
    // AutoWork config, plus the terminal driver for terminal-target AutoWork. The
    // sink (built in AppServices) keeps using the plain singleton —it never needs
    // the autowork-config methods.
    let terminal_driver: Arc<dyn nomifun_terminal::TerminalDriver> = services.terminal_service.clone();
    let terminal_repo: Arc<dyn nomifun_db::ITerminalRepository> =
        Arc::new(nomifun_db::SqliteTerminalRepository::new(pool.clone()));
    // Shared AutoWork waker: the service fires it when a requirement becomes
    // claimable so idle AutoWork loops pick up new work without polling delay.
    let autowork_waker = Arc::new(tokio::sync::Notify::new());
    let requirement_service = Arc::new(
        (*services.requirement_service)
            .clone()
            .with_conversation_service(conv_service.clone(), conv_repo.clone())
            .with_terminal_driver(terminal_driver.clone())
            .with_terminal_repo(terminal_repo)
            .with_autowork_waker(autowork_waker.clone()),
    );

    // -- IDMM: build the supervisor manager + service, sharing the same
    // ConversationService / repo / terminal driver this AutoWork runner drives, so
    // IDMM observes the exact same live sessions. The manager is threaded into
    // AutoWorkRunnerDeps.idmm so AutoWork ensures supervision per turn.
    let idmm_state = build_idmm_state(
        services,
        conv_service.clone(),
        conv_repo.clone(),
        terminal_driver.clone(),
    );
    let idmm_handle: Arc<dyn nomifun_requirement::IdmmHandle> = Arc::new(idmm_state.service.manager().clone());

    // NOTE: the ConversationSupervisionHook for user-driven chat turns is
    // registered in `build_module_states` on the ROUTE ConversationService
    // (`build_conversation_state`'s instance), not here —this `conv_service` is
    // moved into `AutoWorkRunnerDeps` below and only drives AutoWork, which arms
    // IDMM per loop iteration via `AutoWorkRunnerDeps.idmm` (so a hook here was
    // dead code that fooled debugging into thinking arming was wired).

    let deps = Arc::new(nomifun_requirement::AutoWorkRunnerDeps {
        authoritative_user_id: services.authoritative_user_id.clone(),
        service: requirement_service.clone(),
        runtime_registry: services.agent_runtime_registry.clone(),
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
    let auto_work_runner = Arc::new(nomifun_requirement::AutoWorkRunner::new(deps));
    // Start the periodic lease sweeper (re-pends stale claims from dead sessions).
    auto_work_runner.start_sweeper();
    // Resume every persisted-enabled binding so bound sessions work in the
    // background from boot —no foreground/session-page visit required.
    auto_work_runner.resume_persisted_bindings();

    (
        RequirementRouterState {
            requirement_service,
            auto_work_runner,
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
/// `creation_service`. Creation task/asset reconciliation is completed during
/// `AppServices::from_config`, before this router can accept new generation
/// tasks, so a live write cannot race the boot inventory snapshot.
pub fn build_creation_state(services: &AppServices) -> CreationRouterState {
    CreationRouterState::new(services.creation_service.clone())
}

/// Build the single Agent Execution facade shared by REST, model tools and boot
/// recovery. Planner/router/scheduler/executor remain private engine strategies.
pub fn build_agent_execution_engine(
    services: &AppServices,
    conversation: ConversationService,
    preset_service: Arc<nomifun_preset::PresetService>,
) -> Arc<AgentExecutionEngine> {
    let repository: Arc<dyn IAgentExecutionRepository> = Arc::new(
        SqliteAgentExecutionRepository::new(services.database.pool().clone()),
    );
    let template_repository: Arc<dyn IAgentExecutionTemplateRepository> = Arc::new(
        SqliteAgentExecutionTemplateRepository::new(services.database.pool().clone()),
    );
    let provider_repository: Arc<dyn IProviderRepository> = Arc::new(
        SqliteProviderRepository::new(services.database.pool().clone()),
    );
    let engine = Arc::new(AgentExecutionEngine::new(AgentExecutionEngineConfig {
        repository,
        template_repository,
        provider_repository,
        preset_service,
        realtime: services.ws_manager.clone(),
        conversation,
        runtime_registry: services.agent_runtime_registry.clone(),
        encryption_key: services.encryption_key,
        workspace_root: services.work_dir.clone(),
    }));
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            if let Err(error) = engine.recover().await {
                tracing::error!(%error, "Agent Execution recovery failed");
            }
        });
    }
    engine
}

/// **P3-X2**: build the `SecretRouterState` (browser-use credential CRUD).
/// The service holds the app data dir (去 per-pet 键化: browser identity globally
/// shared —one vault under `{data_dir}/browser-secrets/shared`) + the machine-bound
/// `encryption_key` (the same persistent `[u8; 32]` the session/factory
/// side uses to build the `SecretStore`), so a secret registered here decrypts in a session.
pub fn build_secret_state(services: &AppServices) -> SecretRouterState {
    let encryption_key = services.encryption_key;
    let service = nomifun_secret::SecretService::new(services.data_dir.clone(), encryption_key);
    SecretRouterState::new(service)
}

/// Build the `IdmmRouterState` (the IDMM supervisor manager + service). Shares
/// the caller's `ConversationService` / conversation repo / terminal driver so
/// IDMM supervises the same live sessions AutoWork + the UI drive. Constructs
/// fresh provider/client-preference repos from the pool, while reusing the
/// process-wide persistent data-encryption key from [`AppServices`].
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
    let encryption_key = services.encryption_key;

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
        runtime_registry: services.agent_runtime_registry.clone(),
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
    // enforced on insert; this enforces the shared TTL + per-owner backstop). Sweep
    // once at boot, then hourly. Best-effort —a failed sweep only warns and the
    // next tick retries.
    spawn_idmm_record_janitor(records);

    IdmmRouterState::new(service)
}

/// Spawn the IDMM record TTL janitor: sweeps rows older than `TTL_MS` and
/// enforces the per-owner hard cap `PER_USER_ACTIVITY_CAP`. Runs once immediately (boot
/// sweep) then on a ~1h interval. Best-effort —a sweep error only warns and
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
            match records
                .sweep_all_owners(cutoff, nomifun_db::PER_USER_ACTIVITY_CAP)
                .await
            {
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
    preset_service: Arc<nomifun_preset::PresetService>,
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
        services.authoritative_user_id.clone(),
        services.work_dir.clone(),
        services.event_bus.clone(),
        skill_resolver,
        services.agent_runtime_registry.clone(),
        conv_repo,
        agent_metadata_repo,
        acp_session_repo,
        services.execution_conversation_boundary.clone(),
    )
    .with_runtime_state(services.conversation_runtime_state.clone());
    conv_service.with_mcp_server_repo(Arc::new(nomifun_db::SqliteMcpServerRepository::new(
        services.database.pool().clone(),
    )));
    // Companion threads carry `extra.companion_id`, so the conversation service
    // mounts the companion-level knowledge binding ('companion', companion_id) at task start —
    // same injection as the main conversation assembly.
    conv_service.with_knowledge_service(services.knowledge_service.clone());
    conv_service.with_preset_service(preset_service);
    // Phase 3: companion turns run the same nomi send loop, so wire failover too.
    conv_service.with_failover_deps(
        Arc::new(SqliteProviderRepository::new(services.database.pool().clone())),
        Arc::new(SqliteClientPreferenceRepository::new(services.database.pool().clone())),
    );
    if let Some(hook) = services.runtime_registry_delete_hook.clone() {
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
        .attach_companion(Arc::new(conv_service), services.agent_runtime_registry.clone());
    CompanionRouterState::new(services.companion_service.clone())
}

/// Companion-delete cascade hook: drops the deleted companion's knowledge binding via
/// `KnowledgeService::delete_binding("companion", …)`. Failures are logged, never
/// propagated (hook contract —the companion is already gone).
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

/// Conversation-delete cascade for agent-created terminal resources. Normal
/// operation expects the creating agent or the conversation terminal panel to
/// close these explicitly; this hook is the durable owner-lifecycle fallback.
struct ConversationTerminalCascade {
    terminals: Arc<nomifun_terminal::TerminalService>,
}

#[async_trait::async_trait]
impl OnConversationDelete for ConversationTerminalCascade {
    async fn on_conversation_deleted(&self, user_id: &str, conversation_id: &str) {
        let sessions = match self
            .terminals
            .list_for_conversation(user_id, conversation_id)
            .await
        {
            Ok(sessions) => sessions,
            Err(error) => {
                tracing::warn!(
                    conversation_id,
                    error = %error,
                    "failed to enumerate conversation-owned terminals during owner cleanup"
                );
                return;
            }
        };
        for session in sessions {
            if let Err(error) = self.terminals.delete(session.terminal_id.as_str()).await {
                tracing::warn!(
                    conversation_id,
                    terminal_id = %session.terminal_id,
                    error = %error,
                    "failed to delete conversation-owned terminal during owner cleanup"
                );
            }
        }
    }
}

fn spawn_terminal_exit_agent_notice_forwarder(
    services: &AppServices,
    notice_sink: Arc<AgentTerminalLifecycleNotice>,
) {
    let mut events = services.event_bus.subscribe_user();
    let terminal_service = services.terminal_service.clone();
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!(
            "terminal exit Agent-notice forwarder was not started because no Tokio runtime is active"
        );
        return;
    };
    runtime.spawn(async move {
        loop {
            let envelope = match events.recv().await {
                Ok(envelope) => envelope,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        skipped,
                        "terminal exit Agent-notice forwarder lagged; scoped terminal state remains authoritative"
                    );
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if envelope.event.name != "terminal.exit" {
                continue;
            }
            let exit = match serde_json::from_value::<TerminalExitEvent>(
                envelope.event.data,
            ) {
                Ok(exit) => exit,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "ignored malformed terminal.exit event in Agent-notice forwarder"
                    );
                    continue;
                }
            };
            if let Err(error) = terminal_service
                .authorize_user(&envelope.user_id, exit.terminal_id.as_str())
                .await
            {
                tracing::debug!(
                    terminal_id = %exit.terminal_id,
                    error = %error,
                    "ignored terminal.exit event whose owner no longer authorizes the terminal"
                );
                continue;
            }
            let session = match terminal_service.get(exit.terminal_id.as_str()).await {
                Ok(session) => session,
                Err(error) => {
                    tracing::debug!(
                        terminal_id = %exit.terminal_id,
                        error = %error,
                        "terminal exited but its owner conversation could not be resolved"
                    );
                    continue;
                }
            };
            // A queued exit event can race with relaunch of the same terminal
            // id. Only forward it while the durable row still describes this
            // exact exit; otherwise the event belongs to an obsolete PTY epoch
            // and would incorrectly tell the Agent that the replacement is
            // gone.
            if !terminal_exit_matches_current_state(
                &session.last_status,
                session.exit_code,
                exit.exit_code,
            ) {
                tracing::debug!(
                    terminal_id = %exit.terminal_id,
                    event_exit_code = ?exit.exit_code,
                    current_status = session.last_status,
                    current_exit_code = ?session.exit_code,
                    "ignored stale terminal.exit event after terminal state advanced"
                );
                continue;
            }
            let Some(conversation_id) = session.owner_conversation_id else {
                continue;
            };
            notice_sink.notify_terminal_exit(
                conversation_id.as_str(),
                exit.terminal_id.as_str(),
                exit.exit_code,
            );
        }
    });
}

fn terminal_exit_matches_current_state(
    current_status: &str,
    current_exit_code: Option<i32>,
    event_exit_code: Option<i32>,
) -> bool {
    current_status == "exited" && current_exit_code == event_exit_code
}

/// Feed terminal lifecycle state back into a Nomi runtime at its next model
/// boundary. This is intentionally best-effort: scoped terminal tools remain
/// the durable source of truth, while the trusted system-resource notice keeps
/// a present runtime from relying on stale process state.
struct AgentTerminalLifecycleNotice {
    runtimes: Arc<dyn AgentRuntimeRegistry>,
}

impl AgentTerminalLifecycleNotice {
    fn notify(
        &self,
        conversation_id: &str,
        terminal_id: &str,
        lifecycle: String,
    ) {
        let Some(runtime) = self.runtimes.get_runtime(conversation_id) else {
            tracing::debug!(
                conversation_id,
                terminal_id,
                lifecycle,
                "terminal lifecycle changed with no registered Agent runtime"
            );
            return;
        };
        let notice = format!(
            "Terminal {terminal_id} {lifecycle}. Treat any previous running \
             state as stale and call nomi_list_terminals before further \
             terminal actions."
        );
        match runtime.notify_system_resource(notice) {
            Ok(delivery) => {
                tracing::debug!(
                    conversation_id,
                    terminal_id,
                    lifecycle,
                    ?delivery,
                    "queued terminal lifecycle as trusted Agent resource state"
                );
            }
            Err(error) => {
                tracing::info!(
                    conversation_id,
                    terminal_id,
                    lifecycle,
                    agent_type = runtime.agent_type().serde_name(),
                    error = %error,
                    "Agent runtime has no trusted system-resource channel; terminal notice remains best-effort via scoped state"
                );
            }
        }
    }

    fn notify_terminal_exit(
        &self,
        conversation_id: &str,
        terminal_id: &str,
        exit_code: Option<i32>,
    ) {
        let lifecycle = match exit_code {
            Some(code) => format!("transitioned to exited (exit_code={code})"),
            None => "transitioned to exited (exit code unavailable)".to_owned(),
        };
        self.notify(conversation_id, terminal_id, lifecycle);
    }
}

impl nomifun_terminal::TerminalConversationNoticeSink for AgentTerminalLifecycleNotice {
    fn notify_terminal_lifecycle(
        &self,
        conversation_id: &str,
        terminal_id: &str,
        event: &'static str,
    ) {
        self.notify(
            conversation_id,
            terminal_id,
            format!("received lifecycle event `{event}`"),
        );
    }
}

/// Session-delete cascade for IDMM decision records: `idmm_interventions` has a
/// polymorphic `target_id` (no FK to cascade), so when a conversation or terminal
/// is deleted the app layer clears its disposable audit trail here. Best-effort:
/// failures are logged, never propagated (hook contract —the session is already
/// gone). Lives in `nomifun-app` so `nomifun-conversation` / `nomifun-terminal`
/// stay unaware of the IDMM record repo. The `target_id` string matches the
/// supervisor's probe target (`conversation_id` / `terminal_id` as decimal).
struct IdmmRecordCascade {
    records: Arc<dyn IIdmmInterventionRepository>,
}

#[async_trait::async_trait]
impl OnConversationDelete for IdmmRecordCascade {
    async fn on_conversation_deleted(&self, user_id: &str, conversation_id: &str) {
        if let Err(e) = self
            .records
            .delete_for_target(user_id, "conversation", &conversation_id.to_string())
            .await
        {
            tracing::warn!(conversation_id, error = %e, "failed to clear IDMM records on conversation delete");
        }
    }
}

#[async_trait::async_trait]
impl OnTerminalDelete for IdmmRecordCascade {
    async fn on_terminal_deleted(&self, user_id: &str, terminal_id: &str) {
        if let Err(e) = self
            .records
            .delete_for_target(user_id, "terminal", terminal_id)
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
pub fn build_cron_state(
    services: &AppServices,
    preset_service: Arc<nomifun_preset::PresetService>,
) -> CronRouterState {
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
        services.authoritative_user_id.clone(),
        services.work_dir.clone(),
        services.event_bus.clone(),
        skill_resolver,
        services.agent_runtime_registry.clone(),
        conv_repo.clone(),
        agent_metadata_repo,
        acp_session_repo,
        services.execution_conversation_boundary.clone(),
    )
    .with_runtime_state(services.conversation_runtime_state.clone());
    conv_service.with_mcp_server_repo(Arc::new(nomifun_db::SqliteMcpServerRepository::new(
        services.database.pool().clone(),
    )));
    // Cron-spawned conversations mount their bound knowledge bases too —
    // same injection as the main conversation assembly.
    conv_service.with_knowledge_service(services.knowledge_service.clone());
    conv_service.with_preset_service(preset_service);
    // Phase 3: cron-spawned nomi conversations run the send loop too.
    conv_service.with_failover_deps(
        Arc::new(SqliteProviderRepository::new(services.database.pool().clone())),
        Arc::new(SqliteClientPreferenceRepository::new(services.database.pool().clone())),
    );

    let busy_guard = Arc::new(nomifun_cron::busy_guard::CronBusyGuard::new());
    let executor = Arc::new(nomifun_cron::executor::JobExecutor::new(
        services.authoritative_user_id.clone(),
        services.agent_runtime_registry.clone(),
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
        move |
            job_id: String,
            user_id: String,
            schedule_revision: i64,
            planned_at_ms: nomifun_common::TimestampMs,
            generation: u64,
        | {
            let svc = tick_ref.0.lock().unwrap().clone();
            tokio::spawn(async move {
                if let Some(svc) = svc {
                    svc.tick_occurrence_with_generation(
                        &user_id,
                        &job_id,
                        schedule_revision,
                        planned_at_ms,
                        generation,
                    )
                    .await;
                }
            });
        },
    )));

    let emitter = CronEventEmitter::new(services.event_bus.clone());
    let cron_service = Arc::new(nomifun_cron::service::CronService::new(
        services.authoritative_user_id.clone(),
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
    let client_pref_repo = Arc::new(SqliteClientPreferenceRepository::new(pool.clone()));
    let client_pref_service = ClientPrefService::new(client_pref_repo);
    let provider_repo = Arc::new(SqliteProviderRepository::new(pool));

    ShellRouterState {
        shell_service: Arc::new(nomifun_shell::ShellService::new(Arc::new(
            nomifun_shell::DefaultSystemOpener,
        ))),
        stt_service: Arc::new(nomifun_shell::SttService::new_dynamic()),
        client_pref_service,
        provider_service: Some(ProviderService::new(provider_repo, services.encryption_key)),
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
        preset_dispatcher: None,
        skill_tag_repo,
        builtin_skill_tags,
    };

    (ext_state, hub_state, skill_state)
}

/// Build the default `WsHandlerState` from application services.
pub fn build_ws_state(services: &AppServices) -> WsHandlerState {
    // NoAuth: every upgrade is accepted (dev / `--insecure-no-auth`).
    if services.auth_policy.is_no_auth() {
        let authoritative_user_id = services.authoritative_user_id.to_string();
        return WsHandlerState {
            manager: services.ws_manager.clone(),
            router: Arc::new(NoopMessageRouter),
            token_authenticator: Arc::new(move |_| Some(authoritative_user_id.clone())),
            token_extractor: Arc::new(|_| Some("local".into())),
        };
    }

    // Required / TrustLocalToken: accept either the per-boot local-trust secret
    // (the desktop webview presents it as a `Sec-WebSocket-Protocol` value,
    // since browsers cannot set custom headers on the WS handshake) or a valid
    // JWT (remote logged-in browser).
    let jwt_service = services.jwt_service.clone();
    let local_secret = services.local_trust_secret.clone();
    let authoritative_user_id = services.authoritative_user_id.to_string();
    let token_authenticator = Arc::new(move |token: &str| {
        if let Some(secret) = local_secret.as_deref()
            && token == secret
        {
            return Some(authoritative_user_id.clone());
        }
        jwt_service.verify(token).ok().map(|claims| claims.user_id.into_string())
    });

    let token_extractor = Arc::new(|headers: &axum::http::HeaderMap| extract_token_from_ws_headers(headers));

    WsHandlerState {
        manager: services.ws_manager.clone(),
        router: Arc::new(NoopMessageRouter),
        token_authenticator,
        token_extractor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::AppConfig;
    use nomifun_extension::{ExtensionSource, ScanPath};

    #[test]
    fn terminal_exit_notice_rejects_stale_relaunch_epochs() {
        assert!(terminal_exit_matches_current_state(
            "exited",
            Some(0),
            Some(0)
        ));
        assert!(
            !terminal_exit_matches_current_state("running", None, Some(0)),
            "an old exit event must not describe a relaunched running PTY"
        );
        assert!(
            !terminal_exit_matches_current_state("exited", Some(1), Some(0)),
            "an exit event from another PTY epoch must not override current state"
        );
    }

    #[test]
    fn every_production_conversation_service_uses_shared_private_event_and_execution_boundaries() {
        let source = include_str!("state.rs");
        let production_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("state source must contain production assembly");
        let constructors = production_source
            .matches("ConversationService::new(")
            .count();
        let shared_boundary_injections = production_source
            .matches("services.execution_conversation_boundary.clone()")
            .count();

        assert_eq!(constructors, 5, "audit every production ConversationService");
        assert_eq!(shared_boundary_injections, constructors);
        assert!(!production_source.contains("with_execution_conversation_boundary"));

        let mut remaining = production_source;
        for constructor_index in 0..constructors {
            let start = remaining
                .find("ConversationService::new(")
                .expect("counted constructor must remain");
            remaining = &remaining[start..];
            let end = remaining
                .find("services.execution_conversation_boundary.clone()")
                .expect("constructor must inject the shared execution boundary");
            let constructor = &remaining[..end];
            assert!(
                constructor.contains("services.event_bus.clone()"),
                "ConversationService constructor {constructor_index} must use the shared scoped event bus"
            );
            assert!(
                !constructor.contains("services.ws_manager.clone()"),
                "ConversationService constructor {constructor_index} must not bypass internal scoped-event observers"
            );
            remaining = &remaining[end..];
        }

        let cron_executor_start = production_source
            .find("nomifun_cron::executor::JobExecutor::new(")
            .expect("production cron executor must be assembled");
        let cron_executor = &production_source[cron_executor_start..];
        let cron_executor_end = cron_executor
            .find("services.agent_registry.clone()")
            .expect("cron executor must inject the shared agent registry");
        let cron_executor = &cron_executor[..cron_executor_end];
        assert!(cron_executor.contains("services.authoritative_user_id.clone()"));
        assert!(cron_executor.contains("services.event_bus.clone()"));
        assert!(!cron_executor.contains("services.ws_manager.clone()"));
        let cron_service_start = production_source
            .find("nomifun_cron::service::CronService::new(")
            .expect("production cron service must be assembled");
        let cron_service = &production_source[cron_service_start..];
        let cron_service_end = cron_service
            .find("services.data_dir.clone()")
            .expect("cron service must receive the application data directory");
        assert!(
            cron_service[..cron_service_end]
                .contains("services.authoritative_user_id.clone()")
        );
        assert!(
            production_source.contains("CronEventEmitter::new(services.event_bus.clone())")
        );
        assert!(
            !production_source.contains("CronEventEmitter::new(services.ws_manager.clone())")
        );
    }

    #[test]
    fn boot_orphan_sweep_is_a_structural_barrier_before_every_work_producer() {
        let source = include_str!("state.rs");
        let build = source
            .split_once("pub async fn build_module_states")
            .expect("module-state builder must exist")
            .1
            .split_once("/// Build the process-wide preset catalog")
            .expect("module-state builder must have a stable end marker")
            .0;

        let conversation = build
            .find("build_conversation_state(")
            .expect("ConversationService must be constructed for the sweep");
        let sweep = build
            .find("reconcile_unsettled_conversation_turns_before_background_work(")
            .expect("boot orphan sweep must be awaited");
        let cron = build
            .find("cron.cron_service.init().await")
            .expect("cron startup must remain explicit");
        let channel = build
            .find("build_channel_state(")
            .expect("channel state startup must remain explicit");
        let autowork = build
            .find("build_requirement_state(")
            .expect("AutoWork state startup must remain explicit");

        assert!(conversation < sweep, "the sweep needs the route ConversationService");
        assert!(sweep < cron, "cron must not initialize before orphan reconciliation");
        assert!(sweep < channel, "channel/plugin assembly must not precede reconciliation");
        assert!(sweep < autowork, "AutoWork persisted resume must not precede reconciliation");
        assert!(
            build[sweep..cron].contains(".await;"),
            "the sweep must be a synchronous startup barrier, not a spawned task"
        );

        let routes = include_str!("routes.rs");
        let module_build = routes
            .find("build_module_states(services).await")
            .expect("router must await module-state construction");
        let channel_loop = routes
            .find(".message_loop")
            .expect("router must start the channel receive loop");
        let plugin_restore = routes
            .find("restore enabled channel")
            .or_else(|| routes.find("Restore enabled channel"))
            .expect("router must restore channel plugins");
        assert!(module_build < channel_loop);
        assert!(module_build < plugin_restore);

        let requirement_builder = source
            .split_once("pub fn build_requirement_state")
            .expect("requirement builder must exist")
            .1;
        assert!(
            requirement_builder.contains("auto_work_runner.resume_persisted_bindings();"),
            "the structural barrier must continue to cover persisted AutoWork resume"
        );
    }

    #[tokio::test]
    async fn boot_orphan_sweep_paginates_past_quarantined_and_retained_rows() {
        fn candidate(
            conversation_id: &str,
            agent_type: &str,
        ) -> BootConversationReconciliationCandidate {
            BootConversationReconciliationCandidate {
                user_id: format!("owner-{conversation_id}"),
                conversation_id: conversation_id.to_owned(),
                agent_type: agent_type.to_owned(),
                status: Some("running".to_owned()),
                admission_epoch: 7,
                operation_id: Some(format!("operation-{conversation_id}")),
            }
        }

        let observed_pages =
            Arc::new(std::sync::Mutex::new(Vec::<(Option<String>, u32)>::new()));
        let observed_reconciliations =
            Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let page_observer = observed_pages.clone();
        let reconcile_observer = observed_reconciliations.clone();
        let summary = reconcile_unsettled_conversation_turn_pages(
            move |after, limit| {
                let page_observer = page_observer.clone();
                async move {
                    page_observer.lock().unwrap().push((after.clone(), limit));
                    let page = match after.as_deref() {
                        None => vec![candidate("a", "nomi"), candidate("b", "remote")],
                        Some("b") => {
                            vec![candidate("c", "nomi"), candidate("d", "nomi")]
                        }
                        Some("d") => Vec::new(),
                        unexpected => panic!("unexpected keyset cursor {unexpected:?}"),
                    };
                    Ok::<_, AppError>(page)
                }
            },
            move |_user_id, conversation_id| {
                let reconcile_observer = reconcile_observer.clone();
                async move {
                    reconcile_observer
                        .lock()
                        .unwrap()
                        .push(conversation_id.clone());
                    match conversation_id.as_str() {
                        "a" => Err(AppError::Conflict(
                            "local parent-death teardown is not queryable terminal proof"
                                .to_owned(),
                        )),
                        "b" => Err(AppError::Conflict(
                            "external terminal proof is unavailable".to_owned(),
                        )),
                        "c" => Ok(
                            QuiescentOrphanReconciliation::RetainedExecutionSkipped,
                        ),
                        "d" => Ok(QuiescentOrphanReconciliation::AlreadyTerminal),
                        unexpected => panic!(
                            "unexpected reconciliation candidate {unexpected}"
                        ),
                    }
                }
            },
        )
        .await;

        assert_eq!(
            summary.reconciled, 0,
            "no current backend has restart-safe process-tree termination proof"
        );
        assert_eq!(summary.quarantined, 2);
        assert_eq!(summary.retained_execution_skipped, 1);
        assert_eq!(summary.already_terminal, 1);
        assert_eq!(
            *observed_reconciliations.lock().unwrap(),
            ["a", "b", "c", "d"]
        );
        assert_eq!(
            *observed_pages.lock().unwrap(),
            [
                (None, MAX_UNSETTLED_TURN_ADMISSION_PAGE_SIZE),
                (
                    Some("b".to_owned()),
                    MAX_UNSETTLED_TURN_ADMISSION_PAGE_SIZE
                ),
                (
                    Some("d".to_owned()),
                    MAX_UNSETTLED_TURN_ADMISSION_PAGE_SIZE
                ),
            ]
        );
    }

    #[test]
    fn every_production_host_attaches_exact_server_lock_authority() {
        for (host, source) in [
            ("embedded", include_str!("../lib.rs")),
            ("nomicore", include_str!("../main.rs")),
            ("desktop", include_str!("../desktop.rs")),
            ("web", include_str!("../../../../../apps/web/src/main.rs")),
        ] {
            let services = source
                .find("AppServices::from_config")
                .expect("production host must construct AppServices");
            let authority = source[services..]
                .find(".with_boot_reconciliation_authority(")
                .expect("production host must retain exact server-lock authority");
            let router = source[services..]
                .find("create_router")
                .or_else(|| source[services..].find("run_server"))
                .expect("production host must eventually publish/start the router");
            assert!(
                authority < router,
                "{host} must attach boot authority before router/background startup"
            );
        }

        let service_source = include_str!("../services.rs");
        assert!(
            service_source.contains("_boot_reconciliation_authority: None"),
            "ordinary tests/third-party AppServices must not infer server-lock ownership"
        );
        let production_source = include_str!("state.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(
            production_source.contains("services.has_valid_boot_reconciliation_authority().await"),
            "ordinary create_router must skip destructive orphan reconciliation without proof"
        );
    }

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
}
