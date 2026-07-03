//! Shared application services for dependency injection.

use std::path::PathBuf;
use std::sync::Arc;

use nomifun_ai_agent::{
    AcpSessionSyncService, AcpSkillManager, AgentFactoryDeps, AgentRegistry, IWorkerTaskManager,
    WorkerTaskManagerImpl, build_agent_factory,
};
use nomifun_api_types::{
    GatewayMcpConfig, GuideMcpConfig, KnowledgeMcpConfig, RequirementMcpConfig,
};
use nomifun_auth::{
    AuthPolicy, CompanionTokenValidator, CookieConfig, JwtService, QrTokenStore, resolve_jwt_secret,
};
use nomifun_common::OnConversationDelete;
use nomifun_conversation::runtime_state::ConversationRuntimeStateService;
use nomifun_db::{
    Database, IAcpSessionRepository, IAgentMetadataRepository, ICompanionTokenRepository,
    IConversationRepository, IMcpServerRepository, IProviderRepository, IUserRepository,
    SqliteAcpSessionRepository, SqliteAgentMetadataRepository, SqliteCompanionTokenRepository,
    SqliteConversationRepository, SqliteMcpServerRepository, SqliteProviderRepository,
    SqliteRemoteAgentRepository, SqliteTerminalRepository, SqliteUserRepository,
};
use nomifun_realtime::{BroadcastEventBus, WebSocketManager};
use nomifun_terminal::{TerminalEventEmitter, TerminalLifecycleServer, TerminalService};

use crate::config::{AppConfig, derive_encryption_key};

pub struct AppServices {
    pub database: Database,
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    /// Per-companion Remote front-door token store (SHA-256 hashes).
    pub companion_token_repo: Arc<dyn ICompanionTokenRepository>,
    /// In-memory validator mapping token -> companion_id (hot-swapped on mint/revoke).
    pub companion_token_validator: Arc<CompanionTokenValidator>,
    /// Provider repository (exposed for the mint-time model-availability guard).
    pub provider_repo: Arc<dyn IProviderRepository>,
    pub cookie_config: Arc<CookieConfig>,
    pub qr_token_store: Arc<QrTokenStore>,
    pub ws_manager: Arc<WebSocketManager>,
    pub event_bus: Arc<BroadcastEventBus>,
    pub worker_task_manager: Arc<dyn IWorkerTaskManager>,
    pub conversation_runtime_state: Arc<ConversationRuntimeStateService>,
    /// Same instance as `worker_task_manager`, exposed through the
    /// `OnConversationDelete` trait so `ConversationService::with_delete_hook`
    /// can wire it up. Optional because tests construct `AppServices` with a
    /// mock `worker_task_manager` that does not implement the trait.
    pub task_manager_delete_hook: Option<Arc<dyn OnConversationDelete>>,
    pub agent_registry: Arc<AgentRegistry>,
    pub conversation_repo: Arc<dyn IConversationRepository>,
    /// Singleton requirement service (shares its repo + WS emitter with the
    /// nomi native-tool sink). The router state attaches a `ConversationService`
    /// to a clone of this for AutoWork config persistence.
    pub requirement_service: Arc<nomifun_requirement::RequirementService>,
    /// Singleton terminal service: owns the live PTYs (one in-memory map). Shared
    /// so the AutoWork orchestrator drives the SAME PTYs the terminal routes
    /// created (a fresh instance would have an empty live map).
    pub terminal_service: Arc<TerminalService>,
    pub acp_session_sync: Arc<AcpSessionSyncService>,
    /// Raw JWT secret string, used to derive encryption keys.
    pub jwt_secret_raw: String,
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
    /// Guide MCP server config. Team Guide MCP is disabled while Team is not
    /// surfaced in the product, so this stays `None` and the diagnostic endpoint
    /// reports it as unavailable.
    pub guide_mcp_config: Option<GuideMcpConfig>,
    /// Requirement MCP server config (port, token, binary_path). `None` when the
    /// server failed to start. Its presence drives
    /// `OrchestratorDeps::requirement_mcp_enabled` so the ACP verdict gate stays
    /// in lock-step with whether the declaration tools are actually injected.
    pub requirement_mcp_config: Option<RequirementMcpConfig>,
    /// Requirement MCP server instance kept alive for the app lifetime.
    pub(crate) _requirement_mcp_server: Option<nomifun_requirement::RequirementMcpServer>,
    /// Desktop Gateway MCP server config (port, token, binary_path). `None`
    /// when the server failed to start — desktopGateway-flagged sessions then
    /// simply lack the `nomi_*` tools (graceful degradation).
    pub gateway_mcp_config: Option<GatewayMcpConfig>,
    /// Desktop Gateway MCP server instance kept alive for the app lifetime.
    /// Its deps are late-wired from `create_router` via
    /// [`AppServices::inject_gateway_deps`] once the module services exist.
    pub(crate) _gateway_mcp_server: Option<nomifun_gateway::GatewayMcpServer>,
    /// Knowledge MCP server instance kept alive for the app lifetime. Its
    /// presence (surfaced to the agent factory as `knowledge_mcp_config`) gates
    /// the scoped `knowledge_search` tool injection into ACP sessions that have
    /// bound knowledge bases. Read-only; mints its own loopback port + token and
    /// never grants the gateway reach. `None` when the server failed to start
    /// (graceful degradation — sessions then lack `knowledge_search`).
    pub(crate) _knowledge_mcp_server: Option<nomifun_knowledge::KnowledgeMcpServer>,
    /// Singleton companion service (nomi desktop companion). Built before the agent
    /// factory so the factory can register the companion memory tools for
    /// companionSession conversations; the router reuses this same instance.
    pub companion_service: Arc<nomifun_companion::CompanionService>,
    /// Singleton public-companion (对外伙伴) service — the enterprise external-
    /// service domain, entirely separate from `companion_service`. Owns its own
    /// `public-agents/` store, roster, and day-partitioned audit.
    pub public_agent_service: Arc<nomifun_public_agent::PublicAgentService>,
    /// Singleton knowledge service (knowledge base platform). Shared between
    /// the `/api/knowledge/*` routes and the `ConversationService`, which
    /// mounts bound bases into session workspaces at task start.
    pub knowledge_service: Arc<nomifun_knowledge::KnowledgeService>,
}

impl AppServices {
    /// Replace the worker task manager after construction.
    ///
    /// Primarily used by tests to inject mock implementations.
    pub fn with_worker_task_manager(mut self, wtm: Arc<dyn IWorkerTaskManager>) -> Self {
        self.worker_task_manager = wtm;
        self
    }

    /// Wire the dependency bundle into the Desktop Gateway MCP server.
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

        // Resolve JWT secret: env var → system user db field → random generation
        let env_secret = std::env::var("JWT_SECRET").ok();
        let system_user = user_repo
            .get_system_user()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get system user: {e}"))?;

        let db_secret = system_user
            .as_ref()
            .and_then(|u| u.jwt_secret.as_deref())
            .filter(|s| !s.is_empty());

        let (secret, is_new) = resolve_jwt_secret(env_secret.as_deref(), db_secret);

        // Persist newly generated secret to database
        if is_new && let Some(user) = &system_user {
            user_repo
                .update_jwt_secret(&user.id, &secret)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to persist JWT secret: {e}"))?;
            tracing::info!("Generated and persisted new JWT secret");
        }

        let encryption_key = derive_encryption_key(&secret);

        let remote_agent_repo = Arc::new(SqliteRemoteAgentRepository::new(database.pool().clone()));
        let provider_repo = Arc::new(SqliteProviderRepository::new(database.pool().clone()));
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

        // Team Guide MCP is intentionally disabled: the product currently does
        // not expose Team design/business flows, so no hidden `nomi_create_team`
        // MCP server should be started or injected into agent sessions.
        let guide_mcp_config: Option<GuideMcpConfig> = None;

        // Event bus is shared by every service that broadcasts WS events.
        // Constructed here (rather than inline in the returned struct) so the
        // requirement service + sink built below share the same bus.
        let event_bus = Arc::new(BroadcastEventBus::new(256));

        // Requirement service + sink. Built before the agent factory because the
        // factory needs the sink to register the nomi native requirement tools.
        let requirement_repo: Arc<dyn nomifun_db::IRequirementRepository> = Arc::new(
            nomifun_db::SqliteRequirementRepository::new(database.pool().clone()),
        );
        let requirement_emitter =
            nomifun_requirement::RequirementEventEmitter::new(event_bus.clone());
        // Completion notifier: on a requirement reaching a terminal state, notify
        // its tag's bound webhook. Injected into the SINGLETON so it fires on BOTH
        // completion paths — the agent self-report sink AND the orchestrator's
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
        // the sink/orchestrator use (held as a Weak), mirroring the guide server.
        let (requirement_mcp_server, requirement_mcp_config) =
            match nomifun_requirement::RequirementMcpServer::start().await {
                Ok(srv) => {
                    srv.set_service(Arc::downgrade(&requirement_service)).await;
                    let config = RequirementMcpConfig {
                        port: srv.http_port(),
                        token: srv.auth_token().to_owned(),
                        binary_path: backend_binary_path.to_string_lossy().to_string(),
                    };
                    tracing::info!(port = config.port, "Requirement MCP server started");
                    (Some(srv), Some(config))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Requirement MCP server failed to start; ACP AutoWork verdict tools disabled");
                    (None, None)
                }
            };

        // Desktop Gateway MCP server: gives desktopGateway-flagged sessions
        // (channel master-agent, companion companion) the `nomi_*` desktop tools over
        // a stdio bridge. Started BEFORE the agent factory so the factory can
        // carry the connection config; the deps bundle is late-wired from
        // `create_router` (the conversation/cron services are built there).
        // Failure is non-fatal — flagged sessions then lack the desktop tools.
        let (gateway_mcp_server, gateway_mcp_config) =
            match nomifun_gateway::GatewayMcpServer::start().await {
                Ok(srv) => {
                    let config = GatewayMcpConfig {
                        port: srv.http_port(),
                        token: srv.auth_token().to_owned(),
                        binary_path: backend_binary_path.to_string_lossy().to_string(),
                    };
                    tracing::info!(port = config.port, "Gateway MCP server started");
                    (Some(srv), Some(config))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Gateway MCP server failed to start; desktop gateway tools disabled");
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
            nomifun_knowledge::KnowledgeEventEmitter::new(event_bus.clone()),
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
        // the `knowledge_search` tool over a stdio bridge (claude/codex/gemini
        // are stdio-only for MCP). Read-only and tightly scoped — it mints its
        // OWN loopback port + bearer token (disjoint from the gateway server),
        // dispatches ONLY `knowledge_search`, and the bound `kb_ids` are baked
        // into the bridge env at injection time so the model cannot widen the
        // searchable base set. Wired to the SAME singleton KnowledgeService the
        // routes use (held as a Weak), mirroring the requirement/guide servers.
        // Failure is non-fatal — sessions then lack `knowledge_search` (graceful
        // degradation identical to having no mounted bases).
        let (knowledge_mcp_server, knowledge_mcp_config) =
            match nomifun_knowledge::KnowledgeMcpServer::start().await {
                Ok(srv) => {
                    srv.set_service(&knowledge_service).await;
                    let config = KnowledgeMcpConfig {
                        port: srv.http_port(),
                        token: srv.auth_token().to_owned(),
                        binary_path: backend_binary_path.to_string_lossy().to_string(),
                    };
                    tracing::info!(port = config.port, "Knowledge MCP server started");
                    (Some(srv), Some(config))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Knowledge MCP server failed to start; scoped knowledge_search tool disabled");
                    (None, None)
                }
            };

        // Singleton terminal service (owns the live PTY map). Shared between the
        // terminal routes and the AutoWork orchestrator's terminal driver.
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
        let lifecycle_endpoint = match TerminalLifecycleServer::start().await {
            Ok(srv) => {
                tracing::info!(port = srv.http_port(), "Terminal lifecycle server started");
                let ep = Some(crate::mcp_endpoints::Endpoint {
                    port: srv.http_port(),
                    token: srv.auth_token().to_owned(),
                });
                terminal_service.with_terminal_lifecycle(
                    std::sync::Arc::new(srv),
                    backend_binary_path.to_string_lossy().to_string(),
                );
                ep
            }
            Err(e) => {
                tracing::warn!(error = %e, "Terminal lifecycle server failed to start; terminal hooks disabled");
                None
            }
        };
        // Write the MCP endpoint beacon (`<data_dir>/mcp-endpoints.json`, 0600).
        // stdio bridges (and externally-registered CLIs) read this at runtime to
        // discover the current boot's port/token without baking stale values into
        // their config. Failure is non-fatal — bridges fall back to legacy env vars.
        {
            let beacon = crate::mcp_endpoints::McpEndpoints {
                knowledge: knowledge_mcp_config
                    .as_ref()
                    .map(|c| crate::mcp_endpoints::Endpoint {
                        port: c.port,
                        token: c.token.clone(),
                    }),
                requirement: requirement_mcp_config.as_ref().map(|c| {
                    crate::mcp_endpoints::Endpoint {
                        port: c.port,
                        token: c.token.clone(),
                    }
                }),
                lifecycle: lifecycle_endpoint,
            };
            if let Err(e) = crate::mcp_endpoints::write_beacon(&data_dir, &beacon) {
                tracing::warn!(error = %e, "Failed to write MCP endpoint beacon; bridges will fall back to env vars");
            } else {
                tracing::info!(path = %crate::mcp_endpoints::beacon_path(&data_dir).display(), "MCP endpoint beacon written");
            }
            // Pass the beacon path to the terminal service so spawned PTYs receive
            // `NOMI_MCP_ENDPOINTS_FILE` — the knowledge bridge reads it for endpoint
            // discovery without needing to compute the data-dir path itself.
            terminal_service.with_mcp_endpoints_path(
                crate::mcp_endpoints::beacon_path(&data_dir)
                    .to_string_lossy()
                    .into_owned(),
            );
        }

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
        // companionSession conversations). The companion router state reuses this same
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
            companion_completer,
            skill_paths.clone(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("companion service start failed: {e}"))?;

        // Public-companion (对外伙伴) domain — its own store under public-agents/.
        // No completer / event bus / memory: it is a controlled enterprise service
        // agent, not a growing personal companion.
        let public_agent_service = nomifun_public_agent::PublicAgentService::start(&data_dir);

        // Headless seed: bind a Remote access token to the default companion so an
        // operator can configure the front door via env on a headless server.
        // (Desktop mints per-companion tokens via /api/webui/companions/{id}/access-token.)
        if let Ok(seed) = std::env::var("NOMIFUN_COMPANION_TOKEN") {
            let seed = seed.trim();
            if !seed.is_empty() && companion_token_validator.resolve(seed).is_none() {
                match companion_service.default_companion_id().await {
                    Some(default_id) => {
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

        let factory = build_agent_factory(AgentFactoryDeps {
            skill_manager: AcpSkillManager::new(skill_paths.clone()),
            remote_agent_repo,
            provider_repo,
            encryption_key,
            agent_registry: agent_registry.clone(),
            acp_agent_service: acp_agent_service.clone(),
            data_dir: data_dir.clone(),
            work_dir: work_dir.clone(),
            backend_binary_path: backend_binary_path.clone(),
            guide_mcp_config: guide_mcp_config.clone(),
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
            // live per build so companion-owned sessions reply in the app's
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
            cron_sink_factory: Some(Arc::new(|conversation_id: &str| {
                nomifun_cron::sink::cron_sink_for(conversation_id.to_string())
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
        let task_manager_concrete = Arc::new(WorkerTaskManagerImpl::new(factory));
        let worker_task_manager: Arc<dyn IWorkerTaskManager> = task_manager_concrete.clone();
        let task_manager_delete_hook: Arc<dyn OnConversationDelete> = task_manager_concrete;
        let conversation_runtime_state = Arc::new(ConversationRuntimeStateService::default());

        Ok(Self {
            database,
            jwt_service: Arc::new(JwtService::new(secret.clone())),
            user_repo,
            companion_token_repo,
            companion_token_validator,
            provider_repo: provider_repo_for_services,
            cookie_config: Arc::new(CookieConfig::from_env()),
            qr_token_store: Arc::new(QrTokenStore::new()),
            ws_manager: Arc::new(WebSocketManager::new()),
            event_bus,
            worker_task_manager,
            conversation_runtime_state,
            task_manager_delete_hook: Some(task_manager_delete_hook),
            agent_registry,
            conversation_repo,
            requirement_service,
            terminal_service,
            acp_session_sync: acp_agent_service,
            jwt_secret_raw: secret,
            data_dir,
            work_dir,
            auth_policy,
            local_trust_secret,
            app_version,
            skill_paths,
            guide_mcp_config: guide_mcp_config.clone(),
            requirement_mcp_config,
            _requirement_mcp_server: requirement_mcp_server,
            gateway_mcp_config,
            _gateway_mcp_server: gateway_mcp_server,
            _knowledge_mcp_server: knowledge_mcp_server,
            companion_service,
            public_agent_service,
            knowledge_service,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_app_services_from_memory_db() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &AppConfig::default())
            .await
            .unwrap();

        // JWT service should be functional
        let token = services.jwt_service.sign("test_user", "testuser").unwrap();
        let payload = services.jwt_service.verify(&token).unwrap();
        assert_eq!(payload.user_id, "test_user");

        // User repo should have system user
        let has_users = services.user_repo.has_users().await.unwrap();
        assert!(!has_users); // system user has empty password → not counted

        services.database.close().await;
    }

    #[tokio::test]
    async fn test_jwt_secret_persisted_to_db() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &AppConfig::default())
            .await
            .unwrap();

        // System user should now have a jwt_secret persisted
        let system_user = services.user_repo.get_system_user().await.unwrap();
        let jwt_secret = system_user.unwrap().jwt_secret;
        assert!(jwt_secret.is_some());
        assert!(!jwt_secret.unwrap().is_empty());

        services.database.close().await;
    }

    #[tokio::test]
    async fn test_app_services_uses_supplied_app_version() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let config = AppConfig {
            app_version: "9.9.9".to_string(),
            ..Default::default()
        };
        let services = AppServices::from_config(db, &config).await.unwrap();

        assert_eq!(services.app_version, "9.9.9");

        services.database.close().await;
    }
}
