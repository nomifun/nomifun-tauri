use crate::error::DbError;
use crate::models::{WorkshopAssetRow, WorkshopCanvasRow};

/// Data access for the 创意工坊 (Creative Workshop) domain: the canvas index
/// (`workshop_canvases`) and the asset library (`workshop_assets`).
///
/// The canvas *body* is a file the `nomifun-workshop` service owns; this repo
/// only touches the two index tables. Asset binaries likewise live on disk —
/// the repo stores/serves metadata only.
#[async_trait::async_trait]
pub trait IWorkshopRepository: Send + Sync {
    // ---- canvases ----

    /// Every canvas, newest-updated first.
    async fn list_canvases(&self) -> Result<Vec<WorkshopCanvasRow>, DbError>;

    /// One canvas by id, or `None`.
    async fn get_canvas(&self, id: &str) -> Result<Option<WorkshopCanvasRow>, DbError>;

    /// Insert a canvas index row (the service creates its dir + empty doc).
    async fn create_canvas(&self, id: &str, title: &str, now: i64) -> Result<WorkshopCanvasRow, DbError>;

    /// Rename a canvas. `DbError::NotFound` when the id is unknown.
    async fn rename_canvas(&self, id: &str, title: &str, now: i64) -> Result<WorkshopCanvasRow, DbError>;

    /// Refresh `node_count` + `updated_at` after a doc save. `DbError::NotFound`
    /// when the id is unknown.
    async fn touch_canvas(&self, id: &str, node_count: i64, now: i64) -> Result<WorkshopCanvasRow, DbError>;

    /// Delete a canvas index row. `DbError::NotFound` when the id is unknown.
    async fn delete_canvas(&self, id: &str) -> Result<(), DbError>;

    /// Set (or replace) a canvas's gallery-thumbnail `rel_path`. The service
    /// writes the thumbnail file first. `DbError::NotFound` when unknown.
    async fn set_canvas_thumbnail(&self, id: &str, thumbnail_rel_path: &str, now: i64) -> Result<WorkshopCanvasRow, DbError>;

    // ---- assets ----

    /// Insert a fully-formed asset row.
    async fn create_asset(&self, row: &WorkshopAssetRow) -> Result<WorkshopAssetRow, DbError>;

    /// One asset by id, or `None`.
    async fn get_asset(&self, id: &str) -> Result<Option<WorkshopAssetRow>, DbError>;

    /// Every asset row (no pagination) — for GC (orphan detection + on-disk
    /// file reconciliation). The asset table is small enough to scan whole.
    async fn list_all_assets(&self) -> Result<Vec<WorkshopAssetRow>, DbError>;

    /// Filtered + paginated listing. Returns `(page_items, total_matching)`.
    async fn list_assets(&self, params: ListAssetsParams<'_>) -> Result<(Vec<WorkshopAssetRow>, i64), DbError>;

    /// Partial update (title/collection/tags/in_library). `DbError::NotFound`
    /// when the id is unknown.
    async fn update_asset(&self, id: &str, params: UpdateAssetParams<'_>, now: i64) -> Result<WorkshopAssetRow, DbError>;

    /// Set (or replace) an asset's thumbnail `rel_path` — used by lazy thumbnail
    /// generation on the serve path. `DbError::NotFound` when the id is unknown.
    async fn set_asset_thumb(&self, id: &str, thumb_rel_path: &str, now: i64) -> Result<(), DbError>;

    /// Delete an asset row (the service removes the file). `DbError::NotFound`
    /// when the id is unknown.
    async fn delete_asset(&self, id: &str) -> Result<(), DbError>;

    /// Bulk-rename a collection: every asset whose `collection` equals `from`
    /// gets `to` (or `NULL` when `to` is `None`, i.e. ungrouped). Returns the
    /// number of rows updated (0 when no asset used `from`). Append-only
    /// management operation.
    async fn rename_collection(&self, from: &str, to: Option<&str>, now: i64) -> Result<u64, DbError>;
}

/// Result ordering for [`IWorkshopRepository::list_assets`]. Append-only (the
/// asset-library management page): existing callers keep the `Default`
/// (newest-created first) via `..Default::default()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AssetSort {
    /// Newest created first (the default, matching the original hard-coded order).
    #[default]
    CreatedDesc,
    /// Oldest created first.
    CreatedAsc,
    /// Most recently updated first.
    UpdatedDesc,
    /// Title A→Z (case-insensitive).
    TitleAsc,
    /// Largest byte size first (text assets carry no `bytes` and sort last).
    SizeDesc,
}

/// Filters + pagination for [`IWorkshopRepository::list_assets`]. All filters
/// are optional; `None` means "no filter on this field".
#[derive(Debug, Default)]
pub struct ListAssetsParams<'a> {
    pub kind: Option<&'a str>,
    pub collection: Option<&'a str>,
    /// Case-insensitive substring over title.
    pub q: Option<&'a str>,
    pub in_library: Option<bool>,
    /// Append-only (M10a): when `true`, restrict to assets with no collection
    /// (`collection IS NULL OR collection = ''`). Callers keep this mutually
    /// exclusive with `collection`; if both are set here the two clauses AND
    /// together (never matching), so the caller is responsible for the split.
    pub ungrouped: bool,
    /// Append-only (asset-library page): exact-match filter on one entry of the
    /// JSON `tags` array. `None` means "no tag filter".
    pub tag: Option<&'a str>,
    /// Append-only (asset-library page): result ordering. Defaults to
    /// [`AssetSort::CreatedDesc`].
    pub sort: AssetSort,
    /// 1-based page (clamped to `>= 1` by the caller).
    pub page: i64,
    /// Rows per page (clamped by the caller).
    pub page_size: i64,
}

/// Partial-update params for [`IWorkshopRepository::update_asset`]. Each `Some`
/// replaces the field; `None` keeps the current value. Inner `Option` (for
/// nullable columns) distinguishes "set to NULL" from "keep".
#[derive(Debug, Default)]
pub struct UpdateAssetParams<'a> {
    pub title: Option<&'a str>,
    pub collection: Option<Option<&'a str>>,
    /// Replacement JSON array string of tags.
    pub tags: Option<&'a str>,
    pub in_library: Option<bool>,
}
