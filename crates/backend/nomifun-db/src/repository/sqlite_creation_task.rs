use nomifun_common::{ProviderId, WorkshopAssetId, WorkshopCanvasId, validate_uuidv7};
use serde_json::Value;
use sqlx::{Sqlite, SqlitePool, Transaction};

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

#[derive(sqlx::FromRow)]
struct CreationTaskDbRow {
    creation_task_id: String,
    canvas_id: Option<String>,
    node_id: Option<String>,
    provider_id: String,
    model: String,
    capability: String,
    params: String,
    status: String,
    error: Option<String>,
    result_asset_ids: String,
    remote_task_id: Option<String>,
    attempt: i64,
    submitted_at: i64,
    started_at: Option<i64>,
    finished_at: Option<i64>,
}

impl TryFrom<CreationTaskDbRow> for CreationTaskRow {
    type Error = DbError;

    fn try_from(row: CreationTaskDbRow) -> Result<Self, Self::Error> {
        let CreationTaskDbRow {
            creation_task_id,
            canvas_id,
            node_id,
            provider_id,
            model,
            capability,
            params,
            status,
            error,
            result_asset_ids,
            remote_task_id,
            attempt,
            submitted_at,
            started_at,
            finished_at,
        } = row;
        validate_creation_task_id(&creation_task_id)?;
        Ok(Self {
            creation_task_id,
            canvas_id,
            node_id,
            provider_id,
            model,
            capability,
            params,
            status,
            error,
            result_asset_ids,
            remote_task_id,
            attempt,
            submitted_at,
            started_at,
            finished_at,
        })
    }
}

fn validate_creation_task_id(creation_task_id: &str) -> Result<(), DbError> {
    validate_uuidv7(creation_task_id).map_err(|error| {
        DbError::Conflict(format!(
            "Creation task creation_task_id '{creation_task_id}' is not a canonical UUIDv7: {error}"
        ))
    })?;
    Ok(())
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

async fn lock_canvas(
    tx: &mut Transaction<'_, Sqlite>,
    canvas_id: Option<&str>,
) -> Result<Option<String>, DbError> {
    let Some(canvas_id) = canvas_id else {
        return Ok(None);
    };
    let canvas_id = WorkshopCanvasId::parse(canvas_id).map_err(|error| {
        DbError::Conflict(format!(
            "Creation task canvas_id '{canvas_id}' is not a canonical UUIDv7: {error}"
        ))
    })?;
    let parent = sqlx::query(
        "UPDATE workshop_canvases SET updated_at = updated_at WHERE canvas_id = ?",
    )
    .bind(canvas_id.as_str())
    .execute(&mut **tx)
    .await?;
    if parent.rows_affected() == 0 {
        return Err(DbError::Conflict(format!(
            "Creation task canvas '{}' does not exist",
            canvas_id
        )));
    }
    Ok(Some(canvas_id.into_string()))
}

/// Canonicalize the task's JSON result asset references.
///
/// These are logical references, not SQLite foreign keys. The asset sink owns
/// the atomic asset write, while the creation service/workshop bridge owns
/// existence, ownership, and locatability audits. Keeping this repository
/// check structural avoids coupling a task state update to a second repository
/// (and permits a provisional result batch to be committed by an alternate
/// asset sink in the same service operation).
fn canonicalize_result_asset_ids(raw: &str) -> Result<String, DbError> {
    let values: Value = serde_json::from_str(raw).map_err(|error| {
        DbError::Conflict(format!(
            "creation task result_asset_ids must be valid JSON: {error}"
        ))
    })?;
    let values = values.as_array().ok_or_else(|| {
        DbError::Conflict("creation task result_asset_ids must be a JSON array".into())
    })?;
    let mut canonical = Vec::with_capacity(values.len());
    let mut seen = std::collections::HashSet::with_capacity(values.len());
    for value in values {
        let raw_id = value.as_str().ok_or_else(|| {
            DbError::Conflict(
                "creation task result_asset_ids must contain only UUIDv7 strings".into(),
            )
        })?;
        let asset_id = WorkshopAssetId::parse(raw_id).map_err(|error| {
            DbError::Conflict(format!(
                "creation task result asset '{raw_id}' is not a canonical UUIDv7: {error}"
            ))
        })?;
        if !seen.insert(asset_id.as_str().to_owned()) {
            return Err(DbError::Conflict(format!(
                "creation task result_asset_ids contains duplicate asset '{}'",
                asset_id
            )));
        }
        canonical.push(asset_id.into_string());
    }
    serde_json::to_string(&canonical)
        .map_err(|error| DbError::Init(format!("encode creation task result_asset_ids: {error}")))
}

#[async_trait::async_trait]
impl ICreationTaskRepository for SqliteCreationTaskRepository {
    async fn create_task(&self, params: CreateCreationTaskParams<'_>) -> Result<CreationTaskRow, DbError> {
        let mut tx = self.pool.begin().await?;
        validate_creation_task_id(params.creation_task_id)?;
        let provider_id = ProviderId::parse(params.provider_id).map_err(|error| {
            DbError::Conflict(format!(
                "Creation task provider_id '{}' is not a canonical UUIDv7: {error}",
                params.provider_id
            ))
        })?;
        let parent = sqlx::query("UPDATE providers SET updated_at = updated_at WHERE provider_id = ?")
            .bind(provider_id.as_str())
            .execute(&mut *tx)
            .await?;
        if parent.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "Creation task provider '{}' does not exist",
                provider_id
            )));
        }

        let canvas_id = lock_canvas(&mut tx, params.canvas_id).await?;
        sqlx::query(
            "INSERT INTO creation_tasks \
                (creation_task_id, canvas_id, node_id, provider_id, model, capability, params, status, error, \
                 result_asset_ids, remote_task_id, attempt, submitted_at, started_at, finished_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, '[]', NULL, 0, ?, NULL, NULL)",
        )
        .bind(params.creation_task_id)
        .bind(&canvas_id)
        .bind(params.node_id)
        .bind(provider_id.as_str())
        .bind(params.model)
        .bind(params.capability)
        .bind(params.params)
        .bind(params.status)
        .bind(params.submitted_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(CreationTaskRow {
            creation_task_id: params.creation_task_id.to_string(),
            canvas_id,
            node_id: params.node_id.map(str::to_string),
            provider_id: provider_id.into_string(),
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

    async fn get_task(
        &self,
        creation_task_id: &str,
    ) -> Result<Option<CreationTaskRow>, DbError> {
        validate_creation_task_id(creation_task_id)?;
        let row = sqlx::query_as::<_, CreationTaskDbRow>(
            "SELECT * FROM creation_tasks WHERE creation_task_id = ?",
        )
            .bind(creation_task_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(TryInto::try_into).transpose()
    }

    async fn list_tasks(&self, params: ListCreationTasksParams<'_>) -> Result<Vec<CreationTaskRow>, DbError> {
        let limit = params.limit.clamp(1, 500);
        let rows = sqlx::query_as::<_, CreationTaskDbRow>(
            "SELECT * FROM creation_tasks \
             WHERE (?1 IS NULL OR canvas_id = ?1) AND (?2 IS NULL OR status = ?2) \
             ORDER BY submitted_at DESC, creation_task_id DESC LIMIT ?3",
        )
        .bind(params.canvas_id)
        .bind(params.status)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    async fn list_all_tasks(&self) -> Result<Vec<CreationTaskRow>, DbError> {
        sqlx::query_as::<_, CreationTaskDbRow>(
            "SELECT * FROM creation_tasks ORDER BY submitted_at ASC, creation_task_id ASC",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
    }

    async fn update_task(
        &self,
        creation_task_id: &str,
        params: UpdateCreationTaskParams<'_>,
    ) -> Result<CreationTaskRow, DbError> {
        validate_creation_task_id(creation_task_id)?;
        let mut tx = self.pool.begin().await?;
        let existing = sqlx::query_as::<_, CreationTaskDbRow>(
            "SELECT * FROM creation_tasks WHERE creation_task_id = ?",
        )
            .bind(creation_task_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| {
                DbError::NotFound(format!("creation task '{creation_task_id}' not found"))
            })?
            .try_into()?;

        let mut m = merge_update_fields(&existing, &params);
        m.result_asset_ids = canonicalize_result_asset_ids(&m.result_asset_ids)?;

        let result = sqlx::query(
            "UPDATE creation_tasks SET status = ?, error = ?, result_asset_ids = ?, remote_task_id = ?, \
             attempt = ?, started_at = ?, finished_at = ? WHERE creation_task_id = ?",
        )
        .bind(&m.status)
        .bind(&m.error)
        .bind(&m.result_asset_ids)
        .bind(&m.remote_task_id)
        .bind(m.attempt)
        .bind(m.started_at)
        .bind(m.finished_at)
        .bind(creation_task_id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(DbError::NotFound(format!(
                "creation task '{creation_task_id}' not found"
            )));
        }
        tx.commit().await?;

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

    async fn update_task_if_live(
        &self,
        creation_task_id: &str,
        params: UpdateCreationTaskParams<'_>,
    ) -> Result<bool, DbError> {
        validate_creation_task_id(creation_task_id)?;
        let mut tx = self.pool.begin().await?;
        let Some(existing) = sqlx::query_as::<_, CreationTaskDbRow>(
            "SELECT * FROM creation_tasks WHERE creation_task_id = ?",
        )
        .bind(creation_task_id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(false); // unknown id → treat as "not live"
        };
        let existing: CreationTaskRow = existing.try_into()?;
        let mut m = merge_update_fields(&existing, &params);
        m.result_asset_ids = canonicalize_result_asset_ids(&m.result_asset_ids)?;

        // The `WHERE ... status IN ('queued','running')` predicate is the
        // compare-and-set: if a concurrent cancel wrote a terminal status
        // between our read and this write, zero rows match and we do not
        // overwrite it.
        let res = sqlx::query(
            "UPDATE creation_tasks SET status = ?, error = ?, result_asset_ids = ?, remote_task_id = ?, \
             attempt = ?, started_at = ?, finished_at = ? \
             WHERE creation_task_id = ? AND status IN ('queued', 'running')",
        )
        .bind(&m.status)
        .bind(&m.error)
        .bind(&m.result_asset_ids)
        .bind(&m.remote_task_id)
        .bind(m.attempt)
        .bind(m.started_at)
        .bind(m.finished_at)
        .bind(creation_task_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(res.rows_affected() > 0)
    }

    async fn set_remote_task_id_if_live(
        &self,
        creation_task_id: &str,
        remote_task_id: &str,
    ) -> Result<bool, DbError> {
        validate_creation_task_id(creation_task_id)?;
        let result = sqlx::query(
            "UPDATE creation_tasks SET remote_task_id = ? \
             WHERE creation_task_id = ? AND status IN ('queued', 'running')",
        )
        .bind(remote_task_id)
        .bind(creation_task_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn list_live_tasks(&self) -> Result<Vec<CreationTaskRow>, DbError> {
        let rows = sqlx::query_as::<_, CreationTaskDbRow>(
            "SELECT * FROM creation_tasks WHERE status IN ('queued', 'running') ORDER BY submitted_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;
    use nomifun_common::{WorkshopAssetId, WorkshopCanvasId, generate_id};
    use std::sync::Arc;

    async fn repo() -> (SqliteCreationTaskRepository, crate::Database, String) {
        let db = init_database_memory().await.unwrap();
        let provider_id = ProviderId::new().into_string();
        sqlx::query(
            "INSERT INTO providers \
                (provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
                 capabilities, created_at, updated_at) \
             VALUES (?, 'openai', 'Creation Test Provider', \
                 'https://example.invalid', 'encrypted', '[]', 1, '[]', 0, 0)",
        )
        .bind(&provider_id)
        .execute(db.pool())
        .await
        .unwrap();
        let repo = SqliteCreationTaskRepository::new(db.pool().clone());
        (repo, db, provider_id)
    }

    fn create_params<'a>(
        creation_task_id: &'a str,
        canvas: Option<&'a str>,
        provider_id: &'a str,
    ) -> CreateCreationTaskParams<'a> {
        CreateCreationTaskParams {
            creation_task_id,
            canvas_id: canvas,
            node_id: None,
            provider_id,
            model: "m",
            capability: "t2i",
            params: r#"{"prompt":"cat"}"#,
            status: "queued",
            submitted_at: 100,
        }
    }

    #[tokio::test]
    async fn create_get_and_update_flow() {
        let (repo, _db, provider_id) = repo().await;
        let creation_task_id = generate_id();
        let t = repo
            .create_task(create_params(&creation_task_id, None, &provider_id))
            .await
            .unwrap();
        assert_eq!(t.creation_task_id, creation_task_id);
        assert_eq!(t.status, "queued");
        assert_eq!(t.result_asset_ids, "[]");
        assert_eq!(t.attempt, 0);

        // M0 shape: immediately fail with adapter_unavailable.
        let failed = repo
            .update_task(
                &creation_task_id,
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

        let missing_id = generate_id();
        assert!(matches!(
            repo.update_task(&missing_id, UpdateCreationTaskParams::default()).await.unwrap_err(),
            DbError::NotFound(_)
        ));
    }

    #[test]
    fn creation_task_business_id_rejects_non_uuidv7_boundaries() {
        for invalid in [
            "1",
            "task_0190f5fe-7c00-7a00-8000-000000000001",
            "0190f5fe-7c00-4a00-8000-000000000001",
            "0190F5FE-7C00-7A00-8000-000000000001",
            "0190f5fe7c007a008000000000000001",
            "0190f5fe-7c00-7a00-8000-000000000001 ",
        ] {
            assert!(validate_uuidv7(invalid).is_err());
            assert!(matches!(
                validate_creation_task_id(invalid),
                Err(DbError::Conflict(message)) if message.contains("canonical UUIDv7")
            ));
        }
        validate_creation_task_id("0190f5fe-7c00-7a00-8000-000000000001").unwrap();
    }

    #[tokio::test]
    async fn result_asset_ids_are_structural_logical_references() {
        let (repo, _db, provider_id) = repo().await;
        let creation_task_id = generate_id();
        repo.create_task(create_params(&creation_task_id, None, &provider_id))
            .await
            .unwrap();
        let asset_id = WorkshopAssetId::new().into_string();
        let ids_json = serde_json::to_string(&[asset_id.as_str()]).unwrap();

        // The task repository canonicalizes the JSON/UUIDv7 shape but does not
        // emulate a physical FK into workshop_assets. Existence, task ownership,
        // and file locatability are audited by CreationService + AssetSink.
        let updated = repo
            .update_task(
                &creation_task_id,
                UpdateCreationTaskParams {
                    result_asset_ids: Some(&ids_json),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&updated.result_asset_ids).unwrap(),
            vec![asset_id.clone()]
        );

        let duplicate_json = serde_json::to_string(&[asset_id.as_str(), asset_id.as_str()]).unwrap();
        assert!(matches!(
            repo.update_task(
                &creation_task_id,
                UpdateCreationTaskParams {
                    result_asset_ids: Some(&duplicate_json),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err(),
            DbError::Conflict(message) if message.contains("duplicate asset")
        ));
    }

    #[tokio::test]
    async fn list_filters_and_live() {
        let (repo, db, provider_id) = repo().await;
        let canvas_ids = [
            WorkshopCanvasId::new().into_string(),
            WorkshopCanvasId::new().into_string(),
        ];
        for id in &canvas_ids {
            sqlx::query(
                "INSERT INTO workshop_canvases \
                    (canvas_id, title, node_count, created_at, updated_at) \
                 VALUES (?, ?, 0, 0, 0)",
            )
            .bind(id)
            .bind(id)
            .execute(db.pool())
            .await
            .unwrap();
        }
        let task_ids = [generate_id(), generate_id()];
        repo.create_task(create_params(&task_ids[0], Some(&canvas_ids[0]), &provider_id))
            .await
            .unwrap();
        repo.create_task(create_params(&task_ids[1], Some(&canvas_ids[1]), &provider_id))
            .await
            .unwrap();
        repo.update_task(&task_ids[1], UpdateCreationTaskParams { status: Some("running"), ..Default::default() })
            .await
            .unwrap();

        // canvas filter
        let list = repo
            .list_tasks(ListCreationTasksParams { canvas_id: Some(&canvas_ids[0]), limit: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].creation_task_id, task_ids[0]);

        // status filter
        let list = repo
            .list_tasks(ListCreationTasksParams { status: Some("running"), limit: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].creation_task_id, task_ids[1]);

        // both queued+running are "live"
        let live = repo.list_live_tasks().await.unwrap();
        assert_eq!(live.len(), 2);
        assert_eq!(repo.list_all_tasks().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn update_task_if_live_refuses_terminal_overwrite() {
        let (repo, _db, provider_id) = repo().await;
        let canceled_id = generate_id();
        repo.create_task(create_params(&canceled_id, None, &provider_id))
            .await
            .unwrap();
        // queued → running (still live)
        repo.update_task(&canceled_id, UpdateCreationTaskParams { status: Some("running"), ..Default::default() })
            .await
            .unwrap();
        // A cancel writes the terminal status (cancel path is unconditional).
        repo.update_task(
            &canceled_id,
            UpdateCreationTaskParams { status: Some("canceled"), finished_at: Some(Some(1)), ..Default::default() },
        )
        .await
        .unwrap();
        // finalize's terminal write must NOT overwrite the canceled row.
        let applied = repo
            .update_task_if_live(
                &canceled_id,
                UpdateCreationTaskParams { status: Some("succeeded"), finished_at: Some(Some(2)), ..Default::default() },
            )
            .await
            .unwrap();
        assert!(!applied, "terminal (canceled) row must not be overwritten");
        assert_eq!(repo.get_task(&canceled_id).await.unwrap().unwrap().status, "canceled");

        // A still-live task IS updated by the conditional write.
        let succeeded_id = generate_id();
        repo.create_task(create_params(&succeeded_id, None, &provider_id))
            .await
            .unwrap();
        let applied2 = repo
            .update_task_if_live(&succeeded_id, UpdateCreationTaskParams { status: Some("succeeded"), ..Default::default() })
            .await
            .unwrap();
        assert!(applied2);
        assert_eq!(repo.get_task(&succeeded_id).await.unwrap().unwrap().status, "succeeded");

        // Unknown id → Ok(false), no error.
        let missing_id = generate_id();
        let applied3 = repo
            .update_task_if_live(&missing_id, UpdateCreationTaskParams { status: Some("failed"), ..Default::default() })
            .await
            .unwrap();
        assert!(!applied3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_id_patch_racing_cancel_never_resurrects_task() {
        let (repo, _db, provider_id) = repo().await;
        let repo = Arc::new(repo);
        for _ in 0..64 {
            let creation_task_id = generate_id();
            repo.create_task(create_params(&creation_task_id, None, &provider_id))
                .await
                .unwrap();
            repo.update_task(
                &creation_task_id,
                UpdateCreationTaskParams {
                    status: Some("running"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

            let cancel_repo = repo.clone();
            let cancel_id = creation_task_id.clone();
            let cancel = tokio::spawn(async move {
                cancel_repo
                    .update_task(
                        &cancel_id,
                        UpdateCreationTaskParams {
                            status: Some("canceled"),
                            finished_at: Some(Some(1)),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
            });
            let remote_repo = repo.clone();
            let remote_id = creation_task_id.clone();
            let remote = tokio::spawn(async move {
                remote_repo
                    .set_remote_task_id_if_live(&remote_id, "remote-race")
                    .await
                    .unwrap()
            });
            let (_, remote_applied) = tokio::join!(cancel, remote);
            let _ = remote_applied.unwrap();

            let row = repo.get_task(&creation_task_id).await.unwrap().unwrap();
            assert_eq!(row.status, "canceled");
            assert!(
                !repo
                    .set_remote_task_id_if_live(&creation_task_id, "remote-after-cancel")
                    .await
                    .unwrap(),
                "terminal cancel must make subsequent remote patches no-op"
            );
            assert_eq!(
                repo.get_task(&creation_task_id).await.unwrap().unwrap().status,
                "canceled"
            );
        }
    }

    #[tokio::test]
    async fn create_task_rejects_missing_provider_atomically() {
        let (repo, db, _provider_id) = repo().await;
        let missing_provider = ProviderId::new().into_string();
        let creation_task_id = generate_id();

        let error = repo
            .create_task(create_params(&creation_task_id, None, &missing_provider))
            .await
            .unwrap_err();
        assert!(matches!(error, DbError::Conflict(_)));

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM creation_tasks")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}
