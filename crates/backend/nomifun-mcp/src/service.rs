use std::sync::Arc;

use nomifun_api_types::{
    BatchImportMcpServersRequest, CreateMcpServerRequest, McpConnectionTestResult, McpServerResponse,
    UpdateMcpServerRequest,
};
use nomifun_common::now_ms;
use nomifun_db::{CreateMcpServerParams, IMcpServerRepository, UpdateMcpServerParams};
use tracing::{info, warn};

use crate::error::McpError;
use crate::types::{McpServer, McpServerTransport};

const SPLITTABLE_STDIO_LAUNCHERS: &[&str] = &["npx", "pnpx", "bunx", "uvx", "uv", "node", "python", "python3", "deno"];

// ---------------------------------------------------------------------------
// McpConfigService
// ---------------------------------------------------------------------------

/// MCP server configuration CRUD service.
///
/// Handles create/read/update/delete operations on MCP server configs,
/// delegating persistence to `IMcpServerRepository`. Business rules:
///
/// - **add**: upsert by name (existing → update, new → create)
/// - **delete**: removes the stored MCP definition
/// - **toggle**: flips enabled state
/// - **batch_import**: sequential upsert by name
#[derive(Clone)]
pub struct McpConfigService {
    repo: Arc<dyn IMcpServerRepository>,
}

impl McpConfigService {
    pub fn new(repo: Arc<dyn IMcpServerRepository>) -> Self {
        Self { repo }
    }

    /// List all MCP servers.
    pub async fn list_servers(&self) -> Result<Vec<McpServerResponse>, McpError> {
        let rows = self.repo.list().await?;
        rows.into_iter()
            .map(|row| McpServer::from_row(row).map(McpServer::into_response))
            .collect()
    }

    /// Get a single MCP server by ID.
    pub async fn get_server(&self, id: &str) -> Result<McpServerResponse, McpError> {
        let id = parse_server_id(id)?;
        let row = self
            .repo
            .find_by_id(id)
            .await?
            .ok_or_else(|| McpError::NotFound(id.to_string()))?;
        let server = McpServer::from_row(row)?;
        Ok(server.into_response())
    }

    /// Add (or upsert) an MCP server.
    ///
    /// If a server with the same name already exists, it is updated
    /// (transport, description, original_json) rather than creating a duplicate.
    pub async fn add_server(&self, req: CreateMcpServerRequest) -> Result<McpServerResponse, McpError> {
        let transport = normalize_transport(McpServerTransport::from(req.transport))?;
        self.upsert_server(
            &req.name,
            req.description.as_deref(),
            &transport,
            req.original_json.as_deref(),
            req.builtin,
            false,
        )
        .await
    }

    /// Edit an existing MCP server (partial update).
    pub async fn edit_server(&self, id: &str, req: UpdateMcpServerRequest) -> Result<McpServerResponse, McpError> {
        let id = parse_server_id(id)?;
        // Verify the server exists
        let existing_server = self
            .repo
            .find_by_id(id)
            .await?
            .ok_or_else(|| McpError::NotFound(id.to_string()))?;

        if let Some(ref new_name) = req.name
            && new_name != &existing_server.name
        {
            return Err(McpError::InvalidEdit(format!(
                "MCP server name cannot be changed during edit; keep '{current_name}'",
                current_name = existing_server.name
            )));
        }

        // Check name uniqueness if renaming
        if let Some(ref new_name) = req.name
            && let Some(existing) = self.repo.find_by_name_any(new_name).await?
            && existing.id != id
        {
            if existing.builtin {
                return Err(McpError::Conflict(format!(
                    "Builtin MCP server name '{new_name}' is reserved"
                )));
            }
            return Err(McpError::Conflict(new_name.clone()));
        }

        // Build transport fields if provided
        let transport = req
            .transport
            .map(McpServerTransport::from)
            .map(normalize_transport)
            .transpose()?;
        let config_json = transport.as_ref().map(McpServerTransport::to_config_json).transpose()?;

        let params = UpdateMcpServerParams {
            name: req.name.as_deref(),
            description: req.description.as_ref().map(|opt| opt.as_deref()),
            transport_type: transport.as_ref().map(McpServerTransport::transport_type),
            transport_config: config_json.as_deref(),
            original_json: req.original_json.as_ref().map(|opt| opt.as_deref()),
            builtin: req.builtin,
            ..Default::default()
        };

        let row = self.repo.update(id, params).await?;
        let server = McpServer::from_row(row)?;
        Ok(server.into_response())
    }

    /// Soft-delete an MCP server by ID.
    ///
    /// Returns whether the deleted server was enabled.
    pub async fn delete_server(&self, id: &str) -> Result<bool, McpError> {
        let id = parse_server_id(id)?;
        let row = self
            .repo
            .find_by_id(id)
            .await?
            .ok_or_else(|| McpError::NotFound(id.to_string()))?;
        let was_enabled = row.enabled;
        self.repo.delete(id).await?;
        Ok(was_enabled)
    }

    /// Toggle the enabled state of an MCP server.
    ///
    /// Returns the updated server response.
    pub async fn toggle_server(&self, id: &str) -> Result<McpServerResponse, McpError> {
        let id = parse_server_id(id)?;
        let row = self
            .repo
            .find_by_id(id)
            .await?
            .ok_or_else(|| McpError::NotFound(id.to_string()))?;

        let new_enabled = !row.enabled;
        let params = UpdateMcpServerParams {
            enabled: Some(new_enabled),
            ..Default::default()
        };
        let updated = self.repo.update(id, params).await?;
        let server = McpServer::from_row(updated)?;
        Ok(server.into_response())
    }

    /// Batch import MCP servers (upsert by name).
    ///
    /// Each server is processed individually: existing names are updated,
    /// new names are created.
    pub async fn batch_import(&self, req: BatchImportMcpServersRequest) -> Result<Vec<McpServerResponse>, McpError> {
        let requested_count = req.servers.len();
        let mut rows = Vec::with_capacity(requested_count);
        let mut skipped_reserved_count = 0usize;
        for server_req in req.servers {
            if let Some(existing) = self.repo.find_by_name_any(&server_req.name).await?
                && existing.builtin
            {
                skipped_reserved_count += 1;
                warn!(
                    name = %server_req.name,
                    "skipping batch import for builtin MCP name"
                );
                continue;
            }

            let transport = normalize_transport(McpServerTransport::from(server_req.transport))?;
            let server = self
                .upsert_server(
                    &server_req.name,
                    server_req.description.as_deref(),
                    &transport,
                    server_req.original_json.as_deref(),
                    server_req.builtin,
                    server_req.enabled.unwrap_or(false),
                )
                .await?;
            rows.push(server);
        }
        info!(
            requested_count,
            imported_count = rows.len(),
            skipped_reserved_count,
            enabled_count = rows.iter().filter(|row| row.enabled).count(),
            "batch imported MCP servers"
        );
        Ok(rows)
    }

    /// Persist the latest connection test result for an existing MCP server.
    pub async fn persist_test_result(&self, id: i64, result: &McpConnectionTestResult) -> Result<(), McpError> {
        let status = if result.success { "connected" } else { "error" };
        let last_connected = if result.success { Some(now_ms()) } else { None };
        let tools_json = result.tools.as_ref().map(serde_json::to_string).transpose()?;

        self.repo.update_status(id, status, last_connected).await?;
        self.repo.update_tools(id, tools_json.as_deref()).await?;
        Ok(())
    }

    async fn upsert_server(
        &self,
        name: &str,
        description: Option<&str>,
        transport: &McpServerTransport,
        original_json: Option<&str>,
        builtin: bool,
        enabled: bool,
    ) -> Result<McpServerResponse, McpError> {
        let config_json = transport.to_config_json()?;

        if let Some(existing) = self.repo.find_by_name_any(name).await? {
            if existing.builtin {
                return Err(McpError::Conflict(format!(
                    "Builtin MCP server name '{name}' is reserved"
                )));
            }

            let params = UpdateMcpServerParams {
                description: Some(description),
                enabled: Some(enabled),
                transport_type: Some(transport.transport_type()),
                transport_config: Some(&config_json),
                original_json: Some(original_json),
                builtin: Some(existing.builtin || builtin),
                deleted_at: Some(None),
                ..Default::default()
            };
            let updated = self.repo.update(existing.id, params).await?;
            let server = McpServer::from_row(updated)?;
            return Ok(server.into_response());
        }

        let params = CreateMcpServerParams {
            name,
            description,
            enabled,
            transport_type: transport.transport_type(),
            transport_config: &config_json,
            tools: None,
            original_json,
            builtin,
        };
        let row = self.repo.create(params).await?;
        let server = McpServer::from_row(row)?;
        Ok(server.into_response())
    }
}

/// Parses a host-local MCP server primary key carried as a URL path string into
/// an `i64`. A non-numeric id can never match a stored row, so it is reported
/// as `NotFound` rather than a parse error.
fn parse_server_id(id: &str) -> Result<i64, McpError> {
    id.parse::<i64>().map_err(|_| McpError::NotFound(id.to_owned()))
}

fn normalize_transport(transport: McpServerTransport) -> Result<McpServerTransport, McpError> {    match transport {
        McpServerTransport::Stdio { command, args, env } if args.is_empty() => {
            let Some((normalized_command, normalized_args)) = split_stdio_command(&command)? else {
                return Ok(McpServerTransport::Stdio { command, args, env });
            };
            Ok(McpServerTransport::Stdio {
                command: normalized_command,
                args: normalized_args,
                env,
            })
        }
        _ => Ok(transport),
    }
}

fn split_stdio_command(command: &str) -> Result<Option<(String, Vec<String>)>, McpError> {
    let trimmed = command.trim();
    if trimmed.is_empty() || !trimmed.contains(char::is_whitespace) {
        return Ok(None);
    }

    let first_token = trimmed
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(&['"', '\''][..]);
    if !SPLITTABLE_STDIO_LAUNCHERS.contains(&first_token) {
        return Ok(None);
    }

    let tokens = shell_split(trimmed).map_err(McpError::InvalidTransport)?;
    if tokens.len() < 2 {
        return Ok(None);
    }

    Ok(Some((tokens[0].clone(), tokens[1..].to_vec())))
}

fn shell_split(input: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(ch) = chars.next() {
        match quote {
            Some(active) => {
                if ch == active {
                    quote = None;
                } else if ch == '\\' && active == '"' {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '"' | '\'' => quote = Some(ch),
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                c if c.is_whitespace() => {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(ch),
            },
        }
    }

    if quote.is_some() {
        return Err("Unterminated quoted command string".to_owned());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::{ImportMcpServerRequest, McpTransport};
    use nomifun_common::{McpServerStatus, TimestampMs};
    use nomifun_db::models::McpServerRow;
    use nomifun_db::{CreateMcpServerParams, DbError, UpdateMcpServerParams};
    use std::collections::HashMap;
    use std::sync::Mutex;

    // -- In-memory mock repository -------------------------------------------

    #[derive(Debug)]
    struct MockMcpServerRepo {
        servers: Mutex<Vec<McpServerRow>>,
        id_counter: Mutex<i64>,
    }

    impl MockMcpServerRepo {
        fn new() -> Self {
            Self {
                servers: Mutex::new(Vec::new()),
                id_counter: Mutex::new(0),
            }
        }

        fn next_id(&self) -> i64 {
            let mut counter = self.id_counter.lock().unwrap();
            *counter += 1;
            *counter
        }

        fn now() -> TimestampMs {
            1000
        }
    }

    #[async_trait::async_trait]
    impl IMcpServerRepository for MockMcpServerRepo {
        async fn list(&self) -> Result<Vec<McpServerRow>, DbError> {
            let servers = self.servers.lock().unwrap();
            Ok(servers.iter().filter(|s| s.deleted_at.is_none()).cloned().collect())
        }

        async fn find_by_id(&self, id: i64) -> Result<Option<McpServerRow>, DbError> {
            let servers = self.servers.lock().unwrap();
            Ok(servers.iter().find(|s| s.id == id && s.deleted_at.is_none()).cloned())
        }

        async fn find_by_name(&self, name: &str) -> Result<Option<McpServerRow>, DbError> {
            let servers = self.servers.lock().unwrap();
            Ok(servers
                .iter()
                .find(|s| s.name == name && s.deleted_at.is_none())
                .cloned())
        }

        async fn find_by_id_any(&self, id: i64) -> Result<Option<McpServerRow>, DbError> {
            let servers = self.servers.lock().unwrap();
            Ok(servers.iter().find(|s| s.id == id).cloned())
        }

        async fn find_by_name_any(&self, name: &str) -> Result<Option<McpServerRow>, DbError> {
            let servers = self.servers.lock().unwrap();
            Ok(servers.iter().find(|s| s.name == name).cloned())
        }

        async fn list_by_ids_any(&self, ids: &[i64]) -> Result<Vec<McpServerRow>, DbError> {
            let servers = self.servers.lock().unwrap();
            Ok(servers
                .iter()
                .filter(|server| ids.iter().any(|id| *id == server.id))
                .cloned()
                .collect())
        }

        async fn create(&self, params: CreateMcpServerParams<'_>) -> Result<McpServerRow, DbError> {
            let mut servers = self.servers.lock().unwrap();
            if servers.iter().any(|s| s.name == params.name) {
                return Err(DbError::Conflict(format!(
                    "MCP server name '{}' already exists",
                    params.name
                )));
            }
            let row = McpServerRow {
                id: self.next_id(),
                name: params.name.to_owned(),
                description: params.description.map(String::from),
                enabled: params.enabled,
                transport_type: params.transport_type.to_owned(),
                transport_config: params.transport_config.to_owned(),
                tools: params.tools.map(String::from),
                last_test_status: "disconnected".to_owned(),
                last_connected: None,
                original_json: params.original_json.map(String::from),
                builtin: params.builtin,
                deleted_at: None,
                created_at: Self::now(),
                updated_at: Self::now(),
            };
            servers.push(row.clone());
            Ok(row)
        }

        async fn update(&self, id: i64, params: UpdateMcpServerParams<'_>) -> Result<McpServerRow, DbError> {
            let mut servers = self.servers.lock().unwrap();
            let idx = servers
                .iter()
                .position(|s| s.id == id)
                .ok_or_else(|| DbError::NotFound(format!("MCP server {id}")))?;

            // Check name conflict
            if let Some(new_name) = params.name {
                if servers.iter().any(|s| s.name == new_name && s.id != id) {
                    return Err(DbError::Conflict(format!(
                        "MCP server name '{new_name}' already exists"
                    )));
                }
                servers[idx].name = new_name.to_owned();
            }
            if let Some(desc) = params.description {
                servers[idx].description = desc.map(String::from);
            }
            if let Some(enabled) = params.enabled {
                servers[idx].enabled = enabled;
            }
            if let Some(tt) = params.transport_type {
                servers[idx].transport_type = tt.to_owned();
            }
            if let Some(tc) = params.transport_config {
                servers[idx].transport_config = tc.to_owned();
            }
            if let Some(tools) = params.tools {
                servers[idx].tools = tools.map(String::from);
            }
            if let Some(oj) = params.original_json {
                servers[idx].original_json = oj.map(String::from);
            }
            if let Some(b) = params.builtin {
                servers[idx].builtin = b;
            }
            if let Some(deleted_at) = params.deleted_at {
                servers[idx].deleted_at = deleted_at;
            }
            servers[idx].updated_at = Self::now();
            Ok(servers[idx].clone())
        }

        async fn delete(&self, id: i64) -> Result<(), DbError> {
            let mut servers = self.servers.lock().unwrap();
            let idx = servers
                .iter()
                .position(|s| s.id == id && s.deleted_at.is_none())
                .ok_or_else(|| DbError::NotFound(format!("MCP server {id}")))?;
            servers[idx].enabled = false;
            servers[idx].deleted_at = Some(Self::now());
            servers[idx].updated_at = Self::now();
            Ok(())
        }

        async fn batch_upsert(&self, params_list: &[CreateMcpServerParams<'_>]) -> Result<Vec<McpServerRow>, DbError> {
            let mut results = Vec::new();
            for params in params_list {
                let mut servers = self.servers.lock().unwrap();
                if let Some(idx) = servers.iter().position(|s| s.name == params.name) {
                    // Update existing
                    servers[idx].description = params.description.map(String::from);
                    servers[idx].transport_type = params.transport_type.to_owned();
                    servers[idx].transport_config = params.transport_config.to_owned();
                    servers[idx].original_json = params.original_json.map(String::from);
                    servers[idx].updated_at = Self::now();
                    results.push(servers[idx].clone());
                } else {
                    // Create new
                    let row = McpServerRow {
                        id: self.next_id(),
                        name: params.name.to_owned(),
                        description: params.description.map(String::from),
                        enabled: params.enabled,
                        transport_type: params.transport_type.to_owned(),
                        transport_config: params.transport_config.to_owned(),
                        tools: params.tools.map(String::from),
                        last_test_status: "disconnected".to_owned(),
                        last_connected: None,
                        original_json: params.original_json.map(String::from),
                        builtin: params.builtin,
                        deleted_at: None,
                        created_at: Self::now(),
                        updated_at: Self::now(),
                    };
                    servers.push(row.clone());
                    results.push(row);
                }
            }
            Ok(results)
        }

        async fn update_status(
            &self,
            id: i64,
            status: &str,
            last_connected: Option<TimestampMs>,
        ) -> Result<(), DbError> {
            let mut servers = self.servers.lock().unwrap();
            let idx = servers
                .iter()
                .position(|s| s.id == id)
                .ok_or_else(|| DbError::NotFound(format!("MCP server {id}")))?;
            servers[idx].last_test_status = status.to_owned();
            if let Some(lc) = last_connected {
                servers[idx].last_connected = Some(lc);
            }
            Ok(())
        }

        async fn update_tools(&self, id: i64, tools: Option<&str>) -> Result<(), DbError> {
            let mut servers = self.servers.lock().unwrap();
            let idx = servers
                .iter()
                .position(|s| s.id == id)
                .ok_or_else(|| DbError::NotFound(format!("MCP server {id}")))?;
            servers[idx].tools = tools.map(String::from);
            Ok(())
        }
    }

    fn make_service() -> McpConfigService {
        McpConfigService::new(Arc::new(MockMcpServerRepo::new()))
    }

    fn stdio_create_req(name: &str) -> CreateMcpServerRequest {
        CreateMcpServerRequest {
            name: name.to_owned(),
            description: Some("test server".to_owned()),
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@test/server".into()],
                env: HashMap::new(),
            },
            original_json: None,
            builtin: false,
        }
    }

    fn http_create_req(name: &str) -> CreateMcpServerRequest {
        CreateMcpServerRequest {
            name: name.to_owned(),
            description: None,
            transport: McpTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: HashMap::new(),
            },
            original_json: None,
            builtin: false,
        }
    }

    fn stdio_import_req(name: &str) -> ImportMcpServerRequest {
        ImportMcpServerRequest {
            name: name.to_owned(),
            description: Some("test server".to_owned()),
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@test/server".into()],
                env: HashMap::new(),
            },
            original_json: None,
            builtin: false,
            enabled: None,
        }
    }

    fn http_import_req(name: &str) -> ImportMcpServerRequest {
        ImportMcpServerRequest {
            name: name.to_owned(),
            description: None,
            transport: McpTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: HashMap::new(),
            },
            original_json: None,
            builtin: false,
            enabled: None,
        }
    }

    // -- list_servers --------------------------------------------------------

    #[tokio::test]
    async fn list_servers_empty() {
        let svc = make_service();
        let result = svc.list_servers().await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn list_servers_returns_all() {
        let svc = make_service();
        svc.add_server(stdio_create_req("a")).await.unwrap();
        svc.add_server(http_create_req("b")).await.unwrap();

        let result = svc.list_servers().await.unwrap();
        assert_eq!(result.len(), 2);
    }

    // -- get_server ----------------------------------------------------------

    #[tokio::test]
    async fn get_server_found() {
        let svc = make_service();
        let created = svc.add_server(stdio_create_req("test")).await.unwrap();
        let found = svc.get_server(&created.id.to_string()).await.unwrap();
        assert_eq!(found.id, created.id);
        assert_eq!(found.name, "test");
    }

    #[tokio::test]
    async fn get_server_not_found() {
        let svc = make_service();
        let result = svc.get_server("nonexistent").await;
        assert!(matches!(result, Err(McpError::NotFound(_))));
    }

    // -- add_server ----------------------------------------------------------

    #[tokio::test]
    async fn add_server_creates_new() {
        let svc = make_service();
        let resp = svc.add_server(stdio_create_req("new-srv")).await.unwrap();
        assert_eq!(resp.name, "new-srv");
        assert!(!resp.enabled);
        assert_eq!(resp.last_test_status, McpServerStatus::Disconnected);
        assert_eq!(resp.description.as_deref(), Some("test server"));
    }

    #[tokio::test]
    async fn add_server_upserts_existing() {
        let svc = make_service();
        let first = svc.add_server(stdio_create_req("upsert-test")).await.unwrap();

        // Second add with same name updates existing
        let updated = svc.add_server(http_create_req("upsert-test")).await.unwrap();
        assert_eq!(updated.id, first.id);
        // Transport should be updated to http
        match updated.transport {
            McpTransport::Http { ref url, .. } => {
                assert_eq!(url, "https://example.com/mcp");
            }
            _ => panic!("expected Http transport after upsert"),
        }
    }

    #[tokio::test]
    async fn add_server_stdio_complete() {
        let svc = make_service();
        let resp = svc
            .add_server(CreateMcpServerRequest {
                name: "stdio-full".into(),
                description: Some("full stdio".into()),
                transport: McpTransport::Stdio {
                    command: "node".into(),
                    args: vec!["index.js".into()],
                    env: HashMap::from([("KEY".into(), "val".into())]),
                },
                original_json: Some(r#"{"name":"stdio-full"}"#.into()),
                builtin: true,
            })
            .await
            .unwrap();
        assert_eq!(resp.name, "stdio-full");
        assert!(resp.builtin);
        assert_eq!(resp.original_json.as_deref(), Some(r#"{"name":"stdio-full"}"#));
    }

    // -- edit_server ---------------------------------------------------------

    #[tokio::test]
    async fn edit_server_rejects_name_change() {
        let svc = make_service();
        let created = svc.add_server(stdio_create_req("old-name")).await.unwrap();
        let err = svc
            .edit_server(
                &created.id.to_string(),
                UpdateMcpServerRequest {
                    name: Some("new-name".into()),
                    description: None,
                    transport: None,
                    original_json: None,
                    builtin: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidEdit(_)));
    }

    #[tokio::test]
    async fn edit_server_updates_transport() {
        let svc = make_service();
        let created = svc.add_server(stdio_create_req("test")).await.unwrap();
        let updated = svc
            .edit_server(
                &created.id.to_string(),
                UpdateMcpServerRequest {
                    name: None,
                    description: None,
                    transport: Some(McpTransport::Http {
                        url: "https://new.url".into(),
                        headers: HashMap::new(),
                    }),
                    original_json: None,
                    builtin: None,
                },
            )
            .await
            .unwrap();
        match updated.transport {
            McpTransport::Http { ref url, .. } => assert_eq!(url, "https://new.url"),
            _ => panic!("expected Http"),
        }
    }

    #[tokio::test]
    async fn edit_server_clears_description() {
        let svc = make_service();
        let created = svc.add_server(stdio_create_req("test")).await.unwrap();
        assert!(created.description.is_some());

        let updated = svc
            .edit_server(
                &created.id.to_string(),
                UpdateMcpServerRequest {
                    name: None,
                    description: Some(None), // clear
                    transport: None,
                    original_json: None,
                    builtin: None,
                },
            )
            .await
            .unwrap();
        assert!(updated.description.is_none());
    }

    #[tokio::test]
    async fn edit_server_not_found() {
        let svc = make_service();
        let result = svc
            .edit_server(
                "nonexistent",
                UpdateMcpServerRequest {
                    name: Some("x".into()),
                    description: None,
                    transport: None,
                    original_json: None,
                    builtin: None,
                },
            )
            .await;
        assert!(matches!(result, Err(McpError::NotFound(_))));
    }

    #[tokio::test]
    async fn edit_server_name_conflict() {
        let svc = make_service();
        svc.add_server(stdio_create_req("server-a")).await.unwrap();
        let b = svc.add_server(stdio_create_req("server-b")).await.unwrap();

        let result = svc
            .edit_server(
                &b.id.to_string(),
                UpdateMcpServerRequest {
                    name: Some("server-a".into()), // conflict
                    description: None,
                    transport: None,
                    original_json: None,
                    builtin: None,
                },
            )
            .await;
        assert!(matches!(result, Err(McpError::InvalidEdit(_))));
    }

    #[tokio::test]
    async fn edit_server_rename_to_same_name() {
        let svc = make_service();
        let a = svc.add_server(stdio_create_req("server-a")).await.unwrap();

        // Renaming to the same name should succeed
        let result = svc
            .edit_server(
                &a.id.to_string(),
                UpdateMcpServerRequest {
                    name: Some("server-a".into()),
                    description: None,
                    transport: None,
                    original_json: None,
                    builtin: None,
                },
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn edit_server_updates_builtin_flag() {
        let svc = make_service();
        let created = svc.add_server(stdio_create_req("chrome-devtools")).await.unwrap();
        assert!(!created.builtin);

        let updated = svc
            .edit_server(
                &created.id.to_string(),
                UpdateMcpServerRequest {
                    name: None,
                    description: None,
                    transport: None,
                    original_json: None,
                    builtin: Some(true),
                },
            )
            .await
            .unwrap();
        assert!(updated.builtin);
    }

    // -- delete_server -------------------------------------------------------

    #[tokio::test]
    async fn delete_server_removes_and_returns_enabled_status() {
        let svc = make_service();
        let created = svc.add_server(stdio_create_req("test")).await.unwrap();

        // Not enabled
        let was_enabled = svc.delete_server(&created.id.to_string()).await.unwrap();
        assert!(!was_enabled);

        // Should be hidden from active queries
        let result = svc.get_server(&created.id.to_string()).await;
        assert!(matches!(result, Err(McpError::NotFound(_))));
    }

    #[tokio::test]
    async fn delete_enabled_server_returns_true() {
        let svc = make_service();
        let created = svc.add_server(stdio_create_req("test")).await.unwrap();
        svc.toggle_server(&created.id.to_string()).await.unwrap(); // enable

        let was_enabled = svc.delete_server(&created.id.to_string()).await.unwrap();
        assert!(was_enabled);
    }

    #[tokio::test]
    async fn delete_server_not_found() {
        let svc = make_service();
        let result = svc.delete_server("nonexistent").await;
        assert!(matches!(result, Err(McpError::NotFound(_))));
    }

    // -- toggle_server -------------------------------------------------------

    #[tokio::test]
    async fn toggle_server_enables_then_disables() {
        let svc = make_service();
        let created = svc.add_server(stdio_create_req("toggle")).await.unwrap();
        assert!(!created.enabled);

        let toggled = svc.toggle_server(&created.id.to_string()).await.unwrap();
        assert!(toggled.enabled);

        let toggled_back = svc.toggle_server(&created.id.to_string()).await.unwrap();
        assert!(!toggled_back.enabled);
    }

    #[tokio::test]
    async fn toggle_server_not_found() {
        let svc = make_service();
        let result = svc.toggle_server("nonexistent").await;
        assert!(matches!(result, Err(McpError::NotFound(_))));
    }

    // -- batch_import --------------------------------------------------------

    #[tokio::test]
    async fn batch_import_creates_new_servers() {
        let svc = make_service();
        let req = BatchImportMcpServersRequest {
            servers: vec![stdio_import_req("a"), http_import_req("b")],
        };
        let results = svc.batch_import(req).await.unwrap();
        assert_eq!(results.len(), 2);

        let all = svc.list_servers().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn batch_import_upserts_existing() {
        let svc = make_service();
        svc.add_server(stdio_create_req("existing")).await.unwrap();

        let req = BatchImportMcpServersRequest {
            servers: vec![
                http_import_req("existing"),   // update
                stdio_import_req("brand-new"), // create
            ],
        };
        let results = svc.batch_import(req).await.unwrap();
        assert_eq!(results.len(), 2);

        let all = svc.list_servers().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn add_server_restores_soft_deleted_row() {
        let svc = make_service();
        let created = svc.add_server(stdio_create_req("restored")).await.unwrap();
        svc.delete_server(&created.id.to_string()).await.unwrap();

        let restored = svc.add_server(http_create_req("restored")).await.unwrap();
        assert_eq!(restored.id, created.id);
        match restored.transport {
            McpTransport::Http { .. } => {}
            _ => panic!("expected Http after restore"),
        }
    }

    #[tokio::test]
    async fn add_server_rejects_overriding_builtin_name() {
        let svc = make_service();
        svc.add_server(CreateMcpServerRequest {
            name: "chrome-devtools".into(),
            description: Some("builtin".into()),
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "chrome-devtools-mcp@latest".into()],
                env: HashMap::new(),
            },
            original_json: None,
            builtin: true,
        })
        .await
        .unwrap();

        let err = svc.add_server(stdio_create_req("chrome-devtools")).await.unwrap_err();
        assert!(matches!(err, McpError::Conflict(_)));
    }

    #[tokio::test]
    async fn add_server_rejects_overriding_builtin_name_even_with_builtin_payload() {
        let svc = make_service();
        svc.add_server(CreateMcpServerRequest {
            name: "chrome-devtools".into(),
            description: Some("builtin".into()),
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "chrome-devtools-mcp@latest".into()],
                env: HashMap::new(),
            },
            original_json: None,
            builtin: true,
        })
        .await
        .unwrap();

        let err = svc
            .add_server(CreateMcpServerRequest {
                name: "chrome-devtools".into(),
                description: Some("malicious override".into()),
                transport: McpTransport::Http {
                    url: "https://example.com/mcp".into(),
                    headers: HashMap::new(),
                },
                original_json: None,
                builtin: true,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Conflict(_)));
    }

    #[tokio::test]
    async fn batch_import_skips_reserved_builtin_name() {
        let svc = make_service();
        svc.add_server(CreateMcpServerRequest {
            name: "chrome-devtools".into(),
            description: Some("builtin".into()),
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "chrome-devtools-mcp@latest".into()],
                env: HashMap::new(),
            },
            original_json: None,
            builtin: true,
        })
        .await
        .unwrap();

        let results = svc
            .batch_import(BatchImportMcpServersRequest {
                servers: vec![
                    ImportMcpServerRequest {
                        name: "chrome-devtools".into(),
                        description: Some("imported".into()),
                        transport: McpTransport::Http {
                            url: "https://example.com/mcp".into(),
                            headers: HashMap::new(),
                        },
                        original_json: None,
                        builtin: false,
                        enabled: Some(false),
                    },
                    ImportMcpServerRequest {
                        name: "playwright".into(),
                        description: Some("imported".into()),
                        transport: McpTransport::Stdio {
                            command: "npx".into(),
                            args: vec!["@playwright/mcp@latest".into()],
                            env: HashMap::new(),
                        },
                        original_json: None,
                        builtin: false,
                        enabled: Some(false),
                    },
                ],
            })
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "playwright");
    }

    #[tokio::test]
    async fn add_server_normalizes_shell_style_stdio_command() {
        let svc = make_service();
        let created = svc
            .add_server(CreateMcpServerRequest {
                name: "sentry".into(),
                description: None,
                transport: McpTransport::Stdio {
                    command: "npx @sentry/mcp-server@latest --organization-slug=demo".into(),
                    args: vec![],
                    env: HashMap::new(),
                },
                original_json: None,
                builtin: false,
            })
            .await
            .unwrap();

        match created.transport {
            McpTransport::Stdio { command, args, .. } => {
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["@sentry/mcp-server@latest", "--organization-slug=demo"]);
            }
            _ => panic!("expected stdio transport"),
        }
    }

    #[tokio::test]
    async fn batch_import_preserves_enabled_state() {
        let svc = make_service();
        let mut req = stdio_import_req("enabled-mcp");
        req.enabled = Some(true);
        let result = svc
            .batch_import(BatchImportMcpServersRequest { servers: vec![req] })
            .await
            .unwrap();

        assert_eq!(result[0].name, "enabled-mcp");
        assert!(result[0].enabled);
    }

    #[tokio::test]
    async fn batch_import_empty_list() {
        let svc = make_service();
        let req = BatchImportMcpServersRequest { servers: vec![] };
        let results = svc.batch_import(req).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn persist_test_result_records_success_status_and_tools() {
        let svc = make_service();
        let created = svc.add_server(stdio_create_req("persist-success")).await.unwrap();
        let result = McpConnectionTestResult {
            success: true,
            tools: Some(vec![nomifun_api_types::McpToolResponse {
                name: "read_file".into(),
                description: Some("Read a file".into()),
                input_schema: None,
            }]),
            error: None,
            code: None,
            details: None,
            needs_auth: None,
            auth_method: None,
            www_authenticate: None,
        };

        svc.persist_test_result(created.id, &result).await.unwrap();

        let updated = svc.get_server(&created.id.to_string()).await.unwrap();
        assert_eq!(updated.last_test_status, nomifun_common::McpServerStatus::Connected);
        assert_eq!(updated.tools.unwrap().len(), 1);
        assert!(updated.last_connected.is_some());
    }

    #[tokio::test]
    async fn persist_test_result_records_error_and_clears_tools() {
        let svc = make_service();
        let created = svc.add_server(stdio_create_req("persist-error")).await.unwrap();

        let success = McpConnectionTestResult {
            success: true,
            tools: Some(vec![nomifun_api_types::McpToolResponse {
                name: "read_file".into(),
                description: Some("Read a file".into()),
                input_schema: None,
            }]),
            error: None,
            code: None,
            details: None,
            needs_auth: None,
            auth_method: None,
            www_authenticate: None,
        };
        svc.persist_test_result(created.id, &success).await.unwrap();

        let failure = McpConnectionTestResult {
            success: false,
            tools: None,
            error: Some("boom".into()),
            code: None,
            details: None,
            needs_auth: None,
            auth_method: None,
            www_authenticate: None,
        };
        svc.persist_test_result(created.id, &failure).await.unwrap();

        let updated = svc.get_server(&created.id.to_string()).await.unwrap();
        assert_eq!(updated.last_test_status, nomifun_common::McpServerStatus::Error);
        assert!(updated.tools.is_none());
        assert!(updated.last_connected.is_some());
    }
}
