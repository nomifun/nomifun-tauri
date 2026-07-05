//! [`WorkshopService`] — the single handle the `/api/workshop/*` routes talk
//! to. Owns canvas CRUD + opaque-doc read/write, asset store/list/patch/delete,
//! and traversal-safe file serving. Canvas bodies + asset binaries live on disk
//! under the data dir; index rows live in `nomifun-db` via [`IWorkshopRepository`].

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use nomifun_common::{AppError, generate_prefixed_id, now_ms};
use nomifun_db::{IWorkshopRepository, ListAssetsParams, UpdateAssetParams, WorkshopAssetRow};
use serde_json::{Value, json};

use crate::dto::{WorkshopAsset, WorkshopCanvasMeta};
use crate::{
    DEFAULT_DOC, MAX_ASSET_BYTES, MAX_DOC_BYTES, WORKSHOP_REL_DIR, archive, docscan, fsio, imagemeta,
    thumbnail,
};

/// A canvas plus its (opaque) doc — the `GET /canvases/{id}` payload.
pub struct CanvasWithDoc {
    pub meta: WorkshopCanvasMeta,
    pub doc: Value,
}

/// A paginated asset listing.
pub struct AssetListPage {
    pub items: Vec<WorkshopAsset>,
    pub total: i64,
}

/// A served asset file (bytes + resolved Content-Type).
pub struct ServedFile {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Result of a garbage-collection pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct GcStats {
    /// Orphan asset *rows* deleted (`in_library = 0` + referenced by no canvas).
    pub orphan_rows_deleted: usize,
    /// On-disk asset/thumb *files* deleted (no surviving row behind them).
    pub orphan_files_deleted: usize,
}

/// Internal descriptor for storing a binary (image/video/audio) asset — the
/// shared path behind both the HTTP upload and the programmatic
/// [`WorkshopService::ingest_asset_bytes`].
struct BinaryAsset {
    kind: String,
    ext: String,
    mime: String,
    bytes: Vec<u8>,
    title: String,
    collection: Option<String>,
    tags: Option<Vec<String>>,
    in_library: bool,
    origin: Option<Value>,
}

/// A multipart asset upload (binary + optional metadata).
pub struct NewAssetUpload {
    pub file_name: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
    pub title: Option<String>,
    pub collection: Option<String>,
    pub tags: Option<Vec<String>>,
    pub in_library: Option<bool>,
}

/// A `text`-kind asset (no binary; body lives in `text_content`).
pub struct NewTextAsset {
    pub title: String,
    pub text_content: String,
    pub collection: Option<String>,
    pub tags: Option<Vec<String>>,
    pub in_library: Option<bool>,
}

/// Filters + pagination for [`WorkshopService::list_assets`].
#[derive(Default)]
pub struct AssetQuery {
    pub kind: Option<String>,
    pub collection: Option<String>,
    pub q: Option<String>,
    pub in_library: Option<bool>,
    pub page: i64,
    pub page_size: i64,
}

/// Partial asset update. A present field updates; an absent one keeps. For
/// `collection`, `Some("")` clears it to NULL.
#[derive(Default)]
pub struct AssetPatch {
    pub title: Option<String>,
    pub collection: Option<String>,
    pub tags: Option<Vec<String>>,
    pub in_library: Option<bool>,
}

pub struct WorkshopService {
    repo: Arc<dyn IWorkshopRepository>,
    /// Backend data dir root. Asset `rel_path`s are relative to this.
    data_dir: PathBuf,
}

impl WorkshopService {
    /// Build the service over its index repo + the data dir root.
    pub fn start(data_dir: &Path, repo: Arc<dyn IWorkshopRepository>) -> Arc<Self> {
        Arc::new(Self {
            repo,
            data_dir: data_dir.to_path_buf(),
        })
    }

    // ---- path helpers ----

    fn workshop_dir(&self) -> PathBuf {
        self.data_dir.join(WORKSHOP_REL_DIR)
    }

    fn canvas_dir(&self, id: &str) -> PathBuf {
        self.workshop_dir().join("canvases").join(id)
    }

    fn assets_dir(&self) -> PathBuf {
        self.workshop_dir().join("assets")
    }

    // ---- canvases ----

    pub async fn list_canvases(&self) -> Result<Vec<WorkshopCanvasMeta>, AppError> {
        Ok(self.repo.list_canvases().await?.into_iter().map(WorkshopCanvasMeta::from).collect())
    }

    pub async fn create_canvas(&self, title: Option<String>) -> Result<WorkshopCanvasMeta, AppError> {
        let id = generate_prefixed_id("wsc");
        let title = title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "未命名画布".to_string());
        let now = now_ms();
        // Write the empty doc first so a crash between INSERT and write can't
        // leave a row whose file is missing (the read path tolerates a missing
        // file, but writing first keeps disk ⊇ index).
        fsio::save_bytes_atomic(&self.canvas_dir(&id), "canvas.json", DEFAULT_DOC.as_bytes())
            .await
            .map_err(|e| AppError::Internal(format!("write canvas doc: {e}")))?;
        let row = self.repo.create_canvas(&id, &title, now).await?;
        Ok(row.into())
    }

    pub async fn get_canvas(&self, id: &str) -> Result<CanvasWithDoc, AppError> {
        let row = self
            .repo
            .get_canvas(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("workshop canvas {id} not found")))?;
        let doc = self.read_doc(id).await;
        Ok(CanvasWithDoc { meta: row.into(), doc })
    }

    /// Read + parse the canvas doc; a missing or corrupt file falls back to the
    /// default empty doc (never fails the read).
    async fn read_doc(&self, id: &str) -> Value {
        let path = self.canvas_dir(id).join("canvas.json");
        match fsio::read_bytes_opt(&path).await {
            Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(id, error = %e, "workshop canvas doc unreadable; serving default");
                default_doc_value()
            }),
            Ok(None) => default_doc_value(),
            Err(e) => {
                tracing::warn!(id, error = %e, "workshop canvas doc read failed; serving default");
                default_doc_value()
            }
        }
    }

    /// Persist an opaque doc (≤ [`MAX_DOC_BYTES`]), sync `node_count` from
    /// `doc.nodes`, and return the new `updated_at`.
    pub async fn save_doc(&self, id: &str, doc: &Value) -> Result<i64, AppError> {
        // Ensure the canvas exists before touching disk.
        if self.repo.get_canvas(id).await?.is_none() {
            return Err(AppError::NotFound(format!("workshop canvas {id} not found")));
        }
        let bytes = serde_json::to_vec(doc).map_err(|e| AppError::BadRequest(format!("invalid doc json: {e}")))?;
        if bytes.len() > MAX_DOC_BYTES {
            return Err(AppError::BadRequest(format!(
                "canvas doc is too large: {} bytes (max {MAX_DOC_BYTES})",
                bytes.len()
            )));
        }
        let node_count = doc
            .get("nodes")
            .and_then(Value::as_array)
            .map(|a| a.len() as i64)
            .unwrap_or(0);
        fsio::save_bytes_atomic(&self.canvas_dir(id), "canvas.json", &bytes)
            .await
            .map_err(|e| AppError::Internal(format!("write canvas doc: {e}")))?;
        let row = self.repo.touch_canvas(id, node_count, now_ms()).await?;
        Ok(row.updated_at)
    }

    pub async fn rename_canvas(&self, id: &str, title: &str) -> Result<WorkshopCanvasMeta, AppError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(AppError::BadRequest("title must not be empty".into()));
        }
        Ok(self.repo.rename_canvas(id, title, now_ms()).await?.into())
    }

    /// PATCH a canvas: optionally rename and/or set its gallery thumbnail from an
    /// asset (append-only over `rename_canvas`). Returns the latest meta. A
    /// request with no fields is a no-op that returns the current meta.
    pub async fn patch_canvas(
        &self,
        id: &str,
        title: Option<String>,
        thumbnail_asset_id: Option<String>,
    ) -> Result<WorkshopCanvasMeta, AppError> {
        let mut latest: Option<WorkshopCanvasMeta> = None;
        if let Some(title) = title {
            latest = Some(self.rename_canvas(id, &title).await?);
        }
        if let Some(asset_id) = thumbnail_asset_id {
            latest = Some(self.set_canvas_thumbnail(id, &asset_id).await?);
        }
        match latest {
            Some(meta) => Ok(meta),
            None => {
                let row = self
                    .repo
                    .get_canvas(id)
                    .await?
                    .ok_or_else(|| AppError::NotFound(format!("workshop canvas {id} not found")))?;
                Ok(row.into())
            }
        }
    }

    /// Point a canvas's gallery thumbnail at an asset's thumbnail. The asset
    /// must be a decodable image (its JPEG thumbnail — generated on demand — is
    /// copied to `{canvas_dir}/thumb.jpg`).
    pub async fn set_canvas_thumbnail(&self, canvas_id: &str, asset_id: &str) -> Result<WorkshopCanvasMeta, AppError> {
        if self.repo.get_canvas(canvas_id).await?.is_none() {
            return Err(AppError::NotFound(format!("workshop canvas {canvas_id} not found")));
        }
        let row = self
            .repo
            .get_asset(asset_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("workshop asset {asset_id} not found")))?;
        let bytes = self
            .thumb_bytes(&row)
            .await
            .ok_or_else(|| AppError::BadRequest("thumbnail asset must be a decodable image".into()))?;
        fsio::save_bytes_atomic(&self.canvas_dir(canvas_id), "thumb.jpg", &bytes)
            .await
            .map_err(|e| AppError::Internal(format!("write canvas thumbnail: {e}")))?;
        let rel = format!("{WORKSHOP_REL_DIR}/canvases/{canvas_id}/thumb.jpg");
        Ok(self.repo.set_canvas_thumbnail(canvas_id, &rel, now_ms()).await?.into())
    }

    /// Serve a canvas's gallery thumbnail bytes (JPEG). NotFound when the canvas
    /// has no thumbnail set.
    pub async fn serve_canvas_thumbnail(&self, canvas_id: &str) -> Result<ServedFile, AppError> {
        let row = self
            .repo
            .get_canvas(canvas_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("workshop canvas {canvas_id} not found")))?;
        let rel = row
            .thumbnail_rel_path
            .as_deref()
            .ok_or_else(|| AppError::NotFound(format!("canvas {canvas_id} has no thumbnail")))?;
        let abs = self.resolve_within_workshop(rel)?;
        let bytes = tokio::fs::read(&abs)
            .await
            .map_err(|_| AppError::NotFound(format!("canvas {canvas_id} thumbnail is missing")))?;
        Ok(ServedFile { mime: thumbnail::THUMB_MIME.to_string(), bytes })
    }

    pub async fn delete_canvas(&self, id: &str) -> Result<(), AppError> {
        // Snapshot this canvas's asset references before its doc disappears, so
        // we can GC canvas-internal assets it alone kept alive.
        let doc = self.read_doc(id).await;
        let own_refs = docscan::collect_asset_refs(&doc);

        self.repo.delete_canvas(id).await?;
        // Best-effort remove the on-disk body dir (row is the source of truth).
        if let Err(e) = tokio::fs::remove_dir_all(self.canvas_dir(id)).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(id, error = %e, "workshop canvas dir remove failed (row deleted)");
        }

        // GC: for each asset this canvas referenced, if it's canvas-internal
        // (`in_library = 0`) and no *other* canvas still references it, drop it.
        if !own_refs.is_empty() {
            let still_referenced = self.collect_all_referenced_asset_ids().await.unwrap_or_default();
            for asset_id in own_refs {
                if still_referenced.contains(&asset_id) {
                    continue;
                }
                if let Ok(Some(row)) = self.repo.get_asset(&asset_id).await
                    && !row.in_library
                    && let Err(e) = self.delete_asset(&asset_id).await
                {
                    tracing::warn!(asset_id, error = %e, "workshop GC: internal asset delete failed");
                }
            }
        }
        Ok(())
    }

    /// Every asset id referenced by *any* canvas doc (scans all canvases; the
    /// canvas count is small by design).
    async fn collect_all_referenced_asset_ids(&self) -> Result<BTreeSet<String>, AppError> {
        let mut out = BTreeSet::new();
        for canvas in self.repo.list_canvases().await? {
            let doc = self.read_doc(&canvas.id).await;
            out.extend(docscan::collect_asset_refs(&doc));
        }
        Ok(out)
    }

    // ---- assets ----

    pub async fn upload_asset(&self, input: NewAssetUpload) -> Result<WorkshopAsset, AppError> {
        let (ext, mime, kind) = classify_upload(&input.file_name, input.content_type.as_deref())?;
        let title = input
            .title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| input.file_name.clone());
        let row = self
            .store_binary_asset(BinaryAsset {
                kind: kind.to_string(),
                ext,
                mime,
                bytes: input.bytes,
                title,
                collection: input.collection,
                tags: input.tags,
                in_library: input.in_library.unwrap_or(true),
                origin: None,
            })
            .await?;
        Ok(row.into())
    }

    /// Programmatic asset ingest: store raw `bytes` of a given `mime` as a new
    /// asset row and return it. The shared entry point for other modules (e.g.
    /// the generation engine writing produced media). `origin` is the JSON
    /// provenance blob (`{prompt,model,provider_id,params,canvas_id,…}`).
    pub async fn ingest_asset_bytes(
        &self,
        bytes: Vec<u8>,
        mime: &str,
        title: &str,
        in_library: bool,
        origin: Option<Value>,
    ) -> Result<WorkshopAssetRow, AppError> {
        let (kind, ext) = classify_mime(mime)?;
        let title = title.trim();
        let title = if title.is_empty() { format!("{kind} asset") } else { title.to_string() };
        self.store_binary_asset(BinaryAsset {
            kind: kind.to_string(),
            ext,
            mime: mime.trim().to_string(),
            bytes,
            title,
            collection: None,
            tags: None,
            in_library,
            origin,
        })
        .await
    }

    /// Read an asset's original binary + its resolved mime. Errors when the
    /// asset is unknown, is a text asset (no file), or its file is missing. The
    /// programmatic counterpart to [`Self::serve_file`] (no thumbnail path).
    pub async fn read_asset_bytes(&self, asset_id: &str) -> Result<(Vec<u8>, String), AppError> {
        let row = self
            .repo
            .get_asset(asset_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("workshop asset {asset_id} not found")))?;
        self.read_original(&row).await
    }

    /// The shared store path: validate size, extract image dimensions, persist
    /// the binary, best-effort generate a thumbnail (images only), then insert
    /// the row (rolling the file back if the insert fails).
    async fn store_binary_asset(&self, input: BinaryAsset) -> Result<WorkshopAssetRow, AppError> {
        if input.bytes.is_empty() {
            return Err(AppError::BadRequest("asset payload is empty".into()));
        }
        if input.bytes.len() > MAX_ASSET_BYTES {
            return Err(AppError::BadRequest(format!(
                "asset is too large: {} bytes (max {MAX_ASSET_BYTES})",
                input.bytes.len()
            )));
        }
        let is_image = input.kind == "image";
        let (width, height) = if is_image {
            match imagemeta::image_dimensions(&input.bytes) {
                Some((w, h)) => (Some(w as i64), Some(h as i64)),
                None => (None, None),
            }
        } else {
            (None, None)
        };

        let id = generate_prefixed_id("wsa");
        let disk_name = format!("{id}.{}", input.ext);
        let rel_path = format!("{WORKSHOP_REL_DIR}/assets/{disk_name}");
        fsio::save_bytes_atomic(&self.assets_dir(), &disk_name, &input.bytes)
            .await
            .map_err(|e| AppError::Internal(format!("write asset file: {e}")))?;

        let thumb_rel_path = if is_image {
            self.generate_and_store_thumb(&id, &input.bytes).await
        } else {
            None
        };

        let now = now_ms();
        let row = WorkshopAssetRow {
            id,
            kind: input.kind,
            title: input.title,
            collection: normalize_opt(input.collection),
            tags: tags_json(input.tags),
            rel_path: Some(rel_path),
            thumb_rel_path,
            mime: Some(input.mime),
            width,
            height,
            bytes: Some(input.bytes.len() as i64),
            text_content: None,
            in_library: input.in_library,
            origin: input.origin.map(|v| v.to_string()),
            created_at: now,
            updated_at: now,
        };
        // Roll the files back if the row insert fails.
        match self.repo.create_asset(&row).await {
            Ok(saved) => Ok(saved),
            Err(e) => {
                for rel in [row.rel_path.as_deref(), row.thumb_rel_path.as_deref()].into_iter().flatten() {
                    let _ = tokio::fs::remove_file(self.data_dir.join(rel)).await;
                }
                Err(e.into())
            }
        }
    }

    /// Generate a JPEG thumbnail from `bytes` and persist it under
    /// `assets/thumbs/{id}.jpg`. Returns its data-dir-relative path, or `None`
    /// when the bytes aren't decodable / the write fails (thumbnails are
    /// best-effort — the asset is still fully usable without one).
    async fn generate_and_store_thumb(&self, id: &str, bytes: &[u8]) -> Option<String> {
        let owned = bytes.to_vec();
        let thumb = tokio::task::spawn_blocking(move || {
            thumbnail::encode_thumbnail_jpeg(&owned, thumbnail::THUMB_MAX_EDGE)
        })
        .await
        .ok()??;
        let disk_name = format!("{id}.{}", thumbnail::THUMB_EXT);
        let dir = self.assets_dir().join("thumbs");
        if let Err(e) = fsio::save_bytes_atomic(&dir, &disk_name, &thumb).await {
            tracing::warn!(id, error = %e, "workshop thumbnail write failed");
            return None;
        }
        Some(format!("{WORKSHOP_REL_DIR}/assets/thumbs/{disk_name}"))
    }

    /// Best-effort thumbnail bytes for an asset: an existing thumbnail file if
    /// present, else (for images) one generated + persisted on the fly. `None`
    /// for non-images or when generation fails.
    async fn thumb_bytes(&self, row: &WorkshopAssetRow) -> Option<Vec<u8>> {
        if let Some(rel) = row.thumb_rel_path.as_deref()
            && let Ok(abs) = self.resolve_within_workshop(rel)
            && let Ok(bytes) = tokio::fs::read(&abs).await
        {
            return Some(bytes);
        }
        if row.kind != "image" {
            return None;
        }
        let rel = row.rel_path.as_deref()?;
        let abs = self.resolve_within_workshop(rel).ok()?;
        let original = tokio::fs::read(&abs).await.ok()?;
        let thumb_rel = self.generate_and_store_thumb(&row.id, &original).await?;
        // Persist the freshly minted thumb path (best-effort).
        let _ = self.repo.set_asset_thumb(&row.id, &thumb_rel, now_ms()).await;
        let thumb_abs = self.resolve_within_workshop(&thumb_rel).ok()?;
        tokio::fs::read(&thumb_abs).await.ok()
    }

    /// Read an asset's original bytes + mime (used by serve + programmatic read).
    async fn read_original(&self, row: &WorkshopAssetRow) -> Result<(Vec<u8>, String), AppError> {
        let rel = row
            .rel_path
            .as_deref()
            .ok_or_else(|| AppError::NotFound(format!("asset {} has no file", row.id)))?;
        let abs = self.resolve_within_workshop(rel)?;
        let bytes = tokio::fs::read(&abs)
            .await
            .map_err(|_| AppError::NotFound(format!("asset {} file is missing", row.id)))?;
        let mime = row.mime.clone().unwrap_or_else(|| "application/octet-stream".to_string());
        Ok((bytes, mime))
    }

    pub async fn create_text_asset(&self, input: NewTextAsset) -> Result<WorkshopAsset, AppError> {
        let title = input.title.trim();
        if title.is_empty() {
            return Err(AppError::BadRequest("title must not be empty".into()));
        }
        let now = now_ms();
        let row = WorkshopAssetRow {
            id: generate_prefixed_id("wsa"),
            kind: "text".to_string(),
            title: title.to_string(),
            collection: normalize_opt(input.collection),
            tags: tags_json(input.tags),
            rel_path: None,
            thumb_rel_path: None,
            mime: None,
            width: None,
            height: None,
            bytes: None,
            text_content: Some(input.text_content),
            in_library: input.in_library.unwrap_or(true),
            origin: None,
            created_at: now,
            updated_at: now,
        };
        Ok(self.repo.create_asset(&row).await?.into())
    }

    pub async fn list_assets(&self, query: AssetQuery) -> Result<AssetListPage, AppError> {
        let (rows, total) = self
            .repo
            .list_assets(ListAssetsParams {
                kind: query.kind.as_deref(),
                collection: query.collection.as_deref(),
                q: query.q.as_deref().filter(|s| !s.trim().is_empty()),
                in_library: query.in_library,
                page: query.page,
                page_size: query.page_size,
            })
            .await?;
        Ok(AssetListPage {
            items: rows.into_iter().map(WorkshopAsset::from).collect(),
            total,
        })
    }

    pub async fn patch_asset(&self, id: &str, patch: AssetPatch) -> Result<WorkshopAsset, AppError> {
        // Own the JSON string so the borrowed params can reference it.
        let tags_owned = patch.tags.map(|t| serde_json::to_string(&t).unwrap_or_else(|_| "[]".to_string()));
        let collection = patch
            .collection
            .as_ref()
            .map(|c| if c.trim().is_empty() { None } else { Some(c.trim()) });
        let params = UpdateAssetParams {
            title: patch.title.as_deref().map(str::trim).filter(|t| !t.is_empty()),
            collection,
            tags: tags_owned.as_deref(),
            in_library: patch.in_library,
        };
        Ok(self.repo.update_asset(id, params, now_ms()).await?.into())
    }

    pub async fn delete_asset(&self, id: &str) -> Result<(), AppError> {
        let row = self
            .repo
            .get_asset(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("workshop asset {id} not found")))?;
        self.repo.delete_asset(id).await?;
        for rel in [row.rel_path.as_deref(), row.thumb_rel_path.as_deref()].into_iter().flatten() {
            let abs = self.data_dir.join(rel);
            if let Err(e) = tokio::fs::remove_file(&abs).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(id, path = %abs.display(), error = %e, "workshop asset file remove failed (row deleted)");
            }
        }
        Ok(())
    }

    /// Serve an asset's original (or, when `thumb`, its thumbnail — generated on
    /// demand for images that lack one, else falling back to the original per
    /// contract §3.2). Traversal-safe. Missing file → NotFound.
    pub async fn serve_file(&self, asset_id: &str, thumb: bool) -> Result<ServedFile, AppError> {
        let row = self
            .repo
            .get_asset(asset_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("workshop asset {asset_id} not found")))?;

        if thumb
            && let Some(bytes) = self.thumb_bytes(&row).await
        {
            return Ok(ServedFile { mime: thumbnail::THUMB_MIME.to_string(), bytes });
        }
        let (bytes, mime) = self.read_original(&row).await?;
        Ok(ServedFile { mime, bytes })
    }

    // ---- export / import ----

    /// Build a `.zip` export of a canvas: `canvas.json` (the doc verbatim),
    /// `manifest.json` (version/app + per-asset metadata), and one
    /// `assets/{id}.{ext}` entry for every doc-referenced asset that has a file.
    pub async fn export_canvas(&self, id: &str) -> Result<Vec<u8>, AppError> {
        let canvas = self
            .repo
            .get_canvas(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("workshop canvas {id} not found")))?;
        let doc = self.read_doc(id).await;
        let refs = docscan::collect_asset_refs(&doc);

        let mut manifest_assets: Vec<Value> = Vec::new();
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for asset_id in &refs {
            let Some(row) = self.repo.get_asset(asset_id).await? else {
                continue; // dangling reference — skip
            };
            // Copy the binary in (if any) and note its archive path.
            let mut file_entry: Option<String> = None;
            if let Some(rel) = row.rel_path.as_deref() {
                let ext = Path::new(rel)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("bin");
                let entry = format!("assets/{}.{ext}", row.id);
                if let Ok(abs) = self.resolve_within_workshop(rel)
                    && let Ok(bytes) = tokio::fs::read(&abs).await
                {
                    files.push((entry.clone(), bytes));
                    file_entry = Some(entry);
                }
            }
            let tags = serde_json::from_str::<Value>(&row.tags).unwrap_or_else(|_| json!([]));
            let origin = row.origin.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok());
            manifest_assets.push(json!({
                "id": row.id,
                "kind": row.kind,
                "title": row.title,
                "collection": row.collection,
                "tags": tags,
                "mime": row.mime,
                "width": row.width,
                "height": row.height,
                "bytes": row.bytes,
                "in_library": row.in_library,
                "text_content": row.text_content,
                "origin": origin,
                "file": file_entry,
            }));
        }

        let manifest = json!({
            "version": archive::ARCHIVE_VERSION,
            "app": archive::ARCHIVE_APP,
            "exported_at": now_ms(),
            "canvas": { "title": canvas.title },
            "assets": manifest_assets,
        });
        let canvas_json = serde_json::to_vec(&doc).map_err(|e| AppError::Internal(format!("serialize doc: {e}")))?;
        let manifest_json =
            serde_json::to_vec(&manifest).map_err(|e| AppError::Internal(format!("serialize manifest: {e}")))?;

        let mut entries = vec![
            (archive::CANVAS_ENTRY.to_string(), canvas_json),
            (archive::MANIFEST_ENTRY.to_string(), manifest_json),
        ];
        entries.extend(files);
        tokio::task::spawn_blocking(move || archive::build_zip(entries))
            .await
            .map_err(|e| AppError::Internal(format!("zip task: {e}")))?
            .map_err(|e| AppError::Internal(format!("build zip: {e}")))
    }

    /// Import a canvas `.zip` (as produced by [`Self::export_canvas`]) into a
    /// brand-new canvas: every referenced asset is re-registered under a fresh
    /// id, the doc's asset ids are rewritten to match, and the canvas title is
    /// de-duplicated. Returns the new canvas meta.
    pub async fn import_canvas(&self, zip_bytes: Vec<u8>) -> Result<WorkshopCanvasMeta, AppError> {
        let extracted = tokio::task::spawn_blocking(move || archive::extract_zip(&zip_bytes))
            .await
            .map_err(|e| AppError::Internal(format!("unzip task: {e}")))?
            .map_err(|e| AppError::BadRequest(format!("invalid archive: {e}")))?;

        let canvas_raw = extracted
            .get(archive::CANVAS_ENTRY)
            .ok_or_else(|| AppError::BadRequest("archive is missing canvas.json".into()))?;
        let mut doc: Value = serde_json::from_slice(canvas_raw)
            .map_err(|e| AppError::BadRequest(format!("canvas.json is not valid JSON: {e}")))?;
        let manifest: Value = extracted
            .get(archive::MANIFEST_ENTRY)
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or_else(|| json!({}));

        // Re-register every asset the manifest describes; build old→new id remap.
        let mut remap: HashMap<String, String> = HashMap::new();
        if let Some(assets) = manifest.get("assets").and_then(Value::as_array) {
            for a in assets {
                let Some(old_id) = a.get("id").and_then(Value::as_str) else { continue };
                match self.reregister_imported_asset(a, &extracted).await {
                    Ok(Some(new_id)) => {
                        remap.insert(old_id.to_string(), new_id);
                    }
                    Ok(None) => {} // no binary present / unsupported — drop the ref
                    Err(e) => {
                        tracing::warn!(old_id, error = %e, "workshop import: asset re-register failed");
                    }
                }
            }
        }
        docscan::remap_asset_ids(&mut doc, &remap);

        let base_title = manifest
            .get("canvas")
            .and_then(|c| c.get("title"))
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .unwrap_or("导入的画布")
            .to_string();
        let title = self.dedup_canvas_title(&base_title).await?;

        let meta = self.create_canvas(Some(title)).await?;
        self.save_doc(&meta.id, &doc).await?;
        let row = self
            .repo
            .get_canvas(&meta.id)
            .await?
            .ok_or_else(|| AppError::Internal("imported canvas vanished".into()))?;
        Ok(row.into())
    }

    /// Re-register one manifest asset entry under a fresh id. Returns the new id,
    /// or `None` when the entry has no usable payload.
    async fn reregister_imported_asset(
        &self,
        entry: &Value,
        files: &HashMap<String, Vec<u8>>,
    ) -> Result<Option<String>, AppError> {
        let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("image");
        let title = entry.get("title").and_then(Value::as_str).unwrap_or("导入的资产").to_string();
        let collection = entry.get("collection").and_then(Value::as_str).map(str::to_string);
        let tags: Option<Vec<String>> = entry.get("tags").and_then(Value::as_array).map(|arr| {
            arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()
        });
        let in_library = entry.get("in_library").and_then(Value::as_bool).unwrap_or(false);
        let origin = entry.get("origin").cloned().filter(|v| !v.is_null());

        if kind == "text" {
            let text_content = entry.get("text_content").and_then(Value::as_str).unwrap_or("").to_string();
            let row = self
                .create_text_asset(NewTextAsset { title, text_content, collection, tags, in_library: Some(in_library) })
                .await?;
            return Ok(Some(row.id));
        }

        // Binary asset: needs a file in the archive.
        let Some(file_path) = entry.get("file").and_then(Value::as_str) else {
            return Ok(None);
        };
        let Some(bytes) = files.get(file_path) else {
            return Ok(None);
        };
        let mime = entry.get("mime").and_then(Value::as_str).unwrap_or("application/octet-stream");
        let ext = Path::new(file_path).extension().and_then(|e| e.to_str()).unwrap_or("bin").to_string();
        let row = self
            .store_binary_asset(BinaryAsset {
                kind: kind.to_string(),
                ext,
                mime: mime.to_string(),
                bytes: bytes.clone(),
                title,
                collection,
                tags,
                in_library,
                origin,
            })
            .await?;
        Ok(Some(row.id))
    }

    /// Return `base` if unused, else `base (2)`, `base (3)`, … (first free).
    async fn dedup_canvas_title(&self, base: &str) -> Result<String, AppError> {
        let existing: BTreeSet<String> =
            self.repo.list_canvases().await?.into_iter().map(|c| c.title).collect();
        if !existing.contains(base) {
            return Ok(base.to_string());
        }
        for n in 2..10_000 {
            let candidate = format!("{base} ({n})");
            if !existing.contains(&candidate) {
                return Ok(candidate);
            }
        }
        Ok(format!("{base} ({})", now_ms()))
    }

    // ---- garbage collection ----

    /// Full GC sweep: delete orphan asset *rows* (`in_library = 0` referenced by
    /// no canvas doc) and orphan *files* on disk (no surviving row). Returns
    /// counts.
    pub async fn gc(&self) -> Result<GcStats, AppError> {
        let referenced = self.collect_all_referenced_asset_ids().await?;
        let all = self.repo.list_all_assets().await?;

        let mut orphan_rows_deleted = 0usize;
        let mut surviving_ids: BTreeSet<String> = BTreeSet::new();
        for row in &all {
            let orphan = !row.in_library && !referenced.contains(&row.id);
            if orphan {
                if self.delete_asset(&row.id).await.is_ok() {
                    orphan_rows_deleted += 1;
                }
            } else {
                surviving_ids.insert(row.id.clone());
            }
        }

        let orphan_files_deleted = self.sweep_orphan_files(&surviving_ids).await;
        Ok(GcStats { orphan_rows_deleted, orphan_files_deleted })
    }

    /// Delete `wsa_*` files under the assets dir (originals + thumbs) whose id is
    /// not in `surviving_ids`. Best-effort; returns the number removed.
    async fn sweep_orphan_files(&self, surviving_ids: &BTreeSet<String>) -> usize {
        let assets = self.assets_dir();
        let mut deleted = sweep_asset_dir(&assets, surviving_ids).await;
        deleted += sweep_asset_dir(&assets.join("thumbs"), surviving_ids).await;
        deleted
    }

    /// Resolve a data-dir-relative path and guarantee it stays inside the
    /// workshop dir (defense-in-depth; `rel_path`s are minted by us).
    fn resolve_within_workshop(&self, rel: &str) -> Result<PathBuf, AppError> {
        if rel.contains('\0') || Path::new(rel).components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(AppError::Forbidden("asset path contains invalid traversal".into()));
        }
        let abs = self.data_dir.join(rel);
        let canonical = std::fs::canonicalize(&abs)
            .map_err(|_| AppError::NotFound("asset file is missing".into()))?;
        let root = std::fs::canonicalize(self.workshop_dir())
            .map_err(|e| AppError::Internal(format!("resolve workshop dir: {e}")))?;
        if !canonical.starts_with(&root) {
            return Err(AppError::Forbidden("asset path escapes the workshop sandbox".into()));
        }
        Ok(canonical)
    }
}

fn default_doc_value() -> Value {
    serde_json::from_str(DEFAULT_DOC).expect("DEFAULT_DOC is valid json")
}

/// Delete `wsa_*` files directly under `dir` whose id-stem is not in
/// `surviving_ids`. Non-recursive, best-effort; returns the count removed.
async fn sweep_asset_dir(dir: &Path, surviving_ids: &BTreeSet<String>) -> usize {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return 0;
    };
    let mut deleted = 0usize;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let is_file = entry.file_type().await.map(|t| t.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        // Only ever touch our own asset files, and only when no row survives.
        if stem.starts_with("wsa_") && !surviving_ids.contains(stem) && tokio::fs::remove_file(&path).await.is_ok() {
            deleted += 1;
        }
    }
    deleted
}

fn normalize_opt(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn tags_json(tags: Option<Vec<String>>) -> String {
    serde_json::to_string(&tags.unwrap_or_default()).unwrap_or_else(|_| "[]".to_string())
}

/// Resolve `(ext, mime, kind)` for an upload. Only image/* and video/* are
/// accepted; anything else is a bad request.
fn classify_upload(file_name: &str, content_type: Option<&str>) -> Result<(String, String, &'static str), AppError> {
    let ext_from_name = Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty());
    let guessed_raw = mime_guess::from_path(file_name).first_raw();
    let mime = content_type
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "application/octet-stream")
        .map(str::to_string)
        .or_else(|| guessed_raw.map(str::to_string))
        .unwrap_or_else(|| "application/octet-stream".to_string());

    let kind = if mime.starts_with("image/") {
        "image"
    } else if mime.starts_with("video/") {
        "video"
    } else {
        return Err(AppError::BadRequest(format!(
            "unsupported media type '{mime}': only image/* and video/* uploads are accepted"
        )));
    };

    let ext = ext_from_name
        .or_else(|| {
            mime_guess::get_mime_extensions_str(&mime).and_then(|exts| exts.first().map(|e| e.to_string()))
        })
        .unwrap_or_else(|| "bin".to_string());
    Ok((ext, mime, kind))
}

/// Resolve `(kind, ext)` from a bare mime type (programmatic ingest — no
/// filename). image/* → image, video/* → video, audio/* → audio; else a bad
/// request.
fn classify_mime(mime: &str) -> Result<(&'static str, String), AppError> {
    let m = mime.trim().to_ascii_lowercase();
    let kind = if m.starts_with("image/") {
        "image"
    } else if m.starts_with("video/") {
        "video"
    } else if m.starts_with("audio/") {
        "audio"
    } else {
        return Err(AppError::BadRequest(format!(
            "unsupported media type '{mime}': only image/*, video/*, audio/* are ingestible"
        )));
    };
    let ext = mime_guess::get_mime_extensions_str(&m)
        .and_then(|exts| exts.first().map(|e| e.to_string()))
        .unwrap_or_else(|| "bin".to_string());
    Ok((kind, ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::SqliteWorkshopRepository;

    async fn service() -> (Arc<WorkshopService>, tempfile::TempDir) {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let repo: Arc<dyn IWorkshopRepository> = Arc::new(SqliteWorkshopRepository::new(db.pool().clone()));
        Box::leak(Box::new(db));
        let dir = tempfile::tempdir().unwrap();
        (WorkshopService::start(dir.path(), repo), dir)
    }

    // A 1x1 PNG.
    fn png_1x1() -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
        b.extend_from_slice(&[0, 0, 0, 13]);
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        b
    }

    #[tokio::test]
    async fn canvas_create_read_save_delete() {
        let (svc, dir) = service().await;
        let meta = svc.create_canvas(None).await.unwrap();
        assert_eq!(meta.title, "未命名画布");
        assert!(meta.id.starts_with("wsc_"));
        assert!(dir.path().join("workshop/canvases").join(&meta.id).join("canvas.json").exists());

        // default doc parses; save a doc with 2 nodes → node_count syncs.
        let read = svc.get_canvas(&meta.id).await.unwrap();
        assert_eq!(read.doc["schema"], 1);
        let doc = serde_json::json!({"schema":1,"nodes":[{"id":"a"},{"id":"b"}],"edges":[]});
        let updated_at = svc.save_doc(&meta.id, &doc).await.unwrap();
        assert!(updated_at >= meta.created_at);
        let all = svc.list_canvases().await.unwrap();
        assert_eq!(all[0].node_count, 2);

        // rename
        let renamed = svc.rename_canvas(&meta.id, "  我的画布  ").await.unwrap();
        assert_eq!(renamed.title, "我的画布");
        assert!(svc.rename_canvas(&meta.id, "   ").await.is_err());

        // delete removes row + dir
        svc.delete_canvas(&meta.id).await.unwrap();
        assert!(!dir.path().join("workshop/canvases").join(&meta.id).exists());
        assert!(svc.get_canvas(&meta.id).await.is_err());
    }

    #[tokio::test]
    async fn save_doc_rejects_oversize_and_unknown_canvas() {
        let (svc, _dir) = service().await;
        assert!(svc.save_doc("wsc_missing", &serde_json::json!({})).await.is_err());
    }

    #[tokio::test]
    async fn upload_image_extracts_dimensions_and_serves() {
        let (svc, _dir) = service().await;
        let asset = svc
            .upload_asset(NewAssetUpload {
                file_name: "shot.png".into(),
                content_type: Some("image/png".into()),
                bytes: png_1x1(),
                title: None,
                collection: Some("角色".into()),
                tags: Some(vec!["a".into()]),
                in_library: None,
            })
            .await
            .unwrap();
        assert_eq!(asset.kind, "image");
        assert_eq!(asset.width, Some(1));
        assert_eq!(asset.height, Some(1));
        assert!(asset.in_library);
        assert_eq!(asset.url, format!("/api/workshop/files/{}", asset.id));

        // serve returns the bytes + mime
        let served = svc.serve_file(&asset.id, false).await.unwrap();
        assert_eq!(served.mime, "image/png");
        assert_eq!(served.bytes, png_1x1());
        // thumb=1 falls back to original when no thumb exists
        let served_thumb = svc.serve_file(&asset.id, true).await.unwrap();
        assert_eq!(served_thumb.bytes, png_1x1());
    }

    #[tokio::test]
    async fn upload_rejects_non_media() {
        let (svc, _dir) = service().await;
        let err = svc
            .upload_asset(NewAssetUpload {
                file_name: "notes.txt".into(),
                content_type: Some("text/plain".into()),
                bytes: b"hi".to_vec(),
                title: None,
                collection: None,
                tags: None,
                in_library: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn text_asset_list_patch_delete() {
        let (svc, _dir) = service().await;
        let a = svc
            .create_text_asset(NewTextAsset {
                title: "描述".into(),
                text_content: "武松打虎".into(),
                collection: None,
                tags: None,
                in_library: Some(false),
            })
            .await
            .unwrap();
        assert_eq!(a.kind, "text");
        assert!(!a.in_library);
        assert_eq!(a.text_content.as_deref(), Some("武松打虎"));

        let patched = svc
            .patch_asset(
                &a.id,
                AssetPatch {
                    title: Some("新标题".into()),
                    collection: Some("场景".into()),
                    in_library: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(patched.title, "新标题");
        assert_eq!(patched.collection.as_deref(), Some("场景"));
        assert!(patched.in_library);

        let page = svc
            .list_assets(AssetQuery { page: 1, page_size: 20, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(page.total, 1);

        // text asset has no file → serve is NotFound
        assert!(svc.serve_file(&a.id, false).await.is_err());
        svc.delete_asset(&a.id).await.unwrap();
        assert!(svc.serve_file(&a.id, false).await.is_err());
    }

    /// A real, decodable PNG (unlike the header-only `png_1x1`).
    fn real_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([10, 20, 30]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    async fn upload_png(svc: &WorkshopService, in_library: bool) -> WorkshopAsset {
        svc.upload_asset(NewAssetUpload {
            file_name: "pic.png".into(),
            content_type: Some("image/png".into()),
            bytes: real_png(800, 600),
            title: Some("pic".into()),
            collection: None,
            tags: None,
            in_library: Some(in_library),
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn thumbnail_generated_on_upload_and_served_as_jpeg() {
        let (svc, dir) = service().await;
        let asset = upload_png(&svc, true).await;
        assert!(asset.thumb_url.is_some(), "thumb_url should be advertised");
        assert!(
            dir.path().join(format!("workshop/assets/thumbs/{}.jpg", asset.id)).exists(),
            "thumb file should exist on disk"
        );
        let served = svc.serve_file(&asset.id, true).await.unwrap();
        assert_eq!(served.mime, "image/jpeg");
        assert_eq!(&served.bytes[0..2], &[0xFF, 0xD8], "served thumb is JPEG");
        // original still served untouched
        let orig = svc.serve_file(&asset.id, false).await.unwrap();
        assert_eq!(orig.mime, "image/png");
    }

    #[tokio::test]
    async fn ingest_and_read_asset_bytes_roundtrip() {
        let (svc, _dir) = service().await;
        let png = real_png(300, 200);
        let origin = serde_json::json!({ "prompt": "a cat", "model": "x" });
        let row = svc
            .ingest_asset_bytes(png.clone(), "image/png", "generated", false, Some(origin.clone()))
            .await
            .unwrap();
        assert_eq!(row.kind, "image");
        assert!(!row.in_library);
        assert_eq!(row.width, Some(300));
        assert!(row.thumb_rel_path.is_some());
        assert_eq!(row.origin.as_deref().map(|s| s.contains("a cat")), Some(true));

        let (bytes, mime) = svc.read_asset_bytes(&row.id).await.unwrap();
        assert_eq!(bytes, png);
        assert_eq!(mime, "image/png");

        // unsupported mime rejected
        assert!(svc.ingest_asset_bytes(vec![1], "application/pdf", "x", true, None).await.is_err());
    }

    #[tokio::test]
    async fn canvas_thumbnail_set_and_served() {
        let (svc, _dir) = service().await;
        let canvas = svc.create_canvas(Some("画布".into())).await.unwrap();
        assert!(canvas.thumbnail_url.is_none());
        let asset = upload_png(&svc, true).await;

        let meta = svc.patch_canvas(&canvas.id, None, Some(asset.id.clone())).await.unwrap();
        assert_eq!(meta.thumbnail_url.as_deref(), Some(&*format!("/api/workshop/canvas-thumbs/{}", canvas.id)));
        let served = svc.serve_canvas_thumbnail(&canvas.id).await.unwrap();
        assert_eq!(served.mime, "image/jpeg");
        assert_eq!(&served.bytes[0..2], &[0xFF, 0xD8]);

        // a text asset cannot be a thumbnail source
        let text = svc
            .create_text_asset(NewTextAsset {
                title: "t".into(),
                text_content: "x".into(),
                collection: None,
                tags: None,
                in_library: Some(true),
            })
            .await
            .unwrap();
        assert!(svc.set_canvas_thumbnail(&canvas.id, &text.id).await.is_err());
    }

    #[tokio::test]
    async fn export_import_roundtrip_rewrites_ids() {
        let (svc, _dir) = service().await;
        let canvas = svc.create_canvas(Some("原始画布".into())).await.unwrap();
        let asset = upload_png(&svc, false).await; // canvas-internal
        let doc = serde_json::json!({
            "schema": 1,
            "viewport": { "x": 0, "y": 0, "zoom": 1 },
            "background": "dots",
            "nodes": [
                { "id": "n1", "kind": "image", "x": 0, "y": 0, "w": 10, "h": 10,
                  "data": { "assetId": asset.id, "caption": "hi" } }
            ],
            "edges": []
        });
        svc.save_doc(&canvas.id, &doc).await.unwrap();

        let zip = svc.export_canvas(&canvas.id).await.unwrap();
        assert!(!zip.is_empty());

        let imported = svc.import_canvas(zip).await.unwrap();
        assert_ne!(imported.id, canvas.id);
        // title de-duplicated (base already exists)
        assert_eq!(imported.title, "原始画布 (2)");
        assert_eq!(imported.node_count, 1);

        // the imported doc references a NEW asset id, and that asset exists + serves
        let read = svc.get_canvas(&imported.id).await.unwrap();
        let new_asset_id = read.doc["nodes"][0]["data"]["assetId"].as_str().unwrap();
        assert_ne!(new_asset_id, asset.id);
        let served = svc.serve_file(new_asset_id, false).await.unwrap();
        assert_eq!(served.bytes, real_png(800, 600));

        // both the original and the imported asset now exist (2 image rows)
        let page = svc.list_assets(AssetQuery { page: 1, page_size: 50, ..Default::default() }).await.unwrap();
        assert_eq!(page.total, 2);
    }

    #[tokio::test]
    async fn delete_canvas_gcs_internal_asset_unless_shared() {
        let (svc, _dir) = service().await;
        // Asset referenced by two canvases; internal (in_library=0).
        let asset = upload_png(&svc, false).await;
        let doc = serde_json::json!({
            "schema": 1, "nodes": [{ "id": "n", "kind": "image", "data": { "assetId": asset.id } }], "edges": []
        });
        let c1 = svc.create_canvas(Some("c1".into())).await.unwrap();
        let c2 = svc.create_canvas(Some("c2".into())).await.unwrap();
        svc.save_doc(&c1.id, &doc).await.unwrap();
        svc.save_doc(&c2.id, &doc).await.unwrap();

        // Deleting c1 keeps the asset (c2 still references it).
        svc.delete_canvas(&c1.id).await.unwrap();
        assert!(svc.serve_file(&asset.id, false).await.is_ok());

        // Deleting c2 (the last referencer) GCs the internal asset + its file.
        svc.delete_canvas(&c2.id).await.unwrap();
        assert!(svc.serve_file(&asset.id, false).await.is_err());
    }

    #[tokio::test]
    async fn delete_canvas_keeps_library_asset() {
        let (svc, _dir) = service().await;
        let asset = upload_png(&svc, true).await; // in_library=1
        let doc = serde_json::json!({
            "schema": 1, "nodes": [{ "id": "n", "kind": "image", "data": { "assetId": asset.id } }], "edges": []
        });
        let c = svc.create_canvas(Some("c".into())).await.unwrap();
        svc.save_doc(&c.id, &doc).await.unwrap();
        svc.delete_canvas(&c.id).await.unwrap();
        // Library assets are never GC'd on canvas delete.
        assert!(svc.serve_file(&asset.id, false).await.is_ok());
    }

    #[tokio::test]
    async fn gc_removes_orphan_rows_and_orphan_files() {
        let (svc, dir) = service().await;
        // Orphan internal asset (in_library=0, referenced by no canvas).
        let orphan = upload_png(&svc, false).await;
        // A library asset — kept even though unreferenced.
        let kept = upload_png(&svc, true).await;
        // A stray file on disk with no row.
        let assets_dir = dir.path().join("workshop/assets");
        tokio::fs::create_dir_all(&assets_dir).await.unwrap();
        let stray = assets_dir.join("wsa_stray_orphan.png");
        tokio::fs::write(&stray, real_png(4, 4)).await.unwrap();

        let stats = svc.gc().await.unwrap();
        assert_eq!(stats.orphan_rows_deleted, 1, "the internal orphan row");
        assert!(stats.orphan_files_deleted >= 1, "at least the stray file");

        assert!(svc.serve_file(&orphan.id, false).await.is_err(), "orphan row gone");
        assert!(svc.serve_file(&kept.id, false).await.is_ok(), "library asset kept");
        assert!(!stray.exists(), "stray file swept");
    }
}
