use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::CreationTaskRow;
use crate::repository::ICreationTaskRepository;
use crate::repository::creation_task::{
    CreateCreationTaskParams, ListCreationTasksParams, UpdateCreationTaskParams,
};

/// SQLite-backed implementation of [`ICreationTaskRepository`].
#[derive(Clone, Debug)]
pub struct SqliteCreationTaskRepository {
    pool: SqlitePool,
}

impl SqliteCreationTaskRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// The concrete column values written by both the unconditional and conditional
/// update paths — `params` merged over the current row (`Some` replaces, `None`
/// keeps; inner `Option` distinguishes "set NULL" from "keep").
struct MergedTaskUpdate {
    status: String,
    error: Option<String>,
    result_asset_ids: String,
    remote_task_id: Option<String>,
    attempt: i64,
    started_at: Option<i64>,
    finished_at: Option<i64>,
}

fn merge_update_fields(existing: &CreationTaskRow, params: &UpdateCreationTaskParams<'_>) -> MergedTaskUpdate {
    MergedTaskUpdate {
        status: params.status.unwrap_or(&existing.status).to_string(),
        error: match params.error {
            Some(e) => e.map(str::to_string),
            None => existing.error.clone(),
        },
        result_asset_ids: params.result_asset_ids.unwrap_or(&existing.result_asset_ids).to_string(),
        remote_task_id: match params.remote_task_id {
            Some(r) => r.map(str::to_string),
            None => existing.remote_task_id.clone(),
        },
        attempt: params.attempt.unwrap_or(existing.attempt),
        started_at: match params.started_at {
            Some(s) => s,
            None => existing.started_at,
        },
        finished_at: match params.finished_at {
            Some(f) => f,
            None => existing.finished_at,
        },
    }
}

#[async_trait::async_trait]
impl ICreationTaskRepository for SqliteCreationTaskRepository {
    async fn create_task(&self, params: CreateCreationTaskParams<'_>) -> Result<CreationTaskRow, DbError> {
        sqlx::query(
            "INSERT INTO creation_tasks \
                (id, canvas_id, node_id, provider_id, model, capability, params, status, error, \
                 result_asset_ids, remote_task_id, attempt, submitted_at, started_at, finished_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, '[]', NULL, 0, ?, NULL, NULL)",
        )
        .bind(params.id)
        .bind(params.canvas_id)
        .bind(params.node_id)
        .bind(params.provider_id)
        .bind(params.model)
        .bind(params.capability)
        .bind(params.params)
        .bind(params.status)
        .bind(params.submitted_at)
        .execute(&self.pool)
        .await?;

        Ok(CreationTaskRow {
            id: params.id.to_string(),
            canvas_id: params.canvas_id.map(str::to_string),
            node_id: params.node_id.map(str::to_string),
            provider_id: params.provider_id.to_string(),
            model: params.model.to_string(),
            capability: params.capability.to_string(),
            params: params.params.to_string(),
            status: params.status.to_string(),
            error: None,
            result_asset_ids: "[]".to_string(),
            remote_task_id: None,
            attempt: 0,
            submitted_at: params.submitted_at,
            started_at: None,
            finished_at: None,
        })
    }

    async fn get_task(&self, id: &str) -> Result<Option<CreationTaskRow>, DbError> {
        let row = sqlx::query_as::<_, CreationTaskRow>("SELECT * FROM creation_tasks WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn list_tasks(&self, params: ListCreationTasksParams<'_>) -> Result<Vec<CreationTaskRow>, DbError> {
        let limit = params.limit.clamp(1, 500);
        let rows = sqlx::query_as::<_, CreationTaskRow>(
            "SELECT * FROM creation_tasks \
             WHERE (?1 IS NULL OR canvas_id = ?1) AND (?2 IS NULL OR status = ?2) \
             ORDER BY submitted_at DESC, id DESC LIMIT ?3",
        )
        .bind(params.canvas_id)
        .bind(params.status)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn update_task(&self, id: &str, params: UpdateCreationTaskParams<'_>) -> Result<CreationTaskRow, DbError> {
        let existing = self
            .get_task(id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("creation task '{id}' not found")))?;

        let m = merge_update_fields(&existing, &params);

        sqlx::query(
            "UPDATE creation_tasks SET status = ?, error = ?, result_asset_ids = ?, remote_task_id = ?, \
             attempt = ?, started_at = ?, finished_at = ? WHERE id = ?",
        )
        .bind(&m.status)
        .bind(&m.error)
        .bind(&m.result_asset_ids)
        .bind(&m.remote_task_id)
        .bind(m.attempt)
        .bind(m.started_at)
        .bind(m.finished_at)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(CreationTaskRow {
            status: m.status,
            error: m.error,
            result_asset_ids: m.result_asset_ids,
            remote_task_id: m.remote_task_id,
            attempt: m.attempt,
            started_at: m.started_at,
            finished_at: m.finished_at,
            ..existing
        })
    }

    async fn update_task_if_live(&self, id: &str, params: UpdateCreationTaskParams<'_>) -> Result<bool, DbError> {
        let Some(existing) = self.get_task(id).await? else {
            return Ok(false); // unknown id → treat as "not live"
        };
        let m = merge_update_fields(&existing, &params);

        // The `WHERE ... status IN ('queued','running')` predicate is the
        // compare-and-set: if a concurrent cancel wrote a terminal status
        // between our read and this write, zero rows match and we do not
        // overwrite it.
        let res = sqlx::query(
            "UPDATE creation_tasks SET status = ?, error = ?, result_asset_ids = ?, remote_task_id = ?, \
             attempt = ?, started_at = ?, finished_at = ? WHERE id = ? AND status IN ('queued', 'running')",
        )
        .bind(&m.status)
        .bind(&m.error)
        .bind(&m.result_asset_ids)
        .bind(&m.remote_task_id)
        .bind(m.attempt)
        .bind(m.started_at)
        .bind(m.finished_at)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(res.rows_affected() > 0)
    }

    async fn list_live_tasks(&self) -> Result<Vec<CreationTaskRow>, DbError> {
        let rows = sqlx::query_as::<_, CreationTaskRow>(
            "SELECT * FROM creation_tasks WHERE status IN ('queued', 'running') ORDER BY submitted_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn repo() -> (SqliteCreationTaskRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteCreationTaskRepository::new(db.pool().clone());
        (repo, db)
    }

    fn create_params<'a>(id: &'a str, canvas: Option<&'a str>) -> CreateCreationTaskParams<'a> {
        CreateCreationTaskParams {
            id,
            canvas_id: canvas,
            node_id: None,
            provider_id: "prov_x",
            model: "m",
            capability: "t2i",
            params: r#"{"prompt":"cat"}"#,
            status: "queued",
            submitted_at: 100,
        }
    }

    #[tokio::test]
    async fn create_get_and_update_flow() {
        let (repo, _db) = repo().await;
        let t = repo.create_task(create_params("wst_1", Some("wsc_1"))).await.unwrap();
        assert_eq!(t.status, "queued");
        assert_eq!(t.result_asset_ids, "[]");
        assert_eq!(t.attempt, 0);

        // M0 shape: immediately fail with adapter_unavailable.
        let failed = repo
            .update_task(
                "wst_1",
                UpdateCreationTaskParams {
                    status: Some("failed"),
                    error: Some(Some(r#"{"kind":"adapter_unavailable","message":"no adapter"}"#)),
                    finished_at: Some(Some(200)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(failed.finished_at, Some(200));
        assert!(failed.error.as_deref().unwrap().contains("adapter_unavailable"));
        // unchanged fields preserved
        assert_eq!(failed.model, "m");
        assert_eq!(failed.capability, "t2i");

        assert!(matches!(
            repo.update_task("nope", UpdateCreationTaskParams::default()).await.unwrap_err(),
            DbError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn list_filters_and_live() {
        let (repo, _db) = repo().await;
        repo.create_task(create_params("wst_a", Some("wsc_1"))).await.unwrap();
        repo.create_task(create_params("wst_b", Some("wsc_2"))).await.unwrap();
        repo.update_task("wst_b", UpdateCreationTaskParams { status: Some("running"), ..Default::default() })
            .await
            .unwrap();

        // canvas filter
        let list = repo
            .list_tasks(ListCreationTasksParams { canvas_id: Some("wsc_1"), limit: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "wst_a");

        // status filter
        let list = repo
            .list_tasks(ListCreationTasksParams { status: Some("running"), limit: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "wst_b");

        // both queued+running are "live"
        let live = repo.list_live_tasks().await.unwrap();
        assert_eq!(live.len(), 2);
    }

    #[tokio::test]
    async fn update_task_if_live_refuses_terminal_overwrite() {
        let (repo, _db) = repo().await;
        repo.create_task(create_params("wst_c", None)).await.unwrap();
        // queued → running (still live)
        repo.update_task("wst_c", UpdateCreationTaskParams { status: Some("running"), ..Default::default() })
            .await
            .unwrap();
        // A cancel writes the terminal status (cancel path is unconditional).
        repo.update_task(
            "wst_c",
            UpdateCreationTaskParams { status: Some("canceled"), finished_at: Some(Some(1)), ..Default::default() },
        )
        .await
        .unwrap();
        // finalize's terminal write must NOT overwrite the canceled row.
        let applied = repo
            .update_task_if_live(
                "wst_c",
                UpdateCreationTaskParams { status: Some("succeeded"), finished_at: Some(Some(2)), ..Default::default() },
            )
            .await
            .unwrap();
        assert!(!applied, "terminal (canceled) row must not be overwritten");
        assert_eq!(repo.get_task("wst_c").await.unwrap().unwrap().status, "canceled");

        // A still-live task IS updated by the conditional write.
        repo.create_task(create_params("wst_d", None)).await.unwrap();
        let applied2 = repo
            .update_task_if_live("wst_d", UpdateCreationTaskParams { status: Some("succeeded"), ..Default::default() })
            .await
            .unwrap();
        assert!(applied2);
        assert_eq!(repo.get_task("wst_d").await.unwrap().unwrap().status, "succeeded");

        // Unknown id → Ok(false), no error.
        let applied3 = repo
            .update_task_if_live("nope", UpdateCreationTaskParams { status: Some("failed"), ..Default::default() })
            .await
            .unwrap();
        assert!(!applied3);
    }
}
