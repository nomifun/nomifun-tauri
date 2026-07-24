use std::collections::HashMap;

use nomifun_common::{McpServerId, TimestampMs};
use sqlx::QueryBuilder;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::McpServerRow;
use crate::repository::mcp_server::{CreateMcpServerParams, IMcpServerRepository, UpdateMcpServerParams};

const MCP_SERVER_COLUMNS: &str = "\
    mcp_server_id, name, description, enabled, transport_type, transport_config, \
    tools, last_test_status, last_connected, original_json, builtin, deleted_at, \
    created_at, updated_at";

/// SQLite-backed implementation of [`IMcpServerRepository`].
#[derive(Clone, Debug)]
pub struct SqliteMcpServerRepository {
    pool: SqlitePool,
}

impl SqliteMcpServerRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IMcpServerRepository for SqliteMcpServerRepository {
    async fn list(&self) -> Result<Vec<McpServerRow>, DbError> {
        Ok(sqlx::query_as::<_, McpServerRow>(&format!(
            "SELECT {MCP_SERVER_COLUMNS} FROM mcp_servers \
             WHERE deleted_at IS NULL ORDER BY created_at ASC, id ASC"
        ))
        .fetch_all(&self.pool)
        .await?)
    }

    async fn find_by_id(&self, mcp_server_id: &str) -> Result<Option<McpServerRow>, DbError> {
        Ok(sqlx::query_as::<_, McpServerRow>(&format!(
            "SELECT {MCP_SERVER_COLUMNS} FROM mcp_servers \
             WHERE mcp_server_id = ? AND deleted_at IS NULL"
        ))
        .bind(mcp_server_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn find_by_name(&self, name: &str) -> Result<Option<McpServerRow>, DbError> {
        Ok(sqlx::query_as::<_, McpServerRow>(&format!(
            "SELECT {MCP_SERVER_COLUMNS} FROM mcp_servers \
             WHERE name = ? AND deleted_at IS NULL"
        ))
        .bind(name)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn find_by_id_any(&self, mcp_server_id: &str) -> Result<Option<McpServerRow>, DbError> {
        Ok(sqlx::query_as::<_, McpServerRow>(&format!(
            "SELECT {MCP_SERVER_COLUMNS} FROM mcp_servers WHERE mcp_server_id = ?"
        ))
        .bind(mcp_server_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn find_by_name_any(&self, name: &str) -> Result<Option<McpServerRow>, DbError> {
        Ok(sqlx::query_as::<_, McpServerRow>(&format!(
            "SELECT {MCP_SERVER_COLUMNS} FROM mcp_servers WHERE name = ?"
        ))
        .bind(name)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_by_ids_any(&self, mcp_server_ids: &[String]) -> Result<Vec<McpServerRow>, DbError> {
        if mcp_server_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut query = QueryBuilder::new(format!(
            "SELECT {MCP_SERVER_COLUMNS} FROM mcp_servers WHERE mcp_server_id IN ("
        ));
        let mut separated = query.separated(", ");
        for mcp_server_id in mcp_server_ids {
            separated.push_bind(mcp_server_id);
        }
        separated.push_unseparated(") ORDER BY created_at ASC, id ASC");

        let rows = query.build_query_as::<McpServerRow>().fetch_all(&self.pool).await?;
        let rows_by_id: HashMap<_, _> = rows
            .into_iter()
            .map(|row| (row.mcp_server_id.clone(), row))
            .collect();

        Ok(mcp_server_ids
            .iter()
            .filter_map(|mcp_server_id| rows_by_id.get(mcp_server_id).cloned())
            .collect())
    }

    async fn create(&self, params: CreateMcpServerParams<'_>) -> Result<McpServerRow, DbError> {
        let now = nomifun_common::now_ms();
        let mcp_server_id = McpServerId::new().into_string();
        let last_test_status = "disconnected";

        sqlx::query(
            "INSERT INTO mcp_servers \
                (mcp_server_id, name, description, enabled, transport_type, transport_config, \
                 tools, last_test_status, last_connected, original_json, builtin, \
                 deleted_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&mcp_server_id)
        .bind(params.name)
        .bind(params.description)
        .bind(params.enabled)
        .bind(params.transport_type)
        .bind(params.transport_config)
        .bind(params.tools)
        .bind(last_test_status)
        .bind(Option::<TimestampMs>::None)
        .bind(params.original_json)
        .bind(params.builtin)
        .bind(Option::<TimestampMs>::None)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
                DbError::Conflict(format!("MCP server name '{}' already exists", params.name))
            }
            _ => DbError::Query(e),
        })?;

        Ok(McpServerRow {
            mcp_server_id,
            name: params.name.to_string(),
            description: params.description.map(String::from),
            enabled: params.enabled,
            transport_type: params.transport_type.to_string(),
            transport_config: params.transport_config.to_string(),
            tools: params.tools.map(String::from),
            last_test_status: last_test_status.to_string(),
            last_connected: None,
            original_json: params.original_json.map(String::from),
            builtin: params.builtin,
            deleted_at: None,
            created_at: now,
            updated_at: now,
        })
    }

    async fn update(
        &self,
        mcp_server_id: &str,
        params: UpdateMcpServerParams<'_>,
    ) -> Result<McpServerRow, DbError> {
        let existing = self
            .find_by_id_any(mcp_server_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("MCP server '{mcp_server_id}' not found")))?;
        let merged = merge_update(existing, params);

        sqlx::query(
            "UPDATE mcp_servers SET \
                name = ?, description = ?, enabled = ?, transport_type = ?, \
                transport_config = ?, tools = ?, original_json = ?, \
                builtin = ?, deleted_at = ?, updated_at = ? \
             WHERE mcp_server_id = ?",
        )
        .bind(&merged.name)
        .bind(&merged.description)
        .bind(merged.enabled)
        .bind(&merged.transport_type)
        .bind(&merged.transport_config)
        .bind(&merged.tools)
        .bind(&merged.original_json)
        .bind(merged.builtin)
        .bind(merged.deleted_at)
        .bind(merged.updated_at)
        .bind(mcp_server_id)
        .execute(&self.pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
                DbError::Conflict(format!("MCP server name '{}' already exists", merged.name))
            }
            _ => DbError::Query(e),
        })?;

        Ok(merged)
    }

    async fn delete(&self, mcp_server_id: &str) -> Result<(), DbError> {
        let now = nomifun_common::now_ms();
        let mut transaction = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE mcp_servers SET updated_at = updated_at \
             WHERE mcp_server_id = ? AND deleted_at IS NULL",
        )
        .bind(mcp_server_id)
        .execute(&mut *transaction)
        .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("MCP server '{mcp_server_id}' not found")));
        }

        sqlx::query("DELETE FROM conversation_mcp_servers WHERE mcp_server_id = ?")
            .bind(mcp_server_id)
            .execute(&mut *transaction)
            .await?;

        sqlx::query(
            "UPDATE mcp_servers SET enabled = 0, deleted_at = ?, updated_at = ? \
             WHERE mcp_server_id = ? AND deleted_at IS NULL",
        )
        .bind(now)
        .bind(now)
        .bind(mcp_server_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn batch_upsert(&self, servers: &[CreateMcpServerParams<'_>]) -> Result<Vec<McpServerRow>, DbError> {
        let mut results = Vec::with_capacity(servers.len());

        for params in servers {
            let row = match self.find_by_name(params.name).await? {
                Some(existing) => {
                    let update_params = UpdateMcpServerParams {
                        description: Some(params.description),
                        enabled: Some(params.enabled),
                        transport_type: Some(params.transport_type),
                        transport_config: Some(params.transport_config),
                        tools: Some(params.tools),
                        original_json: Some(params.original_json),
                        builtin: Some(params.builtin),
                        ..Default::default()
                    };
                    self.update(&existing.mcp_server_id, update_params).await?
                }
                None => self.create(params.clone()).await?,
            };
            results.push(row);
        }

        Ok(results)
    }

    async fn update_status(
        &self,
        mcp_server_id: &str,
        status: &str,
        last_connected: Option<TimestampMs>,
    ) -> Result<(), DbError> {
        let now = nomifun_common::now_ms();
        let result = sqlx::query(
            "UPDATE mcp_servers SET last_test_status = ?, \
             last_connected = COALESCE(?, last_connected), \
             updated_at = ? WHERE mcp_server_id = ? AND deleted_at IS NULL",
        )
        .bind(status)
        .bind(last_connected)
        .bind(now)
        .bind(mcp_server_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("MCP server '{mcp_server_id}' not found")));
        }
        Ok(())
    }

    async fn update_tools(&self, mcp_server_id: &str, tools: Option<&str>) -> Result<(), DbError> {
        let now = nomifun_common::now_ms();
        let result = sqlx::query(
            "UPDATE mcp_servers SET tools = ?, updated_at = ? \
             WHERE mcp_server_id = ? AND deleted_at IS NULL",
        )
        .bind(tools)
        .bind(now)
        .bind(mcp_server_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("MCP server '{mcp_server_id}' not found")));
        }
        Ok(())
    }
}

fn merge_update(existing: McpServerRow, params: UpdateMcpServerParams<'_>) -> McpServerRow {
    let now = nomifun_common::now_ms();
    McpServerRow {
        mcp_server_id: existing.mcp_server_id,
        name: params.name.unwrap_or(&existing.name).to_string(),
        description: params.description.map_or(existing.description, |v| v.map(String::from)),
        enabled: params.enabled.unwrap_or(existing.enabled),
        transport_type: params.transport_type.unwrap_or(&existing.transport_type).to_string(),
        transport_config: params
            .transport_config
            .unwrap_or(&existing.transport_config)
            .to_string(),
        tools: params.tools.map_or(existing.tools, |v| v.map(String::from)),
        last_test_status: existing.last_test_status,
        last_connected: existing.last_connected,
        original_json: params
            .original_json
            .map_or(existing.original_json, |v| v.map(String::from)),
        builtin: params.builtin.unwrap_or(existing.builtin),
        deleted_at: params.deleted_at.unwrap_or(existing.deleted_at),
        created_at: existing.created_at,
        updated_at: now,
    }
}

fn is_unique_violation(err: &dyn sqlx::error::DatabaseError) -> bool {
    err.code().is_some_and(|c| c == "2067")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteMcpServerRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteMcpServerRepository::new(db.pool().clone());
        (repo, db)
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

    #[tokio::test]
    async fn list_empty() {
        let (repo, _db) = setup().await;
        let servers = repo.list().await.unwrap();
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn create_returns_populated_fields() {
        let (repo, _db) = setup().await;
        let server = repo.create(stdio_params()).await.unwrap();

        assert!(nomifun_common::validate_uuidv7(&server.mcp_server_id).is_ok());
        assert_eq!(server.name, "test-mcp");
        assert_eq!(server.description.as_deref(), Some("A test MCP server"));
        assert!(!server.enabled);
        assert_eq!(server.transport_type, "stdio");
        assert!(server.transport_config.contains("npx"));
        assert!(server.tools.is_none());
        assert_eq!(server.last_test_status, "disconnected");
        assert!(server.last_connected.is_none());
        assert!(server.original_json.is_some());
        assert!(!server.builtin);
        assert!(server.created_at > 0);
        assert_eq!(server.created_at, server.updated_at);
    }

    #[tokio::test]
    async fn create_duplicate_name_returns_conflict() {
        let (repo, _db) = setup().await;
        repo.create(stdio_params()).await.unwrap();

        let err = repo.create(stdio_params()).await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn find_by_id_returns_record() {
        let (repo, _db) = setup().await;
        let created = repo.create(stdio_params()).await.unwrap();

        let found = repo.find_by_id(&created.mcp_server_id).await.unwrap().unwrap();
        assert_eq!(found.mcp_server_id, created.mcp_server_id);
        assert_eq!(found.name, "test-mcp");
    }

    #[tokio::test]
    async fn find_by_id_nonexistent() {
        let (repo, _db) = setup().await;
        assert!(repo.find_by_id("0190f5fe-7c00-7a00-8000-000000000999").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn find_by_name_returns_record() {
        let (repo, _db) = setup().await;
        let created = repo.create(stdio_params()).await.unwrap();

        let found = repo.find_by_name("test-mcp").await.unwrap().unwrap();
        assert_eq!(found.mcp_server_id, created.mcp_server_id);
    }

    #[tokio::test]
    async fn find_by_name_nonexistent() {
        let (repo, _db) = setup().await;
        assert!(repo.find_by_name("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_returns_all_ordered() {
        let (repo, _db) = setup().await;
        let s1 = repo.create(stdio_params()).await.unwrap();
        let s2 = repo.create(http_params()).await.unwrap();

        let all = repo.list().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].mcp_server_id, s1.mcp_server_id);
        assert_eq!(all[1].mcp_server_id, s2.mcp_server_id);
    }

    #[tokio::test]
    async fn update_partial_fields() {
        let (repo, _db) = setup().await;
        let created = repo.create(stdio_params()).await.unwrap();

        let updated = repo
            .update(
                &created.mcp_server_id,
                UpdateMcpServerParams {
                    enabled: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert!(updated.enabled);
        assert_eq!(updated.name, "test-mcp");
        assert_eq!(updated.transport_type, "stdio");
        assert!(updated.updated_at >= created.updated_at);
    }

    #[tokio::test]
    async fn update_name_conflict_returns_conflict() {
        let (repo, _db) = setup().await;
        repo.create(stdio_params()).await.unwrap();
        let s2 = repo.create(http_params()).await.unwrap();

        let err = repo
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

    #[tokio::test]
    async fn update_clear_optional_fields() {
        let (repo, _db) = setup().await;
        let created = repo.create(stdio_params()).await.unwrap();
        assert!(created.description.is_some());

        let updated = repo
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

    #[tokio::test]
    async fn update_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update("0190f5fe-7c00-7a00-8000-000000000999", UpdateMcpServerParams::default())
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_existing() {
        let (repo, _db) = setup().await;
        let created = repo.create(stdio_params()).await.unwrap();

        repo.delete(&created.mcp_server_id).await.unwrap();
        assert!(repo.find_by_id(&created.mcp_server_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.delete("0190f5fe-7c00-7a00-8000-000000000999").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn batch_upsert_creates_new_and_updates_existing() {
        let (repo, _db) = setup().await;
        let existing = repo.create(stdio_params()).await.unwrap();
        assert!(!existing.enabled);

        let results = repo
            .batch_upsert(&[
                CreateMcpServerParams {
                    enabled: true,
                    ..stdio_params()
                },
                http_params(),
            ])
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        // Existing was updated (same ID, enabled changed)
        assert_eq!(results[0].mcp_server_id, existing.mcp_server_id);
        assert!(results[0].enabled);
        // New was created
        assert_eq!(results[1].name, "http-mcp");
        assert!(nomifun_common::validate_uuidv7(&results[1].mcp_server_id).is_ok());
    }

    #[tokio::test]
    async fn update_status_sets_status_and_last_connected() {
        let (repo, _db) = setup().await;
        let created = repo.create(stdio_params()).await.unwrap();

        let ts = nomifun_common::now_ms();
        repo.update_status(&created.mcp_server_id, "connected", Some(ts)).await.unwrap();

        let found = repo.find_by_id(&created.mcp_server_id).await.unwrap().unwrap();
        assert_eq!(found.last_test_status, "connected");
        assert_eq!(found.last_connected, Some(ts));
    }

    #[tokio::test]
    async fn update_status_without_timestamp_preserves_existing() {
        let (repo, _db) = setup().await;
        let created = repo.create(stdio_params()).await.unwrap();

        let ts = nomifun_common::now_ms();
        repo.update_status(&created.mcp_server_id, "connected", Some(ts)).await.unwrap();

        repo.update_status(&created.mcp_server_id, "error", None).await.unwrap();

        let found = repo.find_by_id(&created.mcp_server_id).await.unwrap().unwrap();
        assert_eq!(found.last_test_status, "error");
        assert_eq!(found.last_connected, Some(ts));
    }

    #[tokio::test]
    async fn update_status_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.update_status("0190f5fe-7c00-7a00-8000-000000000999", "connected", None).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_tools_sets_tools_json() {
        let (repo, _db) = setup().await;
        let created = repo.create(stdio_params()).await.unwrap();
        assert!(created.tools.is_none());

        let tools_json = r#"[{"name":"read_file","description":"Read a file"}]"#;
        repo.update_tools(&created.mcp_server_id, Some(tools_json)).await.unwrap();

        let found = repo.find_by_id(&created.mcp_server_id).await.unwrap().unwrap();
        assert_eq!(found.tools.as_deref(), Some(tools_json));
    }

    #[tokio::test]
    async fn update_tools_clear() {
        let (repo, _db) = setup().await;
        let created = repo
            .create(CreateMcpServerParams {
                tools: Some(r#"[{"name":"tool"}]"#),
                ..stdio_params()
            })
            .await
            .unwrap();
        assert!(created.tools.is_some());

        repo.update_tools(&created.mcp_server_id, None).await.unwrap();

        let found = repo.find_by_id(&created.mcp_server_id).await.unwrap().unwrap();
        assert!(found.tools.is_none());
    }

    #[tokio::test]
    async fn update_tools_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.update_tools("0190f5fe-7c00-7a00-8000-000000000999", Some("[]")).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }
}
