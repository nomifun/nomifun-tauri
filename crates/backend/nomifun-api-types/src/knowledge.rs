use nomifun_common::{ConnectorCredentialId, KnowledgeBaseId};
use serde::{Deserialize, Serialize};

/// A live (URL-backed) source attached to a knowledge base. Snapshots of
/// such sources can go stale between syncs, so the knowledge context builder
/// surfaces the URLs in a dedicated "Realtime sources" section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnowledgeSourceEntry {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// **P3-K3**: route this URL through the rendering backend (`BrowserFetcher`,
    /// a real headless browser) instead of the default HTTP fetcher. Set for
    /// JS-heavy SPAs whose content a plain HTTP GET cannot see. `#[serde(default)]`
    /// keeps old persisted `extra.source` rows (which lack the key) deserializing
    /// to `false` ⇒ HTTP — full backward compatibility. When `true` but no render
    /// backend is wired (`browser-use` feature off / not injected), the fetch
    /// gracefully falls back to HTTP at the dispatch site (`prepare_snapshot_body`).
    #[serde(default)]
    pub rendered: bool,
}

/// How a URL source feeds the knowledge base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KnowledgeSourceMode {
    /// URLs are surfaced to the agent as realtime sources (rendered into the
    /// knowledge context); nothing is fetched at create time.
    Live,
    /// URLs are fetched and persisted as markdown snapshots under
    /// `{kb_root}/snapshots/` (re-fetchable via the refresh endpoint).
    Snapshot,
}

/// URL source configuration of a knowledge base. Persisted as JSON in the
/// registry row's forward-compatible `extra` column under the `source` key —
/// every field added later MUST be `#[serde(default)]`-compatible.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeSource {
    /// Source kind discriminator; `"url"` is the only kind today (connector
    /// kinds like `feishu`/`notion` are design-stage).
    #[serde(default = "default_source_kind")]
    pub kind: String,
    pub mode: KnowledgeSourceMode,
    #[serde(default)]
    pub entries: Vec<KnowledgeSourceEntry>,
    /// Last successful snapshot fetch (epoch ms); `None` until the first
    /// fetch. Live-mode sources are never fetched at create time so they
    /// start as `None`, but the refresh-source endpoint snapshots live
    /// sources too and stamps this field once an entry succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fetched_at: Option<i64>,
    /// **P3 connector**: reference to a `connector_credentials` row (never a
    /// secret). Present for connector-backed sources (`kind != "url"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<ConnectorCredentialId>,
    /// Connector-specific scope (e.g. a Feishu wiki space `{ "space_id": .. }`).
    /// Opaque to the core; interpreted by the connector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<serde_json::Value>,
    /// Connector sync state (cursor + interval + last outcome). `None` for URL
    /// sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<ConnectorSyncState>,
}

fn default_source_kind() -> String {
    "url".to_owned()
}

/// Wire-safe summary of a stored connector credential. **Never** carries the
/// secret payload — only the fields the UI needs to list/pick credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectorCredentialSummary {
    pub credential_id: ConnectorCredentialId,
    /// Connector discriminator: "feishu", "notion", …
    pub kind: String,
    pub name: String,
    pub created_at: i64,
}

/// Persisted sync state for a connector-backed knowledge base (lives in
/// `extra.source.sync`). All fields `#[serde(default)]` for back-compat.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorSyncState {
    /// Optional periodic-sync interval (minutes); `None` = manual only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_minutes: Option<u32>,
    /// Last successful sync (epoch ms, local wall-clock). For display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_at: Option<i64>,
    /// Remote high-water-mark: the max document `edit_time` (epoch ms, the
    /// REMOTE's clock) successfully synced so far. This — not `last_sync_at` —
    /// is the incremental filter watermark fed back on the next run. Keeping
    /// them separate avoids clock-skew misses: a local `now_ms()` watermark can
    /// exceed the remote `edit_time` of docs edited near sync time and skip
    /// them forever. `None` on legacy rows falls back to `last_sync_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark: Option<i64>,
    /// Connector-specific opaque cursor carried across runs.
    #[serde(default)]
    pub cursor: serde_json::Value,
    /// Last sync error message, if the most recent run failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// A knowledge base mounted into a session workspace. Carried in
/// `AcpBuildExtra` / `NomiBuildExtra` (and future build extras) so the
/// shared context builder (`nomifun_knowledge::context`) can tell the agent
/// what extended knowledge is available and where.
///
/// Serialized into `extra.knowledge_mounts`; every field added after the
/// initial shape MUST be `#[serde(default)]`-compatible so old persisted
/// extras keep deserializing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeMountInfo {
    pub knowledge_base_id: KnowledgeBaseId,
    pub name: String,
    pub description: String,
    /// Workspace-relative mount path, e.g. `.nomi/knowledge/领域知识`.
    pub rel_path: String,
    /// Lightweight table of contents — one line per document
    /// (`rel/path.md — first heading`), budgeted at mount time so the prompt
    /// stays bounded; overflow is aggregated into `dir/ — N files` lines.
    /// Lets the agent target the right file instead of crawling the
    /// directory.
    #[serde(default)]
    pub toc: Vec<String>,
    /// First non-heading paragraph of the base's root `README.md`, truncated
    /// to ≤400 chars at mount time. `None` when the base has no README (the
    /// AI-autogen README task fills these in over time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Live URL sources backing (parts of) this base. Rendered as a
    /// "Realtime sources" context section when non-empty. Populated from
    /// `extra.source` when the base has a live-mode URL source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub live_sources: Vec<KnowledgeSourceEntry>,
}

/// A user-defined tag that can be assigned to knowledge bases for
/// categorization / filtering in the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeTag {
    pub key: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub sort_order: i64,
}

/// Request body for creating a new knowledge tag.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreateKnowledgeTagRequest {
    pub label: String,
    #[serde(default)]
    pub color: Option<String>,
}

/// Request body for partially updating an existing knowledge tag.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct UpdateKnowledgeTagRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub sort_order: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_credential_summary_uses_named_id_and_rejects_legacy_id() {
        let credential_id = ConnectorCredentialId::new();
        let credential = serde_json::json!({
            "credentialId": credential_id,
            "kind": "feishu",
            "name": "tenant",
            "createdAt": 1
        });
        let summary: ConnectorCredentialSummary =
            serde_json::from_value(credential).expect("named credential id should deserialize");
        assert_eq!(summary.credential_id, credential_id);
        let wire = serde_json::to_value(summary).unwrap();
        assert_eq!(wire["credentialId"], credential_id.as_str());
        assert!(wire.get("id").is_none(), "legacy generic id must stay off the wire: {wire}");

        let legacy = serde_json::json!({
            "id": credential_id,
            "kind": "feishu",
            "name": "tenant",
            "createdAt": 1
        });
        assert!(serde_json::from_value::<ConnectorCredentialSummary>(legacy).is_err());
    }

    #[test]
    fn connector_credential_ids_reject_noncanonical_values() {
        let valid = ConnectorCredentialId::new();
        for invalid in [
            serde_json::json!(7),
            serde_json::json!("7"),
            serde_json::json!("550e8400-e29b-41d4-a716-446655440000"),
            serde_json::json!(valid.as_str().to_ascii_uppercase()),
            serde_json::json!(format!("conn_{valid}")),
        ] {
            let summary = serde_json::json!({
                "credentialId": invalid,
                "kind": "feishu",
                "name": "tenant",
                "createdAt": 1
            });
            assert!(
                serde_json::from_value::<ConnectorCredentialSummary>(summary).is_err()
            );
        }

        let generic_id = serde_json::json!({
            "id": valid,
            "kind": "feishu",
            "name": "tenant",
            "createdAt": 1
        });
        assert!(serde_json::from_value::<ConnectorCredentialSummary>(generic_id).is_err());
    }

    #[test]
    fn knowledge_source_credential_ref_rejects_legacy_and_noncanonical_ids() {
        let valid = ConnectorCredentialId::new();
        let valid_source = serde_json::json!({
            "kind": "feishu",
            "mode": "snapshot",
            "credentialRef": valid,
            "scope": { "space_id": "space" }
        });
        assert!(
            serde_json::from_value::<KnowledgeSource>(valid_source).is_ok()
        );

        for invalid in [
            serde_json::json!(7),
            serde_json::json!("7"),
            serde_json::json!("550e8400-e29b-41d4-a716-446655440000"),
            serde_json::json!(valid.as_str().to_ascii_uppercase()),
            serde_json::json!(format!("conn_{valid}")),
        ] {
            let source = serde_json::json!({
                "kind": "feishu",
                "mode": "snapshot",
                "credentialRef": invalid,
                "scope": { "space_id": "space" }
            });
            assert!(serde_json::from_value::<KnowledgeSource>(source).is_err());
        }

        for generic_key in ["id", "credential_id", "credential_ref"] {
            let source = serde_json::json!({
                "kind": "feishu",
                "mode": "snapshot",
                generic_key: valid,
                "scope": { "space_id": "space" }
            });
            assert!(serde_json::from_value::<KnowledgeSource>(source).is_err());
        }
    }

    #[test]
    fn knowledge_mount_info_uses_named_id_and_rejects_legacy_id() {
        let knowledge_base_id = KnowledgeBaseId::new();
        let mount = serde_json::json!({
            "knowledge_base_id": knowledge_base_id,
            "name": "docs",
            "description": "",
            "rel_path": ".nomi/knowledge/docs"
        });
        let info: KnowledgeMountInfo =
            serde_json::from_value(mount).expect("named knowledge base id should deserialize");
        assert_eq!(info.knowledge_base_id, knowledge_base_id);
        let wire = serde_json::to_value(info).unwrap();
        assert_eq!(wire["knowledge_base_id"], knowledge_base_id.as_str());
        assert!(wire.get("id").is_none(), "legacy generic id must stay off the wire: {wire}");

        let legacy = serde_json::json!({
            "id": knowledge_base_id,
            "name": "docs",
            "description": "",
            "rel_path": ".nomi/knowledge/docs"
        });
        assert!(serde_json::from_value::<KnowledgeMountInfo>(legacy).is_err());
    }

    /// The `extra.source` wire shape (camelCase + lowercase mode) is a
    /// frontend/gateway contract — pin it.
    #[test]
    fn knowledge_source_serde_shape() {
        let source = KnowledgeSource {
            kind: "url".into(),
            mode: KnowledgeSourceMode::Snapshot,
            entries: vec![KnowledgeSourceEntry {
                url: "https://example.com/docs".into(),
                title: Some("Docs".into()),
                rendered: false,
            }],
            last_fetched_at: Some(1_770_000_000_000),
            credential_ref: None,
            scope: None,
            sync: None,
        };
        let v = serde_json::to_value(&source).unwrap();
        assert_eq!(v["kind"], "url");
        assert_eq!(v["mode"], "snapshot");
        assert_eq!(v["entries"][0]["url"], "https://example.com/docs");
        assert_eq!(v["lastFetchedAt"], 1_770_000_000_000_i64);
        assert!(v.get("last_fetched_at").is_none(), "must be camelCase: {v}");

        let live = serde_json::json!({"mode": "live", "entries": [{"url": "https://e.com"}]});
        let parsed: KnowledgeSource = serde_json::from_value(live).unwrap();
        assert_eq!(parsed.mode, KnowledgeSourceMode::Live);
        assert_eq!(parsed.kind, "url", "kind defaults to url");
        assert_eq!(parsed.last_fetched_at, None);
        let round = serde_json::to_value(&parsed).unwrap();
        assert!(round.get("lastFetchedAt").is_none(), "None stays off the wire: {round}");
    }

    /// **P3-K3**: the `rendered` flag must be additive/backward-compatible —
    /// old persisted `extra.source` rows have no `rendered` key and MUST
    /// deserialize to `false` (= HTTP fetcher). A present flag round-trips.
    #[test]
    fn knowledge_source_entry_rendered_is_backward_compatible() {
        // Old wire shape (no `rendered` key) → defaults to false.
        let legacy: KnowledgeSourceEntry =
            serde_json::from_value(serde_json::json!({"url": "https://old.example.com"})).unwrap();
        assert!(!legacy.rendered, "missing `rendered` key must default to false (HTTP)");

        // Present-and-true round-trips through the wire.
        let entry = KnowledgeSourceEntry {
            url: "https://spa.example.com".into(),
            title: None,
            rendered: true,
        };
        let v = serde_json::to_value(&entry).unwrap();
        assert_eq!(v["rendered"], true);
        let back: KnowledgeSourceEntry = serde_json::from_value(v).unwrap();
        assert!(back.rendered);

        // A whole source with one legacy entry and one rendered entry survives.
        let mixed = serde_json::json!({
            "kind": "url",
            "mode": "snapshot",
            "entries": [
                {"url": "https://plain.example.com"},
                {"url": "https://spa.example.com", "rendered": true}
            ]
        });
        let parsed: KnowledgeSource = serde_json::from_value(mixed).unwrap();
        assert!(!parsed.entries[0].rendered, "legacy entry defaults to HTTP");
        assert!(parsed.entries[1].rendered, "explicit rendered entry preserved");
    }

    /// Tag DTO wire shape: camelCase keys, optional fields omit-when-None,
    /// request DTOs accept partial payloads.
    #[test]
    fn knowledge_tag_serde_shape() {
        // Full KnowledgeTag serializes in camelCase with color present.
        let tag = KnowledgeTag {
            key: "k1".into(),
            label: "Research".into(),
            color: Some("#ff0000".into()),
            sort_order: 2,
        };
        let v = serde_json::to_value(&tag).unwrap();
        assert_eq!(v["key"], "k1");
        assert_eq!(v["label"], "Research");
        assert_eq!(v["color"], "#ff0000");
        assert_eq!(v["sortOrder"], 2);
        assert!(v.get("sort_order").is_none(), "must be camelCase: {v}");

        // color=None stays off the wire.
        let tag_no_color = KnowledgeTag {
            key: "k2".into(),
            label: "Archive".into(),
            color: None,
            sort_order: 0,
        };
        let v2 = serde_json::to_value(&tag_no_color).unwrap();
        assert!(v2.get("color").is_none(), "None color must be omitted: {v2}");

        // CreateKnowledgeTagRequest — minimal (color defaults to None).
        let create: CreateKnowledgeTagRequest =
            serde_json::from_value(serde_json::json!({"label": "New"})).unwrap();
        assert_eq!(create.label, "New");
        assert_eq!(create.color, None);

        // UpdateKnowledgeTagRequest — all-None (empty patch).
        let update: UpdateKnowledgeTagRequest =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(update.label, None);
        assert_eq!(update.color, None);
        assert_eq!(update.sort_order, None);

        // UpdateKnowledgeTagRequest — partial patch.
        let update2: UpdateKnowledgeTagRequest =
            serde_json::from_value(serde_json::json!({"sortOrder": 5})).unwrap();
        assert_eq!(update2.sort_order, Some(5));
    }
}
