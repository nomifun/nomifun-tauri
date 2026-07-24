//! Wire DTOs for the `/api/workshop/*` surface (contract §3.1/§3.2). All fields
//! are snake_case (serde default) per the wire contract. These are response
//! shapes the frontend `types.ts` mirrors; the domain crate owns them (the
//! shared `api-types` crate is not in this module's ownership).

use nomifun_common::{AppError, TimestampMs};
use nomifun_db::{WorkshopAssetRow, WorkshopCanvasRow};
use serde::Serialize;
use serde_json::Value;

/// A canvas index entry. `thumbnail_url` is populated once a canvas thumbnail
/// has been set (via `PATCH …/{canvas_id}` with `thumbnail_asset_id`); it points
/// at the dedicated `GET /api/workshop/canvas-thumbs/{canvas_id}` serve route.
#[derive(Debug, Clone, Serialize)]
pub struct WorkshopCanvasMeta {
    pub canvas_id: String,
    pub title: String,
    pub thumbnail_url: Option<String>,
    pub node_count: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

impl From<WorkshopCanvasRow> for WorkshopCanvasMeta {
    fn from(row: WorkshopCanvasRow) -> Self {
        // Advertise a thumbnail URL only when a thumbnail file was actually
        // written (rel_path present) — never a URL with no bytes behind it.
        let thumbnail_url = row
            .thumbnail_rel_path
            .as_ref()
            .map(|_| format!("/api/workshop/canvas-thumbs/{}", row.canvas_id));
        Self {
            canvas_id: row.canvas_id,
            title: row.title,
            thumbnail_url,
            node_count: row.node_count,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// A workshop asset. `url` always points at the files route (a `text` asset has
/// no binary, so its `url` 404s — the frontend uses `text_content` for those).
#[derive(Debug, Clone, Serialize)]
pub struct WorkshopAsset {
    pub asset_id: String,
    pub kind: String,
    pub title: String,
    pub collection: Option<String>,
    pub tags: Vec<String>,
    pub mime: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub bytes: Option<i64>,
    pub in_library: bool,
    pub text_content: Option<String>,
    pub origin: Option<Value>,
    pub url: String,
    pub thumb_url: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

impl TryFrom<WorkshopAssetRow> for WorkshopAsset {
    type Error = AppError;

    fn try_from(row: WorkshopAssetRow) -> Result<Self, Self::Error> {
        // Tags remain presentation metadata and can degrade to an empty list.
        // Origin is durable provenance: corruption must fail closed instead of
        // being silently presented as "no provenance".
        let tags = serde_json::from_str::<Vec<String>>(&row.tags).unwrap_or_default();
        let origin = row
            .origin
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
            .map_err(|error| {
                AppError::Internal(format!(
                    "workshop asset {} has invalid origin JSON: {error}",
                    row.asset_id
                ))
            })?;
        let url = format!("/api/workshop/files/{}", row.asset_id);
        let thumb_url = row
            .thumb_rel_path
            .as_ref()
            .map(|_| format!("/api/workshop/files/{}?thumb=1", row.asset_id));
        Ok(Self {
            asset_id: row.asset_id,
            kind: row.kind,
            title: row.title,
            collection: row.collection,
            tags,
            mime: row.mime,
            width: row.width,
            height: row.height,
            bytes: row.bytes,
            in_library: row.in_library,
            text_content: row.text_content,
            origin,
            url,
            thumb_url,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset_row() -> WorkshopAssetRow {
        let asset_id = "0190f5fe-7c00-7a00-8000-000000000001";
        WorkshopAssetRow {
            id: 1,
            asset_id: asset_id.into(),
            kind: "image".into(),
            title: "t".into(),
            collection: Some("角色".into()),
            tags: r#"["a","b"]"#.into(),
            rel_path: Some(format!("workshop/assets/{asset_id}.png")),
            thumb_rel_path: None,
            mime: Some("image/png".into()),
            width: Some(10),
            height: Some(20),
            bytes: Some(99),
            text_content: None,
            in_library: true,
            origin: Some(r#"{"prompt":"cat"}"#.into()),
            created_at: 1,
            updated_at: 2,
        }
    }

    #[test]
    fn asset_dto_parses_tags_origin_and_builds_url() {
        let dto = WorkshopAsset::try_from(asset_row()).unwrap();
        assert_eq!(dto.tags, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(dto.origin.unwrap()["prompt"], "cat");
        assert_eq!(
            dto.url,
            "/api/workshop/files/0190f5fe-7c00-7a00-8000-000000000001"
        );
        assert!(dto.thumb_url.is_none());
    }

    #[test]
    fn asset_dto_corrupt_tags_degrade_but_corrupt_origin_fails_closed() {
        let mut row = asset_row();
        row.tags = "not json".into();
        let dto = WorkshopAsset::try_from(row.clone()).unwrap();
        assert!(dto.tags.is_empty());

        row.origin = Some("also not json".into());
        assert!(matches!(
            WorkshopAsset::try_from(row),
            Err(AppError::Internal(message)) if message.contains("invalid origin JSON")
        ));
    }

    #[test]
    fn canvas_meta_advertises_thumbnail_when_rel_path_present() {
        let canvas_id = "0190f5fe-7c00-7a00-8000-000000000011";
        let row = WorkshopCanvasRow {
            id: 1,
            canvas_id: canvas_id.into(),
            title: "c".into(),
            thumbnail_rel_path: Some(format!("workshop/canvases/{canvas_id}/thumb.jpg")),
            node_count: 3,
            created_at: 1,
            updated_at: 2,
        };
        let meta = WorkshopCanvasMeta::from(row);
        assert_eq!(
            meta.thumbnail_url.as_deref(),
            Some("/api/workshop/canvas-thumbs/0190f5fe-7c00-7a00-8000-000000000011")
        );
        assert_eq!(meta.node_count, 3);
    }

    #[test]
    fn canvas_meta_no_thumbnail_url_when_absent() {
        let row = WorkshopCanvasRow {
            id: 2,
            canvas_id: "0190f5fe-7c00-7a00-8000-000000000012".into(),
            title: "c".into(),
            thumbnail_rel_path: None,
            node_count: 0,
            created_at: 1,
            updated_at: 2,
        };
        assert!(WorkshopCanvasMeta::from(row).thumbnail_url.is_none());
    }
}
