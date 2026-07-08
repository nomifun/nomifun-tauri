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
use serde_json::Value;

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

    /// Persist a generated text artifact as a `kind='text'` asset row — no file,
    /// the body lives inline in `text_content` (mirrors the workshop layer's
    /// `create_text_asset` row shape). `in_library` is honored as the engine
    /// passed it; `title` is derived from the origin prompt.
    async fn persist_text(
        &self,
        bytes: Vec<u8>,
        mime: String,
        in_library: bool,
        origin: &Value,
    ) -> Result<String, CreationError> {
        let id = generate_prefixed_id("wsa");
        let now = now_ms();
        let row = WorkshopAssetRow {
            id: id.clone(),
            kind: "text".to_string(),
            title: title_from_origin(origin, &id),
            collection: None,
            tags: "[]".to_string(),
            rel_path: None,
            thumb_rel_path: None,
            mime: Some(mime),
            width: None,
            height: None,
            bytes: None,
            text_content: Some(String::from_utf8_lossy(&bytes).into_owned()),
            in_library,
            origin: serde_json::to_string(origin).ok(),
            created_at: now,
            updated_at: now,
        };
        self.repo
            .create_asset(&row)
            .await
            .map(|saved| saved.id)
            .map_err(|e| CreationError::new("asset_index", format!("register text asset row: {e}")))
    }
}

#[async_trait]
impl AssetSink for WorkshopAssetBridge {
    async fn persist(&self, asset: PersistAsset) -> Result<String, CreationError> {
        let PersistAsset { canvas_id, node_id: _, bytes, mime, in_library, origin } = asset;
        // Tie the produced asset to its canvas via origin JSON (already stamped);
        // the explicit column set on `workshop_assets` has no canvas_id, matching
        // the contract (canvas linkage lives in origin + the node's resultAssetIds).
        let _ = canvas_id;

        // Text artifacts have no file: index them as `kind='text'` rows carrying
        // the body inline in `text_content`.
        if mime.starts_with("text/plain") {
            return self.persist_text(bytes, mime, in_library, &origin).await;
        }

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
        let now = now_ms();
        let row = WorkshopAssetRow {
            id: id.clone(),
            kind: kind.to_string(),
            title: title_from_origin(&origin, &id),
            collection: None,
            tags: "[]".to_string(),
            rel_path: Some(rel_path),
            thumb_rel_path: None,
            mime: Some(mime),
            width: None,  // best-effort omitted (P0); the workshop upload path fills these
            height: None,
            bytes: Some(byte_len),
            text_content: None,
            in_library,
            origin: origin_json,
            created_at: now,
            updated_at: now,
        };

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

        // File-backed assets (image/video) are read from disk; text assets carry
        // their body inline (`text_content`, no file) — return it as UTF-8 bytes
        // so a text asset can be reused as a prompt input.
        if let Some(rel) = row.rel_path {
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
        } else if let Some(text) = row.text_content {
            let mime = row.mime.unwrap_or_else(|| "text/plain; charset=utf-8".to_string());
            Ok(LoadedAsset { bytes: text.into_bytes(), mime })
        } else {
            Err(CreationError::new(
                "asset_no_file",
                format!("input asset '{asset_id}' has no file or text body"),
            ))
        }
    }
}

/// A short, human-ish title from the origin prompt (falls back to the asset id)
/// — the asset library shows this.
fn title_from_origin(origin: &Value, fallback_id: &str) -> String {
    origin
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(60).collect::<String>())
        .unwrap_or_else(|| fallback_id.to_string())
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
    use nomifun_db::{SqliteWorkshopRepository, init_database_memory};
    use serde_json::json;

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

    #[test]
    fn title_from_origin_truncates_or_falls_back() {
        assert_eq!(title_from_origin(&json!({"prompt": "a fox"}), "wsa_1"), "a fox");
        assert_eq!(title_from_origin(&json!({"prompt": "   "}), "wsa_1"), "wsa_1");
        assert_eq!(title_from_origin(&json!({}), "wsa_1"), "wsa_1");
        let long = "x".repeat(80);
        assert_eq!(title_from_origin(&json!({"prompt": long}), "wsa_1").chars().count(), 60);
    }

    async fn bridge() -> (WorkshopAssetBridge, tempfile::TempDir, nomifun_db::Database) {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IWorkshopRepository> = Arc::new(SqliteWorkshopRepository::new(db.pool().clone()));
        let dir = tempfile::tempdir().unwrap();
        let bridge = WorkshopAssetBridge::new(dir.path().to_path_buf(), repo);
        (bridge, dir, db)
    }

    #[tokio::test]
    async fn persist_text_writes_row_not_file() {
        let (bridge, dir, _db) = bridge().await;
        let id = bridge
            .persist(PersistAsset {
                canvas_id: Some("wsc_1".into()),
                node_id: Some("n_1".into()),
                bytes: "generated story".as_bytes().to_vec(),
                mime: "text/plain; charset=utf-8".into(),
                in_library: true,
                origin: json!({"prompt": "write a story about a fox", "model": "gpt-4o"}),
            })
            .await
            .unwrap();
        assert!(id.starts_with("wsa_"));

        let row = bridge.repo.get_asset(&id).await.unwrap().unwrap();
        assert_eq!(row.kind, "text");
        assert_eq!(row.text_content.as_deref(), Some("generated story"));
        assert_eq!(row.rel_path, None);
        assert!(row.mime.as_deref().unwrap().starts_with("text/plain"));
        assert_eq!(row.title, "write a story about a fox");
        assert!(row.in_library);
        assert!(row.origin.is_some(), "origin JSON should be stamped");

        // No file written under the assets dir (text assets are file-less).
        let assets_dir = dir.path().join("workshop").join("assets");
        let count = std::fs::read_dir(&assets_dir).map(|rd| rd.count()).unwrap_or(0);
        assert_eq!(count, 0, "text asset must not write a file");
    }

    #[tokio::test]
    async fn persist_text_honors_in_library_false_and_title_fallback() {
        let (bridge, _dir, _db) = bridge().await;
        let id = bridge
            .persist(PersistAsset {
                canvas_id: None,
                node_id: None,
                bytes: b"draft".to_vec(),
                mime: "text/plain; charset=utf-8".into(),
                in_library: false,
                origin: json!({}),
            })
            .await
            .unwrap();
        let row = bridge.repo.get_asset(&id).await.unwrap().unwrap();
        assert!(!row.in_library);
        assert_eq!(row.title, id, "title falls back to id when no prompt");
    }

    #[tokio::test]
    async fn load_text_asset_returns_utf8_bytes() {
        let (bridge, _dir, _db) = bridge().await;
        let id = bridge
            .persist(PersistAsset {
                canvas_id: None,
                node_id: None,
                bytes: "reusable prompt text".as_bytes().to_vec(),
                mime: "text/plain; charset=utf-8".into(),
                in_library: true,
                origin: json!({"prompt": "seed"}),
            })
            .await
            .unwrap();
        let loaded = bridge.load(&id).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&loaded.bytes), "reusable prompt text");
        assert!(loaded.mime.starts_with("text/plain"));
    }
}
