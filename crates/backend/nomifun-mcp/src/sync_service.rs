use std::sync::Arc;

use dashmap::DashMap;
use nomifun_api_types::{DetectedMcpServerEntry, DetectedMcpServerResponse};
use nomifun_common::McpSource;
use nomifun_db::IMcpServerRepository;
use tokio::sync::Mutex;
use tracing::warn;

use crate::adapter::{DetectedServer, McpAgentAdapter};
use crate::error::McpError;

/// Discovers MCP configuration currently installed in external Agent CLIs.
///
/// This service is intentionally read-only. It serializes detection work to
/// avoid concurrent CLI scans from spawning overlapping child processes.
#[derive(Clone)]
pub struct McpSyncService {
    adapters: Arc<Vec<Arc<dyn McpAgentAdapter>>>,
    service_lock: Arc<Mutex<()>>,
    agent_locks: Arc<DashMap<McpSource, Arc<Mutex<()>>>>,
}

impl McpSyncService {
    pub fn new(_repo: Arc<dyn IMcpServerRepository>, adapters: Vec<Arc<dyn McpAgentAdapter>>) -> Self {
        Self {
            adapters: Arc::new(adapters),
            service_lock: Arc::new(Mutex::new(())),
            agent_locks: Arc::new(DashMap::new()),
        }
    }

    /// Scan all installed Agent CLIs and return each one's current MCP
    /// server configurations.
    ///
    /// Agents that are not installed are silently skipped.
    pub async fn get_agent_configs(&self) -> Result<Vec<DetectedMcpServerResponse>, McpError> {
        let _guard = self.service_lock.lock().await;

        let mut results = Vec::new();
        for adapter in self.adapters.iter() {
            let _agent_guard = self.agent_lock(adapter.source()).await;

            let installed = adapter.is_installed().await.unwrap_or(false);
            if !installed {
                continue;
            }

            match adapter.detect_existing().await {
                Ok(detected) => {
                    let servers = detected.into_iter().map(detected_to_response).collect();
                    results.push(DetectedMcpServerResponse {
                        source: adapter.source(),
                        servers,
                    });
                }
                Err(e) => {
                    warn!(
                        agent = ?adapter.source(),
                        error = %e,
                        "failed to detect existing MCP servers"
                    );
                }
            }
        }

        Ok(results)
    }

    async fn agent_lock(&self, source: McpSource) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .agent_locks
            .entry(source)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        lock.lock_owned().await
    }
}

fn detected_to_response(detected: DetectedServer) -> DetectedMcpServerEntry {
    let normalized_skip_reason = detected.import_skip_reason.as_deref().map(normalize_import_skip_reason);
    let importable = detected.importable || normalized_skip_reason.as_deref() == Some("Connected");

    DetectedMcpServerEntry {
        name: detected.name,
        description: None,
        transport: detected.transport.into(),
        original_json: None,
        importable,
        import_skip_reason: if importable { None } else { normalized_skip_reason },
    }
}

fn normalize_import_skip_reason(reason: &str) -> String {
    reason
        .trim()
        .trim_start_matches(|c: char| {
            matches!(c, '✓' | '✗' | '!' | '•' | '-' | '*' | '✔' | '✘' | ':' | '[' | ']') || c.is_whitespace()
        })
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::McpServerTransport;
    use nomifun_common::TimestampMs;
    use nomifun_db::models::McpServerRow;
    use nomifun_db::{CreateMcpServerParams, DbError, UpdateMcpServerParams};
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    struct MockAdapter {
        source: McpSource,
        installed: bool,
        servers: Arc<StdMutex<Vec<DetectedServer>>>,
    }

    impl MockAdapter {
        fn new(source: McpSource, installed: bool) -> Self {
            Self {
                source,
                installed,
                servers: Arc::new(StdMutex::new(Vec::new())),
            }
        }

        fn with_existing(mut self, servers: Vec<DetectedServer>) -> Self {
            self.servers = Arc::new(StdMutex::new(servers));
            self
        }
    }

    #[async_trait::async_trait]
    impl McpAgentAdapter for MockAdapter {
        fn source(&self) -> McpSource {
            self.source
        }

        async fn is_installed(&self) -> Result<bool, McpError> {
            Ok(self.installed)
        }

        async fn detect_existing(&self) -> Result<Vec<DetectedServer>, McpError> {
            if !self.installed {
                return Err(McpError::AgentNotInstalled(format!("{:?}", self.source)));
            }
            Ok(self.servers.lock().unwrap().clone())
        }

        async fn install_server(&self, _name: &str, _transport: &McpServerTransport) -> Result<(), McpError> {
            unreachable!("write-to-CLI is no longer supported")
        }

        async fn remove_server(&self, _name: &str) -> Result<(), McpError> {
            unreachable!("write-to-CLI is no longer supported")
        }
    }

    #[derive(Debug)]
    struct MockRepo;

    #[async_trait::async_trait]
    impl IMcpServerRepository for MockRepo {
        async fn list(&self) -> Result<Vec<McpServerRow>, DbError> {
            Ok(Vec::new())
        }

        async fn find_by_id(&self, _mcp_server_id: &str) -> Result<Option<McpServerRow>, DbError> {
            Ok(None)
        }

        async fn find_by_name(&self, _name: &str) -> Result<Option<McpServerRow>, DbError> {
            Ok(None)
        }

        async fn create(&self, _params: CreateMcpServerParams<'_>) -> Result<McpServerRow, DbError> {
            unimplemented!("not needed for detection tests")
        }

        async fn update(&self, _mcp_server_id: &str, _params: UpdateMcpServerParams<'_>) -> Result<McpServerRow, DbError> {
            unimplemented!("not needed for detection tests")
        }

        async fn delete(&self, _mcp_server_id: &str) -> Result<(), DbError> {
            unimplemented!("not needed for detection tests")
        }

        async fn batch_upsert(&self, _params_list: &[CreateMcpServerParams<'_>]) -> Result<Vec<McpServerRow>, DbError> {
            unimplemented!("not needed for detection tests")
        }

        async fn update_status(
            &self,
            _mcp_server_id: &str,
            _status: &str,
            _last_connected: Option<TimestampMs>,
        ) -> Result<(), DbError> {
            unimplemented!("not needed for detection tests")
        }

        async fn update_tools(&self, _mcp_server_id: &str, _tools: Option<&str>) -> Result<(), DbError> {
            unimplemented!("not needed for detection tests")
        }
    }

    fn stdio_transport() -> McpServerTransport {
        McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "@test/server".into()],
            env: HashMap::new(),
        }
    }

    fn make_service(adapters: Vec<Arc<dyn McpAgentAdapter>>) -> McpSyncService {
        McpSyncService::new(Arc::new(MockRepo), adapters)
    }

    #[tokio::test]
    async fn get_agent_configs_returns_installed_only() {
        let adapter_a = Arc::new(
            MockAdapter::new(McpSource::Claude, true).with_existing(vec![DetectedServer {
                name: "srv1".into(),
                transport: stdio_transport(),
                importable: true,
                import_skip_reason: None,
            }]),
        );
        let adapter_b = Arc::new(MockAdapter::new(McpSource::Gemini, false));
        let adapter_c = Arc::new(MockAdapter::new(McpSource::Qwen, true).with_existing(vec![]));

        let svc = make_service(vec![adapter_a, adapter_b, adapter_c]);
        let configs = svc.get_agent_configs().await.unwrap();

        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].source, McpSource::Claude);
        assert_eq!(configs[0].servers.len(), 1);
        assert_eq!(configs[0].servers[0].name, "srv1");
        assert_eq!(configs[1].source, McpSource::Qwen);
        assert!(configs[1].servers.is_empty());
    }

    #[test]
    fn detected_to_response_normalizes_connected_skip_reason() {
        let resp = detected_to_response(DetectedServer {
            name: "sentry".into(),
            transport: stdio_transport(),
            importable: false,
            import_skip_reason: Some("✓ Connected".into()),
        });

        assert!(resp.importable);
        assert_eq!(resp.import_skip_reason, None);
    }

    #[tokio::test]
    async fn get_agent_configs_no_adapters() {
        let svc = make_service(vec![]);
        let configs = svc.get_agent_configs().await.unwrap();
        assert!(configs.is_empty());
    }

    #[test]
    fn detected_to_response_fields() {
        let detected = DetectedServer {
            name: "my-srv".into(),
            transport: stdio_transport(),
            importable: false,
            import_skip_reason: Some("Needs authentication".into()),
        };
        let resp = detected_to_response(detected);
        assert_eq!(resp.name, "my-srv");
        assert!(!resp.importable);
        assert_eq!(resp.import_skip_reason.as_deref(), Some("Needs authentication"));
    }
}
