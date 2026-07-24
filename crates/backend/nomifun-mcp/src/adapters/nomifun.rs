use std::sync::Arc;

use nomifun_common::McpSource;
use nomifun_db::IMcpServerRepository;

use crate::adapter::{DetectedServer, McpAgentAdapter};
use crate::error::McpError;
use crate::types::{McpServer, McpServerTransport};

/// MCP Agent adapter for Nomi itself.
///
/// Unlike CLI-based adapters, this adapter reads/writes directly to the
/// local database. It is always "installed" since Nomi is the host
/// application.
///
/// # Behavior
///
/// - `is_installed()` → always `true`
/// - `detect_existing()` → reads all MCP servers from the DB
/// - `install_server()` → no-op (DB writes are handled by `McpConfigService`)
/// - `remove_server()` → no-op (configuration is managed via the frontend)
pub struct NomifunAdapter {
    repo: Arc<dyn IMcpServerRepository>,
}

impl NomifunAdapter {
    pub fn new(repo: Arc<dyn IMcpServerRepository>) -> Self {
        Self { repo }
    }
}

#[async_trait::async_trait]
impl McpAgentAdapter for NomifunAdapter {
    fn source(&self) -> McpSource {
        McpSource::Nomifun
    }

    async fn is_installed(&self) -> Result<bool, McpError> {
        Ok(true)
    }

    async fn detect_existing(&self) -> Result<Vec<DetectedServer>, McpError> {
        let rows = self.repo.list().await?;

        let mut servers = Vec::new();
        for row in rows {
            let server = McpServer::from_row(row)?;
            servers.push(DetectedServer {
                name: server.name,
                transport: server.transport,
                importable: true,
                import_skip_reason: None,
            });
        }

        Ok(servers)
    }

    async fn install_server(&self, _name: &str, _transport: &McpServerTransport) -> Result<(), McpError> {
        // No-op: DB writes are handled by McpConfigService.
        // The sync service calls install_server on all adapters, but for
        // Nomi the server is already in the DB.
        Ok(())
    }

    async fn remove_server(&self, _name: &str) -> Result<(), McpError> {
        // No-op: configuration is managed via the frontend/REST API.
        // Removing from the DB is done through McpConfigService.delete_server().
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::types::McpServerTransport;
    use nomifun_db::models::McpServerRow;

    /// In-memory mock repository for testing.
    struct MockRepo {
        servers: Vec<McpServerRow>,
    }

    impl MockRepo {
        fn new(servers: Vec<McpServerRow>) -> Self {
            Self { servers }
        }
    }

    #[async_trait::async_trait]
    impl IMcpServerRepository for MockRepo {
        async fn list(&self) -> Result<Vec<McpServerRow>, nomifun_db::DbError> {
            Ok(self.servers.clone())
        }

        async fn find_by_id(&self, mcp_server_id: &str) -> Result<Option<McpServerRow>, nomifun_db::DbError> {
            Ok(self
                .servers
                .iter()
                .find(|s| s.mcp_server_id == mcp_server_id)
                .cloned())
        }

        async fn find_by_name(&self, name: &str) -> Result<Option<McpServerRow>, nomifun_db::DbError> {
            Ok(self.servers.iter().find(|s| s.name == name).cloned())
        }

        async fn create(
            &self,
            _params: nomifun_db::CreateMcpServerParams<'_>,
        ) -> Result<McpServerRow, nomifun_db::DbError> {
            unimplemented!("not needed for adapter tests")
        }

        async fn update(
            &self,
            _mcp_server_id: &str,
            _params: nomifun_db::UpdateMcpServerParams<'_>,
        ) -> Result<McpServerRow, nomifun_db::DbError> {
            unimplemented!("not needed for adapter tests")
        }

        async fn delete(&self, _mcp_server_id: &str) -> Result<(), nomifun_db::DbError> {
            unimplemented!("not needed for adapter tests")
        }

        async fn batch_upsert(
            &self,
            _servers: &[nomifun_db::CreateMcpServerParams<'_>],
        ) -> Result<Vec<McpServerRow>, nomifun_db::DbError> {
            unimplemented!("not needed for adapter tests")
        }

        async fn update_status(
            &self,
            _mcp_server_id: &str,
            _status: &str,
            _last_connected: Option<nomifun_common::TimestampMs>,
        ) -> Result<(), nomifun_db::DbError> {
            unimplemented!("not needed for adapter tests")
        }

        async fn update_tools(&self, _mcp_server_id: &str, _tools: Option<&str>) -> Result<(), nomifun_db::DbError> {
            unimplemented!("not needed for adapter tests")
        }
    }

    fn make_row(name: &str, transport_type: &str, transport_config: &str) -> McpServerRow {
        McpServerRow {
            // Host-local integer PK; never compared in adapter tests (detection keys on name).
            mcp_server_id: nomifun_common::generate_id(),
            name: name.to_owned(),
            description: None,
            enabled: true,
            transport_type: transport_type.into(),
            transport_config: transport_config.into(),
            tools: None,
            last_test_status: "disconnected".into(),
            last_connected: None,
            original_json: None,
            builtin: false,
            deleted_at: None,
            created_at: 1000,
            updated_at: 1000,
        }
    }

    #[test]
    fn source_is_nomifun() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter = NomifunAdapter::new(repo);
        assert_eq!(adapter.source(), McpSource::Nomifun);
    }

    #[tokio::test]
    async fn is_always_installed() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter = NomifunAdapter::new(repo);
        assert!(adapter.is_installed().await.unwrap());
    }

    #[tokio::test]
    async fn detect_existing_returns_db_servers() {
        let rows = vec![
            make_row("srv-a", "stdio", r#"{"command":"npx","args":[]}"#),
            make_row("srv-b", "http", r#"{"url":"https://b.com/mcp","headers":{}}"#),
        ];
        let repo = Arc::new(MockRepo::new(rows));
        let adapter = NomifunAdapter::new(repo);

        let servers = adapter.detect_existing().await.unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "srv-a");
        assert_eq!(servers[1].name, "srv-b");
        assert!(matches!(servers[0].transport, McpServerTransport::Stdio { .. }));
        assert!(matches!(servers[1].transport, McpServerTransport::Http { .. }));
    }

    #[tokio::test]
    async fn detect_existing_empty_db() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter = NomifunAdapter::new(repo);

        let servers = adapter.detect_existing().await.unwrap();
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn install_server_is_noop() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter = NomifunAdapter::new(repo);

        let transport = McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec![],
            env: HashMap::new(),
        };
        // Should succeed without side effects
        adapter.install_server("test", &transport).await.unwrap();
    }

    #[tokio::test]
    async fn remove_server_is_noop() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter = NomifunAdapter::new(repo);

        // Should succeed without side effects
        adapter.remove_server("test").await.unwrap();
    }

    #[tokio::test]
    async fn trait_is_object_safe() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter: Arc<dyn McpAgentAdapter> = Arc::new(NomifunAdapter::new(repo));
        assert_eq!(adapter.source(), McpSource::Nomifun);
        assert!(adapter.is_installed().await.unwrap());
    }
}
