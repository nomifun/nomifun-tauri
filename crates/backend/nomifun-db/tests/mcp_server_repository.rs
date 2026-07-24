//! Black-box integration tests for `IMcpServerRepository`.
//!
//! Tests exercise the repository trait interface without knowledge of
//! the underlying SQLite implementation details.

use std::sync::Arc;

use nomifun_db::{
    CreateMcpServerParams, DbError, IMcpServerRepository, SqliteMcpServerRepository,
    UpdateMcpServerParams, init_database_memory,
};

fn missing_id() -> String {
    "0190f5fe-7c00-7a00-8000-000000000999".to_owned()
}

async fn repo() -> (Arc<dyn IMcpServerRepository>, nomifun_db::Database) {
    let db = init_database_memory().await.unwrap();
    let r = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    (r as Arc<dyn IMcpServerRepository>, db)
}

fn stdio_params() -> CreateMcpServerParams<'static> {
    CreateMcpServerParams {
        name: "test-mcp",
        description: Some("A test MCP server"),
        enabled: false,
        transport_type: "stdio",
        transport_config: r#"{"command":"npx","args":["-y","test-server"]}"#,
        tools: None,
        original_json: Some(r#"{"name":"test-mcp"}"#),
        builtin: false,
    }
}

fn http_params() -> CreateMcpServerParams<'static> {
    CreateMcpServerParams {
        name: "http-mcp",
        description: None,
        enabled: true,
        transport_type: "http",
        transport_config: r#"{"url":"https://example.com/mcp"}"#,
        tools: None,
        original_json: None,
        builtin: false,
    }
}

fn sse_params() -> CreateMcpServerParams<'static> {
    CreateMcpServerParams {
        name: "sse-mcp",
        description: Some("SSE transport server"),
        enabled: false,
        transport_type: "sse",
        transport_config: r#"{"url":"https://example.com/sse","headers":{"Authorization":"Bearer xxx"}}"#,
        tools: None,
        original_json: None,
        builtin: false,
    }
}

// -- C-1/C-2/C-3: Create servers with different transport types --

#[tokio::test]
async fn create_stdio_server() {
    let (r, _db) = repo().await;
    let server = r.create(stdio_params()).await.unwrap();

    assert!(nomifun_common::validate_uuidv7(&server.mcp_server_id).is_ok());
    assert_eq!(server.name, "test-mcp");
    assert_eq!(server.description.as_deref(), Some("A test MCP server"));
    assert!(!server.enabled);
    assert_eq!(server.transport_type, "stdio");
    assert!(server.transport_config.contains("npx"));
    assert!(server.tools.is_none());
    assert_eq!(server.last_test_status, "disconnected");
    assert!(server.last_connected.is_none());
    assert!(!server.builtin);
    assert!(server.created_at > 0);
}

#[tokio::test]
async fn create_http_server() {
    let (r, _db) = repo().await;
    let server = r.create(http_params()).await.unwrap();

    assert_eq!(server.transport_type, "http");
    assert!(server.enabled);
    assert!(server.transport_config.contains("example.com/mcp"));
}

#[tokio::test]
async fn create_sse_server() {
    let (r, _db) = repo().await;
    let server = r.create(sse_params()).await.unwrap();

    assert_eq!(server.transport_type, "sse");
    assert!(server.transport_config.contains("Bearer xxx"));
}

// -- C-4: Duplicate name returns conflict --

#[tokio::test]
async fn create_duplicate_name_returns_conflict() {
    let (r, _db) = repo().await;
    r.create(stdio_params()).await.unwrap();

    let err = r.create(stdio_params()).await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)));
}

// -- R-1/R-2: Get by ID --

#[tokio::test]
async fn find_by_id_returns_full_record() {
    let (r, _db) = repo().await;
    let created = r.create(stdio_params()).await.unwrap();

    let found = r.find_by_id(&created.mcp_server_id).await.unwrap().unwrap();
    assert_eq!(found.mcp_server_id, created.mcp_server_id);
    assert_eq!(found.name, "test-mcp");
    assert_eq!(found.transport_type, "stdio");
    assert_eq!(found.original_json.as_deref(), Some(r#"{"name":"test-mcp"}"#));
}

#[tokio::test]
async fn find_by_id_nonexistent_returns_none() {
    let (r, _db) = repo().await;
    assert!(r.find_by_id(&missing_id()).await.unwrap().is_none());
}

// -- Find by name --

#[tokio::test]
async fn find_by_name_returns_matching_record() {
    let (r, _db) = repo().await;
    let created = r.create(stdio_params()).await.unwrap();

    let found = r.find_by_name("test-mcp").await.unwrap().unwrap();
    assert_eq!(found.mcp_server_id, created.mcp_server_id);
}

#[tokio::test]
async fn find_by_name_nonexistent_returns_none() {
    let (r, _db) = repo().await;
    assert!(r.find_by_name("nope").await.unwrap().is_none());
}

// -- R-3/R-4: List servers --

#[tokio::test]
async fn list_empty_returns_empty_vec() {
    let (r, _db) = repo().await;
    let servers = r.list().await.unwrap();
    assert!(servers.is_empty());
}

#[tokio::test]
async fn list_returns_all_ordered_by_created_at() {
    let (r, _db) = repo().await;
    let s1 = r.create(stdio_params()).await.unwrap();
    let s2 = r.create(http_params()).await.unwrap();
    let s3 = r.create(sse_params()).await.unwrap();

    let all = r.list().await.unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].mcp_server_id, s1.mcp_server_id);
    assert_eq!(all[1].mcp_server_id, s2.mcp_server_id);
    assert_eq!(all[2].mcp_server_id, s3.mcp_server_id);
}

// -- U-1/U-2/U-3: Update fields --

#[tokio::test]
async fn update_name_only_preserves_other_fields() {
    let (r, _db) = repo().await;
    let created = r.create(stdio_params()).await.unwrap();

    let updated = r
        .update(
            &created.mcp_server_id,
            UpdateMcpServerParams {
                name: Some("renamed-mcp"),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.name, "renamed-mcp");
    assert_eq!(updated.transport_type, created.transport_type);
    assert_eq!(updated.transport_config, created.transport_config);
    assert_eq!(updated.enabled, created.enabled);
    assert!(updated.updated_at >= created.updated_at);
}

#[tokio::test]
async fn update_transport_type_and_config() {
    let (r, _db) = repo().await;
    let created = r.create(stdio_params()).await.unwrap();

    let updated = r
        .update(
            &created.mcp_server_id,
            UpdateMcpServerParams {
                transport_type: Some("http"),
                transport_config: Some(r#"{"url":"https://new.example.com"}"#),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.transport_type, "http");
    assert!(updated.transport_config.contains("new.example.com"));
}

#[tokio::test]
async fn update_description() {
    let (r, _db) = repo().await;
    let created = r.create(stdio_params()).await.unwrap();

    let updated = r
        .update(
            &created.mcp_server_id,
            UpdateMcpServerParams {
                description: Some(Some("new desc")),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.description.as_deref(), Some("new desc"));
}

// -- U-4: Update nonexistent --

#[tokio::test]
async fn update_nonexistent_returns_not_found() {
    let (r, _db) = repo().await;
    let err = r
        .update(&missing_id(), UpdateMcpServerParams::default())
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::NotFound(_)));
}

// -- U-5: Name conflict on update --

#[tokio::test]
async fn update_name_to_existing_name_returns_conflict() {
    let (r, _db) = repo().await;
    r.create(stdio_params()).await.unwrap();
    let s2 = r.create(http_params()).await.unwrap();

    let err = r
        .update(
            &s2.mcp_server_id,
            UpdateMcpServerParams {
                name: Some("test-mcp"),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)));
}

// -- Update: clear optional fields --

#[tokio::test]
async fn update_can_clear_optional_fields() {
    let (r, _db) = repo().await;
    let created = r.create(stdio_params()).await.unwrap();
    assert!(created.description.is_some());
    assert!(created.original_json.is_some());

    let updated = r
        .update(
            &created.mcp_server_id,
            UpdateMcpServerParams {
                description: Some(None),
                original_json: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(updated.description.is_none());
    assert!(updated.original_json.is_none());
}

// -- Update persists --

#[tokio::test]
async fn update_persists_to_database() {
    let (r, _db) = repo().await;
    let created = r.create(stdio_params()).await.unwrap();

    r.update(
        &created.mcp_server_id,
        UpdateMcpServerParams {
            enabled: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let found = r.find_by_id(&created.mcp_server_id).await.unwrap().unwrap();
    assert!(found.enabled);
}

// -- D-1/D-2/D-3: Delete --

#[tokio::test]
async fn delete_existing_removes_record() {
    let (r, _db) = repo().await;
    let created = r.create(stdio_params()).await.unwrap();

    r.delete(&created.mcp_server_id).await.unwrap();
    assert!(r.find_by_id(&created.mcp_server_id).await.unwrap().is_none());
    let deleted = r
        .find_by_id_any(&created.mcp_server_id)
        .await
        .unwrap()
        .unwrap();
    assert!(deleted.deleted_at.is_some());
    assert!(!deleted.enabled);
}

#[tokio::test]
async fn soft_delete_cascades_conversation_selection_links() {
    let (r, db) = repo().await;
    let server = r.create(stdio_params()).await.unwrap();
    let user_id = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
    let conversation_id = nomifun_common::ConversationId::new().into_string();
    nomifun_db::sqlx::query(
        "INSERT INTO conversations \
         (conversation_id, user_id, name, type, created_at, updated_at) \
         VALUES (?, ?, 'MCP selection', 'nomi', 1, 1)",
    )
    .bind(&conversation_id)
    .bind(&user_id)
    .execute(db.pool())
    .await
    .unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO conversation_mcp_servers \
         (conversation_id, mcp_server_id, sort_order) VALUES (?, ?, 0)",
    )
    .bind(&conversation_id)
    .bind(&server.mcp_server_id)
    .execute(db.pool())
    .await
    .unwrap();

    r.delete(&server.mcp_server_id).await.unwrap();

    let link_count: i64 = nomifun_db::sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_mcp_servers WHERE mcp_server_id = ?",
    )
    .bind(&server.mcp_server_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(link_count, 0);
}

#[tokio::test]
async fn delete_nonexistent_returns_not_found() {
    let (r, _db) = repo().await;
    let err = r.delete(&missing_id()).await.unwrap_err();
    assert!(matches!(err, DbError::NotFound(_)));
}

#[tokio::test]
async fn delete_one_does_not_affect_others() {
    let (r, _db) = repo().await;
    let s1 = r.create(stdio_params()).await.unwrap();
    let s2 = r.create(http_params()).await.unwrap();

    r.delete(&s1.mcp_server_id).await.unwrap();

    let remaining = r.list().await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].mcp_server_id, s2.mcp_server_id);
}

#[tokio::test]
async fn list_by_ids_any_includes_soft_deleted_rows() {
    let (r, _db) = repo().await;
    let active = r.create(stdio_params()).await.unwrap();
    let deleted = r.create(http_params()).await.unwrap();
    r.delete(&deleted.mcp_server_id).await.unwrap();

    let rows = r
        .list_by_ids_any(&[
            deleted.mcp_server_id.clone(),
            active.mcp_server_id.clone(),
        ])
        .await
        .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].mcp_server_id, deleted.mcp_server_id);
    assert!(rows[0].deleted_at.is_some());
    assert_eq!(rows[1].mcp_server_id, active.mcp_server_id);
    assert!(rows[1].deleted_at.is_none());
}

// -- B-1/B-2/B-3: Batch upsert --

#[tokio::test]
async fn batch_upsert_creates_new_servers() {
    let (r, _db) = repo().await;

    let results = r
        .batch_upsert(&[stdio_params(), http_params(), sse_params()])
        .await
        .unwrap();

    assert_eq!(results.len(), 3);
    assert_eq!(results[0].name, "test-mcp");
    assert_eq!(results[1].name, "http-mcp");
    assert_eq!(results[2].name, "sse-mcp");

    let all = r.list().await.unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn batch_upsert_updates_existing_by_name() {
    let (r, _db) = repo().await;
    let existing = r.create(stdio_params()).await.unwrap();
    assert!(!existing.enabled);

    let results = r
        .batch_upsert(&[
            CreateMcpServerParams {
                enabled: true,
                description: Some("Updated via batch"),
                ..stdio_params()
            },
            http_params(),
        ])
        .await
        .unwrap();

    assert_eq!(results.len(), 2);
    // Existing updated: same ID, new values
    assert_eq!(results[0].mcp_server_id, existing.mcp_server_id);
    assert!(results[0].enabled);
    assert_eq!(results[0].description.as_deref(), Some("Updated via batch"));
    // New created
    assert_eq!(results[1].name, "http-mcp");
}

#[tokio::test]
async fn batch_upsert_empty_list() {
    let (r, _db) = repo().await;
    let results = r.batch_upsert(&[]).await.unwrap();
    assert!(results.is_empty());
}

// -- Status updates --

#[tokio::test]
async fn update_status_with_timestamp() {
    let (r, _db) = repo().await;
    let created = r.create(stdio_params()).await.unwrap();

    let ts = nomifun_common::now_ms();
    r.update_status(&created.mcp_server_id, "connected", Some(ts))
        .await
        .unwrap();

    let found = r.find_by_id(&created.mcp_server_id).await.unwrap().unwrap();
    assert_eq!(found.last_test_status, "connected");
    assert_eq!(found.last_connected, Some(ts));
}

#[tokio::test]
async fn update_status_without_timestamp_preserves_existing() {
    let (r, _db) = repo().await;
    let created = r.create(stdio_params()).await.unwrap();

    let ts = nomifun_common::now_ms();
    r.update_status(&created.mcp_server_id, "connected", Some(ts))
        .await
        .unwrap();

    r.update_status(&created.mcp_server_id, "error", None)
        .await
        .unwrap();

    let found = r.find_by_id(&created.mcp_server_id).await.unwrap().unwrap();
    assert_eq!(found.last_test_status, "error");
    assert_eq!(found.last_connected, Some(ts));
}

#[tokio::test]
async fn update_status_nonexistent_returns_not_found() {
    let (r, _db) = repo().await;
    let err = r
        .update_status(&missing_id(), "connected", None)
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::NotFound(_)));
}

// -- Tools updates --

#[tokio::test]
async fn update_tools_sets_json() {
    let (r, _db) = repo().await;
    let created = r.create(stdio_params()).await.unwrap();

    let tools_json = r#"[{"name":"read_file","description":"Read a file"}]"#;
    r.update_tools(&created.mcp_server_id, Some(tools_json))
        .await
        .unwrap();

    let found = r.find_by_id(&created.mcp_server_id).await.unwrap().unwrap();
    assert_eq!(found.tools.as_deref(), Some(tools_json));
}

#[tokio::test]
async fn update_tools_clears_to_null() {
    let (r, _db) = repo().await;
    let created = r
        .create(CreateMcpServerParams {
            tools: Some(r#"[{"name":"tool"}]"#),
            ..stdio_params()
        })
        .await
        .unwrap();
    assert!(created.tools.is_some());

    r.update_tools(&created.mcp_server_id, None).await.unwrap();

    let found = r.find_by_id(&created.mcp_server_id).await.unwrap().unwrap();
    assert!(found.tools.is_none());
}

#[tokio::test]
async fn update_tools_nonexistent_returns_not_found() {
    let (r, _db) = repo().await;
    let err = r
        .update_tools(&missing_id(), Some("[]"))
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::NotFound(_)));
}

// -- Full CRUD lifecycle --

#[tokio::test]
async fn full_crud_lifecycle() {
    let (r, _db) = repo().await;

    // Create
    let created = r.create(stdio_params()).await.unwrap();
    assert_eq!(created.name, "test-mcp");

    // Read
    let found = r.find_by_id(&created.mcp_server_id).await.unwrap().unwrap();
    assert_eq!(found.mcp_server_id, created.mcp_server_id);

    // Update
    let updated = r
        .update(
            &created.mcp_server_id,
            UpdateMcpServerParams {
                name: Some("renamed-mcp"),
                enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.name, "renamed-mcp");
    assert!(updated.enabled);

    // Find by new name
    let by_name = r.find_by_name("renamed-mcp").await.unwrap().unwrap();
    assert_eq!(by_name.mcp_server_id, created.mcp_server_id);

    // Update status
    r.update_status(
        &created.mcp_server_id,
        "connected",
        Some(nomifun_common::now_ms()),
    )
        .await
        .unwrap();
    let after_status = r.find_by_id(&created.mcp_server_id).await.unwrap().unwrap();
    assert_eq!(after_status.last_test_status, "connected");

    // Delete
    r.delete(&created.mcp_server_id).await.unwrap();
    assert!(r.find_by_id(&created.mcp_server_id).await.unwrap().is_none());
    assert!(r.list().await.unwrap().is_empty());
}

// -- Builtin server --

#[tokio::test]
async fn create_builtin_server() {
    let (r, _db) = repo().await;
    let server = r
        .create(CreateMcpServerParams {
            name: "builtin-img",
            builtin: true,
            enabled: true,
            ..stdio_params()
        })
        .await
        .unwrap();

    assert!(server.builtin);
    assert!(server.enabled);
}
