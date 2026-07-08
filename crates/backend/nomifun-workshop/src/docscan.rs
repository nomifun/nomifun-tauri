//! Asset-reference scanning over an opaque canvas doc.
//!
//! The canvas doc is opaque JSON to the backend, but export/import/GC must know
//! which assets a canvas references. Per contract, a reference is **any JSON
//! string value that is an asset id** (`wsa_` prefix). We treat the whole doc as
//! a bag of strings: node `data.assetId`, generator `resultAssetIds[]`,
//! `mentions[]`, etc. all surface the same way, so we never couple to the (still
//! evolving) doc schema.

use std::collections::BTreeSet;
use std::collections::HashMap;

use serde_json::Value;

/// Asset-id prefix (contract §1). A doc string equal-prefixed with this is an
/// asset reference.
pub(crate) const ASSET_ID_PREFIX: &str = "wsa_";

/// Collect every asset id (`wsa_…`) referenced anywhere in `doc`.
pub(crate) fn collect_asset_refs(doc: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    walk_collect(doc, &mut out);
    out
}

fn walk_collect(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::String(s) if s.starts_with(ASSET_ID_PREFIX) => {
            out.insert(s.clone());
        }
        Value::Array(items) => items.iter().for_each(|i| walk_collect(i, out)),
        Value::Object(map) => map.values().for_each(|i| walk_collect(i, out)),
        _ => {}
    }
}

/// Rewrite every asset id in `doc` in place using `remap` (old id → new id).
/// Strings not present in the map are left untouched. Used on import, where
/// every referenced asset is re-registered under a fresh id.
pub(crate) fn remap_asset_ids(doc: &mut Value, remap: &HashMap<String, String>) {
    match doc {
        Value::String(s) => {
            if let Some(new_id) = remap.get(s.as_str()) {
                *s = new_id.clone();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| remap_asset_ids(i, remap)),
        Value::Object(map) => map.values_mut().for_each(|i| remap_asset_ids(i, remap)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_doc() -> Value {
        json!({
            "schema": 1,
            "nodes": [
                { "id": "n1", "kind": "image", "data": { "assetId": "wsa_a", "caption": "hi" } },
                { "id": "n2", "kind": "generator", "data": {
                    "prompt": "cat",
                    "mentions": ["wsa_b", "wsa_c"],
                    "resultAssetIds": ["wsa_a", "wsa_d"],
                    "status": "idle"
                }},
                { "id": "n3", "kind": "text", "data": { "content": "no refs here" } }
            ],
            "edges": []
        })
    }

    #[test]
    fn collects_all_distinct_refs() {
        let refs = collect_asset_refs(&sample_doc());
        let got: Vec<&str> = refs.iter().map(String::as_str).collect();
        assert_eq!(got, vec!["wsa_a", "wsa_b", "wsa_c", "wsa_d"]);
    }

    #[test]
    fn ignores_non_asset_strings_and_empty_doc() {
        assert!(collect_asset_refs(&json!({})).is_empty());
        assert!(collect_asset_refs(&json!({ "x": "wscanvas", "y": "awsa_" })).is_empty());
    }

    #[test]
    fn remap_rewrites_only_known_ids() {
        let mut doc = sample_doc();
        let remap: HashMap<String, String> = [
            ("wsa_a".to_string(), "wsa_X".to_string()),
            ("wsa_b".to_string(), "wsa_Y".to_string()),
            ("wsa_c".to_string(), "wsa_Z".to_string()),
            ("wsa_d".to_string(), "wsa_W".to_string()),
        ]
        .into_iter()
        .collect();
        remap_asset_ids(&mut doc, &remap);
        let refs = collect_asset_refs(&doc);
        let got: Vec<&str> = refs.iter().map(String::as_str).collect();
        assert_eq!(got, vec!["wsa_W", "wsa_X", "wsa_Y", "wsa_Z"]);
    }
}
