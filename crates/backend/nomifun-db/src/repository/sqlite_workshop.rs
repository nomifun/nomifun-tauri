use sqlx::{QueryBuilder, Sqlite, SqlitePool};
use serde_json::Value;

use crate::error::DbError;
use crate::models::{WorkshopAssetRow, WorkshopCanvasRow};
use crate::repository::IWorkshopRepository;
use crate::repository::workshop::{AssetSort, ListAssetsParams, UpdateAssetParams};

/// SQLite-backed implementation of [`IWorkshopRepository`].
#[derive(Clone, Debug)]
pub struct SqliteWorkshopRepository {
    pool: SqlitePool,
}

/// Map a [`AssetSort`] to its ORDER BY clause. The strings are fixed literals
/// (never user input), each with an `id` tiebreaker for a stable total order.
fn order_by_sql(sort: AssetSort) -> &'static str {
    match sort {
        AssetSort::CreatedDesc => "created_at DESC, id DESC",
        AssetSort::CreatedAsc => "created_at ASC, id ASC",
        AssetSort::UpdatedDesc => "updated_at DESC, id DESC",
        AssetSort::TitleAsc => "title COLLATE NOCASE ASC, id DESC",
        AssetSort::SizeDesc => "bytes DESC, id DESC",
    }
}

impl SqliteWorkshopRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

struct OriginReferences {
    provider_id: Option<String>,
    canvas_id: Option<String>,
    creation_task_id: Option<String>,
}

fn optional_origin_id(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, DbError> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => {
            nomifun_common::validate_uuidv7(value).map_err(|error| {
                DbError::Conflict(format!(
                    "workshop asset origin.{key} is not a canonical UUIDv7: {error}"
                ))
            })?;
            Ok(Some(value.clone()))
        }
        Some(Value::Null) => Err(DbError::Conflict(format!(
            "workshop asset origin.{key} must be omitted when absent; JSON null is not valid"
        ))),
        Some(_) => Err(DbError::Conflict(format!(
            "workshop asset origin.{key} must be a canonical UUIDv7 string"
        ))),
    }
}

fn origin_references(origin: Option<&str>) -> Result<OriginReferences, DbError> {
    let Some(origin) = origin else {
        return Ok(OriginReferences {
            provider_id: None,
            canvas_id: None,
            creation_task_id: None,
        });
    };
    let value: Value = serde_json::from_str(origin)
        .map_err(|error| DbError::Conflict(format!("invalid workshop asset origin JSON: {error}")))?;
    let object = value.as_object().ok_or_else(|| {
        DbError::Conflict("workshop asset origin must be a JSON object".into())
    })?;
    for retired_key in [
        "task_id",
        "providerId",
        "canvasId",
        "nodeId",
        "creationTaskId",
    ] {
        if object.contains_key(retired_key) {
            return Err(DbError::Conflict(format!(
                "workshop asset origin contains unsupported ID field {retired_key:?}"
            )));
        }
    }
    let provider_id = optional_origin_id(object, "provider_id")?;
    let canvas_id = optional_origin_id(object, "canvas_id")?;
    let _node_id = optional_origin_id(object, "node_id")?;
    let creation_task_id = optional_origin_id(object, "creation_task_id")?;
    Ok(OriginReferences {
        provider_id,
        canvas_id,
        creation_task_id,
    })
}

fn validate_asset_row(row: &WorkshopAssetRow) -> Result<(), DbError> {
    nomifun_common::WorkshopAssetId::parse(&row.asset_id).map_err(|error| {
        DbError::Conflict(format!(
            "workshop asset asset_id {:?} is not a canonical UUIDv7: {error}",
            row.asset_id
        ))
    })?;
    origin_references(row.origin.as_deref()).map_err(|error| {
        DbError::Conflict(format!(
            "workshop asset {} has invalid durable origin: {error}",
            row.asset_id
        ))
    })?;
    Ok(())
}

fn validate_asset_rows(rows: &[WorkshopAssetRow]) -> Result<(), DbError> {
    for row in rows {
        validate_asset_row(row)?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl IWorkshopRepository for SqliteWorkshopRepository {
    async fn provider_exists(&self, provider_id: &str) -> Result<bool, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM providers WHERE provider_id = ?)",
        )
        .bind(provider_id)
        .fetch_one(&self.pool)
        .await?)
    }

    // ---- canvases ----

    async fn list_canvases(&self) -> Result<Vec<WorkshopCanvasRow>, DbError> {
        let rows = sqlx::query_as::<_, WorkshopCanvasRow>(
            "SELECT * FROM workshop_canvases ORDER BY updated_at DESC, id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_canvas(&self, id: &str) -> Result<Option<WorkshopCanvasRow>, DbError> {
        let row = sqlx::query_as::<_, WorkshopCanvasRow>(
            "SELECT * FROM workshop_canvases WHERE canvas_id = ?",
        )
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn create_canvas(&self, id: &str, title: &str, now: i64) -> Result<WorkshopCanvasRow, DbError> {
        let row_id: i64 = sqlx::query_scalar(
            "INSERT INTO workshop_canvases \
                (canvas_id, title, thumbnail_rel_path, node_count, created_at, updated_at) \
             VALUES (?, ?, NULL, 0, ?, ?) RETURNING id",
        )
        .bind(id)
        .bind(title)
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(WorkshopCanvasRow {
            id: row_id,
            canvas_id: id.to_string(),
            title: title.to_string(),
            thumbnail_rel_path: None,
            node_count: 0,
            created_at: now,
            updated_at: now,
        })
    }

    async fn rename_canvas(&self, id: &str, title: &str, now: i64) -> Result<WorkshopCanvasRow, DbError> {
        let result = sqlx::query(
            "UPDATE workshop_canvases SET title = ?, updated_at = ? WHERE canvas_id = ?",
        )
            .bind(title)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("workshop canvas '{id}' not found")));
        }
        self.get_canvas(id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("workshop canvas '{id}' not found")))
    }

    async fn touch_canvas(&self, id: &str, node_count: i64, now: i64) -> Result<WorkshopCanvasRow, DbError> {
        let result = sqlx::query(
            "UPDATE workshop_canvases SET node_count = ?, updated_at = ? WHERE canvas_id = ?",
        )
            .bind(node_count)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("workshop canvas '{id}' not found")));
        }
        self.get_canvas(id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("workshop canvas '{id}' not found")))
    }

    async fn delete_canvas(&self, id: &str) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE workshop_canvases SET updated_at = updated_at WHERE canvas_id = ?",
        )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("workshop canvas '{id}' not found")));
        }
        sqlx::query("UPDATE creation_tasks SET canvas_id = NULL WHERE canvas_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // workshop_assets.origin.canvas_id is immutable provenance with the
        // registry's KEEP_HISTORY policy. It intentionally retains the former
        // business ID after the Canvas row is deleted.
        sqlx::query("DELETE FROM workshop_canvases WHERE canvas_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn set_canvas_thumbnail(
        &self,
        id: &str,
        thumbnail_rel_path: &str,
        now: i64,
    ) -> Result<WorkshopCanvasRow, DbError> {
        let result = sqlx::query(
            "UPDATE workshop_canvases SET thumbnail_rel_path = ?, updated_at = ? WHERE canvas_id = ?",
        )
            .bind(thumbnail_rel_path)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("workshop canvas '{id}' not found")));
        }
        self.get_canvas(id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("workshop canvas '{id}' not found")))
    }

    // ---- assets ----

    async fn create_asset(&self, row: &WorkshopAssetRow) -> Result<WorkshopAssetRow, DbError> {
        validate_asset_row(row)?;
        let references = origin_references(row.origin.as_deref())?;
        let mut tx = self.pool.begin().await?;
        if let Some(provider_id) = references.provider_id {
            let locked = sqlx::query(
                "UPDATE providers SET updated_at = updated_at WHERE provider_id = ?",
            )
            .bind(&provider_id)
            .execute(&mut *tx)
            .await?;
            if locked.rows_affected() == 0 {
                return Err(DbError::Conflict(format!(
                    "workshop asset origin references missing provider '{provider_id}'"
                )));
            }
        }
        if let Some(canvas_id) = references.canvas_id {
            let locked = sqlx::query(
                "UPDATE workshop_canvases SET updated_at = updated_at WHERE canvas_id = ?",
            )
            .bind(&canvas_id)
            .execute(&mut *tx)
            .await?;
            if locked.rows_affected() == 0 {
                return Err(DbError::Conflict(format!(
                    "workshop asset origin references missing canvas '{canvas_id}'"
                )));
            }
        }
        if let Some(creation_task_id) = references.creation_task_id {
            let task = sqlx::query(
                "UPDATE creation_tasks SET status = status WHERE creation_task_id = ?",
            )
            .bind(&creation_task_id)
            .execute(&mut *tx)
            .await?;
            if task.rows_affected() == 0 {
                return Err(DbError::Conflict(format!(
                    "workshop asset origin references missing creation task '{creation_task_id}'"
                )));
            }
        }
        sqlx::query(
            "INSERT INTO workshop_assets \
                (asset_id, kind, title, collection, tags, rel_path, thumb_rel_path, mime, width, height, bytes, \
                 text_content, in_library, origin, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.asset_id)
        .bind(&row.kind)
        .bind(&row.title)
        .bind(&row.collection)
        .bind(&row.tags)
        .bind(&row.rel_path)
        .bind(&row.thumb_rel_path)
        .bind(&row.mime)
        .bind(row.width)
        .bind(row.height)
        .bind(row.bytes)
        .bind(&row.text_content)
        .bind(row.in_library)
        .bind(&row.origin)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row.clone())
    }

    async fn get_asset(&self, id: &str) -> Result<Option<WorkshopAssetRow>, DbError> {
        let row = sqlx::query_as::<_, WorkshopAssetRow>(
            "SELECT * FROM workshop_assets WHERE asset_id = ?",
        )
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        if let Some(row) = &row {
            validate_asset_row(row)?;
        }
        Ok(row)
    }

    async fn list_all_assets(&self) -> Result<Vec<WorkshopAssetRow>, DbError> {
        let rows = sqlx::query_as::<_, WorkshopAssetRow>("SELECT * FROM workshop_assets")
            .fetch_all(&self.pool)
            .await?;
        validate_asset_rows(&rows)?;
        Ok(rows)
    }

    async fn list_assets(&self, params: ListAssetsParams<'_>) -> Result<(Vec<WorkshopAssetRow>, i64), DbError> {
        // Shared WHERE assembly for both the COUNT and the page query.
        fn push_filters<'a>(qb: &mut QueryBuilder<'a, Sqlite>, p: &ListAssetsParams<'a>) {
            let mut first = true;
            let mut clause = |qb: &mut QueryBuilder<'a, Sqlite>| {
                qb.push(if first { " WHERE " } else { " AND " });
                first = false;
            };
            if let Some(kind) = p.kind {
                clause(qb);
                qb.push("kind = ").push_bind(kind);
            }
            if let Some(collection) = p.collection {
                clause(qb);
                qb.push("collection = ").push_bind(collection);
            }
            if p.ungrouped {
                clause(qb);
                qb.push("(collection IS NULL OR collection = '')");
            }
            if let Some(q) = p.q {
                clause(qb);
                qb.push("LOWER(title) LIKE ").push_bind(format!("%{}%", q.to_lowercase()));
            }
            if let Some(tag) = p.tag {
                clause(qb);
                // Match one entry of the JSON `tags` array (stored as e.g.
                // `["人物","场景"]`) via a case-sensitive substring search for the
                // quoted needle `"tag"`. `instr` (unlike LIKE) is case-sensitive
                // and treats `%`/`_` literally, so no metachar escaping is needed.
                qb.push("instr(tags, ").push_bind(format!("\"{tag}\"")).push(") > 0");
            }
            if let Some(in_library) = p.in_library {
                clause(qb);
                qb.push("in_library = ").push_bind(in_library);
            }
        }

        let mut count_qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT COUNT(*) FROM workshop_assets");
        push_filters(&mut count_qb, &params);
        let total: i64 = count_qb.build_query_scalar().fetch_one(&self.pool).await?;

        let page = params.page.max(1);
        let page_size = params.page_size.clamp(1, 200);
        let offset = (page - 1) * page_size;

        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT * FROM workshop_assets");
        push_filters(&mut qb, &params);
        // ORDER BY is a fixed static clause chosen from a closed enum (never
        // user text), so pushing it verbatim is injection-safe. Every variant
        // carries an `id` tiebreaker for a stable total order.
        qb.push(" ORDER BY ")
            .push(order_by_sql(params.sort))
            .push(" LIMIT ")
            .push_bind(page_size)
            .push(" OFFSET ")
            .push_bind(offset);
        let items = qb.build_query_as::<WorkshopAssetRow>().fetch_all(&self.pool).await?;
        validate_asset_rows(&items)?;

        Ok((items, total))
    }

    async fn update_asset(
        &self,
        id: &str,
        params: UpdateAssetParams<'_>,
        now: i64,
    ) -> Result<WorkshopAssetRow, DbError> {
        let existing = self
            .get_asset(id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("workshop asset '{id}' not found")))?;

        let title = params.title.unwrap_or(&existing.title).to_string();
        let collection = match params.collection {
            Some(c) => c.map(str::to_string),
            None => existing.collection.clone(),
        };
        let tags = params.tags.unwrap_or(&existing.tags).to_string();
        let in_library = params.in_library.unwrap_or(existing.in_library);

        sqlx::query(
            "UPDATE workshop_assets SET title = ?, collection = ?, tags = ?, in_library = ?, updated_at = ? \
             WHERE asset_id = ?",
        )
        .bind(&title)
        .bind(&collection)
        .bind(&tags)
        .bind(in_library)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(WorkshopAssetRow {
            title,
            collection,
            tags,
            in_library,
            updated_at: now,
            ..existing
        })
    }

    async fn set_asset_thumb(&self, id: &str, thumb_rel_path: &str, now: i64) -> Result<(), DbError> {
        let result = sqlx::query(
            "UPDATE workshop_assets SET thumb_rel_path = ?, updated_at = ? WHERE asset_id = ?",
        )
            .bind(thumb_rel_path)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("workshop asset '{id}' not found")));
        }
        Ok(())
    }

    async fn delete_asset(&self, id: &str) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE workshop_assets SET updated_at = updated_at WHERE asset_id = ?",
        )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("workshop asset '{id}' not found")));
        }
        let referencing_tasks: Vec<(i64, String)> = sqlx::query_as(
            "SELECT DISTINCT task.id, task.result_asset_ids \
             FROM creation_tasks task, json_each(task.result_asset_ids) item \
             WHERE item.value = ?",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        for (task_id, encoded) in referencing_tasks {
            let mut asset_ids: Vec<String> = serde_json::from_str(&encoded).map_err(|error| {
                DbError::Conflict(format!(
                    "creation task '{task_id}' has invalid result_asset_ids: {error}"
                ))
            })?;
            asset_ids.retain(|asset_id| asset_id != id);
            let encoded = serde_json::to_string(&asset_ids).map_err(|error| {
                DbError::Init(format!(
                    "failed to encode creation task '{task_id}' result_asset_ids: {error}"
                ))
            })?;
            sqlx::query("UPDATE creation_tasks SET result_asset_ids = ? WHERE id = ?")
                .bind(encoded)
                .bind(task_id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM workshop_assets WHERE asset_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn rename_collection(&self, from: &str, to: Option<&str>, now: i64) -> Result<u64, DbError> {
        let result = sqlx::query("UPDATE workshop_assets SET collection = ?, updated_at = ? WHERE collection = ?")
            .bind(to)
            .bind(now)
            .bind(from)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    const CANVAS_A: &str = "0190f5fe-7c00-7a00-8abc-000000000001";
    const CANVAS_1: &str = "0190f5fe-7c00-7a00-8abc-000000000002";
    const CANVAS_2: &str = "0190f5fe-7c00-7a00-8abc-000000000003";

    const ASSET_1: &str = "0190f5fe-7c00-7a00-8abc-000000000101";
    const ASSET_2: &str = "0190f5fe-7c00-7a00-8abc-000000000102";
    const ASSET_3: &str = "0190f5fe-7c00-7a00-8abc-000000000103";
    const ASSET_NULL: &str = "0190f5fe-7c00-7a00-8abc-000000000111";
    const ASSET_EMPTY: &str = "0190f5fe-7c00-7a00-8abc-000000000112";
    const ASSET_GRP: &str = "0190f5fe-7c00-7a00-8abc-000000000113";
    const ASSET_X: &str = "0190f5fe-7c00-7a00-8abc-000000000121";
    const ASSET_T: &str = "0190f5fe-7c00-7a00-8abc-000000000131";
    const ASSET_TA: &str = "0190f5fe-7c00-7a00-8abc-000000000141";
    const ASSET_TB: &str = "0190f5fe-7c00-7a00-8abc-000000000142";
    const ASSET_S1: &str = "0190f5fe-7c00-7a00-8abc-000000000151";
    const ASSET_S2: &str = "0190f5fe-7c00-7a00-8abc-000000000152";
    const ASSET_S3: &str = "0190f5fe-7c00-7a00-8abc-000000000153";
    const ASSET_C1: &str = "0190f5fe-7c00-7a00-8abc-000000000161";
    const ASSET_C2: &str = "0190f5fe-7c00-7a00-8abc-000000000162";
    const ASSET_C3: &str = "0190f5fe-7c00-7a00-8abc-000000000163";

    async fn repo() -> (SqliteWorkshopRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteWorkshopRepository::new(db.pool().clone());
        (repo, db)
    }

    fn sample_asset(id: i64, asset_id: &str, kind: &str, title: &str) -> WorkshopAssetRow {
        WorkshopAssetRow {
            id,
            asset_id: asset_id.to_string(),
            kind: kind.to_string(),
            title: title.to_string(),
            collection: None,
            tags: "[]".to_string(),
            rel_path: Some(format!("workshop/assets/{asset_id}.png")),
            thumb_rel_path: None,
            mime: Some("image/png".to_string()),
            width: Some(10),
            height: Some(20),
            bytes: Some(123),
            text_content: None,
            in_library: true,
            origin: None,
            created_at: 1000,
            updated_at: 1000,
        }
    }

    #[tokio::test]
    async fn canvas_crud_flow() {
        let (repo, _db) = repo().await;
        let c = repo.create_canvas(CANVAS_A, "画布", 1).await.unwrap();
        assert!(c.id > 0);
        assert_eq!(c.canvas_id, CANVAS_A);
        assert_eq!(c.node_count, 0);
        assert_eq!(c.title, "画布");

        let renamed = repo.rename_canvas(CANVAS_A, "新名", 2).await.unwrap();
        assert_eq!(renamed.title, "新名");
        assert_eq!(renamed.updated_at, 2);

        let touched = repo.touch_canvas(CANVAS_A, 7, 3).await.unwrap();
        assert_eq!(touched.node_count, 7);
        assert_eq!(touched.updated_at, 3);

        assert_eq!(repo.list_canvases().await.unwrap().len(), 1);
        repo.delete_canvas(CANVAS_A).await.unwrap();
        assert!(repo.get_canvas(CANVAS_A).await.unwrap().is_none());
        assert!(matches!(repo.delete_canvas(CANVAS_A).await.unwrap_err(), DbError::NotFound(_)));
        assert!(matches!(repo.rename_canvas("nope", "x", 1).await.unwrap_err(), DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_canvases_orders_by_updated_desc() {
        let (repo, _db) = repo().await;
        repo.create_canvas(CANVAS_1, "a", 100).await.unwrap();
        repo.create_canvas(CANVAS_2, "b", 200).await.unwrap();
        let all = repo.list_canvases().await.unwrap();
        assert_eq!(all[0].id, 2);
        assert_eq!(all[0].canvas_id, CANVAS_2);
        assert_eq!(all[1].id, 1);
        assert_eq!(all[1].canvas_id, CANVAS_1);
    }

    #[tokio::test]
    async fn asset_crud_and_filters() {
        let (repo, _db) = repo().await;
        repo.create_asset(&sample_asset(1, ASSET_1, "image", "红色卖点图")).await.unwrap();
        repo.create_asset(&sample_asset(2, ASSET_2, "video", "宣传视频")).await.unwrap();
        let mut text = sample_asset(3, ASSET_3, "text", "描述");
        text.rel_path = None;
        text.in_library = false;
        repo.create_asset(&text).await.unwrap();

        // no filter → all 3
        let (items, total) = repo
            .list_assets(ListAssetsParams { page: 1, page_size: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 3);
        assert_eq!(items.len(), 3);

        // kind filter
        let (items, total) = repo
            .list_assets(ListAssetsParams { kind: Some("image"), page: 1, page_size: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(items[0].id, 1);
        assert_eq!(items[0].asset_id, ASSET_1);

        // in_library filter
        let (_, total) = repo
            .list_assets(ListAssetsParams { in_library: Some(false), page: 1, page_size: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 1);

        // substring q filter (case-insensitive)
        let (_, total) = repo
            .list_assets(ListAssetsParams { q: Some("视频"), page: 1, page_size: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 1);

        // pagination: page 1 size 2 → 2 of 3
        let (items, total) = repo
            .list_assets(ListAssetsParams { page: 1, page_size: 2, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 3);
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn asset_origin_uses_creation_task_business_id_only() {
        let (repo, db) = repo().await;
        let provider_id = nomifun_common::generate_id();
        sqlx::query(
            "INSERT INTO providers \
             (provider_id, platform, name, base_url, api_key_encrypted, created_at, updated_at) \
             VALUES (?, 'test', 'origin provider', 'https://example.invalid', '', 1, 1)",
        )
        .bind(&provider_id)
        .execute(db.pool())
        .await
        .unwrap();
        let creation_task_id = nomifun_common::generate_id();
        sqlx::query(
            "INSERT INTO creation_tasks \
             (creation_task_id, provider_id, model, capability, params, status, submitted_at) \
             VALUES (?, ?, 'model', 'image', '{}', 'succeeded', 1)",
        )
        .bind(&creation_task_id)
        .bind(&provider_id)
        .execute(db.pool())
        .await
        .unwrap();

        let mut valid = sample_asset(1, ASSET_1, "image", "business origin");
        valid.origin =
            Some(serde_json::json!({ "creation_task_id": creation_task_id.clone() }).to_string());
        let created = repo.create_asset(&valid).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(created.origin.as_deref().unwrap())
                .unwrap()["creation_task_id"],
            creation_task_id
        );

        let mut missing_parent = sample_asset(2, ASSET_2, "image", "missing task");
        missing_parent.origin = Some(
            serde_json::json!({ "creation_task_id": nomifun_common::generate_id() })
                .to_string(),
        );
        assert!(
            matches!(
                repo.create_asset(&missing_parent).await,
                Err(DbError::Conflict(message))
                    if message.contains("references missing creation task")
            ),
            "origin.creation_task_id must resolve through creation_tasks.creation_task_id"
        );

        for (label, origin) in [
            ("unsupported task_id integer", serde_json::json!({ "task_id": 1 })),
            (
                "unsupported task_id numeric string",
                serde_json::json!({ "task_id": "1" }),
            ),
            (
                "unsupported task_id UUIDv7",
                serde_json::json!({ "task_id": nomifun_common::generate_id() }),
            ),
            (
                "explicit null canvas_id",
                serde_json::json!({ "canvas_id": null }),
            ),
            (
                "explicit null node_id",
                serde_json::json!({ "node_id": null }),
            ),
            (
                "camel-case canvasId",
                serde_json::json!({ "canvasId": nomifun_common::generate_id() }),
            ),
            (
                "integer creation_task_id",
                serde_json::json!({ "creation_task_id": 1 }),
            ),
            (
                "numeric-string creation_task_id",
                serde_json::json!({ "creation_task_id": "1" }),
            ),
            (
                "prefixed creation_task_id",
                serde_json::json!({
                    "creation_task_id": format!("task_{}", nomifun_common::generate_id())
                }),
            ),
            (
                "uuidv4 creation_task_id",
                serde_json::json!({
                    "creation_task_id": "550e8400-e29b-41d4-a716-446655440000"
                }),
            ),
            (
                "uppercase creation_task_id",
                serde_json::json!({
                    "creation_task_id": nomifun_common::generate_id().to_ascii_uppercase()
                }),
            ),
        ] {
            let invalid_asset_id = nomifun_common::generate_id();
            let mut invalid = sample_asset(2, &invalid_asset_id, "image", label);
            invalid.origin = Some(origin.to_string());
            assert!(
                repo.create_asset(&invalid).await.is_err(),
                "{label} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn list_assets_ungrouped_filter() {
        let (repo, _db) = repo().await;
        // Two ungrouped (NULL and empty-string collection) + one grouped.
        repo.create_asset(&sample_asset(1, ASSET_NULL, "image", "no collection")).await.unwrap();
        let mut empty = sample_asset(2, ASSET_EMPTY, "image", "empty collection");
        empty.collection = Some(String::new());
        repo.create_asset(&empty).await.unwrap();
        let mut grouped = sample_asset(3, ASSET_GRP, "image", "grouped");
        grouped.collection = Some("角色".to_string());
        repo.create_asset(&grouped).await.unwrap();

        // ungrouped=true → the NULL + empty-string rows only.
        let (items, total) = repo
            .list_assets(ListAssetsParams { ungrouped: true, page: 1, page_size: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 2);
        let ids: std::collections::BTreeSet<&str> = items.iter().map(|a| a.asset_id.as_str()).collect();
        assert!(ids.contains(ASSET_NULL) && ids.contains(ASSET_EMPTY));
        assert!(!ids.contains(ASSET_GRP));

        // named collection filter still returns only the grouped row.
        let (_, total) = repo
            .list_assets(ListAssetsParams { collection: Some("角色"), page: 1, page_size: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 1);

        // ungrouped composes with other filters (kind).
        let (_, total) = repo
            .list_assets(ListAssetsParams {
                ungrouped: true,
                kind: Some("image"),
                page: 1,
                page_size: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(total, 2);
    }

    #[tokio::test]
    async fn asset_update_partial_and_delete() {
        let (repo, _db) = repo().await;
        repo.create_asset(&sample_asset(1, ASSET_X, "image", "old")).await.unwrap();
        let updated = repo
            .update_asset(
                ASSET_X,
                UpdateAssetParams {
                    title: Some("new"),
                    collection: Some(Some("角色")),
                    in_library: Some(false),
                    ..Default::default()
                },
                2000,
            )
            .await
            .unwrap();
        assert_eq!(updated.title, "new");
        assert_eq!(updated.collection.as_deref(), Some("角色"));
        assert!(!updated.in_library);
        assert_eq!(updated.updated_at, 2000);
        // unchanged field preserved
        assert_eq!(updated.mime.as_deref(), Some("image/png"));

        repo.delete_asset(ASSET_X).await.unwrap();
        assert!(repo.get_asset(ASSET_X).await.unwrap().is_none());
        assert!(matches!(repo.delete_asset(ASSET_X).await.unwrap_err(), DbError::NotFound(_)));
        assert!(matches!(
            repo.update_asset("nope", UpdateAssetParams::default(), 1).await.unwrap_err(),
            DbError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn set_canvas_thumbnail_and_asset_thumb() {
        let (repo, _db) = repo().await;
        repo.create_canvas(CANVAS_A, "画布", 1).await.unwrap();
        let thumbnail = format!("workshop/canvases/{CANVAS_A}/thumb.jpg");
        let c = repo.set_canvas_thumbnail(CANVAS_A, &thumbnail, 5).await.unwrap();
        assert_eq!(c.thumbnail_rel_path.as_deref(), Some(thumbnail.as_str()));
        assert_eq!(c.updated_at, 5);
        assert!(matches!(
            repo.set_canvas_thumbnail("nope", "x", 1).await.unwrap_err(),
            DbError::NotFound(_)
        ));

        repo.create_asset(&sample_asset(1, ASSET_T, "image", "img")).await.unwrap();
        let thumb = format!("workshop/assets/thumbs/{ASSET_T}.jpg");
        repo.set_asset_thumb(ASSET_T, &thumb, 6).await.unwrap();
        let a = repo.get_asset(ASSET_T).await.unwrap().unwrap();
        assert_eq!(a.thumb_rel_path.as_deref(), Some(thumb.as_str()));
        assert!(matches!(repo.set_asset_thumb("nope", "x", 1).await.unwrap_err(), DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_all_assets_returns_every_row() {
        let (repo, _db) = repo().await;
        repo.create_asset(&sample_asset(1, ASSET_1, "image", "a")).await.unwrap();
        let mut internal = sample_asset(2, ASSET_2, "image", "b");
        internal.in_library = false;
        repo.create_asset(&internal).await.unwrap();
        let all = repo.list_all_assets().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn list_assets_tag_filter_exact_match() {
        let (repo, _db) = repo().await;
        let mut a = sample_asset(1, ASSET_TA, "image", "带标签");
        a.tags = r#"["人物","场景"]"#.to_string();
        repo.create_asset(&a).await.unwrap();
        let mut b = sample_asset(2, ASSET_TB, "image", "另一个");
        b.tags = r#"["场景"]"#.to_string();
        repo.create_asset(&b).await.unwrap();

        // "人物" → only the asset with ASSET_TA
        let (items, total) = repo
            .list_assets(ListAssetsParams { tag: Some("人物"), page: 1, page_size: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(items[0].id, 1);
        assert_eq!(items[0].asset_id, ASSET_TA);

        // "场景" → both
        let (_, total) = repo
            .list_assets(ListAssetsParams { tag: Some("场景"), page: 1, page_size: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 2);

        // exact match: a partial "人" must NOT match "人物"
        let (_, total) = repo
            .list_assets(ListAssetsParams { tag: Some("人"), page: 1, page_size: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn list_assets_sort_variants() {
        let (repo, _db) = repo().await;
        let mut a = sample_asset(1, ASSET_S1, "image", "Banana");
        (a.created_at, a.updated_at, a.bytes) = (100, 400, Some(50));
        repo.create_asset(&a).await.unwrap();
        let mut b = sample_asset(2, ASSET_S2, "image", "apple");
        (b.created_at, b.updated_at, b.bytes) = (200, 300, Some(999));
        repo.create_asset(&b).await.unwrap();
        let mut c = sample_asset(3, ASSET_S3, "image", "Cherry");
        (c.created_at, c.updated_at, c.bytes) = (300, 100, Some(10));
        repo.create_asset(&c).await.unwrap();

        let ids = |items: &[WorkshopAssetRow]| items.iter().map(|r| r.asset_id.clone()).collect::<Vec<_>>();
        let list = |sort: AssetSort| ListAssetsParams { sort, page: 1, page_size: 50, ..Default::default() };

        let (items, _) = repo.list_assets(list(AssetSort::CreatedDesc)).await.unwrap();
        assert_eq!(ids(&items), [ASSET_S3, ASSET_S2, ASSET_S1]);
        let (items, _) = repo.list_assets(list(AssetSort::CreatedAsc)).await.unwrap();
        assert_eq!(ids(&items), [ASSET_S1, ASSET_S2, ASSET_S3]);
        let (items, _) = repo.list_assets(list(AssetSort::UpdatedDesc)).await.unwrap();
        assert_eq!(ids(&items), [ASSET_S1, ASSET_S2, ASSET_S3]); // updated 400,300,100
        let (items, _) = repo.list_assets(list(AssetSort::TitleAsc)).await.unwrap();
        assert_eq!(ids(&items), [ASSET_S2, ASSET_S1, ASSET_S3]); // apple,Banana,Cherry (NOCASE)
        let (items, _) = repo.list_assets(list(AssetSort::SizeDesc)).await.unwrap();
        assert_eq!(ids(&items), [ASSET_S2, ASSET_S1, ASSET_S3]); // 999,50,10
    }

    #[tokio::test]
    async fn rename_collection_bulk_and_ungroup() {
        let (repo, _db) = repo().await;
        for (id, asset_id, coll) in [
            (1, ASSET_C1, "旧集合"),
            (2, ASSET_C2, "旧集合"),
            (3, ASSET_C3, "其他"),
        ] {
            let mut row = sample_asset(id, asset_id, "image", asset_id);
            row.collection = Some(coll.to_string());
            repo.create_asset(&row).await.unwrap();
        }

        // rename 旧集合 → 新集合 (2 rows)
        let updated = repo.rename_collection("旧集合", Some("新集合"), 5000).await.unwrap();
        assert_eq!(updated, 2);
        let (_, total) = repo
            .list_assets(ListAssetsParams { collection: Some("新集合"), page: 1, page_size: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 2);

        // ungroup 其他 (to = None → NULL)
        let updated = repo.rename_collection("其他", None, 6000).await.unwrap();
        assert_eq!(updated, 1);
        let (_, total) = repo
            .list_assets(ListAssetsParams { ungrouped: true, page: 1, page_size: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 1);

        // no match → 0 rows updated
        let updated = repo.rename_collection("不存在", Some("x"), 7000).await.unwrap();
        assert_eq!(updated, 0);
    }
}
