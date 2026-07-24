//! Integration tests for file-based MCP Agent adapters (Opencode, Nomi, Nomi).
//!
//! These tests exercise the real filesystem read/write logic using temp
//! directories. CLI detection (`is_installed`, `which`) is NOT tested here
//! because it depends on the host environment.
//!
//! For Nomi, we use a mock repository since it reads from the DB.

use std::collections::HashMap;
use std::sync::Arc;

use nomifun_common::McpSource;
use nomifun_mcp::{McpAgentAdapter, McpServerTransport, NomifunAdapter};

// ===========================================================================
// Nomi adapter (DB-backed)
// ===========================================================================

mod nomifun {
    use super::*;
    use nomifun_db::models::McpServerRow;
    use nomifun_db::{CreateMcpServerParams, DbError, IMcpServerRepository, UpdateMcpServerParams};

    struct MockRepo {
        servers: tokio::sync::Mutex<Vec<McpServerRow>>,
    }

    impl MockRepo {
        fn new(servers: Vec<McpServerRow>) -> Self {
            Self {
                servers: tokio::sync::Mutex::new(servers),
            }
        }
    }

    #[async_trait::async_trait]
    impl IMcpServerRepository for MockRepo {
        async fn list(&self) -> Result<Vec<McpServerRow>, DbError> {
            Ok(self.servers.lock().await.clone())
        }

        async fn find_by_id(&self, mcp_server_id: &str) -> Result<Option<McpServerRow>, DbError> {
            Ok(self
                .servers
                .lock()
                .await
                .iter()
                .find(|s| s.mcp_server_id == mcp_server_id)
                .cloned())
        }

        async fn find_by_name(&self, name: &str) -> Result<Option<McpServerRow>, DbError> {
            Ok(self.servers.lock().await.iter().find(|s| s.name == name).cloned())
        }

        async fn create(&self, _p: CreateMcpServerParams<'_>) -> Result<McpServerRow, DbError> {
            unimplemented!()
        }

        async fn update(&self, _mcp_server_id: &str, _p: UpdateMcpServerParams<'_>) -> Result<McpServerRow, DbError> {
            unimplemented!()
        }

        async fn delete(&self, _mcp_server_id: &str) -> Result<(), DbError> {
            unimplemented!()
        }

        async fn batch_upsert(&self, _s: &[CreateMcpServerParams<'_>]) -> Result<Vec<McpServerRow>, DbError> {
            unimplemented!()
        }

        async fn update_status(
            &self,
            _mcp_server_id: &str,
            _s: &str,
            _lc: Option<nomifun_common::TimestampMs>,
        ) -> Result<(), DbError> {
            unimplemented!()
        }

        async fn update_tools(&self, _mcp_server_id: &str, _t: Option<&str>) -> Result<(), DbError> {
            unimplemented!()
        }
    }

    fn make_row(name: &str, t_type: &str, t_config: &str) -> McpServerRow {
        McpServerRow {
            mcp_server_id: nomifun_common::generate_id(),
            name: name.to_owned(),
            description: None,
            enabled: true,
            transport_type: t_type.into(),
            transport_config: t_config.into(),
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

    #[tokio::test]
    async fn source_is_nomifun() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter = NomifunAdapter::new(repo);
        assert_eq!(adapter.source(), McpSource::Nomifun);
    }

    #[tokio::test]
    async fn always_installed() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter = NomifunAdapter::new(repo);
        assert!(adapter.is_installed().await.unwrap());
    }

    #[tokio::test]
    async fn detect_returns_all_db_servers() {
        let rows = vec![
            make_row("stdio-srv", "stdio", r#"{"command":"npx","args":[]}"#),
            make_row("http-srv", "http", r#"{"url":"https://example.com/mcp","headers":{}}"#),
            make_row("sse-srv", "sse", r#"{"url":"https://example.com/sse","headers":{}}"#),
        ];
        let repo = Arc::new(MockRepo::new(rows));
        let adapter = NomifunAdapter::new(repo);

        let servers = adapter.detect_existing().await.unwrap();
        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0].name, "stdio-srv");
        assert_eq!(servers[1].name, "http-srv");
        assert_eq!(servers[2].name, "sse-srv");

        assert!(matches!(servers[0].transport, McpServerTransport::Stdio { .. }));
        assert!(matches!(servers[1].transport, McpServerTransport::Http { .. }));
        assert!(matches!(servers[2].transport, McpServerTransport::Sse { .. }));
    }

    #[tokio::test]
    async fn detect_empty_db_returns_empty() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter = NomifunAdapter::new(repo);
        let servers = adapter.detect_existing().await.unwrap();
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn install_is_noop() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter = NomifunAdapter::new(repo.clone());

        let transport = McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec![],
            env: HashMap::new(),
        };
        adapter.install_server("test", &transport).await.unwrap();

        // DB should still be empty since install is a no-op
        let servers = adapter.detect_existing().await.unwrap();
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn remove_is_noop() {
        let rows = vec![make_row("srv", "stdio", r#"{"command":"npx","args":[]}"#)];
        let repo = Arc::new(MockRepo::new(rows));
        let adapter = NomifunAdapter::new(repo);

        adapter.remove_server("srv").await.unwrap();

        // Server should still be in DB since remove is a no-op
        let servers = adapter.detect_existing().await.unwrap();
        assert_eq!(servers.len(), 1);
    }

    #[tokio::test]
    async fn trait_object_safety() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter: Arc<dyn McpAgentAdapter> = Arc::new(NomifunAdapter::new(repo));
        assert_eq!(adapter.source(), McpSource::Nomifun);
        assert!(adapter.is_installed().await.unwrap());
    }
}

// ===========================================================================
// Opencode adapter (filesystem-backed)
// ===========================================================================

// Note: Full lifecycle tests for Opencode require controlling the config
// directory path, which the adapter currently derives from `dirs::config_dir()`.
// The unit tests in opencode.rs thoroughly cover parsing and serialization.
// Here we verify that the adapter implements the trait correctly and that
// the public API surface is accessible from outside the crate.

mod opencode {
    use super::*;
    use nomifun_mcp::OpencodeAdapter;

    #[test]
    fn source_is_opencode() {
        assert_eq!(OpencodeAdapter.source(), McpSource::OpenCode);
    }

    #[test]
    fn trait_object_safety() {
        let adapter: Box<dyn McpAgentAdapter> = Box::new(OpencodeAdapter);
        assert_eq!(adapter.source(), McpSource::OpenCode);
    }
}

// ===========================================================================
// Nomi adapter (CLI + TOML-backed)
// ===========================================================================

// Note: Full lifecycle tests for Nomi require the `nomi` CLI to be
// installed (for `--config-path`). The unit tests in nomi.rs thoroughly
// cover TOML parsing, serialization, and roundtrip behavior. Here we
// verify the public API surface.

mod nomi {
    use super::*;
    use nomifun_mcp::NomiAdapter;

    #[test]
    fn source_is_nomi() {
        assert_eq!(NomiAdapter.source(), McpSource::Nomi);
    }

    #[test]
    fn trait_object_safety() {
        let adapter: Box<dyn McpAgentAdapter> = Box::new(NomiAdapter);
        assert_eq!(adapter.source(), McpSource::Nomi);
    }
}
