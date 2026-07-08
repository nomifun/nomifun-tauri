use sqlx::{QueryBuilder, Sqlite, SqlitePool};

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

#[async_trait::async_trait]
impl IWorkshopRepository for SqliteWorkshopRepository {
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
        let row = sqlx::query_as::<_, WorkshopCanvasRow>("SELECT * FROM workshop_canvases WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn create_canvas(&self, id: &str, title: &str, now: i64) -> Result<WorkshopCanvasRow, DbError> {
        sqlx::query(
            "INSERT INTO workshop_canvases (id, title, thumbnail_rel_path, node_count, created_at, updated_at) \
             VALUES (?, ?, NULL, 0, ?, ?)",
        )
        .bind(id)
        .bind(title)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(WorkshopCanvasRow {
            id: id.to_string(),
            title: title.to_string(),
            thumbnail_rel_path: None,
            node_count: 0,
            created_at: now,
            updated_at: now,
        })
    }

    async fn rename_canvas(&self, id: &str, title: &str, now: i64) -> Result<WorkshopCanvasRow, DbError> {
        let result = sqlx::query("UPDATE workshop_canvases SET title = ?, updated_at = ? WHERE id = ?")
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
        let result = sqlx::query("UPDATE workshop_canvases SET node_count = ?, updated_at = ? WHERE id = ?")
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
        let result = sqlx::query("DELETE FROM workshop_canvases WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("workshop canvas '{id}' not found")));
        }
        Ok(())
    }

    async fn set_canvas_thumbnail(
        &self,
        id: &str,
        thumbnail_rel_path: &str,
        now: i64,
    ) -> Result<WorkshopCanvasRow, DbError> {
        let result = sqlx::query("UPDATE workshop_canvases SET thumbnail_rel_path = ?, updated_at = ? WHERE id = ?")
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
        sqlx::query(
            "INSERT INTO workshop_assets \
                (id, kind, title, collection, tags, rel_path, thumb_rel_path, mime, width, height, bytes, \
                 text_content, in_library, origin, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
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
        .execute(&self.pool)
        .await?;
        Ok(row.clone())
    }

    async fn get_asset(&self, id: &str) -> Result<Option<WorkshopAssetRow>, DbError> {
        let row = sqlx::query_as::<_, WorkshopAssetRow>("SELECT * FROM workshop_assets WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn list_all_assets(&self) -> Result<Vec<WorkshopAssetRow>, DbError> {
        let rows = sqlx::query_as::<_, WorkshopAssetRow>("SELECT * FROM workshop_assets")
            .fetch_all(&self.pool)
            .await?;
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
            "UPDATE workshop_assets SET title = ?, collection = ?, tags = ?, in_library = ?, updated_at = ? WHERE id = ?",
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
        let result = sqlx::query("UPDATE workshop_assets SET thumb_rel_path = ?, updated_at = ? WHERE id = ?")
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
        let result = sqlx::query("DELETE FROM workshop_assets WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("workshop asset '{id}' not found")));
        }
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

    async fn repo() -> (SqliteWorkshopRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteWorkshopRepository::new(db.pool().clone());
        (repo, db)
    }

    fn sample_asset(id: &str, kind: &str, title: &str) -> WorkshopAssetRow {
        WorkshopAssetRow {
            id: id.to_string(),
            kind: kind.to_string(),
            title: title.to_string(),
            collection: None,
            tags: "[]".to_string(),
            rel_path: Some(format!("workshop/assets/{id}.png")),
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
        let c = repo.create_canvas("wsc_a", "画布", 1).await.unwrap();
        assert_eq!(c.node_count, 0);
        assert_eq!(c.title, "画布");

        let renamed = repo.rename_canvas("wsc_a", "新名", 2).await.unwrap();
        assert_eq!(renamed.title, "新名");
        assert_eq!(renamed.updated_at, 2);

        let touched = repo.touch_canvas("wsc_a", 7, 3).await.unwrap();
        assert_eq!(touched.node_count, 7);
        assert_eq!(touched.updated_at, 3);

        assert_eq!(repo.list_canvases().await.unwrap().len(), 1);
        repo.delete_canvas("wsc_a").await.unwrap();
        assert!(repo.get_canvas("wsc_a").await.unwrap().is_none());
        assert!(matches!(repo.delete_canvas("wsc_a").await.unwrap_err(), DbError::NotFound(_)));
        assert!(matches!(repo.rename_canvas("nope", "x", 1).await.unwrap_err(), DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_canvases_orders_by_updated_desc() {
        let (repo, _db) = repo().await;
        repo.create_canvas("wsc_1", "a", 100).await.unwrap();
        repo.create_canvas("wsc_2", "b", 200).await.unwrap();
        let all = repo.list_canvases().await.unwrap();
        assert_eq!(all[0].id, "wsc_2");
        assert_eq!(all[1].id, "wsc_1");
    }

    #[tokio::test]
    async fn asset_crud_and_filters() {
        let (repo, _db) = repo().await;
        repo.create_asset(&sample_asset("wsa_1", "image", "红色卖点图")).await.unwrap();
        repo.create_asset(&sample_asset("wsa_2", "video", "宣传视频")).await.unwrap();
        let mut text = sample_asset("wsa_3", "text", "描述");
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
        assert_eq!(items[0].id, "wsa_1");

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
    async fn list_assets_ungrouped_filter() {
        let (repo, _db) = repo().await;
        // Two ungrouped (NULL and empty-string collection) + one grouped.
        repo.create_asset(&sample_asset("wsa_null", "image", "no collection")).await.unwrap();
        let mut empty = sample_asset("wsa_empty", "image", "empty collection");
        empty.collection = Some(String::new());
        repo.create_asset(&empty).await.unwrap();
        let mut grouped = sample_asset("wsa_grp", "image", "grouped");
        grouped.collection = Some("角色".to_string());
        repo.create_asset(&grouped).await.unwrap();

        // ungrouped=true → the NULL + empty-string rows only.
        let (items, total) = repo
            .list_assets(ListAssetsParams { ungrouped: true, page: 1, page_size: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 2);
        let ids: std::collections::BTreeSet<&str> = items.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains("wsa_null") && ids.contains("wsa_empty"));
        assert!(!ids.contains("wsa_grp"));

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
        repo.create_asset(&sample_asset("wsa_x", "image", "old")).await.unwrap();
        let updated = repo
            .update_asset(
                "wsa_x",
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

        repo.delete_asset("wsa_x").await.unwrap();
        assert!(repo.get_asset("wsa_x").await.unwrap().is_none());
        assert!(matches!(repo.delete_asset("wsa_x").await.unwrap_err(), DbError::NotFound(_)));
        assert!(matches!(
            repo.update_asset("nope", UpdateAssetParams::default(), 1).await.unwrap_err(),
            DbError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn set_canvas_thumbnail_and_asset_thumb() {
        let (repo, _db) = repo().await;
        repo.create_canvas("wsc_a", "画布", 1).await.unwrap();
        let c = repo.set_canvas_thumbnail("wsc_a", "workshop/canvases/wsc_a/thumb.jpg", 5).await.unwrap();
        assert_eq!(c.thumbnail_rel_path.as_deref(), Some("workshop/canvases/wsc_a/thumb.jpg"));
        assert_eq!(c.updated_at, 5);
        assert!(matches!(
            repo.set_canvas_thumbnail("nope", "x", 1).await.unwrap_err(),
            DbError::NotFound(_)
        ));

        repo.create_asset(&sample_asset("wsa_t", "image", "img")).await.unwrap();
        repo.set_asset_thumb("wsa_t", "workshop/assets/thumbs/wsa_t.jpg", 6).await.unwrap();
        let a = repo.get_asset("wsa_t").await.unwrap().unwrap();
        assert_eq!(a.thumb_rel_path.as_deref(), Some("workshop/assets/thumbs/wsa_t.jpg"));
        assert!(matches!(repo.set_asset_thumb("nope", "x", 1).await.unwrap_err(), DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_all_assets_returns_every_row() {
        let (repo, _db) = repo().await;
        repo.create_asset(&sample_asset("wsa_1", "image", "a")).await.unwrap();
        let mut internal = sample_asset("wsa_2", "image", "b");
        internal.in_library = false;
        repo.create_asset(&internal).await.unwrap();
        let all = repo.list_all_assets().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn list_assets_tag_filter_exact_match() {
        let (repo, _db) = repo().await;
        let mut a = sample_asset("wsa_ta", "image", "带标签");
        a.tags = r#"["人物","场景"]"#.to_string();
        repo.create_asset(&a).await.unwrap();
        let mut b = sample_asset("wsa_tb", "image", "另一个");
        b.tags = r#"["场景"]"#.to_string();
        repo.create_asset(&b).await.unwrap();

        // "人物" → only wsa_ta
        let (items, total) = repo
            .list_assets(ListAssetsParams { tag: Some("人物"), page: 1, page_size: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(items[0].id, "wsa_ta");

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
        let mut a = sample_asset("wsa_s1", "image", "Banana");
        (a.created_at, a.updated_at, a.bytes) = (100, 400, Some(50));
        repo.create_asset(&a).await.unwrap();
        let mut b = sample_asset("wsa_s2", "image", "apple");
        (b.created_at, b.updated_at, b.bytes) = (200, 300, Some(999));
        repo.create_asset(&b).await.unwrap();
        let mut c = sample_asset("wsa_s3", "image", "Cherry");
        (c.created_at, c.updated_at, c.bytes) = (300, 100, Some(10));
        repo.create_asset(&c).await.unwrap();

        let ids = |items: &[WorkshopAssetRow]| items.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        let list = |sort: AssetSort| ListAssetsParams { sort, page: 1, page_size: 50, ..Default::default() };

        let (items, _) = repo.list_assets(list(AssetSort::CreatedDesc)).await.unwrap();
        assert_eq!(ids(&items), ["wsa_s3", "wsa_s2", "wsa_s1"]);
        let (items, _) = repo.list_assets(list(AssetSort::CreatedAsc)).await.unwrap();
        assert_eq!(ids(&items), ["wsa_s1", "wsa_s2", "wsa_s3"]);
        let (items, _) = repo.list_assets(list(AssetSort::UpdatedDesc)).await.unwrap();
        assert_eq!(ids(&items), ["wsa_s1", "wsa_s2", "wsa_s3"]); // updated 400,300,100
        let (items, _) = repo.list_assets(list(AssetSort::TitleAsc)).await.unwrap();
        assert_eq!(ids(&items), ["wsa_s2", "wsa_s1", "wsa_s3"]); // apple,Banana,Cherry (NOCASE)
        let (items, _) = repo.list_assets(list(AssetSort::SizeDesc)).await.unwrap();
        assert_eq!(ids(&items), ["wsa_s2", "wsa_s1", "wsa_s3"]); // 999,50,10
    }

    #[tokio::test]
    async fn rename_collection_bulk_and_ungroup() {
        let (repo, _db) = repo().await;
        for (id, coll) in [("wsa_c1", "旧集合"), ("wsa_c2", "旧集合"), ("wsa_c3", "其他")] {
            let mut row = sample_asset(id, "image", id);
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
