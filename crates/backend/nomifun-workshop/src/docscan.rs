//! Durable-reference validation and rewriting over a workshop canvas doc.
//!
//! Node payload semantics remain frontend-owned. The backend nevertheless owns
//! the durable identity envelope (UUIDv7 nodes, UUIDv7 edges, and declared node
//! references) and must find asset references for export/import/GC.

use std::collections::{BTreeSet, HashMap};

use nomifun_common::{ProviderId, WorkshopAssetId, WorkshopEdgeId, WorkshopNodeId};
use serde_json::Value;

/// Collect every canonical workshop asset UUIDv7 from declared asset-bearing
/// document fields. Arbitrary UUID-looking text content is not treated as an
/// asset reference.
pub(crate) fn collect_asset_refs(doc: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    walk_collect(doc, None, &mut out);
    out
}

/// Collect every Provider selected by a generator node.
///
/// The generator model selection is a fixed pair:
/// `data.providerId` (canonical Provider UUIDv7) + non-empty `data.model`.
/// Either both are absent/null or both are valid. This keeps the file-backed
/// logical reference deterministic instead of accepting half-bound state.
pub(crate) fn collect_generator_provider_refs(
    doc: &Value,
) -> Result<BTreeSet<String>, String> {
    let mut out = BTreeSet::new();
    let nodes = doc
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "nodes must be an array".to_string())?;
    for (index, node) in nodes.iter().enumerate() {
        let Some(node) = node.as_object() else {
            continue;
        };
        if node.get("kind").and_then(Value::as_str) != Some("generator") {
            continue;
        }
        let Some(data) = node.get("data") else {
            continue;
        };
        let data = data
            .as_object()
            .ok_or_else(|| format!("nodes[{index}].data must be an object"))?;
        let provider = data.get("providerId").filter(|value| !value.is_null());
        let model = data.get("model").filter(|value| !value.is_null());
        match (provider, model) {
            (None, None) => {}
            (Some(provider), Some(model)) => {
                let provider = provider.as_str().ok_or_else(|| {
                    format!("nodes[{index}].data.providerId must be a string or null")
                })?;
                let provider = ProviderId::parse(provider).map_err(|error| {
                    format!(
                        "nodes[{index}].data.providerId is not canonical: {error}"
                    )
                })?;
                let model = model.as_str().ok_or_else(|| {
                    format!("nodes[{index}].data.model must be a string or null")
                })?;
                if model.trim().is_empty() {
                    return Err(format!(
                        "nodes[{index}].data.model must not be empty when providerId is set"
                    ));
                }
                out.insert(provider.into_string());
            }
            (Some(_), None) => {
                return Err(format!(
                    "nodes[{index}].data.model is required when providerId is set"
                ));
            }
            (None, Some(_)) => {
                return Err(format!(
                    "nodes[{index}].data.providerId is required when model is set"
                ));
            }
        }
    }
    Ok(out)
}

/// Clear one deleted Provider selection from generator nodes. The pair is
/// removed together so the frontend falls back to its current available-model
/// resolution instead of retaining a model name detached from its Provider.
pub(crate) fn clear_generator_provider_reference(
    doc: &mut Value,
    target_provider_id: &str,
) -> Result<bool, String> {
    ProviderId::parse(target_provider_id)
        .map_err(|error| format!("target provider_id is not canonical: {error}"))?;
    let nodes = doc
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "nodes must be an array".to_string())?;
    let mut changed = false;
    for (index, node) in nodes.iter_mut().enumerate() {
        let Some(node) = node.as_object_mut() else {
            continue;
        };
        if node.get("kind").and_then(Value::as_str) != Some("generator") {
            continue;
        }
        let Some(data) = node.get_mut("data") else {
            continue;
        };
        let data = data
            .as_object_mut()
            .ok_or_else(|| format!("nodes[{index}].data must be an object"))?;
        let Some(provider) = data.get("providerId").filter(|value| !value.is_null()) else {
            if data.get("model").is_some_and(|value| !value.is_null()) {
                return Err(format!(
                    "nodes[{index}].data.providerId is required when model is set"
                ));
            }
            continue;
        };
        let provider = provider.as_str().ok_or_else(|| {
            format!("nodes[{index}].data.providerId must be a string or null")
        })?;
        ProviderId::parse(provider).map_err(|error| {
            format!("nodes[{index}].data.providerId is not canonical: {error}")
        })?;
        if provider == target_provider_id {
            data.remove("providerId");
            data.remove("model");
            changed = true;
        }
    }
    Ok(changed)
}

fn walk_collect(v: &Value, field: Option<&str>, out: &mut BTreeSet<String>) {
    match (field, v) {
        (Some("assetId" | "sourceAssetId" | "maskAssetId"), Value::String(id))
            if WorkshopAssetId::parse(id).is_ok() =>
        {
            out.insert(id.clone());
        }
        (Some("mentions"), Value::Array(items)) => {
            for mention in items.iter().filter_map(Value::as_str) {
                if let Some(id) = asset_id_from_mention(mention) {
                    out.insert(id.to_string());
                }
            }
        }
        (Some("resultAssetIds"), Value::Array(items)) => {
            for id in items.iter().filter_map(Value::as_str) {
                if WorkshopAssetId::parse(id).is_ok() {
                    out.insert(id.to_string());
                }
            }
        }
        (_, Value::Array(items)) => items.iter().for_each(|item| walk_collect(item, None, out)),
        (_, Value::Object(map)) => map
            .iter()
            .for_each(|(key, value)| walk_collect(value, Some(key), out)),
        _ => {}
    }
}

fn asset_id_from_mention(mention: &str) -> Option<&str> {
    let mut parts = mention.split(':');
    if parts.next()? != "asset" {
        return None;
    }
    match parts.next()? {
        "image" | "video" | "text" => {}
        _ => return None,
    }
    let id = parts.next()?;
    if parts.next().is_some() || WorkshopAssetId::parse(id).is_err() {
        return None;
    }
    Some(id)
}

/// Rewrite every asset id in `doc` in place using `remap` (old id → new id).
/// Strings not present in the map are left untouched. Used on import, where
/// every referenced asset is re-registered under a fresh id.
pub(crate) fn remap_asset_ids(doc: &mut Value, remap: &HashMap<String, String>) {
    match doc {
        Value::String(s) => {
            if let Some(new_id) = remap.get(s.as_str()) {
                *s = new_id.clone();
            } else if let Some(old_id) = asset_id_from_mention(s)
                && let Some(new_id) = remap.get(old_id)
            {
                let kind = s
                    .strip_prefix("asset:")
                    .and_then(|rest| rest.split_once(':'))
                    .map(|(kind, _)| kind)
                    .expect("validated asset mention");
                *s = format!("asset:{kind}:{new_id}");
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| remap_asset_ids(i, remap)),
        Value::Object(map) => map.values_mut().for_each(|i| remap_asset_ids(i, remap)),
        _ => {}
    }
}

/// Validate the durable identity envelope of a frontend-owned canvas doc.
///
/// This deliberately does not duplicate the complete frontend schema. It only
/// owns identity fields that cross persistence/export/import boundaries, plus
/// referential integrity among those fields.
pub(crate) fn validate_canvas_doc_ids(doc: &Value) -> Result<usize, String> {
    let object = doc
        .as_object()
        .ok_or_else(|| "document must be a JSON object".to_string())?;
    let nodes = object
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "nodes must be an array".to_string())?;
    let edges = object
        .get("edges")
        .and_then(Value::as_array)
        .ok_or_else(|| "edges must be an array".to_string())?;

    let mut node_ids = BTreeSet::new();
    let mut node_references: Vec<(String, String)> = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let node = node
            .as_object()
            .ok_or_else(|| format!("nodes[{index}] must be an object"))?;
        let id = node
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("nodes[{index}].id must be a string"))?;
        WorkshopNodeId::parse(id)
            .map_err(|error| format!("nodes[{index}].id is not canonical: {error}"))?;
        if !node_ids.insert(id.to_string()) {
            return Err(format!("nodes[{index}].id duplicates '{id}'"));
        }

        if let Some(group_id) = node.get("groupId").filter(|value| !value.is_null()) {
            let group_id = group_id
                .as_str()
                .ok_or_else(|| format!("nodes[{index}].groupId must be a string or null"))?;
            WorkshopNodeId::parse(group_id)
                .map_err(|error| format!("nodes[{index}].groupId is not canonical: {error}"))?;
            node_references.push((format!("nodes[{index}].groupId"), group_id.to_string()));
        }

        if let Some(mentions) = node
            .get("data")
            .and_then(Value::as_object)
            .and_then(|data| data.get("mentions"))
            .and_then(Value::as_array)
        {
            for (mention_index, mention) in mentions.iter().enumerate() {
                let Some(reference) = mention.as_str().and_then(|value| value.strip_prefix("node:")) else {
                    continue;
                };
                WorkshopNodeId::parse(reference).map_err(|error| {
                    format!("nodes[{index}].data.mentions[{mention_index}] has a non-canonical node reference: {error}")
                })?;
                node_references.push((
                    format!("nodes[{index}].data.mentions[{mention_index}]"),
                    reference.to_string(),
                ));
            }
        }
    }

    for (path, reference) in node_references {
        if !node_ids.contains(&reference) {
            return Err(format!("{path} references missing node '{reference}'"));
        }
    }

    let mut edge_ids = BTreeSet::new();
    for (index, edge) in edges.iter().enumerate() {
        let edge = edge
            .as_object()
            .ok_or_else(|| format!("edges[{index}] must be an object"))?;
        let id = edge
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("edges[{index}].id must be a string"))?;
        WorkshopEdgeId::parse(id)
            .map_err(|error| format!("edges[{index}].id is not canonical: {error}"))?;
        if !edge_ids.insert(id.to_string()) {
            return Err(format!("edges[{index}].id duplicates '{id}'"));
        }
        for field in ["from", "to"] {
            let reference = edge
                .get(field)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("edges[{index}].{field} must be a string"))?;
            WorkshopNodeId::parse(reference)
                .map_err(|error| format!("edges[{index}].{field} is not canonical: {error}"))?;
            if !node_ids.contains(reference) {
                return Err(format!("edges[{index}].{field} references missing node '{reference}'"));
            }
        }
    }

    Ok(nodes.len())
}

/// Give every durable document entity a fresh identity when importing a canvas
/// as a clone, and rewrite every declared node reference through one remap.
pub(crate) fn remap_canvas_doc_ids_for_clone(doc: &mut Value) -> Result<(), String> {
    validate_canvas_doc_ids(doc)?;

    let mut node_remap = HashMap::new();
    for node in doc["nodes"].as_array().expect("validated nodes array") {
        let old_id = node["id"].as_str().expect("validated node id");
        node_remap.insert(old_id.to_string(), WorkshopNodeId::new().into_string());
    }

    for node in doc["nodes"].as_array_mut().expect("validated nodes array") {
        let object = node.as_object_mut().expect("validated node object");
        let old_id = object["id"].as_str().expect("validated node id").to_string();
        object.insert(
            "id".to_string(),
            Value::String(node_remap[&old_id].clone()),
        );
        if let Some(group_id) = object
            .get("groupId")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            *object.get_mut("groupId").expect("groupId exists") =
                Value::String(node_remap[&group_id].clone());
        }
        if let Some(mentions) = object
            .get_mut("data")
            .and_then(Value::as_object_mut)
            .and_then(|data| data.get_mut("mentions"))
            .and_then(Value::as_array_mut)
        {
            for mention in mentions {
                let Some(old_reference) = mention.as_str().and_then(|value| value.strip_prefix("node:")) else {
                    continue;
                };
                *mention = Value::String(format!("node:{}", node_remap[old_reference]));
            }
        }
    }

    for edge in doc["edges"].as_array_mut().expect("validated edges array") {
        let object = edge.as_object_mut().expect("validated edge object");
        object.insert(
            "id".to_string(),
            Value::String(WorkshopEdgeId::new().into_string()),
        );
        for field in ["from", "to"] {
            let old_reference = object[field]
                .as_str()
                .expect("validated edge node reference")
                .to_string();
            object.insert(field.to_string(), Value::String(node_remap[&old_reference].clone()));
        }
    }

    validate_canvas_doc_ids(doc).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_doc() -> Value {
        let first = WorkshopNodeId::new().into_string();
        let second = WorkshopNodeId::new().into_string();
        let third = WorkshopNodeId::new().into_string();
        let asset_a = "0190f5fe-7c00-7a00-8000-000000000081";
        let asset_b = "0190f5fe-7c00-7a00-8000-000000000082";
        let asset_c = "0190f5fe-7c00-7a00-8000-000000000083";
        let asset_d = "0190f5fe-7c00-7a00-8000-000000000084";
        json!({
            "schema": 1,
            "nodes": [
                { "id": first, "kind": "image", "data": { "assetId": asset_a, "caption": "hi" } },
                { "id": second, "kind": "generator", "data": {
                    "prompt": "cat",
                    "sourceAssetId": asset_b,
                    "maskAssetId": asset_c,
                    "resultAssetIds": [asset_a, asset_d],
                    "mentions": [
                        format!("asset:image:{asset_b}"),
                        format!("asset:video:{asset_c}"),
                        format!("asset:text:{asset_d}")
                    ],
                    "status": "idle"
                }},
                { "id": third, "kind": "text", "data": { "content": "no refs here" } }
            ],
            "edges": []
        })
    }

    #[test]
    fn collects_all_distinct_refs() {
        let refs = collect_asset_refs(&sample_doc());
        let got: Vec<&str> = refs.iter().map(String::as_str).collect();
        assert_eq!(
            got,
            vec![
                "0190f5fe-7c00-7a00-8000-000000000081",
                "0190f5fe-7c00-7a00-8000-000000000082",
                "0190f5fe-7c00-7a00-8000-000000000083",
                "0190f5fe-7c00-7a00-8000-000000000084"
            ]
        );
    }

    #[test]
    fn ignores_non_asset_strings_and_empty_doc() {
        assert!(collect_asset_refs(&json!({})).is_empty());
        assert!(
            collect_asset_refs(&json!({
                "content": "0190f5fe-7c00-7a00-8000-000000000081",
                "assetId": "not-an-id",
                "mentions": [
                    "node:0190f5fe-7c00-7a00-8000-000000000081",
                    "asset:audio:0190f5fe-7c00-7a00-8000-000000000081",
                    "asset:image:not-an-id"
                ]
            }))
            .is_empty()
        );
    }

    #[test]
    fn generator_provider_refs_require_a_canonical_complete_pair() {
        let provider_id = "0190f5fe-7c00-7a00-8000-000000000085";
        let node_id = WorkshopNodeId::new().into_string();
        let doc = serde_json::json!({
            "nodes": [{
                "id": node_id,
                "kind": "generator",
                "data": {"providerId": provider_id, "model": "image-model"}
            }],
            "edges": []
        });
        assert_eq!(
            collect_generator_provider_refs(&doc).unwrap(),
            BTreeSet::from([provider_id.to_string()])
        );

        let mut missing_model = doc.clone();
        missing_model["nodes"][0]["data"]
            .as_object_mut()
            .unwrap()
            .remove("model");
        assert!(
            collect_generator_provider_refs(&missing_model)
                .unwrap_err()
                .contains("model is required")
        );

        let mut noncanonical = doc;
        noncanonical["nodes"][0]["data"]["providerId"] =
            Value::String(format!("provider_{provider_id}"));
        assert!(
            collect_generator_provider_refs(&noncanonical)
                .unwrap_err()
                .contains("not canonical")
        );
    }

    #[test]
    fn clearing_generator_provider_removes_the_model_pair_only() {
        let provider_id = "0190f5fe-7c00-7a00-8000-000000000086";
        let other_provider_id = "0190f5fe-7c00-7a00-8000-000000000087";
        let mut doc = serde_json::json!({
            "nodes": [
                {
                    "kind": "generator",
                    "data": {
                        "providerId": provider_id,
                        "model": "delete-me",
                        "prompt": "keep me"
                    }
                },
                {
                    "kind": "generator",
                    "data": {
                        "providerId": other_provider_id,
                        "model": "keep-me"
                    }
                }
            ]
        });
        assert!(clear_generator_provider_reference(&mut doc, provider_id).unwrap());
        assert!(doc["nodes"][0]["data"].get("providerId").is_none());
        assert!(doc["nodes"][0]["data"].get("model").is_none());
        assert_eq!(doc["nodes"][0]["data"]["prompt"], "keep me");
        assert_eq!(
            doc["nodes"][1]["data"]["providerId"],
            Value::String(other_provider_id.to_string())
        );
    }

    #[test]
    fn remap_rewrites_only_known_ids() {
        let mut doc = sample_doc();
        let remap: HashMap<String, String> = [
            (
                "0190f5fe-7c00-7a00-8000-000000000081".to_string(),
                "0190f5fe-7c00-7a00-8000-000000000091".to_string(),
            ),
            (
                "0190f5fe-7c00-7a00-8000-000000000082".to_string(),
                "0190f5fe-7c00-7a00-8000-000000000092".to_string(),
            ),
            (
                "0190f5fe-7c00-7a00-8000-000000000083".to_string(),
                "0190f5fe-7c00-7a00-8000-000000000093".to_string(),
            ),
            (
                "0190f5fe-7c00-7a00-8000-000000000084".to_string(),
                "0190f5fe-7c00-7a00-8000-000000000094".to_string(),
            ),
        ]
        .into_iter()
        .collect();
        remap_asset_ids(&mut doc, &remap);
        let refs = collect_asset_refs(&doc);
        let got: Vec<&str> = refs.iter().map(String::as_str).collect();
        assert_eq!(
            got,
            vec![
                "0190f5fe-7c00-7a00-8000-000000000091",
                "0190f5fe-7c00-7a00-8000-000000000092",
                "0190f5fe-7c00-7a00-8000-000000000093",
                "0190f5fe-7c00-7a00-8000-000000000094"
            ]
        );
        assert_eq!(
            doc["nodes"][1]["data"]["mentions"][0].as_str(),
            Some("asset:image:0190f5fe-7c00-7a00-8000-000000000092")
        );
        assert_eq!(
            doc["nodes"][1]["data"]["mentions"][1].as_str(),
            Some("asset:video:0190f5fe-7c00-7a00-8000-000000000093")
        );
        assert_eq!(
            doc["nodes"][1]["data"]["mentions"][2].as_str(),
            Some("asset:text:0190f5fe-7c00-7a00-8000-000000000094")
        );
    }

    #[test]
    fn validates_and_remaps_the_complete_document_identity_envelope() {
        let group_id = WorkshopNodeId::new().into_string();
        let member_id = WorkshopNodeId::new().into_string();
        let edge_id = WorkshopEdgeId::new().into_string();
        let mut doc = json!({
            "nodes": [
                {"id": group_id},
                {"id": member_id, "groupId": group_id, "data": {
                    "mentions": [format!("node:{group_id}")]
                }}
            ],
            "edges": [{"id": edge_id, "from": group_id, "to": member_id}]
        });
        assert_eq!(validate_canvas_doc_ids(&doc), Ok(2));

        remap_canvas_doc_ids_for_clone(&mut doc).unwrap();
        let new_group_id = doc["nodes"][0]["id"].as_str().unwrap();
        let new_member_id = doc["nodes"][1]["id"].as_str().unwrap();
        assert_ne!(new_group_id, group_id);
        assert_ne!(new_member_id, member_id);
        assert_eq!(doc["nodes"][1]["groupId"].as_str(), Some(new_group_id));
        assert_eq!(doc["edges"][0]["from"].as_str(), Some(new_group_id));
        assert_eq!(doc["edges"][0]["to"].as_str(), Some(new_member_id));
        let expected_mention = format!("node:{new_group_id}");
        assert_eq!(
            doc["nodes"][1]["data"]["mentions"][0].as_str(),
            Some(expected_mention.as_str())
        );
        assert_eq!(validate_canvas_doc_ids(&doc), Ok(2));
    }

    #[test]
    fn rejects_noncanonical_or_dangling_document_ids() {
        let node_id = WorkshopNodeId::new().into_string();
        let missing_id = WorkshopNodeId::new().into_string();
        let edge_id = WorkshopEdgeId::new().into_string();
        let legacy = json!({"nodes": [{"id": "legacy"}], "edges": []});
        assert!(validate_canvas_doc_ids(&legacy).unwrap_err().contains("not canonical"));

        let dangling = json!({
            "nodes": [{"id": node_id}],
            "edges": [{"id": edge_id, "from": node_id, "to": missing_id}]
        });
        assert!(validate_canvas_doc_ids(&dangling).unwrap_err().contains("missing node"));
    }
}
