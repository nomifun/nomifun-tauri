//! Shared application services for dependency injection.

use std::path::PathBuf;
use std::sync::Arc;

use nomifun_ai_agent::{
    AcpSessionSyncService, AcpSkillManager, AgentFactoryDeps, AgentRegistry, AgentRuntimeRegistry,
    InMemoryAgentRuntimeRegistry, build_agent_factory,
};
use nomifun_api_types::{GatewayMcpConfig, RequirementMcpConfig};
use nomifun_auth::{
    AuthPolicy, CompanionTokenValidator, CookieConfig, JwtService, QrTokenStore, resolve_jwt_secret,
};
use nomifun_common::OnConversationDelete;
use nomifun_conversation::runtime_state::ConversationRuntimeStateService;
use nomifun_conversation::{
    ExecutionConversationBoundary, RepositoryExecutionConversationBoundary,
};
use nomifun_db::{
    Database, IAcpSessionRepository, IAgentMetadataRepository, ICompanionTokenRepository,
    IConversationRepository, IMcpServerRepository, IModelProfileRepository, IProviderRepository,
    IUserRepository, SqliteAcpSessionRepository, SqliteAgentMetadataRepository,
    SqliteCompanionTokenRepository, SqliteConversationRepository, SqliteMcpServerRepository,
    SqliteModelProfileRepository, SqliteProviderRepository, SqliteRemoteAgentRepository,
    SqliteTerminalRepository, SqliteUserRepository,
};
use nomifun_realtime::{BroadcastEventBus, WebSocketManager};
use nomifun_terminal::{TerminalEventEmitter, TerminalLifecycleServer, TerminalService};

use crate::config::{AppConfig, load_or_create_data_encryption_key};

pub struct AppServices {
    pub database: Database,
    /// Canonical owner of every installation-scoped resource. Resolved once
    /// through `installation_identity` at boot; usernames are mutable display
    /// data and must never be used as an authorization identity.
    pub authoritative_user_id: Arc<str>,
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    /// Per-companion Remote front-door token store (SHA-256 hashes).
    pub companion_token_repo: Arc<dyn ICompanionTokenRepository>,
    /// In-memory validator mapping token -> companion_id (hot-swapped on mint/revoke).
    pub companion_token_validator: Arc<CompanionTokenValidator>,
    /// Provider repository (exposed for the mint-time model-availability guard).
    pub provider_repo: Arc<dyn IProviderRepository>,
    /// Unified loopback supply for NomiFun's managed free models.
    pub managed_model_service: Arc<nomifun_system::ManagedModelService>,
    /// Keeps the authenticated loopback OpenAI-compatible listener alive.
    pub(crate) _managed_model_server: nomifun_system::ManagedModelServer,
    /// Keeps the immediate + periodic managed catalog refresh loop alive.
    pub(crate) _managed_model_refresh_task: nomifun_system::ManagedModelRefreshTask,
    /// Authoritative per-model capability profiles (multimodal model hub).
    pub model_profile_repo: Arc<dyn IModelProfileRepository>,
    pub cookie_config: Arc<CookieConfig>,
    pub qr_token_store: Arc<QrTokenStore>,
    pub ws_manager: Arc<WebSocketManager>,
    pub event_bus: Arc<BroadcastEventBus>,
    pub agent_runtime_registry: Arc<dyn AgentRuntimeRegistry>,
    pub conversation_runtime_state: Arc<ConversationRuntimeStateService>,
    /// Same instance as `agent_runtime_registry`, exposed through the
    /// `OnConversationDelete` trait so `ConversationService::with_delete_hook`
    /// can wire it up. Optional because tests construct `AppServices` with a
    /// mock `agent_runtime_registry` that does not implement the trait.
    pub runtime_registry_delete_hook: Option<Arc<dyn OnConversationDelete>>,
    pub agent_registry: Arc<AgentRegistry>,
    pub conversation_repo: Arc<dyn IConversationRepository>,
    /// One mandatory Conversation↔Execution authority shared by every
    /// production ConversationService instance. Keeping it in AppServices
    /// makes incomplete module-specific assembly impossible.
    pub execution_conversation_boundary: Arc<dyn ExecutionConversationBoundary>,
    /// Singleton requirement service (shares its repo + WS emitter with the
    /// nomi native-tool sink). The router state attaches a `ConversationService`
    /// to a clone of this for AutoWork config persistence.
    pub requirement_service: Arc<nomifun_requirement::RequirementService>,
    /// Singleton terminal service: owns the live PTYs (one in-memory map). Shared
    /// so the AutoWork runner drives the SAME PTYs the terminal routes
    /// created (a fresh instance would have an empty live map).
    pub terminal_service: Arc<TerminalService>,
    pub acp_session_sync: Arc<AcpSessionSyncService>,
    /// Raw JWT secret string, used only for authentication/session signing.
    pub jwt_secret_raw: String,
    /// Persistent AES-256-GCM key for encrypted app data.
    pub encryption_key: [u8; 32],
    pub data_dir: PathBuf,
    pub work_dir: PathBuf,
    /// Authentication policy (single source of truth, replaces `local: bool`).
    pub auth_policy: AuthPolicy,
    /// Per-boot secret the desktop's own webview presents to be trusted as the
    /// local client. Only `Some` under `AuthPolicy::TrustLocalToken`.
    pub local_trust_secret: Option<Arc<str>>,
    pub app_version: String,
    /// Resolved skill paths. Shared with the `ConversationService` for
    /// snapshot resolution at create time.
    pub skill_paths: Arc<nomifun_extension::SkillPaths>,
    /// Process-private Requirement MCP issuer (port, root secret, binary path).
    /// It is non-serializable; only per-session child capabilities leave the
    /// main process. `None` when the server failed to start. Its presence drives
    /// `AutoWorkRunnerDeps::requirement_mcp_enabled` so the ACP verdict gate stays
    /// in lock-step with whether the declaration tools are actually injected.
    pub requirement_mcp_config: Option<RequirementMcpConfig>,
    /// Requirement MCP server instance kept alive for the app lifetime.
    pub(crate) _requirement_mcp_server: Option<nomifun_requirement::RequirementMcpServer>,
    /// Process-private Platform Gateway issuer (port, root secret, binary path,
    /// installation owner). It is non-serializable; only short-lived signed
    /// child capabilities leave the main process. `None` when the server failed
    /// to start, so Agent sessions simply lack the `nomi_*` tools.
    pub gateway_mcp_config: Option<GatewayMcpConfig>,
    /// Platform Gateway MCP server instance kept alive for the app lifetime.
    /// Its deps are late-wired from `create_router` via
    /// [`AppServices::inject_gateway_deps`] once the module services exist.
    pub(crate) _gateway_mcp_server: Option<nomifun_gateway::GatewayMcpServer>,
    /// Knowledge MCP server instance kept alive for the app lifetime. Its
    /// presence (surfaced to the agent factory as `knowledge_mcp_config`) gates
    /// scoped knowledge tool injection into ACP sessions that have bound bases.
    /// Its root issuer stays in-process; child capabilities independently scope
    /// search/read/write. `None` when startup fails (graceful degradation).
    pub(crate) _knowledge_mcp_server: Option<nomifun_knowledge::KnowledgeMcpServer>,
    /// Singleton companion service (nomi desktop companion). Built before the agent
    /// factory so the factory can register the companion memory tools for
    /// companion_session conversations; the router reuses this same instance.
    pub companion_service: Arc<nomifun_companion::CompanionService>,
    /// Singleton public-companion (对外伙伴) service — the enterprise external-
    /// service domain, entirely separate from `companion_service`. Owns its own
    /// `public-agents/` store, roster, and day-partitioned audit.
    pub public_agent_service: Arc<nomifun_public_agent::PublicAgentService>,
    /// Singleton 创意工坊 (Creative Workshop) service — canvas/asset CRUD +
    /// on-disk canvas docs / asset binaries under `{data_dir}/workshop/`. Shared
    /// by the `/api/workshop/*` routes.
    pub workshop_service: Arc<nomifun_workshop::WorkshopService>,
    /// Singleton 生成引擎 (creation) service — the media generation task queue
    /// behind the workshop canvas. Shared by the `/api/creation/*` routes.
    pub creation_service: Arc<nomifun_creation::CreationService>,
    /// Singleton knowledge service (knowledge base platform). Shared between
    /// the `/api/knowledge/*` routes and the `ConversationService`, which
    /// mounts bound bases into session workspaces at task start.
    pub knowledge_service: Arc<nomifun_knowledge::KnowledgeService>,
}

impl AppServices {
    /// Replace the process-local Agent runtime registry after construction.
    ///
    /// Primarily used by tests to inject mock implementations.
    pub fn with_agent_runtime_registry(mut self, runtime_registry: Arc<dyn AgentRuntimeRegistry>) -> Self {
        self.agent_runtime_registry = runtime_registry;
        self
    }

    /// Wire the dependency bundle into the Platform Gateway MCP server.
    /// Called from `create_router` after `build_module_states` (the
    /// `ConversationService` / `CronService` instances live there).
    pub(crate) async fn inject_gateway_deps(&self, deps: Arc<nomifun_gateway::GatewayDeps>) {
        if let Some(server) = &self._gateway_mcp_server {
            server.set_deps(deps).await;
        }
    }

    pub async fn from_config(database: Database, config: &AppConfig) -> anyhow::Result<Self> {
        // Brand computer-use permission-error guidance with the host app's name so
        // failures say "grant NomiFun … then quit and reopen NomiFun" instead of a
        // generic "this app" — which a model otherwise misreads as the terminal /
        // editor and sends the user to grant the wrong process. Set once, here, so
        // every later `observe` / screenshot / input failure carries the right name.
        #[cfg(feature = "computer-use")]
        nomi_computer::set_host_app_label("NomiFun");

        let data_dir = config.data_dir.clone();
        let work_dir = config.work_dir.clone();
        // The on-device model feature has been retired. Remove its managed
        // models, runtimes, partial downloads, ASR jobs, and persisted state so
        // upgrades do not leave multi-gigabyte orphaned data behind.
        let retired_model_dir = data_dir.join("local-ai");
        if let Err(error) = std::fs::remove_dir_all(&retired_model_dir)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %retired_model_dir.display(), %error, "Could not remove retired on-device model data");
        }
        // Security hard-cut: older builds persisted live loopback root tokens in
        // this beacon. Scoped child capabilities make discovery without an
        // authoritative session impossible, so remove both the final and
        // interrupted-write files before any new loopback issuer starts.
        for obsolete in ["mcp-endpoints.json", "mcp-endpoints.json.tmp"] {
            let path = data_dir.join(obsolete);
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(path = %path.display(), %error, "Could not remove obsolete MCP secret beacon");
            }
        }
        // Terminal MCP launch files are ephemeral. Older versions embedded a
        // process-wide token in these files; current versions keep even scoped
        // child credentials in the inherited process environment. Reset the
        // directory on every boot so neither historical nor stale session
        // configuration survives a backend restart.
        let terminal_mcp_dir = data_dir.join("terminal-mcp");
        if let Err(error) = std::fs::remove_dir_all(&terminal_mcp_dir)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %terminal_mcp_dir.display(), %error, "Could not reset ephemeral terminal MCP config directory");
        }
        let auth_policy = config.auth_policy;
        let local_trust_secret = config.local_trust_secret.clone();
        let app_version = config.app_version.clone();
        let user_repo: Arc<dyn IUserRepository> =
            Arc::new(SqliteUserRepository::new(database.pool().clone()));

        // Per-companion Remote front-door tokens: the repo persists each
        // companion's token hash; the validator caches `token -> companion_id`
        // in memory, hydrated from the DB at boot. An empty map means the front
        // door stays closed until a token is minted.
        let companion_token_repo: Arc<dyn ICompanionTokenRepository> =
            Arc::new(SqliteCompanionTokenRepository::new(database.pool().clone()));
        let initial_tokens = companion_token_repo.list_all().await.unwrap_or_else(|e| {
            tracing::warn!("failed to load companion access tokens at boot (Remote front door stays closed until a token is minted): {e}");
            Vec::new()
        });
        let companion_token_validator = Arc::new(CompanionTokenValidator::new(initial_tokens));

        // Resolve JWT secret: env var → installation-owner DB field → random generation
        let env_secret = std::env::var("JWT_SECRET").ok();
        let installation_owner = user_repo
            .get_system_user()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get installation owner: {e}"))?
            .ok_or_else(|| {
                anyhow::anyhow!("Database invariant violated: installation owner is missing")
            })?;
        let authoritative_user_id: Arc<str> = Arc::from(installation_owner.id.as_str());

        let db_secret = installation_owner.jwt_secret.as_deref().filter(|s| !s.is_empty());

        let (secret, is_new) = resolve_jwt_secret(env_secret.as_deref(), db_secret);

        // Persist newly generated secret to database
        if is_new {
            user_repo
                .update_jwt_secret(&installation_owner.id, &secret)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to persist JWT secret: {e}"))?;
            tracing::info!("Generated and persisted new JWT secret");
        }

        let encryption_key = load_or_create_data_encryption_key(&data_dir, &secret)
            .map_err(|e| anyhow::anyhow!("Failed to load data encryption key: {e}"))?;

        let remote_agent_repo = Arc::new(SqliteRemoteAgentRepository::new(database.pool().clone()));
        let provider_repo = Arc::new(SqliteProviderRepository::new(database.pool().clone()));
        // Start the stable managed-model loopback supply and provision its
        // provider projection before any model-profile reconciliation or agent
        // factory construction. A seed catalog makes a fresh install usable
        // without blocking boot on third-party discovery.
        let (managed_model_service, managed_model_server) =
            nomifun_system::start_and_provision_free_model_with_preferences(
                provider_repo.clone(),
                Some(Arc::new(nomifun_db::SqliteClientPreferenceRepository::new(
                    database.pool().clone(),
                ))),
                encryption_key,
            )
            .await
            .map_err(|e| anyhow::anyhow!("Failed to provision NomiFun free model service: {e}"))?;
        let model_profile_repo: Arc<dyn IModelProfileRepository> =
            Arc::new(SqliteModelProfileRepository::new(database.pool().clone()));
        // Refresh immediately, then about every six hours with jitter. Failed
        // attempts retain the current catalog and use capped exponential
        // backoff. Successful refreshes atomically seed profiles for any newly
        // discovered models without overwriting concurrent user edits.
        let managed_model_refresh_task = {
            let profile_repo = model_profile_repo.clone();
            nomifun_system::ManagedModelRefreshTask::start_with_success_hook(
                managed_model_service.clone(),
                move |status| {
                    let profile_repo = profile_repo.clone();
                    async move {
                        let Some(provider_id) = status.provider_id.as_deref() else {
                            tracing::warn!("Managed free-model refresh returned no provider id");
                            return;
                        };
                        let models = status
                            .models
                            .iter()
                            .map(|model| model.id.as_str())
                            .collect::<Vec<_>>();
                        match nomifun_system::seed_missing_inferred_profiles(
                            profile_repo.as_ref(),
                            provider_id,
                            nomifun_system::FREE_MODEL_PLATFORM,
                            &models,
                        )
                        .await
                        {
                            Ok(seeded) if seeded > 0 => tracing::info!(
                                seeded,
                                "Managed free-model refresh seeded inferred model profiles"
                            ),
                            Ok(_) => {}
                            Err(error) => tracing::warn!(
                                error = %error,
                                "Managed free-model profile reconciliation failed"
                            ),
                        }
                    }
                },
            )
        };
        // User-configured MCP servers — injected into ACP `session/new`
        // so the agent gets the operator's tools (ELECTRON-1JG fix).
        let mcp_server_repo: Arc<dyn IMcpServerRepository> =
            Arc::new(SqliteMcpServerRepository::new(database.pool().clone()));

        let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
            Arc::new(SqliteAgentMetadataRepository::new(database.pool().clone()));
        let agent_registry = AgentRegistry::new(agent_metadata_repo);
        agent_registry
            .hydrate()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to hydrate agent registry: {e}"))?;

        let acp_session_repo: Arc<dyn IAcpSessionRepository> =
            Arc::new(SqliteAcpSessionRepository::new(database.pool().clone()));
        let acp_agent_service = AcpSessionSyncService::new(acp_session_repo.clone());

        let conversation_repo: Arc<dyn IConversationRepository> =
            Arc::new(SqliteConversationRepository::new(database.pool().clone()));
        let execution_conversation_boundary: Arc<dyn ExecutionConversationBoundary> = Arc::new(
            RepositoryExecutionConversationBoundary::new(Arc::new(
                nomifun_db::SqliteAgentExecutionRepository::new(database.pool().clone()),
            )),
        );

        // Skill paths need app resource dir (for builtin rules) + data dir
        // (for user skills + materialized views). AcpSkillManager uses these
        // for first-message skill index/body loading.
        let app_resource_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok())
            .and_then(|p| p.parent().map(|pp| pp.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let skill_paths = Arc::new(nomifun_extension::resolve_skill_paths(
            &app_resource_dir,
            &data_dir,
        ));

        // Absolute path to this process's binary. Reused as the `command` for
        // stdio MCP bridges spawned by agent sessions.
        let backend_binary_path = Arc::new(
            std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("nomicore")),
        );

        // Event bus is shared by every service that broadcasts WS events.
        // Constructed here (rather than inline in the returned struct) so the
        // requirement service + sink built below share the same bus.
        let event_bus = Arc::new(BroadcastEventBus::new(256));

        // Requirement service + sink. Built before the agent factory because the
        // factory needs the sink to register the nomi native requirement tools.
        let requirement_repo: Arc<dyn nomifun_db::IRequirementRepository> = Arc::new(
            nomifun_db::SqliteRequirementRepository::new(database.pool().clone()),
        );
        let requirement_emitter = nomifun_requirement::RequirementEventEmitter::new(
            event_bus.clone(),
            authoritative_user_id.clone(),
        );
        // Completion notifier: on a requirement reaching a terminal state, notify
        // its tag's bound webhook. Injected into the SINGLETON so it fires on BOTH
        // completion paths — the Agent self-report sink AND the AutoWork runner's
        // `finalize_if_needed` (both clone from this instance, propagating the
        // notifier field). The repos share the same pool as `build_webhook_state`,
        // so they read the same `webhooks` / `tag_settings` tables.
        let webhook_repo_for_notifier: Arc<dyn nomifun_db::IWebhookRepository> = Arc::new(
            nomifun_db::SqliteWebhookRepository::new(database.pool().clone()),
        );
        let tag_setting_repo_for_notifier: Arc<dyn nomifun_db::ITagSettingRepository> = Arc::new(
            nomifun_db::SqliteTagSettingRepository::new(database.pool().clone()),
        );
        let completion_notifier = nomifun_webhook::CompletionNotifierImpl::new(
            tag_setting_repo_for_notifier,
            webhook_repo_for_notifier,
            Arc::new(nomifun_webhook::DefaultWebhookSender::new()),
        )
        .into_arc();
        let attachment_repo: Arc<dyn nomifun_db::IAttachmentRepository> = Arc::new(
            nomifun_db::SqliteAttachmentRepository::new(database.pool().clone()),
        );
        let attachment_store = Arc::new(nomifun_requirement::AttachmentStore::new(
            data_dir.clone(),
            attachment_repo,
        ));
        let requirement_service = Arc::new(
            nomifun_requirement::RequirementService::new(requirement_repo, requirement_emitter)
                .with_completion_notifier(completion_notifier)
                .with_attachment_store(attachment_store),
        );
        let requirement_sink =
            nomifun_requirement::RequirementServiceSink::into_arc(requirement_service.clone());

        // Requirement MCP server: gives ACP AutoWork sessions the
        // `requirement_complete` / `requirement_update_status` declaration tools
        // over a stdio bridge (claude/codex/gemini are stdio-only for MCP).
        // Failure is non-fatal — ACP sessions then keep the tool-free contract
        // and `requirement_mcp_enabled` stays false. Wired to the SAME singleton
        // the sink/AutoWork runner use (held as a Weak).
        let (requirement_mcp_server, requirement_mcp_config) =
            match nomifun_requirement::RequirementMcpServer::start().await {
                Ok(srv) => {
                    srv.set_service(Arc::downgrade(&requirement_service)).await;
                    let config = srv.issuer_config(
                        backend_binary_path.to_string_lossy().to_string(),
                    );
                    tracing::info!(port = config.port(), "Requirement MCP server started");
                    (Some(srv), Some(config))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Requirement MCP server failed to start; ACP AutoWork verdict tools disabled");
                    (None, None)
                }
            };

        // Platform Gateway MCP server: gives owner Agent sessions (Channel
        // Agent and companion conversations included) the `nomi_*` tools over
        // a stdio bridge. Started BEFORE the agent factory so the factory can
        // carry the connection config; the deps bundle is late-wired from
        // `create_router` (the conversation/cron services are built there).
        // Failure is non-fatal — flagged sessions then lack the desktop tools.
        let (gateway_mcp_server, gateway_mcp_config) =
            match nomifun_gateway::GatewayMcpServer::start().await {
                Ok(srv) => {
                    let config = srv.issuer_config(
                        backend_binary_path.to_string_lossy().to_string(),
                        authoritative_user_id.to_string(),
                    );
                    tracing::info!(port = config.port(), "Gateway MCP server started");
                    (Some(srv), Some(config))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Gateway MCP server failed to start; platform tools disabled");
                    (None, None)
                }
            };

        // Reliable-launch (`open`) MCP config — Windows only. macOS/Linux already
        // launch URLs/apps reliably (`open`/`xdg-open`), so the agent needs no
        // nudging there; on Windows it stops the agent from using the fragile
        // `cmd /c start` (which mis-parses URLs as window titles and pops
        // "Windows cannot find '\\'" dialogs). Stateless — no server to start,
        // just the binary path so the assembler can spawn `mcp-open-stdio`.
        let open_mcp_config =
            cfg!(target_os = "windows").then(|| nomifun_api_types::OpenMcpConfig {
                binary_path: backend_binary_path.to_string_lossy().to_string(),
            });

        // Computer-use discrete-tool MCP config — every desktop OS (macOS /
        // Windows / Linux), gated ONLY on the `computer-use` feature (else
        // `mcp-computer-stdio` is a stub, so we'd inject a bridge the binary
        // can't serve). Lets codex/ACP sessions drive the desktop (snapshot /
        // click / type / launch) via `nomicore mcp-computer-stdio`, mirroring the
        // in-process `ComputerTool` the nomi engine already gets on all platforms
        // (`nomi-a11y` implements macOS AX / Windows UIA / Linux AT-SPI backends).
        // Platform reality the bridge surfaces honestly: macOS needs the user to
        // grant TCC (Accessibility + Screen Recording) or ops error out; Linux
        // lacks OCR + cross-app window focus and degrades synthetic input on
        // Wayland. None of that warrants gating the bridge off — the tools simply
        // report `Unsupported` where the OS can't serve them.
        let computer_mcp_config =
            cfg!(feature = "computer-use").then(|| nomifun_api_types::ComputerMcpConfig {
                binary_path: backend_binary_path.to_string_lossy().to_string(),
            });

        // Browser-use discrete-tool MCP config — symmetric with computer-use
        // (P4-2, 裁决①). Every desktop OS, gated ONLY on the `browser-use`
        // feature (else `mcp-browser-stdio` is a stub, so we'd inject a bridge the
        // binary can't serve). Lets codex/ACP sessions drive a managed Chromium
        // (navigate / observe / click / type) via `nomicore mcp-browser-stdio`,
        // mirroring the in-process `BrowserTool` the nomi engine gets. The bridge
        // is stateless fail-safe (R2: no per-pet context over the env boundary;
        // `secret:NAME` fails closed and downloads land in the data-dir sandbox).
        let browser_mcp_config =
            cfg!(feature = "browser-use").then(|| nomifun_api_types::BrowserMcpConfig {
                binary_path: backend_binary_path.to_string_lossy().to_string(),
            });

        // Singleton knowledge service: knowledge base registry + workspace
        // mounting. Shared by the `/api/knowledge/*` routes and the
        // conversation service (mount-at-task-start).
        let knowledge_repo: Arc<dyn nomifun_db::IKnowledgeRepository> = Arc::new(
            nomifun_db::SqliteKnowledgeRepository::new(database.pool().clone()),
        );
        let knowledge_service = Arc::new(nomifun_knowledge::KnowledgeService::new(
            knowledge_repo,
            &data_dir,
            nomifun_knowledge::KnowledgeEventEmitter::new(
                event_bus.clone(),
                authoritative_user_id.clone(),
            ),
        ));
        // Late-wire the LLM seam for knowledge autogen / snapshot compression
        // (`LiveKnowledgeCompleter` resolves the first enabled provider/model
        // per call, so it tolerates providers configured after boot). NOTE:
        // `provider_repo` is moved into `build_agent_factory` below — clone.
        knowledge_service.set_completer(Arc::new(nomifun_ai_agent::LiveKnowledgeCompleter {
            provider_repo: provider_repo.clone() as Arc<dyn nomifun_db::IProviderRepository>,
            encryption_key,
            workspace: data_dir.clone(),
        }));
        // P3-K2: late-wire the rendering page-fetch backend (the engine-backed
        // `BrowserFetcher`) so the knowledge layer CAN fetch JS-rendered pages.
        // Feature-gated: only when `browser-use` is compiled in (desktop host).
        // When OFF, no render backend is registered and every source keeps using
        // the HTTP fetcher (K1 default — zero regression). This only PROVIDES the
        // backend; per-source routing on the `rendered` flag is K3. The crate
        // boundary (`nomifun-knowledge` never depends on the browser engine) holds:
        // `BrowserFetcher` lives in `nomifun-ai-agent` and is injected here as a
        // `dyn PageFetcher` trait object (anti-cycle decision ②, same late-wire as
        // `LiveKnowledgeCompleter` above).
        #[cfg(feature = "browser-use")]
        {
            // 并发隔离：启动 GC 回收上次运行崩溃/硬杀遗留、未经引擎 Drop 清理的 per-instance profile
            // 孤儿（`<data_dir>/browser-data/profiles/<token>`）。此刻本进程尚未启动任何引擎，故这些目录
            // 都是旧运行的孤儿；保守 TTL（1h）以免误删并发 MCP stdio 桥进程刚建的活目录。best-effort。
            nomi_browser_engine::profile::gc_stale_profiles(
                &data_dir.join("browser-data").join("profiles"),
                std::time::Duration::from_secs(3600),
            );
            let browser_fetcher =
                nomifun_ai_agent::BrowserFetcher::new(data_dir.join("knowledge-browser"));
            knowledge_service.set_render_fetcher(Arc::new(browser_fetcher));
        }
        // P3 connectors: register the built-in source connectors and late-wire
        // the encrypted credential store (same machine-bound AES key the
        // provider api-key column uses). Until this runs, the credential and
        // connector-sync endpoints return a clear 409. Feishu is the first
        // connector (self-built app + tenant_access_token, no OAuth redirect).
        knowledge_service.register_connector(Arc::new(
            nomifun_knowledge::connector_feishu::FeishuConnector::new(),
        ));
        let connector_cred_repo: Arc<dyn nomifun_db::IConnectorCredentialRepository> = Arc::new(
            nomifun_db::SqliteConnectorCredentialRepository::new(database.pool().clone()),
        );
        knowledge_service.set_connector_credentials(connector_cred_repo, encryption_key);
        // Boot-resume: re-fetch snapshot-mode URL sources whose create-time
        // fetch never completed (the app exited mid-run — the source is
        // persisted unstamped before fetching). Spawned after the completer
        // wiring so the chained autogen works; never blocks startup.
        tokio::spawn(Arc::clone(&knowledge_service).resume_pending_source_fetches());

        // Knowledge MCP server: gives ACP sessions with bound knowledge bases
        // search/read and policy-gated write tools over a stdio bridge. It owns
        // a domain-separated root issuer kept in this process; each managed
        // child receives only short-lived signed user/session/workspace/base/tool
        // claims. Wired to the SAME singleton KnowledgeService the routes use
        // (held as a Weak), mirroring the requirement server.
        // Failure is non-fatal — sessions then lack `knowledge_search` (graceful
        // degradation identical to having no mounted bases).
        let (knowledge_mcp_server, knowledge_mcp_config) =
            match nomifun_knowledge::KnowledgeMcpServer::start().await {
                Ok(mut srv) => {
                    srv.set_service(&knowledge_service).await;
                    let config = srv.issuer_config(
                        backend_binary_path.to_string_lossy().to_string(),
                    );
                    if let Err(error) = srv
                        .start_external_broker(
                            config.clone(),
                            authoritative_user_id.to_string(),
                        )
                        .await
                    {
                        tracing::warn!(%error, "secure external knowledge MCP broker failed to start");
                    }
                    tracing::info!(port = config.port(), "Knowledge MCP server started");
                    (Some(srv), Some(config))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Knowledge MCP server failed to start; scoped knowledge_search tool disabled");
                    (None, None)
                }
            };

        // Singleton terminal service (owns the live PTY map). Shared between the
        // terminal routes and the AutoWork runner's terminal driver.
        let terminal_repo: Arc<dyn nomifun_db::ITerminalRepository> =
            Arc::new(SqliteTerminalRepository::new(database.pool().clone()));
        let terminal_service = Arc::new(TerminalService::new(
            terminal_repo,
            TerminalEventEmitter::new(event_bus.clone()),
            work_dir.clone(),
        ));
        // Wire the scoped knowledge-search MCP into terminal launches: a
        // terminal whose cwd has mounted bases gets the real knowledge_search
        // tool injected into the native CLI (claude/codex), same bridge as ACP.
        // Config dir is platform-private (under data_dir), never the user cwd.
        if let Some(cfg) = knowledge_mcp_config.clone() {
            terminal_service.with_knowledge_mcp_config(cfg, data_dir.join("terminal-mcp"));
        }
        // Wire the scoped requirement MCP into terminal launches: agent CLIs
        // (claude/codex) get the requirement_complete/requirement_update_status
        // tools injected as a stdio bridge, scoped to the terminal's own id +
        // owner_kind=terminal. Unknown CLIs/shell are unaffected (apply_enhancement
        // skips rendering for them). Mirrors the knowledge MCP wiring above.
        if let Some(cfg) = requirement_mcp_config.clone() {
            terminal_service.with_requirement_mcp_config(cfg);
        }
        // Wire the auto-title completer: a terminal session's first turn (agent
        // CLIs) is summarized into a short work-content title via the default
        // provider/model (same resolution as `LiveKnowledgeCompleter` above).
        // Shell sessions / no provider fall back to the first input line, so this
        // is best-effort and never blocks a launch.
        terminal_service.with_title_completer(Arc::new(nomifun_ai_agent::LiveTerminalTitleCompleter {
            provider_repo: provider_repo.clone(),
            encryption_key,
            workspace: data_dir.clone(),
        }));
        // Start the terminal lifecycle server (house pattern, 4th instance):
        // native CLI hooks (claude --settings / codex -c hooks) POST turn/tool/
        // notification events to it via the `nomicore terminal-hook` shim, and it
        // broadcasts them per terminal_id. Failure is non-fatal — terminals then
        // simply lack lifecycle events (graceful degradation). The backend binary
        // path is needed so injected hook commands invoke `<bin> terminal-hook`.
        match TerminalLifecycleServer::start().await {
            Ok(srv) => {
                tracing::info!(port = srv.http_port(), "Terminal lifecycle server started");
                terminal_service.with_terminal_lifecycle(
                    std::sync::Arc::new(srv),
                    backend_binary_path.to_string_lossy().to_string(),
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Terminal lifecycle server failed to start; terminal hooks disabled");
            }
        };

        // Boot reconciliation: flip ghost 'running' rows (PTYs that died with the
        // previous app run — `live` is empty here) to 'exited'. This makes the
        // state honest so the frontend shows the relaunch entry + replays
        // persisted scrollback instead of a black screen, and a cron-bound
        // terminal's fire-time `live` check takes the relaunch path rather than
        // writing to a dead handle. Runs before cron init (in build_module_states).
        if let Err(e) = terminal_service.reconcile_on_boot().await {
            tracing::warn!(error = %e, "terminal boot reconciliation failed");
        }
        // Debounced scrollback persistence loop so terminal output history
        // survives a restart (dirty live sessions only; never per chunk).
        terminal_service.spawn_scrollback_flusher();

        // Companion service (nomi companion): built BEFORE the agent factory so the
        // factory gets the companion memory sink (recall/save memory tools for
        // companion_session conversations). The companion router state reuses this same
        // instance via `services.companion_service`.
        let companion_completer: Arc<dyn nomifun_companion::learner::CompanionCompleter> =
            Arc::new(nomifun_companion::learner::LiveCompanionCompleter {
                provider_repo: provider_repo.clone() as Arc<dyn nomifun_db::IProviderRepository>,
                encryption_key,
                workspace: data_dir.clone(),
            });
        let companion_service = nomifun_companion::CompanionService::start(
            &data_dir,
            event_bus.clone(),
            authoritative_user_id.as_ref(),
            companion_completer,
            skill_paths.clone(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("companion service start failed: {e}"))?;

        // Public-companion (对外伙伴) domain — its own store under public-agents/.
        // No completer / event bus / memory: it is a controlled enterprise service
        // agent, not a growing personal companion.
        let public_agent_service = nomifun_public_agent::PublicAgentService::start(&data_dir);

        // 创意工坊 (Creative Workshop) + 生成引擎 (creation): the workshop service
        // owns canvas/asset index rows + on-disk docs/binaries; the creation
        // service owns the media generation task queue. Both are plain repo-backed
        // services (no agent-factory dependency), constructed here alongside the
        // other singletons and reused by the router states.
        let workshop_service = nomifun_workshop::WorkshopService::start(
            &data_dir,
            Arc::new(nomifun_db::SqliteWorkshopRepository::new(database.pool().clone())),
        );
        // The generation engine resolves provider rows (endpoint + decrypted key,
        // same machine-bound AES key the provider column uses), runs the media
        // adapters over a proxy-aware HTTP client, and reads/writes canvas assets
        // through the workshop bridge (AssetSource/AssetSink — no crate cycle).
        // `reconcile_on_boot` (running-with-remote resume / else fail-interrupted)
        // is driven from `build_creation_state` at router assembly.
        let creation_http = nomifun_net::http_client();
        let creation_asset_bridge = Arc::new(crate::workshop_bridge::WorkshopAssetBridge::new(
            data_dir.clone(),
            Arc::new(nomifun_db::SqliteWorkshopRepository::new(database.pool().clone())),
        ));
        let creation_adapters = nomifun_creation::default_adapters(creation_http.clone());
        let creation_service = nomifun_creation::CreationService::builder(Arc::new(
            nomifun_db::SqliteCreationTaskRepository::new(database.pool().clone()),
        ))
        .with_http(creation_http.clone())
        .with_provider_repo(
            Arc::new(nomifun_db::SqliteProviderRepository::new(database.pool().clone())),
            encryption_key,
        )
        .with_asset_source(creation_asset_bridge.clone())
        .with_asset_sink(creation_asset_bridge)
        .with_providers(creation_adapters)
        .build();

        // Headless seed: bind a Remote access token to the default companion so an
        // operator can configure the front door via env on a headless server.
        // (Desktop mints per-companion tokens via /api/webui/companions/{id}/access-token.)
        if let Ok(seed) = std::env::var("NOMIFUN_COMPANION_TOKEN") {
            let seed = seed.trim();
            if !seed.is_empty() && companion_token_validator.resolve(seed).is_none() {
                match companion_service.default_companion_id().await {
                    Some(default_id) => {
                        let default_id = nomifun_common::CompanionId::parse(default_id)
                            .map_err(|error| anyhow::anyhow!("default companion has invalid id: {error}"))?;
                        let hash = nomifun_auth::token_sha256_hex(seed);
                        if let Err(e) = companion_token_repo
                            .upsert_for_companion(&default_id, &hash)
                            .await
                        {
                            tracing::warn!("failed to persist NOMIFUN_COMPANION_TOKEN seed: {e}");
                        }
                        companion_token_validator.insert_token(default_id.clone(), hash);
                        tracing::info!(
                            "Remote access token seeded from NOMIFUN_COMPANION_TOKEN, bound to default companion {default_id}"
                        );
                    }
                    None => tracing::warn!(
                        "NOMIFUN_COMPANION_TOKEN set but no companion exists to bind it to; create a companion first"
                    ),
                }
            }
        }

        // Expose the provider repo on AppServices (mint-time model guard reads it)
        // before it is moved into the agent factory below.
        let provider_repo_for_services: Arc<dyn IProviderRepository> =
            provider_repo.clone() as Arc<dyn nomifun_db::IProviderRepository>;

        // Seed authoritative capability profiles for any provider models that
        // lack one (multimodal model hub). Best-effort: never blocks boot on error.
        reconcile_model_profiles(&provider_repo_for_services, &model_profile_repo).await;

        let factory = build_agent_factory(AgentFactoryDeps {
            authoritative_user_id: authoritative_user_id.clone(),
            skill_manager: AcpSkillManager::new(skill_paths.clone()),
            remote_agent_repo,
            provider_repo,
            encryption_key,
            agent_registry: agent_registry.clone(),
            acp_agent_service: acp_agent_service.clone(),
            data_dir: data_dir.clone(),
            work_dir: work_dir.clone(),
            backend_binary_path: backend_binary_path.clone(),
            requirement_mcp_config: requirement_mcp_config.clone(),
            // Scoped knowledge-search MCP. Populated only when the server started
            // above; the assembler further gates injection on bound bases, so a
            // session without mounts never sees the tool. Independent of the
            // gateway config — this token never grants gateway reach.
            knowledge_mcp_config: knowledge_mcp_config.clone(),
            gateway_mcp_config: gateway_mcp_config.clone(),
            open_mcp_config: open_mcp_config.clone(),
            computer_mcp_config: computer_mcp_config.clone(),
            browser_mcp_config: browser_mcp_config.clone(),
            client_prefs: Some(Arc::new(nomifun_db::SqliteClientPreferenceRepository::new(
                database.pool().clone(),
            ))
                as Arc<dyn nomifun_db::IClientPreferenceRepository>),
            // System settings repo: lets the nomi factory read the app UI language
            // live per build so every nomi session thinks and replies in the app's
            // language instead of the old hardcoded Chinese (mirrors client_prefs).
            settings_repo: Some(Arc::new(nomifun_db::SqliteSettingsRepository::new(
                database.pool().clone(),
            )) as Arc<dyn nomifun_db::ISettingsRepository>),
            mcp_server_repo: Some(mcp_server_repo),
            requirement_sink: Some(requirement_sink),
            // Native cron tools: agent schedules/lists/deletes its own recurring
            // prompts. The closure resolves the process CronService lazily (it is
            // registered at startup in router/state.rs, after this factory is
            // built), so by the time a conversation runs the agent the service is
            // present. (Phase 4 platform synergy)
            cron_sink_factory: Some(Arc::new(|user_id: &str, conversation_id: &str| {
                nomifun_cron::sink::cron_sink_for(
                    user_id.to_string(),
                    conversation_id.to_string(),
                )
            })),
            companion_sink: Some(companion_service.memory_sink()),
            // Companion self-evolved skill auto-use (`companion_skill` tool + per-turn
            // when_to_use injection). Only registered for companion sessions (factory gates).
            companion_skill_sink: Some(companion_service.skill_sink()),
            // Live knowledge_search sink: registers the retrieval tool over the
            // shared KnowledgeService. The field's declared type
            // `Option<Arc<dyn KnowledgeRetrievalSink>>` drives the unsized
            // coercion, so no explicit `dyn` annotation is needed here.
            knowledge_retrieval: Some(Arc::new(nomifun_ai_agent::LiveKnowledgeRetrievalSink {
                service: knowledge_service.clone(),
            })),
            // Live knowledge_write (回血) sink: registers the native write-back
            // tool over the same KnowledgeService. Gated downstream on bound
            // bases + write-back enabled, so a read-only session never sees it.
            knowledge_writeback: Some(Arc::new(nomifun_ai_agent::LiveKnowledgeWritebackSink {
                service: knowledge_service.clone(),
            })),
            companion_prompt: Some(
                companion_service.clone() as Arc<dyn nomifun_ai_agent::CompanionPromptProvider>
            ),
            public_agent_provider: Some(
                public_agent_service.clone() as Arc<dyn nomifun_ai_agent::PublicAgentProvider>
            ),
        });

        // Agent factory is now wired. Future extension/custom agents
        // that get written to `agent_metadata` will show up after the
        // relevant service calls `AgentRegistry::hydrate`.
        let runtime_registry_concrete = Arc::new(InMemoryAgentRuntimeRegistry::new(factory));
        let agent_runtime_registry: Arc<dyn AgentRuntimeRegistry> = runtime_registry_concrete.clone();
        let runtime_registry_delete_hook: Arc<dyn OnConversationDelete> = runtime_registry_concrete;
        let conversation_runtime_state = Arc::new(ConversationRuntimeStateService::default());

        Ok(Self {
            database,
            authoritative_user_id,
            jwt_service: Arc::new(JwtService::new(secret.clone())),
            user_repo,
            companion_token_repo,
            companion_token_validator,
            provider_repo: provider_repo_for_services,
            managed_model_service,
            _managed_model_server: managed_model_server,
            _managed_model_refresh_task: managed_model_refresh_task,
            model_profile_repo: model_profile_repo.clone(),
            cookie_config: Arc::new(CookieConfig::from_env()),
            qr_token_store: Arc::new(QrTokenStore::new()),
            ws_manager: Arc::new(WebSocketManager::new()),
            event_bus,
            agent_runtime_registry,
            conversation_runtime_state,
            runtime_registry_delete_hook: Some(runtime_registry_delete_hook),
            agent_registry,
            conversation_repo,
            execution_conversation_boundary,
            requirement_service,
            terminal_service,
            acp_session_sync: acp_agent_service,
            jwt_secret_raw: secret,
            encryption_key,
            data_dir,
            work_dir,
            auth_policy,
            local_trust_secret,
            app_version,
            skill_paths,
            requirement_mcp_config,
            _requirement_mcp_server: requirement_mcp_server,
            gateway_mcp_config,
            _gateway_mcp_server: gateway_mcp_server,
            _knowledge_mcp_server: knowledge_mcp_server,
            companion_service,
            public_agent_service,
            workshop_service,
            creation_service,
            knowledge_service,
        })
    }
}

/// Ensure every provider model has an authoritative [`nomifun_db::ModelProfileRow`].
/// Models without a stored profile are seeded from the name/platform heuristic
/// (`source = "inferred"`); existing profiles (incl. user overrides) are left
/// untouched. Best-effort — logs and returns on any error so boot never fails
/// on profile reconciliation.
async fn reconcile_model_profiles(
    provider_repo: &Arc<dyn IProviderRepository>,
    model_profile_repo: &Arc<dyn IModelProfileRepository>,
) {
    let providers = match provider_repo.list().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("model-profile reconcile: failed to list providers: {e}");
            return;
        }
    };
    let mut seeded = 0usize;
    for provider in &providers {
        let models: Vec<String> = serde_json::from_str(&provider.models).unwrap_or_default();
        match nomifun_system::seed_missing_inferred_profiles(
            model_profile_repo.as_ref(),
            &provider.id,
            &provider.platform,
            &models,
        )
        .await
        {
            Ok(count) => seeded += count,
            Err(error) => tracing::warn!(
                provider_id = %provider.id,
                error = %error,
                "model-profile reconcile failed"
            ),
        }
    }
    if seeded > 0 {
        tracing::info!("model-profile reconcile: seeded {seeded} inferred profile(s)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn test_config(data_dir: &Path) -> AppConfig {
        AppConfig {
            data_dir: data_dir.to_path_buf(),
            work_dir: data_dir.to_path_buf(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_app_services_from_memory_db() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let retired_data = tmp.path().join("local-ai/models/retired-model");
        std::fs::create_dir_all(&retired_data).unwrap();
        std::fs::write(retired_data.join("model.bin"), b"retired").unwrap();
        let config = test_config(tmp.path());
        let services = AppServices::from_config(db, &config).await.unwrap();

        assert!(!tmp.path().join("local-ai").exists());

        // JWT service should be functional
        let test_user_id = "user_0190f5fe-7c00-7a00-8000-000000000001";
        let token = services.jwt_service.sign(test_user_id, "testuser").unwrap();
        let payload = services.jwt_service.verify(&token).unwrap();
        assert_eq!(payload.user_id.as_str(), test_user_id);

        // The installation owner exists but has no login credential yet.
        let has_users = services.user_repo.has_users().await.unwrap();
        assert!(!has_users); // empty owner password → not counted

        services.database.close().await;
    }

    #[tokio::test]
    async fn test_jwt_secret_persisted_to_db() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let services = AppServices::from_config(db, &config).await.unwrap();

        // The installation owner should now have a persisted jwt_secret.
        let installation_owner = services.user_repo.get_system_user().await.unwrap();
        let jwt_secret = installation_owner.unwrap().jwt_secret;
        assert!(jwt_secret.is_some());
        assert!(!jwt_secret.unwrap().is_empty());

        services.database.close().await;
    }

    #[tokio::test]
    async fn test_app_services_uses_supplied_app_version() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let config = AppConfig {
            app_version: "9.9.9".to_string(),
            ..test_config(tmp.path())
        };
        let services = AppServices::from_config(db, &config).await.unwrap();

        assert_eq!(services.app_version, "9.9.9");

        services.database.close().await;
    }
}
