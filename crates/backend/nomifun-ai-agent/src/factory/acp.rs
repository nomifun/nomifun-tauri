use std::sync::Arc;

use crate::runtime_handle::AgentRuntimeHandle;
use crate::factory::AgentFactoryDeps;
use crate::factory::acp_assembler::{WorkspaceInfo, assemble_acp_params};
use crate::factory::context::FactoryContext;
use crate::manager::acp::{AcpAgentManager, CatalogForwarder};
use crate::types::AgentRuntimeBuildOptions;
use agent_client_protocol::schema::{
    EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerSse, McpServerStdio,
};
use nomifun_api_types::{AcpBuildExtra, McpServerId, SessionMcpServer, SessionMcpTransport};
use nomifun_common::{AgentId, AgentKillReason, AppError, CommandSpec};
use nomifun_db::IMcpServerRepository;
use nomifun_db::models::McpServerRow;
use nomifun_mcp::{AcpMcpCapabilities, parse_acp_mcp_capabilities};
use nomifun_runtime::resolve_command_path;
use tracing::{info, warn};

/// A factory future may be dropped by generation-scoped turn cancellation at
/// any await after the ACP process and its self-retaining router tasks exist.
/// Keep an armed teardown guard until the fully initialized handle is handed
/// to the registry so cancellation cannot orphan that process.
struct AcpConstructionGuard {
    agent: Option<Arc<AcpAgentManager>>,
}

impl AcpConstructionGuard {
    fn new(agent: Arc<AcpAgentManager>) -> Self {
        Self { agent: Some(agent) }
    }

    fn disarm(&mut self) {
        self.agent = None;
    }
}

impl Drop for AcpConstructionGuard {
    fn drop(&mut self) {
        if let Some(agent) = self.agent.take() {
            let _ = crate::AgentRuntimeControl::kill(
                agent.as_ref(),
                Some(AgentKillReason::UserCancelled),
            );
        }
    }
}

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    options: AgentRuntimeBuildOptions,
    ctx: FactoryContext,
) -> Result<AgentRuntimeHandle, AppError> {
    let mut config: AcpBuildExtra = serde_json::from_value(options.extra)
        .map_err(|e| AppError::BadRequest(format!("Invalid ACP build options: {e}")))?;
    config.user_id = Some(options.user_id.clone());

    // Resolve the catalog row by its canonical business identity. Backend
    // labels are descriptive metadata and are never lookup keys or aliases.
    let agent_id = config
        .agent_id
        .clone()
        .ok_or_else(|| AppError::BadRequest("ACP agent requires agent_id in extra".into()))?;
    AgentId::parse(agent_id.clone()).map_err(|error| {
        AppError::BadRequest(format!(
            "ACP agent_id '{agent_id}' is not a canonical UUIDv7: {error}"
        ))
    })?;
    let meta = deps
        .agent_registry
        .get(&agent_id)
        .await
        .ok_or_else(|| AppError::BadRequest(format!("ACP agent '{agent_id}' does not exist")))?;

    // Trust the catalog row over any client-supplied backend label.
    config.backend.clone_from(&meta.backend);

    // `factory::build_agent` admits ACP only for the installation owner.  All
    // capability configs are therefore reconstructed from process-owned deps;
    // serialized Conversation JSON is never an authority source.
    config
        .requirement_mcp_config
        .clone_from(&deps.requirement_mcp_config);
    config.knowledge_mcp_config = if config.knowledge_mounts.is_empty() {
        None
    } else {
        deps.knowledge_mcp_config.clone()
    };
    config.open_mcp_config.clone_from(&deps.open_mcp_config);
    config
        .computer_mcp_config
        .clone_from(&deps.computer_mcp_config);
    config
        .browser_mcp_config
        .clone_from(&deps.browser_mcp_config);

    // Every owner ACP runtime is entitled to the platform gateway.  The grant
    // is derived here and represented solely by the process-owned scoped
    // config; there is no persisted boolean authority flag.
    config.gateway_mcp_config.clone_from(&deps.gateway_mcp_config);

    if config.gateway_mcp_config.is_some() {
        info!(
            ctx.conversation_id,
            gateway_mcp_port = deps.gateway_mcp_config.as_ref().map(|config| config.port()),
            "gateway_mcp: injected into owner ACP session"
        );
    }

    // Registry resolved the spawn command via `which()` at
    // hydrate time. A missing `resolved_command` means either the
    // CLI was uninstalled between hydrate and now, or the row
    // never had a command (e.g. remote-only). Either way the
    // caller needs to see a BadRequest, not a confusing
    // spawn-time error.
    let (command, args, mut env, cwd) = (
        meta.resolved_command.clone().ok_or_else(|| {
            AppError::BadRequest(format!("Agent '{}' CLI not found in PATH", meta.name))
        })?,
        meta.args.clone(),
        meta.env
            .iter()
            .map(|e| nomifun_common::EnvVar {
                name: e.name.clone(),
                value: e.value.clone(),
            })
            .collect::<Vec<_>>(),
        Some(ctx.workspace.clone()),
    );
    if meta.backend.as_deref() == Some("claude") {
        let cc_switch_env = crate::cc_switch::read_claude_provider_env();
        if !cc_switch_env.is_empty() {
            let keys: Vec<&str> = cc_switch_env.keys().map(|k| k.as_str()).collect();
            for (name, value) in &cc_switch_env {
                env.push(nomifun_common::EnvVar {
                    name: name.clone(),
                    value: value.clone(),
                });
            }
            tracing::info!(?keys, "cc-switch: env vars injected");
        }
    }

    let command_spec = CommandSpec {
        command,
        args,
        env,
        cwd,
    };
    let session_snapshot = deps
        .acp_agent_service
        .load_snapshot_state(&ctx.conversation_id)
        .await;

    // Load user-configured MCP servers from the DB so they reach
    // ACP `session/new` mcpServers payload. Without this the agent
    // starts with zero MCP tools even when the user configured them
    // via Settings → MCP (ELECTRON-1JG).
    let mcp_capabilities = meta
        .handshake
        .agent_capabilities
        .as_ref()
        .map(parse_acp_mcp_capabilities)
        .unwrap_or_default();

    let user_mcp_servers = match deps.mcp_server_repo.as_ref() {
        Some(repo) => {
            load_user_mcp_servers(
                repo.as_ref(),
                config.mcp_server_ids.as_deref(),
                &ctx.conversation_id,
                &mcp_capabilities,
            )
            .await
        }
        None => Vec::new(),
    };
    let mut session_mcp_servers = user_mcp_servers;
    for server in &config.session_mcp_servers {
        if !session_server_supported_by_capabilities(server, &mcp_capabilities) {
            warn!(
                ctx.conversation_id,
                mcp_server_id = %server.mcp_server_id,
                server_name = %server.name,
                "session_mcp: transport unsupported by ACP agent; skipping"
            );
            continue;
        }
        match session_server_to_sdk_mcp_server(server) {
            Ok(server) => session_mcp_servers.push(server),
            Err(err) => {
                warn!(
                    ctx.conversation_id,
                    mcp_server_id = %server.mcp_server_id,
                    server_name = %server.name,
                    error = %err,
                    "session_mcp: failed to convert session snapshot; skipping"
                );
            }
        }
    }

    let params = Arc::new(
        assemble_acp_params(
            ctx.conversation_id.clone(),
            WorkspaceInfo {
                path: ctx.workspace,
                is_custom: ctx.is_custom_workspace,
            },
            meta,
            command_spec,
            config,
            session_mcp_servers,
            session_snapshot,
            deps.data_dir.clone(),
        )
        .await,
    );

    let skill_mgr = deps.skill_manager.clone();
    let catalog_tx = deps.agent_registry.catalog_sender();

    let (agent, domain_rx, notification_rx) =
        AcpAgentManager::build(params, skill_mgr, &catalog_tx).await?;

    let arc = Arc::new(agent);
    let mut construction_guard = AcpConstructionGuard::new(Arc::clone(&arc));
    arc.start_permission_handler();
    arc.start_session_event_tracker(notification_rx);
    CatalogForwarder::spawn(
        arc.agent_id().to_owned(),
        crate::AgentRuntimeControl::subscribe(arc.as_ref()),
        catalog_tx,
    );

    // Desired (mode/model/config) are seeded from `params.session_snapshot`
    // inside `AcpAgentManager::new`. The CLI-assigned session id is still
    // loaded here so the first turn after a task rebuild takes the resume
    // path.
    if let Some(sid) = deps
        .acp_agent_service
        .load_session_id(&ctx.conversation_id)
        .await
    {
        arc.set_session_id(sid).await;
    }

    // Open the ACP session eagerly so `POST /warmup` returns only after
    // session/new (or claude-meta-resume / session/load) and the first
    // reconcile pass have completed. Matches nomi factory behaviour:
    // the caller sees "warmed up" == "ready for PUT /mode | /model".
    arc.warmup_session().await?;

    let instance = AgentRuntimeHandle::Acp(Arc::clone(&arc));

    // Hand the service the domain event receiver so it can
    // persist user intent changes without reverse-engineering
    // them from CLI observations.
    deps.acp_agent_service
        .attach(ctx.conversation_id, domain_rx)
        .await;

    construction_guard.disarm();
    Ok(instance)
}

/// Load the operator's enabled MCP servers from the DB, log+skip any rows
/// whose `transport_config` JSON fails to parse (better to start without one
/// MCP tool than fail the whole session), and return them in SDK shape ready
/// for `NewSessionRequest::mcp_servers`.
///
/// When `selected_ids` is present, those rows define the session snapshot and
/// are injected regardless of the current global `enabled` flag. Legacy
/// conversations without a snapshot still fall back to "all enabled rows".
/// Builtins are wired through other paths and are not loaded from the user MCP table.
async fn load_user_mcp_servers(
    repo: &dyn IMcpServerRepository,
    selected_ids: Option<&[McpServerId]>,
    conversation_id: &str,
    capabilities: &AcpMcpCapabilities,
) -> Vec<McpServer> {
    let rows_result = match selected_ids {
        Some(ids) => {
            let ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
            repo.list_by_ids_any(&ids).await
        }
        None => repo.list().await,
    };
    let rows = match rows_result {
        Ok(r) => r,
        Err(err) => {
            warn!(
                conversation_id,
                error = %err,
                "user_mcp: list() failed; skipping injection"
            );
            return Vec::new();
        }
    };

    let mut servers = Vec::with_capacity(rows.len());
    for row in rows {
        let selected = selected_ids
            .map(|ids| {
                ids.iter()
                    .any(|id| id.as_str() == row.mcp_server_id)
            })
            .unwrap_or(row.enabled);
        if !selected || row.builtin {
            continue;
        }
        if !row_supported_by_capabilities(&row, capabilities) {
            warn!(
                conversation_id,
                mcp_server_id = %row.mcp_server_id,
                server_name = %row.name,
                transport_type = %row.transport_type,
                "user_mcp: transport unsupported by ACP agent; skipping"
            );
            continue;
        }
        match row_to_sdk_mcp_server(&row) {
            Ok(server) => servers.push(server),
            Err(err) => {
                warn!(
                    conversation_id,
                    mcp_server_id = %row.mcp_server_id,
                    server_name = %row.name,
                    error = %err,
                    "user_mcp: failed to convert row; skipping"
                );
            }
        }
    }

    if !servers.is_empty() {
        info!(
            conversation_id,
            count = servers.len(),
            "user_mcp: injected into session/new"
        );
    }
    servers
}

/// Convert an `McpServerRow` into the SDK `McpServer` shape used by
/// `NewSessionRequest::mcp_servers`. Returns an error string when
/// `transport_config` is malformed or required fields are missing.
fn row_to_sdk_mcp_server(row: &McpServerRow) -> Result<McpServer, String> {
    let value: serde_json::Value = serde_json::from_str(&row.transport_config)
        .map_err(|e| format!("invalid transport_config JSON: {e}"))?;

    match row.transport_type.as_str() {
        "stdio" => {
            let command = value
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "stdio: missing command".to_owned())?;
            let resolved_command = resolve_stdio_command(command);
            let args: Vec<String> = value
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let env: Vec<EnvVariable> = value
                .get("env")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    let mut entries: Vec<(String, String)> = obj
                        .iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                        .collect();
                    // Sort for deterministic ordering across runs.
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    entries
                        .into_iter()
                        .map(|(k, v)| EnvVariable::new(k, v))
                        .collect()
                })
                .unwrap_or_default();

            let stdio = McpServerStdio::new(row.name.clone(), resolved_command)
                .args(args)
                .env(env);
            Ok(McpServer::Stdio(stdio))
        }
        "http" | "streamable_http" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "http: missing url".to_owned())?;
            let headers = parse_headers(value.get("headers"));
            Ok(McpServer::Http(
                McpServerHttp::new(row.name.clone(), url).headers(headers),
            ))
        }
        "sse" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "sse: missing url".to_owned())?;
            let headers = parse_headers(value.get("headers"));
            Ok(McpServer::Sse(
                McpServerSse::new(row.name.clone(), url).headers(headers),
            ))
        }
        other => Err(format!("unknown transport type: {other}")),
    }
}

fn parse_headers(value: Option<&serde_json::Value>) -> Vec<HttpHeader> {
    let Some(obj) = value.and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut entries: Vec<(String, String)> = obj
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
        .into_iter()
        .map(|(k, v)| HttpHeader::new(k, v))
        .collect()
}

fn session_server_to_sdk_mcp_server(server: &SessionMcpServer) -> Result<McpServer, String> {
    match &server.transport {
        SessionMcpTransport::Stdio { command, args, env } => {
            if command.is_empty() {
                return Err("stdio: missing command".to_owned());
            }
            let mut entries: Vec<(String, String)> =
                env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let env = entries
                .into_iter()
                .map(|(k, v)| EnvVariable::new(k, v))
                .collect();
            Ok(McpServer::Stdio(
                McpServerStdio::new(server.name.clone(), resolve_stdio_command(command))
                    .args(args.clone())
                    .env(env),
            ))
        }
        SessionMcpTransport::Http { url, headers }
        | SessionMcpTransport::StreamableHttp { url, headers } => {
            if url.is_empty() {
                return Err("http: missing url".to_owned());
            }
            let mut entries: Vec<(String, String)> = headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let headers = entries
                .into_iter()
                .map(|(k, v)| HttpHeader::new(k, v))
                .collect();
            Ok(McpServer::Http(
                McpServerHttp::new(server.name.clone(), url).headers(headers),
            ))
        }
        SessionMcpTransport::Sse { url, headers } => {
            if url.is_empty() {
                return Err("sse: missing url".to_owned());
            }
            let mut entries: Vec<(String, String)> = headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let headers = entries
                .into_iter()
                .map(|(k, v)| HttpHeader::new(k, v))
                .collect();
            Ok(McpServer::Sse(
                McpServerSse::new(server.name.clone(), url).headers(headers),
            ))
        }
    }
}

fn resolve_stdio_command(command: &str) -> String {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return command.to_owned();
    }

    let path = std::path::Path::new(trimmed);
    if path.is_absolute()
        || trimmed.contains(std::path::MAIN_SEPARATOR)
        || trimmed.contains('/')
        || trimmed.contains('\\')
    {
        return trimmed.to_owned();
    }

    resolve_command_path(trimmed)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| trimmed.to_owned())
}

fn row_supported_by_capabilities(row: &McpServerRow, capabilities: &AcpMcpCapabilities) -> bool {
    match row.transport_type.as_str() {
        "stdio" => capabilities.stdio,
        "http" | "streamable_http" => capabilities.http,
        "sse" => capabilities.sse,
        _ => false,
    }
}

fn session_server_supported_by_capabilities(
    server: &SessionMcpServer,
    capabilities: &AcpMcpCapabilities,
) -> bool {
    match server.transport {
        SessionMcpTransport::Stdio { .. } => capabilities.stdio,
        SessionMcpTransport::Http { .. } | SessionMcpTransport::StreamableHttp { .. } => {
            capabilities.http
        }
        SessionMcpTransport::Sse { .. } => capabilities.sse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_row(
        _fixture_number: i64,
        name: &str,
        transport_type: &str,
        transport_config: &str,
        enabled: bool,
        builtin: bool,
    ) -> McpServerRow {
        McpServerRow {
            mcp_server_id: nomifun_common::generate_id(),
            name: name.to_owned(),
            description: None,
            enabled,
            transport_type: transport_type.into(),
            transport_config: transport_config.into(),
            tools: None,
            last_test_status: "disconnected".into(),
            last_connected: None,
            original_json: None,
            builtin,
            deleted_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn row_to_sdk_stdio_roundtrip() {
        let row = make_row(
            1,
            "ctx7",
            "stdio",
            r#"{"command":"npx","args":["-y","@upstash/context7-mcp"],"env":{"K":"V"}}"#,
            true,
            false,
        );
        let server = row_to_sdk_mcp_server(&row).expect("convert");
        match server {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, "ctx7");
                // `resolve_command_path` may resolve to an absolute path; on
                // Windows that includes the `.cmd`/`.exe` extension.
                let command = s
                    .command
                    .to_string_lossy()
                    .replace('\\', "/")
                    .to_lowercase();
                assert!(
                    command == "npx" || command.ends_with("/npx") || command.ends_with("/npx.cmd"),
                    "unexpected stdio command path: {command}",
                );
                assert_eq!(
                    s.args,
                    vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()]
                );
                assert_eq!(s.env.len(), 1);
                assert_eq!(s.env[0].name, "K");
                assert_eq!(s.env[0].value, "V");
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn row_to_sdk_http_with_headers() {
        let row = make_row(
            2,
            "remote",
            "http",
            r#"{"url":"https://example.com/mcp","headers":{"Authorization":"Bearer tok"}}"#,
            true,
            false,
        );
        let server = row_to_sdk_mcp_server(&row).expect("convert");
        match server {
            McpServer::Http(h) => {
                assert_eq!(h.name, "remote");
                assert_eq!(h.url, "https://example.com/mcp");
                assert_eq!(h.headers.len(), 1);
                assert_eq!(h.headers[0].name, "Authorization");
                assert_eq!(h.headers[0].value, "Bearer tok");
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn row_to_sdk_unknown_transport_type_errors() {
        let row = make_row(3, "bad", "websocket", "{}", true, false);
        assert!(row_to_sdk_mcp_server(&row).is_err());
    }

    #[test]
    fn row_to_sdk_invalid_json_errors() {
        let row = make_row(4, "bad", "stdio", "not-json", true, false);
        assert!(row_to_sdk_mcp_server(&row).is_err());
    }

    #[test]
    fn row_to_sdk_stdio_missing_command_errors() {
        let row = make_row(5, "bad", "stdio", r#"{"args":[]}"#, true, false);
        assert!(row_to_sdk_mcp_server(&row).is_err());
    }

    // -- load_user_mcp_servers integration -----------------------------------

    use async_trait::async_trait;
    use std::sync::Arc;

    struct MockRepo {
        rows: Vec<McpServerRow>,
        fail: bool,
    }

    #[async_trait]
    impl IMcpServerRepository for MockRepo {
        async fn list(&self) -> Result<Vec<McpServerRow>, nomifun_db::DbError> {
            if self.fail {
                Err(nomifun_db::DbError::Init("simulated".into()))
            } else {
                Ok(self.rows.clone())
            }
        }
        async fn find_by_id(&self, _mcp_server_id: &str) -> Result<Option<McpServerRow>, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn find_by_name(
            &self,
            _name: &str,
        ) -> Result<Option<McpServerRow>, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn list_by_ids_any(
            &self,
            mcp_server_ids: &[String],
        ) -> Result<Vec<McpServerRow>, nomifun_db::DbError> {
            if self.fail {
                return Err(nomifun_db::DbError::Init("simulated".into()));
            }
            Ok(mcp_server_ids
                .iter()
                .filter_map(|id| {
                    self.rows
                        .iter()
                        .find(|row| row.mcp_server_id == *id)
                        .cloned()
                })
                .collect())
        }
        async fn create(
            &self,
            _params: nomifun_db::CreateMcpServerParams<'_>,
        ) -> Result<McpServerRow, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn update(
            &self,
            _mcp_server_id: &str,
            _params: nomifun_db::UpdateMcpServerParams<'_>,
        ) -> Result<McpServerRow, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn delete(&self, _mcp_server_id: &str) -> Result<(), nomifun_db::DbError> {
            unimplemented!()
        }
        async fn batch_upsert(
            &self,
            _servers: &[nomifun_db::CreateMcpServerParams<'_>],
        ) -> Result<Vec<McpServerRow>, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn update_status(
            &self,
            _mcp_server_id: &str,
            _status: &str,
            _last_connected: Option<nomifun_common::TimestampMs>,
        ) -> Result<(), nomifun_db::DbError> {
            unimplemented!()
        }
        async fn update_tools(
            &self,
            _mcp_server_id: &str,
            _tools: Option<&str>,
        ) -> Result<(), nomifun_db::DbError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_skips_disabled_and_builtin() {
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![
                make_row(
                    10,
                    "user-enabled",
                    "stdio",
                    r#"{"command":"npx","args":[],"env":{}}"#,
                    true,
                    false,
                ),
                make_row(
                    11,
                    "user-disabled",
                    "stdio",
                    r#"{"command":"npx","args":[],"env":{}}"#,
                    false,
                    false,
                ),
                make_row(
                    12,
                    "builtin",
                    "stdio",
                    r#"{"command":"img-gen","args":[],"env":{}}"#,
                    true,
                    true,
                ),
            ],
            fail: false,
        });
        let servers = load_user_mcp_servers(repo.as_ref(), None, "conv-1", &caps).await;
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "user-enabled"),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_returns_empty_on_repo_failure() {
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![],
            fail: true,
        });
        let servers = load_user_mcp_servers(repo.as_ref(), None, "conv-1", &caps).await;
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn load_user_mcp_servers_skips_malformed_rows_but_keeps_others() {
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![
                make_row(
                    20,
                    "good",
                    "stdio",
                    r#"{"command":"npx","args":[],"env":{}}"#,
                    true,
                    false,
                ),
                make_row(21, "bad", "stdio", "not-json", true, false),
            ],
            fail: false,
        });
        let servers = load_user_mcp_servers(repo.as_ref(), None, "conv-1", &caps).await;
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "good"),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_uses_selected_snapshot_over_enabled_state() {
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let disabled_picked = make_row(
            31,
            "disabled-picked",
            "stdio",
            r#"{"command":"uvx","args":[],"env":{}}"#,
            false,
            false,
        );
        let selected = vec![
            McpServerId::parse(disabled_picked.mcp_server_id.clone())
                .expect("fixture mcp_server_id"),
        ];
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![
                make_row(
                    30,
                    "enabled",
                    "stdio",
                    r#"{"command":"npx","args":[],"env":{}}"#,
                    true,
                    false,
                ),
                disabled_picked,
            ],
            fail: false,
        });

        let servers = load_user_mcp_servers(repo.as_ref(), Some(&selected), "conv-1", &caps).await;

        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "disabled-picked"),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_skips_rows_unsupported_by_capabilities() {
        let caps = AcpMcpCapabilities {
            stdio: false,
            http: true,
            sse: false,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![make_row(
                40,
                "stdio-only",
                "stdio",
                r#"{"command":"npx","args":[],"env":{}}"#,
                true,
                false,
            )],
            fail: false,
        });

        let servers = load_user_mcp_servers(repo.as_ref(), None, "conv-1", &caps).await;
        assert!(servers.is_empty());
    }
}
