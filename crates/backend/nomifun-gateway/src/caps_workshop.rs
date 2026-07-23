//! 创意工坊 (Creative Workshop) capabilities (registry form). The 画布助手
//! (canvas assistant) surface: the companion / a conversation agent can READ a
//! canvas's structure, list assets, APPLY node ops (respecting an open
//! frontend's write authority via the workshop service's op queue), and TRIGGER
//! + inspect generation tasks.
//!
//! Write authority: `nomi_workshop_apply_ops` never edits `canvas.json` under an
//! open editor — the workshop service queues ops for the live frontend to apply,
//! and only applies structural ops (`add_node`/`connect`) directly when no
//! frontend is polling. See `nomifun_workshop::agent_ops`.

use std::sync::Arc;

use nomifun_common::{
    ProviderId, WorkshopAssetId, WorkshopCanvasId, WorkshopNodeId,
};
use nomifun_creation::{CreationInput, NewCreationTask};
use nomifun_workshop::AgentOp;
use nomifun_workshop::service::AssetQuery;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::{CallerCtx, GatewayDeps};
use crate::registry::{Capability, CapabilityMeta, DangerTier};
use crate::server::ok;

fn deserialize_model_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty() || value.trim() != value {
        return Err(serde::de::Error::custom(
            "model must be a non-empty trimmed natural key",
        ));
    }
    Ok(value)
}

/// Max chars of prompt/caption/text folded into a node summary.
const SUMMARY_TEXT_MAX: usize = 120;
/// Max nodes/edges emitted in a canvas summary. A valid canvas doc (≤ 8 MB) can
/// carry tens of thousands of small nodes; bounding the tool result keeps a
/// single `get_canvas` from flooding the agent's context / token budget.
const MAX_SUMMARY_NODES: usize = 200;
const MAX_SUMMARY_EDGES: usize = 400;
/// Default / max canvases returned by `list_canvases`.
const LIST_CANVASES_DEFAULT: i64 = 100;
const LIST_CANVASES_MAX: i64 = 200;

// ── param structs (single source: schema + runtime) ──────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListCanvasesParams {
    /// Optional case-insensitive substring filter over canvas titles.
    #[serde(default)]
    query: Option<String>,
    /// Max canvases to return (default 100, capped at 200).
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetCanvasParams {
    /// Canvas id (bare UUIDv7) from nomi_workshop_list_canvases.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    canvas_id: WorkshopCanvasId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListAssetsParams {
    /// Optional case-insensitive title search.
    #[serde(default)]
    q: Option<String>,
    /// Optional kind filter: `image` | `video` | `text`.
    #[serde(default)]
    kind: Option<String>,
    /// Max rows to return (default 20, capped at 50).
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ApplyOpsParams {
    /// Target canvas id (bare UUIDv7).
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    canvas_id: WorkshopCanvasId,
    /// Ordered operations to apply. Each is tagged by `type`:
    /// `add_node` (create a node), `connect` (link two EXISTING node ids),
    /// `update_node_data` (shallow-merge a node's data), `delete_node`.
    /// Note: when the canvas is open in an editor, ops are queued for that editor
    /// to apply; `connect` must reference node ids that already exist (call
    /// nomi_workshop_get_canvas first).
    ops: Vec<AgentOp>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GenerateParams {
    /// Provider id (from the providers catalog) to run the generation on.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    provider_id: ProviderId,
    /// Model id/name available on that provider.
    #[serde(deserialize_with = "deserialize_model_name")]
    model: String,
    /// Capability: `t2i` | `i2i` | `inpaint` | `t2v` | `i2v` | `v2v` | `tts` | `text`.
    capability: String,
    /// The generation prompt.
    prompt: String,
    /// Optional reference asset ids (bare UUIDv7) fed as `reference` inputs.
    #[serde(default)]
    #[schemars(schema_with = "crate::id_schema::optional_canonical_uuid_v7_array_schema")]
    ref_asset_ids: Option<Vec<WorkshopAssetId>>,
    /// Optional canvas id to associate the produced asset(s) with.
    #[serde(default)]
    #[schemars(schema_with = "crate::id_schema::optional_canonical_uuid_v7_schema")]
    canvas_id: Option<WorkshopCanvasId>,
    /// Extra provider params (`width`/`height`/`quality`/`count`/`seconds`/…).
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetTaskParams {
    /// Creation task business id (bare UUIDv7) returned by nomi_workshop_generate.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    #[serde(deserialize_with = "deserialize_creation_task_id")]
    creation_task_id: String,
}

// ── handlers ──────────────────────────────────────────────────────────────

fn deserialize_creation_task_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    nomifun_common::validate_uuidv7(&value).map_err(serde::de::Error::custom)?;
    Ok(value)
}

async fn list_canvases(deps: Arc<GatewayDeps>, p: ListCanvasesParams) -> Value {
    let query = p.query.map(|q| q.trim().to_lowercase()).filter(|q| !q.is_empty());
    let limit = p.limit.unwrap_or(LIST_CANVASES_DEFAULT).clamp(1, LIST_CANVASES_MAX) as usize;
    match deps.workshop_repo.list_canvases().await {
        Ok(rows) => {
            let filtered: Vec<_> = rows
                .into_iter()
                .filter(|c| query.as_deref().is_none_or(|q| c.title.to_lowercase().contains(q)))
                .collect();
            let total = filtered.len();
            let truncated = total > limit;
            let canvases: Vec<Value> = filtered
                .into_iter()
                .take(limit)
                .map(|c| {
                    json!({
                        "canvas_id": c.canvas_id,
                        "title": c.title,
                        "node_count": c.node_count,
                    })
                })
                .collect();
            ok(json!({ "total": total, "truncated": truncated, "canvases": canvases }))
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn get_canvas(deps: Arc<GatewayDeps>, p: GetCanvasParams) -> Value {
    match deps.workshop_service.get_canvas(p.canvas_id.as_str()).await {
        Ok(c) => ok(summarize_canvas(&c.meta, &c.doc)),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn list_assets(deps: Arc<GatewayDeps>, p: ListAssetsParams) -> Value {
    let limit = p.limit.unwrap_or(20).clamp(1, 50);
    let query = AssetQuery {
        kind: p.kind.filter(|s| !s.trim().is_empty()),
        q: p.q.filter(|s| !s.trim().is_empty()),
        page: 1,
        page_size: limit,
        ..Default::default()
    };
    match deps.workshop_service.list_assets(query).await {
        Ok(page) => {
            let items: Vec<Value> = page
                .items
                .iter()
                .map(|a| {
                    json!({
                        "asset_id": a.asset_id,
                        "kind": a.kind,
                        "title": a.title,
                        "collection": a.collection,
                        "tags": a.tags,
                        "mime": a.mime,
                        "width": a.width,
                        "height": a.height,
                        "in_library": a.in_library,
                    })
                })
                .collect();
            ok(json!({ "total": page.total, "items": items }))
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn apply_ops(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ApplyOpsParams) -> Value {
    // Attribution only (not an access scope) — recorded on the service log.
    let source = if let Some(companion_id) = &ctx.companion_id {
        format!("companion:{companion_id}")
    } else if let Some(conversation_id) = &ctx.conversation_id {
        format!("conversation:{conversation_id}")
    } else {
        "remote".to_owned()
    };
    match deps
        .workshop_service
        .apply_agent_ops(p.canvas_id.as_str(), p.ops, &source)
        .await
    {
        Ok(applied) => ok(json!({ "ops": applied })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn generate(deps: Arc<GatewayDeps>, p: GenerateParams) -> Value {
    // The prompt lives inside `params` per the creation task contract (§3.3).
    let mut params = match p.params {
        Some(Value::Object(m)) => m,
        Some(_) => return json!({ "error": "params must be a JSON object" }),
        None => serde_json::Map::new(),
    };
    params.insert("prompt".into(), json!(p.prompt));
    let inputs: Vec<CreationInput> = p
        .ref_asset_ids
        .unwrap_or_default()
        .into_iter()
        .map(|asset_id| CreationInput {
            asset_id: asset_id.into_string(),
            role: "reference".into(),
        })
        .collect();
    let task = NewCreationTask {
        canvas_id: p.canvas_id.map(WorkshopCanvasId::into_string),
        node_id: None,
        provider_id: p.provider_id.into_string(),
        model: p.model,
        capability: p.capability,
        params: Value::Object(params),
        inputs,
    };
    match deps.creation_service.create_task(task).await {
        Ok(t) => ok(json!({
            "creation_task_id": t.creation_task_id,
            "status": t.status,
            "capability": t.capability,
            "provider_id": t.provider_id,
            "model": t.model,
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn get_task(deps: Arc<GatewayDeps>, p: GetTaskParams) -> Value {
    match deps.creation_service.get_task(&p.creation_task_id).await {
        Ok(t) => ok(json!({
            "creation_task_id": t.creation_task_id,
            "status": t.status,
            "capability": t.capability,
            "provider_id": t.provider_id,
            "model": t.model,
            "result_asset_ids": t.result_asset_ids,
            "error": t.error,
            "canvas_id": t.canvas_id,
            "node_id": t.node_id,
            "submitted_at": t.submitted_at,
            "finished_at": t.finished_at,
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── summary helpers ─────────────────────────────────────────────────────────

/// Build a compact, base64-free canvas summary for the agent: node briefs +
/// edges + meta. Deliberately omits asset bytes/data URLs.
fn summarize_canvas(meta: &nomifun_workshop::WorkshopCanvasMeta, doc: &Value) -> Value {
    let node_arr = doc.get("nodes").and_then(Value::as_array);
    let total_nodes = node_arr.map(Vec::len).unwrap_or(0);
    let nodes: Vec<Value> = node_arr
        .map(|arr| arr.iter().filter_map(summarize_node).take(MAX_SUMMARY_NODES).collect())
        .unwrap_or_default();
    let edge_arr = doc.get("edges").and_then(Value::as_array);
    let total_edges = edge_arr.map(Vec::len).unwrap_or(0);
    let edges: Vec<Value> = edge_arr
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let from = e.get("from").and_then(Value::as_str)?;
                    let to = e.get("to").and_then(Value::as_str)?;
                    WorkshopNodeId::parse(from).ok()?;
                    WorkshopNodeId::parse(to).ok()?;
                    Some(json!({ "from": from, "to": to }))
                })
                .take(MAX_SUMMARY_EDGES)
                .collect()
        })
        .unwrap_or_default();
    json!({
        "canvas_id": meta.canvas_id,
        "title": meta.title,
        "node_count": meta.node_count,
        "updated_at": meta.updated_at,
        "nodes": nodes,
        "edges": edges,
        "total_nodes": total_nodes,
        "total_edges": total_edges,
        "nodes_truncated": total_nodes > MAX_SUMMARY_NODES,
        "edges_truncated": total_edges > MAX_SUMMARY_EDGES,
    })
}

/// One node's brief: id/kind plus (when present) a truncated text (prompt →
/// content → caption), status, referenced asset id, and result asset count.
fn summarize_node(node: &Value) -> Option<Value> {
    let id = node.get("id").and_then(Value::as_str)?;
    WorkshopNodeId::parse(id).ok()?;
    let data = node.get("data");
    let text = data.and_then(|d| {
        d.get("prompt")
            .and_then(Value::as_str)
            .or_else(|| d.get("content").and_then(Value::as_str))
            .or_else(|| d.get("caption").and_then(Value::as_str))
    });
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), json!(id));
    if let Some(kind) = node.get("kind").and_then(Value::as_str).filter(|kind| !kind.is_empty()) {
        obj.insert("kind".into(), json!(kind));
    }
    if let Some(t) = text.filter(|t| !t.trim().is_empty()) {
        obj.insert("text".into(), json!(truncate_chars(t, SUMMARY_TEXT_MAX)));
    }
    if let Some(s) = data.and_then(|d| d.get("status").and_then(Value::as_str)) {
        obj.insert("status".into(), json!(s));
    }
    if let Some(a) = data
        .and_then(|d| d.get("assetId").and_then(Value::as_str))
        .filter(|id| WorkshopAssetId::parse(*id).is_ok())
    {
        obj.insert("asset_id".into(), json!(a));
    }
    let result_count = data
        .and_then(|d| d.get("resultAssetIds").and_then(Value::as_array))
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .filter(|id| WorkshopAssetId::parse(*id).is_ok())
                .count()
        })
        .unwrap_or(0);
    if result_count > 0 {
        obj.insert("result_asset_count".into(), json!(result_count));
    }
    Some(Value::Object(obj))
}

/// Truncate to `max` chars (char-safe for CJK) with an ellipsis when clipped.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let clipped: String = s.chars().take(max).collect();
        format!("{clipped}…")
    }
}

pub(crate) fn register(out: &mut Vec<Capability>) {
    out.push(Capability::new::<ListCanvasesParams, _, _>(
        CapabilityMeta::new(
            "nomi_workshop_list_canvases",
            "workshop",
            "List 创意工坊 canvases (id / title / node_count), optionally filtered by a title substring.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| list_canvases(deps, p),
    ));
    out.push(Capability::new::<GetCanvasParams, _, _>(
        CapabilityMeta::new(
            "nomi_workshop_get_canvas",
            "workshop",
            "Read a 创意工坊 canvas's structure: a compact per-node brief (id/kind/prompt-or-text/status/asset refs) plus edges. No image bytes.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| get_canvas(deps, p),
    ));
    out.push(Capability::new::<ListAssetsParams, _, _>(
        CapabilityMeta::new(
            "nomi_workshop_list_assets",
            "workshop",
            "List 创意工坊 assets (id/kind/title/collection/tags/mime/size), optionally filtered by search text or kind. No image bytes.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| list_assets(deps, p),
    ));
    out.push(Capability::new::<ApplyOpsParams, _, _>(
        CapabilityMeta::new(
            "nomi_workshop_apply_ops",
            "workshop",
            "Apply node ops to a 创意工坊 canvas (add_node / connect / update_node_data / delete_node). Queued for an open editor to apply, or written directly when the canvas is closed.",
            DangerTier::Write,
        ),
        apply_ops,
    ));
    out.push(Capability::new::<GenerateParams, _, _>(
        CapabilityMeta::new(
            "nomi_workshop_generate",
            "workshop",
            "Submit a 创意工坊 generation task (image/video/text) via a provider+model, with an optional prompt, reference assets, and canvas association. Returns a creation_task_id UUIDv7 to poll.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| generate(deps, p),
    ));
    out.push(Capability::new::<GetTaskParams, _, _>(
        CapabilityMeta::new(
            "nomi_workshop_get_task",
            "workshop",
            "Inspect a 创意工坊 generation task by creation_task_id UUIDv7: status, produced asset ids, and any error.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| get_task(deps, p),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Registry, Surface};
    use nomifun_common::WorkshopCanvasId;

    const CREATION_TASK_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
    const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678902";

    fn meta() -> nomifun_workshop::WorkshopCanvasMeta {
        nomifun_workshop::WorkshopCanvasMeta {
            canvas_id: WorkshopCanvasId::new().into_string(),
            title: "c".into(),
            thumbnail_url: None,
            node_count: 250,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn summarize_canvas_caps_nodes_and_edges() {
        let node_ids: Vec<String> = (0..451).map(|_| WorkshopNodeId::new().into_string()).collect();
        let nodes: Vec<Value> = node_ids
            .iter()
            .take(250)
            .map(|id| json!({ "id": id, "kind": "image", "data": {} }))
            .collect();
        let edges: Vec<Value> = (0..450)
            .map(|i| json!({ "from": node_ids[i], "to": node_ids[i + 1] }))
            .collect();
        let doc = json!({ "nodes": nodes, "edges": edges });
        let summary = summarize_canvas(&meta(), &doc);
        assert_eq!(summary["nodes"].as_array().unwrap().len(), MAX_SUMMARY_NODES);
        assert_eq!(summary["edges"].as_array().unwrap().len(), MAX_SUMMARY_EDGES);
        assert_eq!(summary["total_nodes"], 250);
        assert_eq!(summary["total_edges"], 450);
        assert_eq!(summary["nodes_truncated"], true);
        assert_eq!(summary["edges_truncated"], true);
    }

    #[test]
    fn summarize_canvas_small_not_truncated() {
        let node_id = WorkshopNodeId::new().into_string();
        let doc = json!({ "nodes": [ { "id": node_id, "kind": "image" } ], "edges": [] });
        let summary = summarize_canvas(&meta(), &doc);
        assert_eq!(summary["nodes_truncated"], false);
        assert_eq!(summary["edges_truncated"], false);
        assert_eq!(summary["total_nodes"], 1);
        assert_eq!(summary["nodes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn get_task_params_require_named_canonical_uuidv7() {
        let params: GetTaskParams =
            serde_json::from_value(json!({ "creation_task_id": CREATION_TASK_ID })).unwrap();
        assert_eq!(params.creation_task_id, CREATION_TASK_ID);

        for invalid in [
            json!({ "creation_task_id": 1 }),
            json!({ "creation_task_id": "1" }),
            json!({ "creation_task_id": "task_0190f5fe-7c00-7a00-8abc-012345678901" }),
            json!({ "creation_task_id": "0190f5fe-7c00-4a00-8abc-012345678901" }),
            json!({ "creation_task_id": "0190F5FE-7C00-7A00-8ABC-012345678901" }),
            json!({ "creation_task_id": "0190f5fe7c007a008abc012345678901" }),
            json!({ "creation_task_id": "0190f5fe-7c00-7a00-8abc-012345678901 " }),
            json!({ "task_id": CREATION_TASK_ID }),
            json!({ "id": CREATION_TASK_ID }),
            json!({
                "creation_task_id": CREATION_TASK_ID,
                "task_id": CREATION_TASK_ID
            }),
        ] {
            assert!(
                serde_json::from_value::<GetTaskParams>(invalid.clone()).is_err(),
                "accepted invalid get-task arguments: {invalid}"
            );
        }
    }

    #[test]
    fn generate_params_require_provider_id_and_trimmed_model() {
        let params: GenerateParams = serde_json::from_value(json!({
            "provider_id": PROVIDER_ID,
            "model": "image-model",
            "capability": "t2i",
            "prompt": "cat"
        }))
        .unwrap();
        assert_eq!(params.provider_id.as_str(), PROVIDER_ID);
        assert_eq!(params.model, "image-model");

        for invalid in [
            json!({
                "backend": "openai",
                "model": "image-model",
                "capability": "t2i",
                "prompt": "cat"
            }),
            json!({
                "provider_id": PROVIDER_ID,
                "model": " image-model ",
                "capability": "t2i",
                "prompt": "cat"
            }),
            json!({
                "provider_id": "openai",
                "model": "image-model",
                "capability": "t2i",
                "prompt": "cat"
            }),
        ] {
            assert!(serde_json::from_value::<GenerateParams>(invalid).is_err());
        }
    }

    #[test]
    fn workshop_task_schema_exposes_only_creation_task_id() {
        let specs = Registry::global().tool_specs(Surface::Desktop);
        let spec = specs
            .iter()
            .find(|spec| spec.name == "nomi_workshop_get_task")
            .expect("nomi_workshop_get_task registered");
        let properties = spec
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("get-task properties");
        let id_schema = properties
            .get("creation_task_id")
            .expect("creation_task_id schema");

        assert_eq!(properties.len(), 1);
        assert!(!properties.contains_key("task_id"));
        assert!(!properties.contains_key("id"));
        assert_eq!(id_schema.get("type"), Some(&json!("string")));
        assert_eq!(
            id_schema.get("pattern"),
            Some(&json!(
                "^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
            ))
        );
    }
}
