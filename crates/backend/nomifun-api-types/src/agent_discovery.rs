//! Unified agent metadata surfaced to the frontend.
//!
//! A single type replaces the previous split of `DetectedAgent` (API
//! response) and `AgentMetadata` (internal cache): the same shape is
//! stored in the `agent_metadata` table, cached in the process, and
//! returned over HTTP. The DB row feeds everything.
//!
//! Handshake-derived fields (`agent_capabilities` / `auth_methods` /
//! `config_options` / `available_modes` / `available_models` /
//! `available_commands`) stay as opaque JSON so this crate does not
//! depend on the ACP protocol SDK — the ai-agent crate typed-decodes
//! them when it needs to.

use nomifun_common::AgentType;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How an agent row was sourced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentSource {
    /// Ships with the backend binary (no CLI install required — e.g. `nomi`).
    Internal,
    /// Seeded from the migration (ACP vendors, nanobot, openclaw).
    Builtin,
    /// Installed from the extension hub.
    Extension,
    /// User-defined row.
    Custom,
}

/// Environment variable entry passed to a spawned agent process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEnvEntry {
    pub name: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Source-specific bookkeeping (how to probe, how to upgrade, which Hub
/// package it came from).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentSourceInfo {
    /// Primary CLI binary checked for availability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_name: Option<String>,
    /// Extra binary required when the row spawns via a bridge (e.g. `bun`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_binary: Option<String>,
    /// Hub package identifier when `agent_source = "extension"`. Extension
    /// rows still use a bare UUIDv7 `agent_id`; this field carries the
    /// extension's catalog/package identity separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_package_id: Option<String>,
    /// Version string for Hub or custom rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Adapter-side behaviour switches. These drive code branches that used
/// to be hardcoded per `AcpBackend`; new keys are added by extending
/// this struct — we deliberately avoid a free-form "extra" bag so every
/// flag is type-checked at its usage sites.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BehaviorPolicy {
    #[serde(default)]
    pub supports_side_question: bool,

    /// The agent's CLI bakes the model identity into its session
    /// system prompt at launch time and does not refresh it when
    /// `session/set_model` is called. Callers should inject a
    /// `<system-reminder>` before the next prompt so the model
    /// answers with the user-selected identity rather than the
    /// stale cached one.
    #[serde(default)]
    pub self_identity_sticky: bool,

    /// The agent does not implement the generic ACP `session/load`
    /// method. To resume, callers must call `session/new` again and
    /// pass the prior session id through a vendor-specific
    /// `_meta.<vendor>.options.resume` field.
    #[serde(default)]
    pub session_load_via_meta_field: bool,
}

/// Handshake-derived fields captured from the ACP init/session-response.
///
/// All fields are opaque JSON at this layer: they are passed through to
/// the frontend verbatim, and typed-decoded inside `nomifun-ai-agent`
/// when the adapter needs them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentHandshake {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_capabilities: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_methods: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_modes: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_models: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_commands: Option<serde_json::Value>,
}

/// The unified, decoded view of an `agent_metadata` row.
///
/// Also the API response shape: `/api/agents` returns a list of these
/// directly, no adapter required.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMetadata {
    /// Bare UUIDv7 business identifier for every agent row. Catalog lineage
    /// is carried separately by source metadata and never embedded here.
    #[serde(deserialize_with = "crate::serde_util::deserialize_agent_id")]
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_i18n: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_i18n: Option<serde_json::Value>,

    /// Vendor label (e.g. "claude"). `None` for agents without vendor
    /// grouping (remote / internal / nanobot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub agent_type: AgentType,
    pub agent_source: AgentSource,
    #[serde(default)]
    pub agent_source_info: AgentSourceInfo,

    pub enabled: bool,

    /// Whether the spawn command was resolvable on `$PATH` at hydrate time.
    ///
    /// Derived at discovery time — not a persisted column. Serialized so
    /// the frontend can show "installed / missing" status without a
    /// second round-trip.
    pub available: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Absolute path to the spawn command, resolved via `which()` at
    /// hydrate time. `Some` iff `available` is `true`. Server-internal:
    /// the frontend only cares about `available`, so this field is
    /// never serialized over the wire.
    #[serde(default, skip)]
    pub resolved_command: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<AgentEnvEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_skills_dirs: Option<Vec<String>>,

    #[serde(default)]
    pub behavior_policy: BehaviorPolicy,

    /// Native mode id that Nomi's legacy `yolo` / `yoloNoSandbox`
    /// aliases resolve to before calling `session/set_mode`. `None`
    /// means the backend has no "yolo" equivalent and the alias should
    /// pass through unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo_id: Option<String>,

    /// Display ordering key — smaller values appear first. The range
    /// scheme is documented in `007_agent_metadata_sort_order.sql`.
    pub sort_order: i64,

    #[serde(default)]
    pub handshake: AgentHandshake,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_source_serde_roundtrip() {
        for (variant, expected) in [
            (AgentSource::Internal, "internal"),
            (AgentSource::Builtin, "builtin"),
            (AgentSource::Extension, "extension"),
            (AgentSource::Custom, "custom"),
        ] {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, format!("\"{expected}\""));
            let parsed: AgentSource = serde_json::from_str(&s).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn agent_metadata_skips_empty_fields() {
        const AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000101";
        let meta = AgentMetadata {
            agent_id: AGENT_ID.into(),
            icon: None,
            name: "Claude".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("claude".into()),
            agent_type: AgentType::Acp,
            agent_source: AgentSource::Builtin,
            agent_source_info: AgentSourceInfo::default(),
            enabled: true,
            available: true,
            command: None,
            resolved_command: None,
            args: vec![],
            env: vec![],
            native_skills_dirs: None,
            behavior_policy: BehaviorPolicy::default(),
            yolo_id: None,
            sort_order: 3100,
            handshake: AgentHandshake::default(),
        };
        let v = serde_json::to_value(&meta).unwrap();
        assert_eq!(v["agent_id"], AGENT_ID);
        assert!(v.get("id").is_none());
        // Server-internal fields are stripped from the wire form.
        assert!(v.get("resolved_command").is_none());
        assert_eq!(v["backend"], "claude");
        assert_eq!(v["available"], true);
        assert!(v.get("team_capable").is_none());
        assert!(v.get("command").is_none());
        assert!(v.get("icon").is_none());
    }

    #[test]
    fn agent_metadata_rejects_legacy_ambiguous_id() {
        let value = serde_json::json!({
            "id": "0190f5fe-7c00-7a00-8000-000000000101",
            "name": "Claude",
            "agent_type": "acp",
            "agent_source": "builtin",
            "enabled": true,
            "available": true,
            "sort_order": 3100,
            "handshake": {}
        });
        assert!(serde_json::from_value::<AgentMetadata>(value).is_err());
    }

    #[test]
    fn agent_metadata_deserializes_minimal_payload() {
        let payload = json!({
            "agent_id": "0190f5fe-7c00-7a00-8000-000000000101",
            "name": "y",
            "agent_type": "acp",
            "agent_source": "custom",
            "enabled": true,
            "available": false,
            "sort_order": 1100,
        });
        let meta: AgentMetadata = serde_json::from_value(payload).unwrap();
        assert_eq!(meta.agent_type, AgentType::Acp);
        assert_eq!(meta.agent_source, AgentSource::Custom);
        assert!(nomifun_common::AgentId::parse(meta.agent_id).is_ok());
        assert!(!meta.available);
        assert!(!meta.behavior_policy.supports_side_question);
        assert!(meta.handshake.agent_capabilities.is_none());
    }

    #[test]
    fn agent_metadata_rejects_catalog_or_prefixed_agent_ids() {
        for invalid in [
            "agent_builtin_claude",
            "agent_0190f5fe-7c00-7a00-8000-000000000101",
            "claude",
        ] {
            let payload = json!({
                "agent_id": invalid,
                "name": "Claude",
                "agent_type": "acp",
                "agent_source": "builtin",
                "enabled": true,
                "available": false,
                "sort_order": 3100,
            });
            assert!(serde_json::from_value::<AgentMetadata>(payload).is_err());
        }
    }
}

#[cfg(test)]
mod behavior_policy_tests {
    use super::BehaviorPolicy;

    #[test]
    fn deserializes_new_capability_flags() {
        let json = serde_json::json!({
            "supports_side_question": true,
            "self_identity_sticky": true,
            "session_load_via_meta_field": true,
        });
        let policy: BehaviorPolicy = serde_json::from_value(json).unwrap();
        assert!(policy.supports_side_question);
        assert!(policy.self_identity_sticky);
        assert!(policy.session_load_via_meta_field);
    }

    #[test]
    fn defaults_to_false_when_flags_omitted() {
        let policy: BehaviorPolicy = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!policy.supports_side_question);
        assert!(!policy.self_identity_sticky);
        assert!(!policy.session_load_via_meta_field);
    }

    #[test]
    fn retired_supports_team_flag_is_rejected() {
        assert!(
            serde_json::from_str::<BehaviorPolicy>(
                r#"{"supports_team":true,"supports_side_question":true}"#
            )
            .is_err()
        );
    }
}
