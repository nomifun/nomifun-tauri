//! Memory-domain capabilities (registry form), backed by the companion memory
//! store — the desktop's long-term memory, same data as `/api/companion/memories`.
//!
//! Reference migration of `tools_memory.rs` onto the capability registry: the
//! `*Params` structs are now the SINGLE source (schema + runtime deserialization),
//! so the historical drift — `offset` readable at runtime but absent from the MCP
//! schema — is fixed by construction (it is a declared field here).

use std::sync::Arc;

use nomifun_companion::store::{MemoryFilter, MemoryScope};
use nomifun_common::CompanionMemoryId;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::GatewayDeps;
use crate::registry::{Capability, CapabilityMeta, DangerTier};
use crate::server::ok;

const DEFAULT_LIST_LIMIT: i64 = 50;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MemoryListParams {
    /// Filter by kind: profile / preference / knowledge / episode / task / affective.
    #[serde(default)]
    kind: Option<String>,
    /// Substring search over memory content.
    #[serde(default)]
    query: Option<String>,
    /// Include archived memories too (default false: active only).
    #[serde(default)]
    include_archived: Option<bool>,
    /// Maximum rows to return (default 50, clamped to 1..=200).
    #[serde(default)]
    limit: Option<i64>,
    /// Row offset for pagination (default 0).
    #[serde(default)]
    offset: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MemorySaveParams {
    /// The memory content — one self-contained fact.
    content: String,
    /// Kind: profile / preference / knowledge / episode / task / affective (default knowledge).
    #[serde(default)]
    kind: Option<String>,
    /// Optional tags.
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MemoryUpdateParams {
    /// The stable business id of the memory to update (from nomi_memory_list).
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    memory_id: CompanionMemoryId,
    /// New content (omit to keep).
    #[serde(default)]
    content: Option<String>,
    /// Pin (true) or unpin (false) the memory; pinned memories are always injected.
    #[serde(default)]
    pinned: Option<bool>,
    /// "active" or "archived".
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MemoryDeleteParams {
    /// The stable business id of the memory to permanently delete. Prefer
    /// archiving via nomi_memory_update unless the user explicitly asked to delete.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    memory_id: CompanionMemoryId,
}

async fn list(deps: Arc<GatewayDeps>, p: MemoryListParams) -> Value {
    let filter = MemoryFilter {
        kind: p.kind,
        q: p.query,
        status: if p.include_archived.unwrap_or(false) {
            None
        } else {
            Some("active".to_owned())
        },
        // The owner Agent view spans every companion's memories.
        scope_companion_id: None,
        limit: p.limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, 200),
        offset: p.offset.unwrap_or(0).max(0),
    };
    match deps.companion_service.list_memories(&filter).await {
        Ok(memories) => ok(memories),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn save(deps: Arc<GatewayDeps>, p: MemorySaveParams) -> Value {
    let content = p.content.trim();
    if content.is_empty() {
        return json!({ "error": "missing required field: content" });
    }
    let kind = p.kind.unwrap_or_else(|| "knowledge".to_owned());
    let tags = p.tags.unwrap_or_default();
    match deps.companion_service.add_memory(&kind, content, &tags, MemoryScope::Shared).await {
        Ok(memory) => ok(memory),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn update(deps: Arc<GatewayDeps>, p: MemoryUpdateParams) -> Value {
    if p.content.is_none() && p.pinned.is_none() && p.status.is_none() {
        return json!({ "error": "nothing to update: provide at least one of content / pinned / status" });
    }
    match deps
        .companion_service
        .update_memory(
            p.memory_id.as_str(),
            p.content.as_deref(),
            p.pinned,
            p.status.as_deref(),
            None,
        )
        .await
    {
        Ok(()) => json!({ "result": format!("memory {} updated", p.memory_id) }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn delete(deps: Arc<GatewayDeps>, p: MemoryDeleteParams) -> Value {
    match deps.companion_service.delete_memory(p.memory_id.as_str()).await {
        Ok(()) => json!({ "result": format!("memory {} deleted", p.memory_id) }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// Register the memory-domain capabilities.
pub(crate) fn register(out: &mut Vec<Capability>) {
    out.push(Capability::new::<MemoryListParams, _, _>(
        CapabilityMeta::new(
            "nomi_memory_list",
            "memory",
            "List the desktop's long-term memories (active by default; filter by kind/query; include_archived to see archived).",
            DangerTier::Read,
        ),
        |deps, _ctx, p| list(deps, p),
    ));
    out.push(Capability::new::<MemorySaveParams, _, _>(
        CapabilityMeta::new(
            "nomi_memory_save",
            "memory",
            "Persist a new long-term memory (one self-contained fact).",
            DangerTier::Write,
        ),
        |deps, _ctx, p| save(deps, p),
    ));
    out.push(Capability::new::<MemoryUpdateParams, _, _>(
        CapabilityMeta::new(
            "nomi_memory_update",
            "memory",
            "Edit a memory's content, pin/unpin it, or archive/reactivate it.",
            DangerTier::Write,
        ),
        |deps, _ctx, p| update(deps, p),
    ));
    out.push(Capability::new::<MemoryDeleteParams, _, _>(
        CapabilityMeta::new(
            "nomi_memory_delete",
            "memory",
            "Permanently delete a memory. Prefer archiving via nomi_memory_update unless the user asked to delete.",
            DangerTier::Destructive,
        ),
        |deps, _ctx, p| delete(deps, p),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Registry, Surface};
    use serde_json::json;

    const MEMORY_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";

    #[test]
    fn memory_mutation_params_use_named_business_ids() {
        let update: MemoryUpdateParams = serde_json::from_value(json!({
            "memory_id": MEMORY_ID,
            "content": "updated"
        }))
        .unwrap();
        assert_eq!(update.memory_id.as_str(), MEMORY_ID);

        let delete: MemoryDeleteParams =
            serde_json::from_value(json!({"memory_id": MEMORY_ID})).unwrap();
        assert_eq!(delete.memory_id.as_str(), MEMORY_ID);

        assert!(
            serde_json::from_value::<MemoryUpdateParams>(json!({
                "id": MEMORY_ID,
                "content": "legacy field must be rejected"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<MemoryDeleteParams>(json!({"id": MEMORY_ID})).is_err()
        );
    }

    #[test]
    fn memory_mutation_tool_schemas_expose_named_business_ids() {
        let specs = Registry::global().tool_specs(Surface::Desktop);
        for name in ["nomi_memory_update", "nomi_memory_delete"] {
            let spec = specs.iter().find(|spec| spec.name == name).expect("tool registered");
            let properties = spec
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .expect("tool properties");
            assert!(properties.contains_key("memory_id"), "{name}");
            assert!(!properties.contains_key("id"), "{name}");
        }
    }
}
