//! Bridge wiring the 生成引擎 (`nomifun-creation`) to the 创意工坊 asset store
//! (`nomifun-workshop`'s data dir + `nomifun-db` index), without either domain
//! crate depending on the other.
//!
//! The creation engine defines two seams — [`AssetSink`] (persist a produced
//! artifact) and [`AssetSource`] (read a task input) — and this bridge
//! implements both over the workshop asset layout:
//! `{data_dir}/workshop/assets/{id}.{ext}` files + `workshop_assets` rows.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use nomifun_common::{generate_prefixed_id, now_ms};
use nomifun_creation::{AssetSink, AssetSource, CreationError, LoadedAsset, PersistAsset};
use nomifun_db::{IWorkshopRepository, WorkshopAssetRow};
use nomifun_workshop::WORKSHOP_REL_DIR;

/// Persists produced artifacts / reads input assets against the workshop store.
pub struct WorkshopAssetBridge {
    data_dir: PathBuf,
    repo: Arc<dyn IWorkshopRepository>,
}

impl WorkshopAssetBridge {
    pub fn new(data_dir: PathBuf, repo: Arc<dyn IWorkshopRepository>) -> Self {
        Self { data_dir, repo }
    }

    fn assets_dir(&self) -> PathBuf {
        self.data_dir.join(WORKSHOP_REL_DIR).join("assets")
    }
}

#[async_trait]
impl AssetSink for WorkshopAssetBridge {
    async fn persist(&self, asset: PersistAsset) -> Result<String, CreationError> {
        let PersistAsset { canvas_id, node_id: _, bytes, mime, origin } = asset;

        let id = generate_prefixed_id("wsa");
        let ext = ext_for_mime(&mime);
        let disk_name = format!("{id}.{ext}");
        let rel_path = format!("{WORKSHOP_REL_DIR}/assets/{disk_name}");
        let abs = self.assets_dir().join(&disk_name);

        // Write the file first so a crash between write + insert leaves an orphan
        // file (harmless, GC-able) rather than a row whose file is missing.
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| CreationError::new("asset_write", format!("create assets dir: {e}")))?;
        }
        let byte_len = bytes.len() as i64;
        tokio::fs::write(&abs, &bytes)
            .await
            .map_err(|e| CreationError::new("asset_write", format!("write asset file: {e}")))?;

        let kind = kind_for_mime(&mime);
        let origin_json = serde_json::to_string(&origin).ok();
        // A short, human-ish title derived from the origin prompt (falls back to
        // the asset id) — the asset library shows this.
        let title = origin
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(60).collect::<String>())
            .unwrap_or_else(|| id.clone());

        let now = now_ms();
        let row = WorkshopAssetRow {
            id: id.clone(),
            kind: kind.to_string(),
            title,
            collection: None,
            tags: "[]".to_string(),
            rel_path: Some(rel_path),
            thumb_rel_path: None,
            mime: Some(mime),
            width: None,  // best-effort omitted (P0); the workshop upload path fills these
            height: None,
            bytes: Some(byte_len),
            text_content: None,
            in_library: true, // generated products land in the library by default
            origin: origin_json,
            created_at: now,
            updated_at: now,
        };
        // Tie the produced asset to its canvas via origin JSON (already stamped);
        // the explicit column set on `workshop_assets` has no canvas_id, matching
        // the contract (canvas linkage lives in origin + the node's resultAssetIds).
        let _ = canvas_id;

        match self.repo.create_asset(&row).await {
            Ok(saved) => Ok(saved.id),
            Err(e) => {
                // Roll the orphaned file back on insert failure.
                let _ = tokio::fs::remove_file(&abs).await;
                Err(CreationError::new("asset_index", format!("register asset row: {e}")))
            }
        }
    }
}

#[async_trait]
impl AssetSource for WorkshopAssetBridge {
    async fn load(&self, asset_id: &str) -> Result<LoadedAsset, CreationError> {
        let row = self
            .repo
            .get_asset(asset_id)
            .await
            .map_err(|e| CreationError::new("asset_lookup", format!("asset lookup failed: {e}")))?
            .ok_or_else(|| CreationError::new("asset_not_found", format!("input asset '{asset_id}' not found")))?;
        let rel = row
            .rel_path
            .ok_or_else(|| CreationError::new("asset_no_file", format!("input asset '{asset_id}' has no file (text asset?)")))?;
        // rel_path values are minted by the workshop layer; reject traversal defensively.
        if rel.contains("..") || rel.contains('\0') {
            return Err(CreationError::new("asset_path", "asset path contains invalid traversal"));
        }
        let abs = self.data_dir.join(&rel);
        let bytes = tokio::fs::read(&abs)
            .await
            .map_err(|e| CreationError::new("asset_read", format!("read input asset '{asset_id}': {e}")))?;
        let mime = row.mime.unwrap_or_else(|| "application/octet-stream".to_string());
        Ok(LoadedAsset { bytes, mime })
    }
}

/// `image | video | text` for the workshop asset `kind` column, from a MIME.
fn kind_for_mime(mime: &str) -> &'static str {
    if mime.starts_with("video/") {
        "video"
    } else if mime.starts_with("image/") {
        "image"
    } else {
        // Produced artifacts are image/video; default to image for anything else.
        "image"
    }
}

/// A file extension for a produced-artifact MIME (best-effort; `bin` fallback).
fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_mappings() {
        assert_eq!(ext_for_mime("image/png"), "png");
        assert_eq!(ext_for_mime("image/jpeg"), "jpg");
        assert_eq!(ext_for_mime("video/mp4"), "mp4");
        assert_eq!(ext_for_mime("application/pdf"), "bin");
        assert_eq!(kind_for_mime("image/png"), "image");
        assert_eq!(kind_for_mime("video/mp4"), "video");
        assert_eq!(kind_for_mime("application/octet-stream"), "image");
    }
}
