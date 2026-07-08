//! 画布助手 (canvas assistant) agent-op vocabulary + in-memory op queue + a
//! conservative backend doc applier.
//!
//! ## Why a queue (frontend authority)
//! When a canvas is OPEN in a frontend, that frontend is the authoritative
//! writer of `canvas.json` — it re-serializes the WHOLE doc on a debounced
//! autosave. If the backend also mutated `canvas.json` under it, the next
//! autosave would clobber the change. So an agent's writes for an OPEN canvas are
//! **queued**: the live frontend polls, applies each op to the live react-flow
//! graph (which then autosaves), and ACKs.
//!
//! Only when NO frontend is polling (canvas CLOSED) does the backend apply the
//! safe structural subset (`add_node` / `connect`) straight to `canvas.json`.
//! Data-mutating ops (`update_node_data` / `delete_node`) are ALWAYS queued —
//! they wait for a frontend rather than risk a partial backend edit.
//!
//! The queue is in-memory (lost on restart — acceptable) with a 10-minute TTL so
//! ops nobody claims don't linger forever.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use nomifun_common::{generate_prefixed_id, now_ms};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Ops older than this (still unclaimed) are dropped on the next sweep.
const OPS_TTL_MS: i64 = 10 * 60 * 1000;
/// A canvas polled within this window is treated as "open" (a live frontend is
/// authoritative). ~3× the frontend poll interval (2.5 s).
const OPEN_WINDOW_MS: i64 = 8_000;
/// Max ops accepted in a single apply call (abuse guard).
pub const MAX_OPS_PER_CALL: usize = 64;

/// Node kinds an agent may create (M0 contract §4 interactive kinds only).
const CREATABLE_KINDS: [&str; 4] = ["image", "text", "video", "generator"];

/// One agent operation over a canvas graph. Wire-tagged by `type` (snake_case).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentOp {
    /// Create a node. Position/size auto-assigned when omitted.
    AddNode { node: AddNodeSpec },
    /// Connect two EXISTING nodes (directed, from → to).
    Connect {
        from_node_id: String,
        to_node_id: String,
    },
    /// Shallow-merge a patch into an existing node's `data`.
    UpdateNodeData { node_id: String, patch: Value },
    /// Delete a node (and its incident edges).
    DeleteNode { node_id: String },
}

/// The node payload for an `add_node` op. `data` is merged over per-kind defaults
/// (contract §4).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddNodeSpec {
    /// One of: `image` | `text` | `video` | `generator`.
    pub kind: String,
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    #[serde(default)]
    pub w: Option<f64>,
    #[serde(default)]
    pub h: Option<f64>,
    /// Per-kind data (`{ prompt, mode, … }` for generator, `{ caption }` for
    /// image, …). Merged over sensible defaults.
    #[serde(default)]
    pub data: Option<Value>,
}

impl AgentOp {
    /// Structural validation (the batch is rejected if ANY op is invalid, so the
    /// agent gets a clear self-correction signal rather than a partial apply).
    pub fn validate(&self) -> Result<(), String> {
        match self {
            AgentOp::AddNode { node } => {
                if !CREATABLE_KINDS.contains(&node.kind.as_str()) {
                    return Err(format!(
                        "add_node.kind '{}' invalid (expected image|text|video|generator)",
                        node.kind
                    ));
                }
                if let Some(d) = &node.data
                    && !d.is_object()
                {
                    return Err("add_node.data must be a JSON object".into());
                }
                Ok(())
            }
            AgentOp::Connect {
                from_node_id,
                to_node_id,
            } => {
                if from_node_id.trim().is_empty() || to_node_id.trim().is_empty() {
                    return Err("connect requires non-empty from_node_id and to_node_id".into());
                }
                if from_node_id == to_node_id {
                    return Err("connect from_node_id and to_node_id must differ".into());
                }
                Ok(())
            }
            AgentOp::UpdateNodeData { node_id, patch } => {
                if node_id.trim().is_empty() {
                    return Err("update_node_data requires a non-empty node_id".into());
                }
                if !patch.is_object() {
                    return Err("update_node_data.patch must be a JSON object".into());
                }
                Ok(())
            }
            AgentOp::DeleteNode { node_id } => {
                if node_id.trim().is_empty() {
                    return Err("delete_node requires a non-empty node_id".into());
                }
                Ok(())
            }
        }
    }

    /// Whether the backend applier may apply this op straight to the doc when no
    /// frontend is open (only structural adds/connects; data mutations wait).
    pub(crate) fn direct_applicable(&self) -> bool {
        matches!(self, AgentOp::AddNode { .. } | AgentOp::Connect { .. })
    }
}

/// How an op was dispatched (returned to the agent per op).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpDisposition {
    /// Queued for the open frontend to apply on its next poll.
    Queued,
    /// Applied to `canvas.json` directly (the canvas had no live frontend).
    Applied,
    /// Validated but not applied (e.g. `connect` referenced a missing node).
    Skipped,
}

/// The per-op outcome returned to the agent.
#[derive(Debug, Clone, Serialize)]
pub struct AppliedOp {
    pub op_id: String,
    pub disposition: OpDisposition,
    /// New node id for a directly-applied `add_node`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Human-readable reason (present for `skipped`, or a benign note like an
    /// already-existing edge).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A queued op the frontend pulls, applies, and ACKs.
#[derive(Debug, Clone, Serialize)]
pub struct PendingOp {
    pub op_id: String,
    pub op: AgentOp,
    /// Enqueue time (ms) for the TTL sweep — internal, not sent to the frontend.
    #[serde(skip)]
    enqueued_at: i64,
}

impl PendingOp {
    /// Construct a pending op stamped at the current time.
    pub fn new(op_id: String, op: AgentOp) -> Self {
        Self {
            op_id,
            op,
            enqueued_at: now_ms(),
        }
    }
}

/// Mint a fresh op id (`wso_…`).
pub fn new_op_id() -> String {
    generate_prefixed_id("wso")
}

#[derive(Default)]
struct CanvasOps {
    pending: Vec<PendingOp>,
    /// Last frontend poll (ms). Fresh ⇒ the canvas is open.
    last_poll_ms: i64,
}

/// In-memory agent-op queue shared by the workshop service. There is ONE
/// instance (held by the singleton [`crate::WorkshopService`]), so the gateway's
/// enqueue path and the REST poll/ack path act on the SAME state.
#[derive(Default)]
pub struct AgentOpsQueue {
    inner: Mutex<HashMap<String, CanvasOps>>,
}

impl AgentOpsQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Whether a frontend is actively polling this canvas (recent poll).
    pub fn is_open(&self, canvas_id: &str) -> bool {
        let guard = self.inner.lock().unwrap();
        guard
            .get(canvas_id)
            .is_some_and(|c| now_ms().saturating_sub(c.last_poll_ms) <= OPEN_WINDOW_MS)
    }

    /// Register that a frontend just opened this canvas for editing (its doc was
    /// loaded via the REST canvas-doc GET). Bumps the open marker WITHOUT
    /// returning ops, collapsing the "opened but the first pending-ops poll has
    /// not reached the backend yet" window to milliseconds — otherwise an
    /// agent's concurrent `apply_ops` in that gap would be direct-written to
    /// `canvas.json` and then silently clobbered by the editor's first autosave.
    pub fn mark_open(&self, canvas_id: &str) {
        let mut guard = self.inner.lock().unwrap();
        let entry = guard.entry(canvas_id.to_string()).or_default();
        entry.last_poll_ms = now_ms();
    }

    /// Append ops (already assigned op ids) to a canvas's queue.
    pub fn enqueue(&self, canvas_id: &str, ops: Vec<PendingOp>) {
        if ops.is_empty() {
            return;
        }
        let mut guard = self.inner.lock().unwrap();
        let entry = guard.entry(canvas_id.to_string()).or_default();
        entry.pending.extend(ops);
        prune_expired(&mut entry.pending, canvas_id);
    }

    /// Return the pending ops (NOT removed — removal is ACK-driven, so the poll is
    /// idempotent) and record the poll so the canvas registers as open.
    pub fn take_pending(&self, canvas_id: &str) -> Vec<PendingOp> {
        let mut guard = self.inner.lock().unwrap();
        let entry = guard.entry(canvas_id.to_string()).or_default();
        entry.last_poll_ms = now_ms();
        prune_expired(&mut entry.pending, canvas_id);
        entry.pending.clone()
    }

    /// Remove acked ops by id.
    pub fn ack(&self, canvas_id: &str, op_ids: &[String]) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(entry) = guard.get_mut(canvas_id) {
            let acked: HashSet<&str> = op_ids.iter().map(String::as_str).collect();
            entry.pending.retain(|p| !acked.contains(p.op_id.as_str()));
        }
    }
}

/// Drop pending ops older than the TTL, logging how many were discarded.
fn prune_expired(pending: &mut Vec<PendingOp>, canvas_id: &str) {
    let cutoff = now_ms() - OPS_TTL_MS;
    let before = pending.len();
    pending.retain(|p| p.enqueued_at >= cutoff);
    let dropped = before - pending.len();
    if dropped > 0 {
        tracing::warn!(
            canvas_id,
            dropped,
            "workshop agent-ops: dropped expired pending ops (TTL 10m)"
        );
    }
}

// ── Backend doc applier (closed-canvas path) ─────────────────────────────────

/// Default box size per creatable kind (mirrors the frontend `KIND_META`, so a
/// backend-applied node opens at a sensible size).
fn default_size(kind: &str) -> (f64, f64) {
    match kind {
        "text" => (240.0, 132.0),
        "video" => (300.0, 196.0),
        "generator" => (300.0, 220.0),
        _ => (240.0, 200.0), // image + fallback
    }
}

/// Sensible default `data` per kind so a backend-minted node renders (merged
/// UNDER any caller-provided data).
fn default_data(kind: &str) -> Value {
    match kind {
        "text" => json!({ "content": "" }),
        "video" => json!({ "assetId": null }),
        "generator" => json!({
            "mode": "image", "prompt": "", "params": {}, "mentions": [],
            "status": "idle", "resultAssetIds": []
        }),
        _ => json!({ "assetId": null }), // image
    }
}

/// Apply an `add_node` op to a doc, returning the new node id. Coordinates
/// default to a waterfall cascade when omitted.
pub fn apply_add_node(doc: &mut Value, spec: &AddNodeSpec) -> String {
    let count = doc
        .get("nodes")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let id = generate_prefixed_id("wsn");
    let (dw, dh) = default_size(&spec.kind);
    let x = spec.x.unwrap_or(80.0 + (count % 8) as f64 * 40.0);
    let y = spec.y.unwrap_or(80.0 + (count % 8) as f64 * 40.0);

    let mut data = default_data(&spec.kind);
    if let (Some(Value::Object(patch)), Value::Object(base)) = (&spec.data, &mut data) {
        for (k, v) in patch {
            base.insert(k.clone(), v.clone());
        }
    }

    let node = json!({
        "id": id, "kind": spec.kind,
        "x": x, "y": y,
        "w": spec.w.unwrap_or(dw), "h": spec.h.unwrap_or(dh),
        "data": data,
    });
    ensure_array(doc, "nodes").push(node);
    id
}

/// Apply a `connect` op. `Ok(Some(edge_id))` on a new edge, `Ok(None)` if it
/// already existed, `Err(reason)` if a referenced node is missing.
pub fn apply_connect(doc: &mut Value, from: &str, to: &str) -> Result<Option<String>, String> {
    let node_ids: HashSet<String> = doc
        .get("nodes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|n| n.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if !node_ids.contains(from) {
        return Err(format!("connect: from_node_id '{from}' not found on canvas"));
    }
    if !node_ids.contains(to) {
        return Err(format!("connect: to_node_id '{to}' not found on canvas"));
    }
    let edges = ensure_array(doc, "edges");
    let exists = edges.iter().any(|e| {
        e.get("from").and_then(Value::as_str) == Some(from)
            && e.get("to").and_then(Value::as_str) == Some(to)
    });
    if exists {
        return Ok(None);
    }
    let id = generate_prefixed_id("wse");
    edges.push(json!({ "id": id, "from": from, "to": to }));
    Ok(Some(id))
}

/// Borrow `doc[key]` as a JSON array, creating (or repairing) it if absent /
/// wrong-typed. `doc` is coerced to an object first if it somehow is not one.
fn ensure_array<'a>(doc: &'a mut Value, key: &str) -> &'a mut Vec<Value> {
    if !doc.is_object() {
        *doc = json!({});
    }
    let obj = doc.as_object_mut().expect("doc coerced to object above");
    let slot = obj
        .entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !slot.is_array() {
        *slot = Value::Array(Vec::new());
    }
    slot.as_array_mut().expect("array ensured above")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_op(kind: &str) -> AgentOp {
        AgentOp::AddNode {
            node: AddNodeSpec {
                kind: kind.into(),
                x: None,
                y: None,
                w: None,
                h: None,
                data: None,
            },
        }
    }

    #[test]
    fn validate_rejects_bad_ops() {
        assert!(add_op("image").validate().is_ok());
        assert!(add_op("loop").validate().is_err()); // not a creatable kind
        assert!(
            AgentOp::Connect {
                from_node_id: "a".into(),
                to_node_id: "a".into()
            }
            .validate()
            .is_err()
        );
        assert!(
            AgentOp::Connect {
                from_node_id: "a".into(),
                to_node_id: "b".into()
            }
            .validate()
            .is_ok()
        );
        assert!(
            AgentOp::UpdateNodeData {
                node_id: "".into(),
                patch: json!({})
            }
            .validate()
            .is_err()
        );
        assert!(
            AgentOp::UpdateNodeData {
                node_id: "n".into(),
                patch: json!([1, 2])
            }
            .validate()
            .is_err()
        ); // patch must be object
        assert!(
            AgentOp::DeleteNode { node_id: "  ".into() }
                .validate()
                .is_err()
        );
    }

    #[test]
    fn add_node_applies_defaults_and_merges_data() {
        let mut doc = json!({ "schema": 1, "nodes": [], "edges": [] });
        let spec = AddNodeSpec {
            kind: "generator".into(),
            x: Some(10.0),
            y: None,
            w: None,
            h: None,
            data: Some(json!({ "prompt": "a fox", "mode": "image" })),
        };
        let id = apply_add_node(&mut doc, &spec);
        assert!(id.starts_with("wsn_"));
        let node = &doc["nodes"][0];
        assert_eq!(node["id"], json!(id));
        assert_eq!(node["kind"], "generator");
        assert_eq!(node["x"], 10.0); // explicit coord kept
        assert_eq!(node["w"], 300.0); // generator default size
        assert_eq!(node["data"]["prompt"], "a fox"); // merged over default
        assert_eq!(node["data"]["status"], "idle"); // default preserved
    }

    #[test]
    fn connect_validates_nodes_and_dedupes() {
        let mut doc = json!({
            "nodes": [{ "id": "a" }, { "id": "b" }],
            "edges": []
        });
        // missing node → Err
        assert!(apply_connect(&mut doc, "a", "zzz").is_err());
        // fresh edge
        let e1 = apply_connect(&mut doc, "a", "b").unwrap();
        assert!(e1.is_some());
        assert_eq!(doc["edges"].as_array().unwrap().len(), 1);
        // duplicate → Ok(None), no new edge
        let e2 = apply_connect(&mut doc, "a", "b").unwrap();
        assert!(e2.is_none());
        assert_eq!(doc["edges"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn ensure_array_repairs_missing_and_wrong_type() {
        let mut doc = json!({ "nodes": "oops" }); // wrong type
        ensure_array(&mut doc, "nodes").push(json!({ "id": "x" }));
        ensure_array(&mut doc, "edges").push(json!({ "id": "e" }));
        assert_eq!(doc["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(doc["edges"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn queue_enqueue_take_ack_roundtrip() {
        let q = AgentOpsQueue::new();
        assert!(!q.is_open("c1"));
        // Enqueue does not mark open.
        q.enqueue(
            "c1",
            vec![
                PendingOp::new("wso_1".into(), add_op("image")),
                PendingOp::new("wso_2".into(), add_op("text")),
            ],
        );
        assert!(!q.is_open("c1"));

        // take_pending returns everything (idempotent) and marks the canvas open.
        let taken = q.take_pending("c1");
        assert_eq!(taken.len(), 2);
        assert!(q.is_open("c1"));
        // Still there until acked.
        assert_eq!(q.take_pending("c1").len(), 2);

        q.ack("c1", &["wso_1".to_string()]);
        let after = q.take_pending("c1");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].op_id, "wso_2");
    }

    #[test]
    fn ttl_drops_expired_pending() {
        let q = AgentOpsQueue::new();
        // Directly seed an already-expired op (private field reachable in-module).
        {
            let mut guard = q.inner.lock().unwrap();
            let entry = guard.entry("c1".to_string()).or_default();
            entry.pending.push(PendingOp {
                op_id: "old".into(),
                op: add_op("image"),
                enqueued_at: now_ms() - OPS_TTL_MS - 1_000,
            });
            entry.pending.push(PendingOp {
                op_id: "fresh".into(),
                op: add_op("text"),
                enqueued_at: now_ms(),
            });
        }
        let taken = q.take_pending("c1");
        assert_eq!(taken.len(), 1, "expired op swept");
        assert_eq!(taken[0].op_id, "fresh");
    }

    #[test]
    fn pending_op_serializes_without_enqueued_at() {
        let p = PendingOp::new("wso_x".into(), add_op("image"));
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["op_id"], "wso_x");
        assert_eq!(v["op"]["type"], "add_node");
        assert_eq!(v["op"]["node"]["kind"], "image");
        assert!(v.get("enqueued_at").is_none());
    }

    #[test]
    fn agent_op_deserializes_from_wire_tag() {
        let op: AgentOp = serde_json::from_value(json!({
            "type": "connect", "from_node_id": "a", "to_node_id": "b"
        }))
        .unwrap();
        assert!(matches!(op, AgentOp::Connect { .. }));
    }
}
