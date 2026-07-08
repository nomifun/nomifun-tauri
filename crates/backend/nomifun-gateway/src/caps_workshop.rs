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

use nomifun_creation::{CreationInput, NewCreationTask};
use nomifun_workshop::AgentOp;
use nomifun_workshop::service::AssetQuery;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::{CallerCtx, GatewayDeps};
use crate::registry::{Capability, CapabilityMeta, DangerTier};
use crate::server::ok;

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
struct ListCanvasesParams {
    /// Optional case-insensitive substring filter over canvas titles.
    #[serde(default)]
    query: Option<String>,
    /// Max canvases to return (default 100, capped at 200).
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
struct GetCanvasParams {
    /// Canvas id (`wsc_…`) from nomi_workshop_list_canvases.
    canvas_id: String,
}

#[derive(Deserialize, JsonSchema)]
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
struct ApplyOpsParams {
    /// Target canvas id (`wsc_…`).
    canvas_id: String,
    /// Ordered operations to apply. Each is tagged by `type`:
    /// `add_node` (create a node), `connect` (link two EXISTING node ids),
    /// `update_node_data` (shallow-merge a node's data), `delete_node`.
    /// Note: when the canvas is open in an editor, ops are queued for that editor
    /// to apply; `connect` must reference node ids that already exist (call
    /// nomi_workshop_get_canvas first).
    ops: Vec<AgentOp>,
}

#[derive(Deserialize, JsonSchema)]
struct GenerateParams {
    /// Provider id (from the providers catalog) to run the generation on.
    provider_id: String,
    /// Model id/name available on that provider.
    model: String,
    /// Capability: `t2i` | `i2i` | `inpaint` | `t2v` | `i2v` | `v2v` | `tts` | `text`.
    capability: String,
    /// The generation prompt.
    prompt: String,
    /// Optional reference asset ids (`wsa_…`) fed as `reference` inputs.
    #[serde(default)]
    ref_asset_ids: Option<Vec<String>>,
    /// Optional canvas id to associate the produced asset(s) with.
    #[serde(default)]
    canvas_id: Option<String>,
    /// Extra provider params (`width`/`height`/`quality`/`count`/`seconds`/…).
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
struct GetTaskParams {
    /// Generation task id (`wst_…`) from nomi_workshop_generate.
    task_id: String,
}

// ── handlers ──────────────────────────────────────────────────────────────

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
                        "id": c.id,
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
    match deps.workshop_service.get_canvas(&p.canvas_id).await {
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
                        "id": a.id,
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
    let source = ctx
        .companion_id
        .as_deref()
        .map(|c| format!("companion:{c}"))
        .unwrap_or_else(|| format!("conversation:{}", ctx.conversation_id));
    match deps.workshop_service.apply_agent_ops(&p.canvas_id, p.ops, &source).await {
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
        .filter(|id| !id.trim().is_empty())
        .map(|asset_id| CreationInput { asset_id, role: "reference".into() })
        .collect();
    let task = NewCreationTask {
        canvas_id: p.canvas_id,
        node_id: None,
        provider_id: p.provider_id,
        model: p.model,
        capability: p.capability,
        params: Value::Object(params),
        inputs,
    };
    match deps.creation_service.create_task(task).await {
        Ok(t) => ok(json!({
            "task_id": t.id,
            "status": t.status,
            "capability": t.capability,
            "provider_id": t.provider_id,
            "model": t.model,
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn get_task(deps: Arc<GatewayDeps>, p: GetTaskParams) -> Value {
    match deps.creation_service.get_task(&p.task_id).await {
        Ok(t) => ok(json!({
            "id": t.id,
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
        .map(|arr| arr.iter().take(MAX_SUMMARY_NODES).map(summarize_node).collect())
        .unwrap_or_default();
    let edge_arr = doc.get("edges").and_then(Value::as_array);
    let total_edges = edge_arr.map(Vec::len).unwrap_or(0);
    let edges: Vec<Value> = edge_arr
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let from = e.get("from").and_then(Value::as_str)?;
                    let to = e.get("to").and_then(Value::as_str)?;
                    Some(json!({ "from": from, "to": to }))
                })
                .take(MAX_SUMMARY_EDGES)
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": meta.id,
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
fn summarize_node(node: &Value) -> Value {
    let data = node.get("data");
    let text = data.and_then(|d| {
        d.get("prompt")
            .and_then(Value::as_str)
            .or_else(|| d.get("content").and_then(Value::as_str))
            .or_else(|| d.get("caption").and_then(Value::as_str))
    });
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), json!(node.get("id").and_then(Value::as_str).unwrap_or_default()));
    obj.insert("kind".into(), json!(node.get("kind").and_then(Value::as_str).unwrap_or_default()));
    if let Some(t) = text.filter(|t| !t.trim().is_empty()) {
        obj.insert("text".into(), json!(truncate_chars(t, SUMMARY_TEXT_MAX)));
    }
    if let Some(s) = data.and_then(|d| d.get("status").and_then(Value::as_str)) {
        obj.insert("status".into(), json!(s));
    }
    if let Some(a) = data.and_then(|d| d.get("assetId").and_then(Value::as_str)) {
        obj.insert("asset_id".into(), json!(a));
    }
    let result_count = data
        .and_then(|d| d.get("resultAssetIds").and_then(Value::as_array))
        .map(Vec::len)
        .unwrap_or(0);
    if result_count > 0 {
        obj.insert("result_asset_count".into(), json!(result_count));
    }
    Value::Object(obj)
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
            "Submit a 创意工坊 generation task (image/video/text) via a provider+model, with an optional prompt, reference assets, and canvas association. Returns a task id to poll.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| generate(deps, p),
    ));
    out.push(Capability::new::<GetTaskParams, _, _>(
        CapabilityMeta::new(
            "nomi_workshop_get_task",
            "workshop",
            "Inspect a 创意工坊 generation task by id: status, produced asset ids, and any error.",
            DangerTier::Read,
        ),
        |deps, _ctx, p| get_task(deps, p),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> nomifun_workshop::WorkshopCanvasMeta {
        nomifun_workshop::WorkshopCanvasMeta {
            id: "wsc_1".into(),
            title: "c".into(),
            thumbnail_url: None,
            node_count: 250,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn summarize_canvas_caps_nodes_and_edges() {
        let nodes: Vec<Value> = (0..250)
            .map(|i| json!({ "id": format!("n{i}"), "kind": "image", "data": {} }))
            .collect();
        let edges: Vec<Value> = (0..450)
            .map(|i| json!({ "from": format!("n{i}"), "to": format!("n{}", i + 1) }))
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
        let doc = json!({ "nodes": [ { "id": "a", "kind": "image" } ], "edges": [] });
        let summary = summarize_canvas(&meta(), &doc);
        assert_eq!(summary["nodes_truncated"], false);
        assert_eq!(summary["edges_truncated"], false);
        assert_eq!(summary["total_nodes"], 1);
        assert_eq!(summary["nodes"].as_array().unwrap().len(), 1);
    }
}
