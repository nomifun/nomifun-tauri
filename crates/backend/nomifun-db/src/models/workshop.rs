use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `workshop_canvases` table (创意工坊 画布轻索引).
///
/// The canvas *body* (nodes/edges/viewport/settings) lives in a file
/// (`{data_dir}/workshop/canvases/{id}/canvas.json`) and is opaque to the
/// backend — this row only carries the metadata + a `node_count` the service
/// keeps in sync from the doc on each save.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct WorkshopCanvasRow {
    pub id: String,
    pub title: String,
    pub thumbnail_rel_path: Option<String>,
    pub node_count: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `workshop_assets` table (创意工坊 资产库).
///
/// Metadata is indexed here; the binary lives under the data dir at `rel_path`
/// (`workshop/assets/{id}.{ext}`). `text` assets carry their body in
/// `text_content` and have no file. `tags` / `origin` are stored as JSON TEXT
/// and parsed by the service layer.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct WorkshopAssetRow {
    pub id: String,
    /// `image | video | text`.
    pub kind: String,
    pub title: String,
    pub collection: Option<String>,
    /// JSON array of tag strings.
    pub tags: String,
    /// Relative to the data dir; `None` for text assets.
    pub rel_path: Option<String>,
    pub thumb_rel_path: Option<String>,
    pub mime: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub bytes: Option<i64>,
    pub text_content: Option<String>,
    /// `1` = appears in the asset library; `0` = canvas-internal material.
    pub in_library: bool,
    /// JSON object (`{prompt,model,provider_id,params,canvas_id,node_id,task_id}`); `None`.
    pub origin: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `creation_tasks` table (生成引擎 任务队列).
///
/// `params` / `error` / `result_asset_ids` are JSON TEXT parsed by the service
/// layer. `provider_id` is a `providers.id` (TEXT).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CreationTaskRow {
    pub id: String,
    pub canvas_id: Option<String>,
    pub node_id: Option<String>,
    pub provider_id: String,
    pub model: String,
    /// `t2i|i2i|inpaint|t2v|i2v|v2v|tts|text`.
    pub capability: String,
    /// JSON parameter snapshot.
    pub params: String,
    /// `queued|running|succeeded|failed|canceled`.
    pub status: String,
    /// JSON `{kind,message,http_status?}`; `None`.
    pub error: Option<String>,
    /// JSON array of `wsa_` asset ids.
    pub result_asset_ids: String,
    /// Remote task id for async submit→poll protocols (boot resume).
    pub remote_task_id: Option<String>,
    pub attempt: i64,
    pub submitted_at: TimestampMs,
    pub started_at: Option<TimestampMs>,
    pub finished_at: Option<TimestampMs>,
}
