//! Integration tests for McpConfigService with real SQLite.
//!
//! Tests from test-plan §1 (CRUD) at the service layer.

use std::collections::HashMap;
use std::sync::Arc;

use nomifun_api_types::{
    BatchImportMcpServersRequest, CreateMcpServerRequest, ImportMcpServerRequest, McpServerId,
    McpTransport, UpdateMcpServerRequest,
};
use nomifun_db::SqliteMcpServerRepository;
use nomifun_mcp::{McpConfigService, McpError};

async fn make_service() -> McpConfigService {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    McpConfigService::new(repo)
}

fn missing_mcp_server_id() -> McpServerId {
    McpServerId::parse("0190f5fe-7c00-7a00-8000-000000000999").unwrap()
}

fn stdio_req(name: &str) -> CreateMcpServerRequest {
    CreateMcpServerRequest {
        name: name.to_owned(),
        description: Some("test".to_owned()),
        transport: McpTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "@test/server".into()],
            env: HashMap::new(),
        },
        original_json: None,
        builtin: false,
    }
}

fn http_req(name: &str) -> CreateMcpServerRequest {
    CreateMcpServerRequest {
        name: name.to_owned(),
        description: None,
        transport: McpTransport::Http {
            url: "https://example.com/mcp".into(),
            headers: HashMap::from([("Auth".into(), "Bearer tok".into())]),
        },
        original_json: None,
        builtin: false,
    }
}

fn stdio_import_req(name: &str) -> ImportMcpServerRequest {
    ImportMcpServerRequest {
        name: name.to_owned(),
        description: Some("test".to_owned()),
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
            headers: HashMap::from([("Auth".into(), "Bearer tok".into())]),
        },
        original_json: None,
        builtin: false,
        enabled: None,
    }
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_and_get_stdio_server() {
    let svc = make_service().await;
    let resp = svc.add_server(stdio_req("test-stdio")).await.unwrap();

    assert!(nomifun_common::validate_uuidv7(resp.mcp_server_id.as_str()).is_ok());
    assert_eq!(resp.name, "test-stdio");
    assert!(!resp.enabled);
    assert_eq!(resp.description.as_deref(), Some("test"));

    let found = svc.get_server(&resp.mcp_server_id).await.unwrap();
    assert_eq!(found.mcp_server_id, resp.mcp_server_id);
}

#[tokio::test]
async fn create_http_with_headers() {
    let svc = make_service().await;
    let resp = svc.add_server(http_req("test-http")).await.unwrap();

    match resp.transport {
        McpTransport::Http { ref url, ref headers } => {
            assert_eq!(url, "https://example.com/mcp");
            assert_eq!(headers.get("Auth").unwrap(), "Bearer tok");
        }
        _ => panic!("expected Http"),
    }
}

#[tokio::test]
async fn create_same_name_upserts() {
    let svc = make_service().await;
    let first = svc.add_server(stdio_req("dup")).await.unwrap();
    let second = svc.add_server(http_req("dup")).await.unwrap();

    assert_eq!(first.mcp_server_id, second.mcp_server_id);
    match second.transport {
        McpTransport::Http { .. } => {}
        _ => panic!("expected Http after upsert"),
    }
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_empty() {
    let svc = make_service().await;
    assert!(svc.list_servers().await.unwrap().is_empty());
}

#[tokio::test]
async fn list_returns_all() {
    let svc = make_service().await;
    svc.add_server(stdio_req("a")).await.unwrap();
    svc.add_server(http_req("b")).await.unwrap();
    assert_eq!(svc.list_servers().await.unwrap().len(), 2);
}

#[tokio::test]
async fn get_not_found() {
    let svc = make_service().await;
    let err = svc.get_server(&missing_mcp_server_id()).await.unwrap_err();
    assert!(matches!(err, McpError::NotFound(_)));
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_name_is_rejected() {
    let svc = make_service().await;
    let created = svc.add_server(stdio_req("old")).await.unwrap();

    let err = svc
        .edit_server(
            &created.mcp_server_id,
            UpdateMcpServerRequest {
                name: Some("new".into()),
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
async fn edit_transport() {
    let svc = make_service().await;
    let created = svc.add_server(stdio_req("test")).await.unwrap();

    let updated = svc
        .edit_server(
            &created.mcp_server_id,
            UpdateMcpServerRequest {
                name: None,
                description: None,
                transport: Some(McpTransport::Sse {
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
        McpTransport::Sse { ref url, .. } => assert_eq!(url, "https://new.url"),
        _ => panic!("expected Sse"),
    }
}

#[tokio::test]
async fn edit_clears_description() {
    let svc = make_service().await;
    let created = svc.add_server(stdio_req("test")).await.unwrap();
    assert!(created.description.is_some());

    let updated = svc
        .edit_server(
            &created.mcp_server_id,
            UpdateMcpServerRequest {
                name: None,
                description: Some(None),
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
async fn edit_not_found() {
    let svc = make_service().await;
    let err = svc
        .edit_server(
            &missing_mcp_server_id(),
            UpdateMcpServerRequest {
                name: Some("x".into()),
                description: None,
                transport: None,
                original_json: None,
                builtin: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::NotFound(_)));
}

#[tokio::test]
async fn edit_name_conflict() {
    let svc = make_service().await;
    svc.add_server(stdio_req("a")).await.unwrap();
    let b = svc.add_server(stdio_req("b")).await.unwrap();

    let err = svc
        .edit_server(
            &b.mcp_server_id,
            UpdateMcpServerRequest {
                name: Some("a".into()),
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

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_removes_server() {
    let svc = make_service().await;
    let created = svc.add_server(stdio_req("del")).await.unwrap();
    let was_enabled = svc.delete_server(&created.mcp_server_id).await.unwrap();
    assert!(!was_enabled);

    let err = svc.get_server(&created.mcp_server_id).await.unwrap_err();
    assert!(matches!(err, McpError::NotFound(_)));
}

#[tokio::test]
async fn delete_enabled_returns_true() {
    let svc = make_service().await;
    let created = svc.add_server(stdio_req("del-en")).await.unwrap();
    svc.toggle_server(&created.mcp_server_id).await.unwrap();

    let was_enabled = svc.delete_server(&created.mcp_server_id).await.unwrap();
    assert!(was_enabled);
}

#[tokio::test]
async fn delete_not_found() {
    let svc = make_service().await;
    let err = svc.delete_server(&missing_mcp_server_id()).await.unwrap_err();
    assert!(matches!(err, McpError::NotFound(_)));
}

// ---------------------------------------------------------------------------
// Toggle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn toggle_enables_then_disables() {
    let svc = make_service().await;
    let created = svc.add_server(stdio_req("tog")).await.unwrap();
    assert!(!created.enabled);

    let toggled = svc.toggle_server(&created.mcp_server_id).await.unwrap();
    assert!(toggled.enabled);

    let toggled_back = svc.toggle_server(&created.mcp_server_id).await.unwrap();
    assert!(!toggled_back.enabled);
}

// ---------------------------------------------------------------------------
// Batch import
// ---------------------------------------------------------------------------

#[tokio::test]
async fn batch_import_creates_and_upserts() {
    let svc = make_service().await;
    svc.add_server(stdio_req("existing")).await.unwrap();

    let req = BatchImportMcpServersRequest {
        servers: vec![
            http_import_req("existing"), // upsert
            stdio_import_req("new"),     // create
        ],
    };
    let results = svc.batch_import(req).await.unwrap();
    assert_eq!(results.len(), 2);

    let all = svc.list_servers().await.unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn batch_import_preserves_enabled_in_database() {
    let svc = make_service().await;
    let mut req = stdio_import_req("enabled-db-mcp");
    req.enabled = Some(true);

    let result = svc
        .batch_import(BatchImportMcpServersRequest { servers: vec![req] })
        .await
        .unwrap();

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].name, "enabled-db-mcp");
    assert!(result[0].enabled);

    let listed = svc.list_servers().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].enabled);
}
