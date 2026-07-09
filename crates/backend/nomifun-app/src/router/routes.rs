//! Top-level router assembly: middleware stack + module route merges.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::DefaultBodyLimit;
use axum::http::Method;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use axum::{Router, middleware};
use tower_http::cors::{Any, CorsLayer};

use nomifun_ai_agent::{agent_routes, remote_agent_routes};
use nomifun_assets::{AssetRouterState, asset_routes};
use nomifun_assistant::assistant_routes;
use nomifun_auth::{
    AuthRouterState, AuthState, TrustState, auth_middleware, auth_routes, csrf_middleware,
    security_headers_middleware, trust_resolve_middleware,
};
use nomifun_channel::channel_routes;
use nomifun_companion::{companion_public_routes, companion_routes};
use nomifun_public_agent::public_agent_routes;
use nomifun_workshop::{workshop_public_routes, workshop_routes};
use nomifun_creation::creation_routes;
use nomifun_conversation::{conversation_ops_routes, conversation_routes};
use nomifun_cron::cron_routes;
use nomifun_extension::{extension_routes, hub_routes, skill_routes};
use nomifun_file::file_routes;
use nomifun_idmm::idmm_routes;
use nomifun_knowledge::knowledge_routes;
use nomifun_mcp::mcp_routes;
use nomifun_office::{office_proxy_routes, office_routes};
use nomifun_orchestrator::orchestrator_routes;
use nomifun_realtime::{WsHandlerState, ws_upgrade_handler};
use nomifun_requirement::requirement_routes;
use nomifun_shell::shell_routes;
use nomifun_system::{connection_test_routes, system_routes};
use nomifun_terminal::terminal_routes;
use nomifun_webhook::webhook_routes;

use nomifun_secret::secret_routes;

use crate::services::AppServices;

use super::computer_permissions::{
    computer_permission_status, open_permission_settings, request_computer_permission,
};
use super::health::{
    guide_mcp_status, health_check, knowledge_global_status_handler, mcp_register_template_handler,
    register_knowledge_global_handler, register_knowledge_handler,
    unregister_knowledge_global_handler,
};
use super::model_failover::{ModelFailoverRouterState, model_failover_routes};
use super::state::{ModuleStates, build_module_states, build_ws_state};
use super::trace::with_access_log;

/// Create the application router with all routes and global middleware.
///
/// Middleware stack (outermost → innermost):
/// 1. Security response headers (X-Frame-Options, etc.)
/// 2. CSRF protection (Double Submit Cookie)
/// 3. Route handlers (auth routes + system routes + conversation routes + file routes + health check)
pub async fn create_router(services: &AppServices) -> Router {
    let boot = Instant::now();
    tracing::info!("startup: router assembly started");

    // Bridge event bus → WebSocket manager: forward all broadcast events
    // to connected WebSocket clients.
    let mut event_rx = services.event_bus.subscribe();
    let ws_manager = services.ws_manager.clone();
    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            ws_manager.broadcast_all(event);
        }
    });

    let (states, channel_components) = build_module_states(services).await;
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: module states built"
    );

    // Wire the Desktop Gateway MCP deps now that the module services exist.
    // The gateway server itself started inside `AppServices::from_config`
    // (before the agent factory, which carries its connection config).
    //
    // requirement_service / autowork_orchestrator / idmm_service come from the
    // ROUTER STATES (not the bare singletons): those instances carry the
    // conversation-service / terminal-driver attachments the gateway's
    // autowork + idmm tools need, and share the live loop maps with the REST
    // routes so a gateway toggle and a UI toggle act on the same state.
    let gateway_deps = Arc::new(nomifun_gateway::GatewayDeps {
        conversation_service: states.conversation.service.clone(),
        task_manager: services.worker_task_manager.clone(),
        cron_service: states.cron.cron_service.clone(),
        requirement_service: states.requirement.requirement_service.clone(),
        companion_service: services.companion_service.clone(),
        terminal_service: services.terminal_service.clone(),
        provider_repo: Arc::new(nomifun_db::SqliteProviderRepository::new(
            services.database.pool().clone(),
        )),
        idmm_service: states.idmm.service.clone(),
        knowledge_service: services.knowledge_service.clone(),
        // 创意工坊 canvas index: a fresh repo over the same pool the workshop
        // routes/service use, backing the read-only nomi_workshop_list_canvases cap.
        workshop_repo: Arc::new(nomifun_db::SqliteWorkshopRepository::new(
            services.database.pool().clone(),
        )),
        // 创意工坊 canvas/asset + 生成引擎 services: the SAME singletons the
        // `/api/workshop/*` + `/api/creation/*` routes use, so the 画布助手 agent-op
        // queue is shared (gateway enqueues; an open frontend polls/acks the same
        // in-memory queue) and generation tasks land on the one live task queue.
        workshop_service: services.workshop_service.clone(),
        creation_service: services.creation_service.clone(),
        autowork_orchestrator: states.requirement.orchestrator.clone(),
        // System domain: reuse the SAME service instances the system routes use
        // (states.system is still owned here; it is moved into `system_routes`
        // later in `create_router_with_states`). A gateway theme/toggle/provider
        // change and a UI change then act on identical state.
        settings_service: states.system.settings_service.clone(),
        client_pref_service: states.system.client_pref_service.clone(),
        provider_service: states.system.provider_service.clone(),
        model_fetch_service: states.system.model_fetch_service.clone(),
        // Channel domain: same plugin manager / pairing / settings the
        // `/api/channels` routes use (states.channel is cloned, then moved
        // into `channel_routes` later).
        channel_state: states.channel.clone(),
        file_service: states.file.file_service.clone(),
        shell_service: states.shell.shell_service.clone(),
        mcp_config_service: states.mcp.config_service.clone(),
        extension_registry: states.extension.registry.clone(),
        hub_index_manager: states.hub.index_manager.clone(),
        hub_installer: states.hub.installer.clone(),
        skill_paths: states.skill.skill_paths.clone(),
        agent_service: states.agent.service.clone(),
        remote_agent_service: states.remote_agent.service.clone(),
        client_pref_repo: Arc::new(nomifun_db::SqliteClientPreferenceRepository::new(
            services.database.pool().clone(),
        )),
        // 智能编排 Run control-plane: the SAME router-state instances the REST
        // routes + the boot-resume use, so a gateway-created run and a UI-created
        // run act on identical state and the gateway's `start()` registers against
        // the same in-memory handle map the REST cancel/boot-resume drive.
        // `RunEngine` is value-Clone (Arc internals); wrap in Arc per Task 7's
        // field type so the gateway holds the one live instance.
        orchestrator_run_service: states.orchestrator.run_service.clone(),
        orchestrator_run_engine: Arc::new(states.orchestrator.engine.clone()),
        // 助手 (assistants): the SAME service instance the `/api/assistants` routes
        // use, so the caps_orchestrator layer reads the exact assistants (+ their
        // enabled/override state) the UI shows when folding them into a run's fleet
        // snapshot (P4 Task 2).
        assistant_service: states.assistant.service.clone(),
        // P3-GW1 (route A): per-companion browser tool registry, lives in this
        // (main) process. Feature-gated — `None` would mean "browser tools not
        // available", but when the feature is on we always wire it so remote
        // master/companion agents can drive a browser. Uses the default browser
        // config (headless is forced when no display is available anyway); each
        // companion gets an isolated lazily-engined BrowserTool + a mutex (X5).
        #[cfg(feature = "browser-use")]
        browser_registry: Some(
            // P3-X2: pass the machine-bound encryption_key so each companion's
            // gateway-driven BrowserTool loads its per-pet secret vault (secret:NAME
            // resolves, firewall allowlist derived from registered allowed_origins, 裁决⑤).
            // PKG-1: pass the bundled Chrome dir so packaged builds prefer it over download.
            nomifun_gateway::browser_registry::BrowserRegistry::default_for_browser_use()
                .with_secret_key(services.encryption_key)
                .with_bundled_dir(crate::commands::bundled_chrome_dir()),
        ),
        // Computer-use: one shared desktop ComputerTool (no per-companion
        // isolation, no secret vault — the desktop is a single screen).
        #[cfg(feature = "computer-use")]
        computer_registry: Some(nomifun_gateway::computer_registry::ComputerRegistry::new()),
    });
    services.inject_gateway_deps(gateway_deps.clone()).await;
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: gateway MCP deps injected"
    );

    // Start channel orchestrator (message loop)
    tokio::spawn(
        channel_components
            .orchestrator
            .run(channel_components.message_rx, channel_components.confirm_rx),
    );
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: channel orchestrator spawned"
    );

    // Restore enabled channel plugins (starts receiving IM messages)
    let chan_mgr = channel_components.manager;
    let chan_factory = channel_components.plugin_factory;
    {
        let mgr = chan_mgr.clone();
        let factory = chan_factory.clone();
        let companion_service = services.companion_service.clone();
        let public_agent_service = services.public_agent_service.clone();
        tokio::spawn(async move {
            // Self-heal ghost owner bindings BEFORE restoring: a channel row
            // bound to a 伙伴 / 对外伙伴 that was deleted before the delete-hook
            // existed (or missed by it) keeps reserving its bot identity
            // (UNIQUE(type,bot_key)), so re-enabling that bot under a live owner
            // fails with "already bound" forever. Unbind rows whose owner is no
            // longer in the roster so they become adoptable again. Both rosters
            // are scanned into memory at service construction, so an empty list
            // here means the owner really is gone.
            let live_companions: std::collections::HashSet<String> = companion_service
                .list_companions()
                .await
                .into_iter()
                .map(|c| c.id)
                .filter(|id| !id.is_empty())
                .collect();
            let live_public_agents: std::collections::HashSet<String> = public_agent_service
                .list()
                .await
                .into_iter()
                .map(|a| a.id)
                .filter(|id| !id.is_empty())
                .collect();
            // Safety valve: never mass-unbind on an ambiguous "no owners at all"
            // signal (e.g. a roster that failed to load). If the user genuinely
            // has zero companions AND zero public agents, there is nothing to
            // reconcile against — skip rather than risk unbinding every row.
            if live_companions.is_empty() && live_public_agents.is_empty() {
                tracing::info!("reconcile_orphaned_owners: empty roster, skipping to avoid mass-unbind");
            } else {
                mgr.reconcile_orphaned_owners(&live_companions, &live_public_agents).await;
            }

            if let Err(e) = mgr.restore_plugins(&factory).await {
                tracing::warn!(error = %e, "failed to restore channel plugins");
            }
        });
    }
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: channel plugin restore scheduled"
    );

    // Watchdog: plugin receive loops give up after exhausting their
    // reconnect budget, leaving DB + frontend claiming "running" for a dead
    // plugin. The watchdog persists the real status, broadcasts the change,
    // and attempts rate-limited automatic restarts.
    let _channel_watchdog = chan_mgr.spawn_watchdog(
        chan_factory,
        nomifun_channel::manager::WatchdogConfig::default(),
    );
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: channel plugin watchdog spawned"
    );

    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: route tree build started"
    );
    let router = create_router_with_states(services, states);
    // Remote capability front door (/mcp): per-companion-token-authenticated MCP,
    // projecting the SAME Registry/GatewayDeps as the inward stdio bridge. The
    // presented bearer token resolves to a single companion_id (threaded into
    // CallerCtx), so every external connection acts as exactly one companion.
    // `nest` (NOT `merge`) scopes its token-auth layer + fallback to `/mcp` so
    // it can't hijack the app's global 404 fallback. Mounted only here (the full
    // app), not in `create_router_with_states`, so test harnesses that call that
    // directly are unaffected. The LAN listener's host_guard (DNS-rebind) still
    // wraps it at the listener level.
    let router = router.nest(
        "/mcp",
        nomifun_public::public_mcp_router(
            gateway_deps.clone(),
            services.companion_token_validator.clone(),
            None,
        ),
    );
    // Curated "agent" profile endpoint — a tight do-work tool list for external
    // task-delegation agents (sibling of /mcp to avoid the catch-all conflict).
    let router = router.nest(
        "/mcp-agent",
        nomifun_public::public_mcp_router(
            gateway_deps.clone(),
            services.companion_token_validator.clone(),
            Some(nomifun_public::AGENT_PROFILE_DOMAINS),
        ),
    );
    // REST /v1 adapter (human/script-facing), same registry + instance token,
    // also scoped via nest. Supports ?profile=agent.
    let router = router.nest(
        "/v1",
        nomifun_public::public_rest_router(
            gateway_deps,
            services.companion_token_validator.clone(),
        ),
    );
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: router assembly completed"
    );
    router
}

/// Create the application router with custom module states.
///
/// Used for testing when specific service overrides are needed
/// (e.g. injecting a mock HTTP server URL for version check).
pub fn create_router_with_states(services: &AppServices, states: ModuleStates) -> Router {
    let ws_state = build_ws_state(services);
    create_router_with_all_state(services, states, ws_state)
}

/// Create the application router with custom module states and WebSocket state.
///
/// Full-control variant used by tests that need to override
/// module services and WebSocket behaviour.
pub fn create_router_with_all_state(
    services: &AppServices,
    states: ModuleStates,
    ws_state: WsHandlerState,
) -> Router {
    let boot = Instant::now();
    tracing::info!("startup: route tree build with states started");

    let auth_state = AuthRouterState {
        jwt_service: services.jwt_service.clone(),
        user_repo: services.user_repo.clone(),
        cookie_config: services.cookie_config.clone(),
        qr_token_store: services.qr_token_store.clone(),
    };

    let auth_mw_state = AuthState {
        jwt_service: services.jwt_service.clone(),
        user_repo: services.user_repo.clone(),
    };

    // Per-companion Remote access-token mint/revoke/status endpoints. Local-trust
    // gated (the desktop webview's own per-boot secret) — merged into the pre-CSRF
    // section alongside the auth routes so it never falls under cookie-CSRF.
    let companion_token_state = crate::router::companion_token_routes::CompanionTokenRouterState {
        companion_service: services.companion_service.clone(),
        provider_repo: services.provider_repo.clone(),
        token_repo: services.companion_token_repo.clone(),
        token_validator: services.companion_token_validator.clone(),
    };

    // System routes protected by auth middleware
    let system_authenticated = system_routes(states.system)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Conversation routes protected by auth middleware
    let conversation_authenticated = conversation_routes(states.conversation.clone())
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    let conversation_ops_authenticated = conversation_ops_routes(states.conversation)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Remote agent routes protected by auth middleware
    let remote_agent_authenticated = remote_agent_routes(states.remote_agent)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Unified agent listing/refresh/test routes protected by auth middleware
    let agent_authenticated = agent_routes(states.agent)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Phase 3 (review #6/#12): global model-failover config GET/PUT, auth-gated.
    // Path string must match the frontend `agentModelFailover` exactly.
    let model_failover_authenticated = model_failover_routes(ModelFailoverRouterState {
        client_prefs: Arc::new(nomifun_db::SqliteClientPreferenceRepository::new(
            services.database.pool().clone(),
        )),
    })
    .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Connection test routes (Bedrock, Gemini) protected by auth middleware
    let connection_test_authenticated = connection_test_routes(states.connection_test)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // File routes protected by auth middleware
    let file_authenticated = file_routes(states.file)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // MCP routes protected by auth middleware
    let mcp_authenticated = mcp_routes(states.mcp)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Extension routes protected by auth middleware
    let extension_authenticated = extension_routes(states.extension)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Hub routes protected by auth middleware
    let hub_authenticated = hub_routes(states.hub)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Skill routes protected by auth middleware
    let skill_authenticated = skill_routes(states.skill)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Channel routes protected by auth middleware
    let channel_authenticated = channel_routes(states.channel)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Cron routes protected by auth middleware
    let cron_authenticated = cron_routes(states.cron)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Requirements Platform routes protected by auth middleware
    let requirement_authenticated = requirement_routes(states.requirement)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // IDMM (Intelligent Decision-Making Mode) routes protected by auth middleware
    let idmm_authenticated = idmm_routes(states.idmm)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Companion (nomi) routes protected by auth middleware
    let companion_authenticated = companion_routes(states.companion.clone())
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // 对外伙伴 (public companion) enterprise-service domain — its OWN routes,
    // separate from the desktop companion. Protected by auth middleware.
    let public_agent_authenticated = public_agent_routes(states.public_agent.clone())
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // 创意工坊 (Creative Workshop) canvas/asset routes + 生成引擎 (creation) task
    // routes — owner-only management surface, behind auth middleware (same as
    // knowledge). The read-only binary serve routes (`/files/{id}`,
    // `/canvas-thumbs/{id}`) are split off into `workshop_public_routes` below,
    // mounted auth-exempt like the companion figure images. `states.workshop`
    // is cloned so both routers share the one live service (agent-op queue etc).
    let workshop_authenticated = workshop_routes(states.workshop.clone())
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));
    let creation_authenticated = creation_routes(states.creation)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Knowledge Base platform routes protected by auth middleware
    let knowledge_authenticated = knowledge_routes(states.knowledge)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Webhook + tag-settings routes protected by auth middleware
    let webhook_authenticated = webhook_routes(states.webhook)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // 智能编排 (orchestration) fleet + workspace CRUD routes protected by auth middleware
    let orchestrator_authenticated = orchestrator_routes(states.orchestrator)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // P3-X2: per-pet browser-use credential secret routes protected by auth middleware
    let secret_authenticated = secret_routes(states.secret)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Terminal routes protected by auth middleware
    let terminal_authenticated = terminal_routes(states.terminal)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Office routes protected by auth middleware
    let office_authenticated = office_routes(states.office.clone())
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Shell + STT routes protected by auth middleware
    let shell_authenticated = shell_routes(states.shell)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Assistant routes protected by auth middleware (T1a skeleton: all
    // handlers return 500 "not implemented"; T1b wires real service)
    let assistant_authenticated = assistant_routes(states.assistant)
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Guide MCP diagnostic endpoint protected by auth middleware
    let guide_mcp_authenticated = Router::new()
        .route("/api/system/guide-mcp", get(guide_mcp_status))
        .with_state(services.guide_mcp_config.clone())
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Computer-use OS permission status + prompt (macOS TCC). Stateless: the
    // handlers probe/trigger the host process's own grants. Auth-gated like the
    // other diagnostic endpoints. Registered on every build (handlers degrade to
    // null/no-op off macOS / non-computer-use), so the shared settings UI can
    // always query without a 404.
    let computer_permissions_authenticated = Router::new()
        .route("/api/computer/permissions", get(computer_permission_status))
        .route(
            "/api/computer/permissions/request",
            post(request_computer_permission),
        )
        .route(
            "/api/computer/permissions/open-settings",
            post(open_permission_settings),
        )
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // MCP registration template — stateless read-only GET, auth-gated.
    let mcp_register_template_authenticated = Router::new()
        .route(
            "/api/terminals/mcp-register-template",
            get(mcp_register_template_handler),
        )
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // One-click knowledge registration — POST, auth-gated.
    let register_knowledge_authenticated = Router::new()
        .route(
            "/api/terminals/register-knowledge",
            post(register_knowledge_handler),
        )
        .route(
            "/api/terminals/register-knowledge-global",
            post(register_knowledge_global_handler),
        )
        .route(
            "/api/terminals/unregister-knowledge-global",
            post(unregister_knowledge_global_handler),
        )
        .route(
            "/api/terminals/knowledge-global-status",
            get(knowledge_global_status_handler),
        )
        .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));

    // Office proxy routes — exempt from auth (serve iframe content)
    let office_proxy = office_proxy_routes(states.office);
    let public_assets = asset_routes(AssetRouterState::default());
    // Figure-image serving — exempt from auth: `<img>`/`new Image()` can't carry
    // the local-trust header, so the desktop webview would 403 every figure
    // thumbnail and the desktop companion would render blank. GET-only, opaque
    // unguessable ids; listing/creation stay authenticated. See `companion_public_routes`.
    let companion_public = companion_public_routes(states.companion);

    // 创意工坊 asset/thumbnail serving — exempt from auth for the same reason as
    // companion figure images: `<img>`/`<video>` subresource loads can't carry
    // the local-trust header, so an authenticated route would 403 every asset
    // preview and canvas gallery thumbnail. GET-only, opaque unguessable ids
    // (`wsa_`/`wsc_` + uuidv7); listing/upload/mutation stay authenticated.
    let workshop_public = workshop_public_routes(states.workshop);

    // WebSocket upgrade route — exempt from CSRF (no cookie-based
    // double-submit) but still gets security response headers.
    let ws_routes = Router::new()
        .route("/ws", get(ws_upgrade_handler))
        .with_state(ws_state);
    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: route groups built"
    );

    // Phase 2b: 「登录我的浏览器」——用户一键拉起可见登录浏览器(共享 profile),登录一次后静默会话复用。
    // 仅 browser-use 构建(需 CDP 引擎);面向桌面(headful 需显示器)。auth 中间件保护(与其它诊断端点同)。
    #[cfg(feature = "browser-use")]
    let browser_login_authenticated = {
        let browser_data_dir = nomi_config::config::app_config_dir()
            .map(|d| d.join("browser-data"))
            .unwrap_or_else(|| std::env::temp_dir().join("nomifun-browser-data"));
        let login_state = crate::router::browser_login::BrowserLoginState::new(
            browser_data_dir,
            crate::commands::bundled_chrome_dir(),
            services.encryption_key,
        );
        Router::new()
            .route(
                "/api/browser/login/open",
                post(crate::router::browser_login::open_browser_login),
            )
            .route(
                "/api/browser/login/close",
                post(crate::router::browser_login::close_browser_login),
            )
            .route(
                "/api/browser/login/status",
                get(crate::router::browser_login::browser_login_status),
            )
            .with_state(login_state)
            .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware))
    };

    let router = Router::new()
        .route("/health", get(health_check))
        .merge(auth_routes(auth_state))
        .merge(crate::router::companion_token_routes::companion_token_routes(companion_token_state))
        .merge(system_authenticated)
        .merge(computer_permissions_authenticated)
        .merge(conversation_authenticated)
        .merge(conversation_ops_authenticated)
        .merge(remote_agent_authenticated)
        .merge(agent_authenticated)
        .merge(model_failover_authenticated)
        .merge(connection_test_authenticated)
        .merge(file_authenticated)
        .merge(mcp_authenticated)
        .merge(extension_authenticated)
        .merge(hub_authenticated)
        .merge(skill_authenticated)
        .merge(channel_authenticated)
        .merge(cron_authenticated)
        .merge(requirement_authenticated)
        .merge(idmm_authenticated)
        .merge(companion_authenticated)
        .merge(public_agent_authenticated)
        .merge(workshop_authenticated)
        .merge(creation_authenticated)
        .merge(knowledge_authenticated)
        .merge(webhook_authenticated)
        .merge(orchestrator_authenticated)
        .merge(secret_authenticated)
        .merge(terminal_authenticated)
        .merge(office_authenticated)
        .merge(shell_authenticated)
        .merge(assistant_authenticated)
        .merge(guide_mcp_authenticated)
        .merge(mcp_register_template_authenticated)
        .merge(register_knowledge_authenticated);

    // Phase 2b: mount the login-browser routes (browser-use builds only).
    #[cfg(feature = "browser-use")]
    let router = router.merge(browser_login_authenticated);

    // CSRF (Double Submit Cookie) protects cookie-authenticated (remote
    // browser) requests. It is skipped entirely under NoAuth, and skips
    // per-request for locally-trusted (header-trusted) requests inside the
    // middleware itself.
    let router = if services.auth_policy.is_no_auth() {
        router
    } else {
        router.layer(middleware::from_fn_with_state(
            services.cookie_config.clone(),
            csrf_middleware,
        ))
    }
    .merge(ws_routes)
    .merge(office_proxy)
    .merge(public_assets)
    .merge(companion_public)
    .merge(workshop_public)
    .layer(middleware::from_fn(security_headers_middleware));

    // Raise the default request body limit from axum's 2MB default to
    // `BODY_LIMIT` (10MB). Routes that need a larger cap (e.g. `/api/fs/upload`)
    // disable this default and install their own `RequestBodyLimitLayer`.
    let router = router.layer(DefaultBodyLimit::max(nomifun_common::constants::BODY_LIMIT));

    let router = with_access_log(router);

    // Global, OUTERMOST trust resolution: runs before CSRF and per-route auth so
    // both can read the `LocalTrusted` marker / injected system `CurrentUser`.
    // Under TrustLocalToken the desktop webview's per-boot secret header grants
    // trust; under NoAuth every request is trusted; under Required none is.
    let trust_state = TrustState {
        policy: services.auth_policy,
        local_trust_secret: services.local_trust_secret.clone(),
    };
    let router = router.layer(middleware::from_fn_with_state(
        trust_state,
        trust_resolve_middleware,
    ));

    tracing::info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: route tree build with states completed"
    );

    // Permissive CORS for the desktop's own cross-origin webview (its document
    // origin is `tauri://` / `http://tauri.localhost`, not the loopback port).
    // Safe even on the LAN-bound listener: the trust secret rides a header (not
    // a cookie), so an `Any`-origin attacker page can neither read it nor read
    // cross-origin responses. Remote browsers are served same-origin and do not
    // rely on CORS.
    if services.auth_policy.allows_local_webview() {
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers(Any);
        router.layer(cors)
    } else {
        router
    }
}
