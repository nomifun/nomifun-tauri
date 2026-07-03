//! The `PublicAgentConfig` — the persisted profile of one 对外伙伴 (public
//! companion). Stored as `public-agents/{id}/config.json`. Enterprise-service
//! shaped and deliberately DISJOINT from `CompanionProfileConfig`.

use nomifun_common::{generate_prefixed_id, now_ms};
use serde::{Deserialize, Serialize};

/// Default day-level audit retention (see `crate::audit`).
pub const DEFAULT_AUDIT_RETENTION_DAYS: u32 = 30;

/// The model a public companion answers with. A tiny, self-contained shape
/// (not the desktop companion's `ModelConfig`) — this domain shares no config
/// types with `nomifun-companion`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PublicAgentModel {
    /// Provider row id backing the model (empty = unconfigured).
    pub provider_id: String,
    /// Model label shown to the owner.
    pub model: String,
    /// Concrete model id sent to the provider (falls back to `model` when empty).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_model: Option<String>,
}

impl PublicAgentModel {
    /// Whether a usable model is configured.
    pub fn is_configured(&self) -> bool {
        !self.provider_id.trim().is_empty() && !self.model.trim().is_empty()
    }
}

/// One public companion's full configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PublicAgentConfig {
    /// Stable id (`pubagent_…`). An empty id after `load` means the file was
    /// missing/corrupt — callers must discard such profiles.
    pub id: String,
    /// Display-only short number (`#1`, `#2`, …). Allocated by the registry from
    /// its private high-watermark so a deleted agent's number is never reused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Owner-facing display name / brand.
    pub name: String,
    /// Opening / welcome message shown to strangers on first contact.
    pub greeting: String,
    /// Tone & style guidelines (free-text in P1; injected into the system prompt).
    pub tone: String,
    /// The model the agent answers with.
    pub model: PublicAgentModel,
    /// Bound platform knowledge bases (grounded retrieval source of truth). The
    /// runtime bakes these into the scoped knowledge tool so a turn can never
    /// widen the base set.
    pub knowledge_base_ids: Vec<String>,
    /// Grounded (strict) mode: only answer from the bound knowledge bases; when
    /// nothing is found, politely decline / suggest escalation — never fabricate.
    pub grounded_mode: bool,
    /// Service policy / 服务守则 (business scope, forbidden topics, compliance
    /// tone). Owner-authored; injected as a hard system directive. Free-text in
    /// P1 (structured policy is P2).
    pub service_policy: String,
    /// Day-level audit retention (see `crate::audit`).
    pub audit_retention_days: u32,
    /// Whether the agent is live (serving) or paused.
    pub enabled: bool,
    /// Creation timestamp (epoch ms).
    pub created_at: i64,
}

impl PublicAgentConfig {
    /// Fresh agent with a generated id and sensible enterprise defaults
    /// (grounded ON — anti-hallucination is the safe default; enabled ON).
    pub fn new(name: &str) -> Self {
        Self {
            id: generate_prefixed_id("pubagent"),
            seq: None,
            name: name.to_owned(),
            greeting: String::new(),
            tone: String::new(),
            model: PublicAgentModel::default(),
            knowledge_base_ids: Vec::new(),
            grounded_mode: true,
            service_policy: String::new(),
            audit_retention_days: DEFAULT_AUDIT_RETENTION_DAYS,
            enabled: true,
            created_at: now_ms(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_agent_has_enterprise_defaults() {
        let a = PublicAgentConfig::new("客服");
        assert!(a.id.starts_with("pubagent_"));
        assert_eq!(a.name, "客服");
        assert!(a.grounded_mode, "grounded is the anti-hallucination default");
        assert!(a.enabled);
        assert_eq!(a.audit_retention_days, DEFAULT_AUDIT_RETENTION_DAYS);
        assert!(!a.model.is_configured());
        assert!(a.seq.is_none());
    }

    #[test]
    fn config_json_roundtrips_and_tolerates_missing_fields() {
        // A minimal historical file (only id/name) must load with defaults.
        let v = serde_json::json!({ "id": "pubagent_x", "name": "A" });
        let a: PublicAgentConfig = serde_json::from_value(v).unwrap();
        assert_eq!(a.id, "pubagent_x");
        assert!(!a.grounded_mode, "absent bool → serde default false (registry sets true on create)");
        assert_eq!(a.audit_retention_days, 0, "absent → 0; create() sets the real default");
    }
}
