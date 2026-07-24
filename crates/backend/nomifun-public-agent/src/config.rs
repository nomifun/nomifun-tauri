//! The `PublicAgentConfig` — the persisted profile of one 对外伙伴 (public
//! companion). Stored as `public-agents/{public_agent_id}/config.json`. Enterprise-service
//! shaped and deliberately DISJOINT from `CompanionProfileConfig`.

use nomifun_common::{AppError, KnowledgeBaseId, ProviderId, PublicAgentId, now_ms};
use serde::{Deserialize, Serialize};

/// Default day-level audit retention (see `crate::audit`).
pub const DEFAULT_AUDIT_RETENTION_DAYS: u32 = 30;

/// The model a public companion answers with. A tiny, self-contained shape
/// (not the desktop companion's `ModelConfig`) — this domain shares no config
/// types with `nomifun-companion`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PublicAgentModel {
    /// Canonical provider business ID backing the model.
    pub provider_id: ProviderId,
    /// Model label shown to the owner.
    pub model: String,
}

impl PublicAgentModel {
    fn validate(&self) -> Result<(), AppError> {
        let model = self.model.trim();
        if model.is_empty() || model != self.model {
            return Err(AppError::BadRequest(
                "public-agent model must be non-empty and trimmed".into(),
            ));
        }
        Ok(())
    }
}

/// One public companion's full configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PublicAgentConfig {
    /// Canonical stable bare UUIDv7 entity id.
    pub public_agent_id: PublicAgentId,
    /// Display-only short number (`#1`, `#2`, …). Allocated by the registry from
    /// its private high-watermark so a deleted agent's number is never reused.
    pub seq: u64,
    /// Owner-facing display name / brand.
    pub name: String,
    /// Opening / welcome message shown to strangers on first contact.
    pub greeting: String,
    /// Tone & style guidelines (free-text in P1; injected into the system prompt).
    pub tone: String,
    /// The model the agent answers with. `None` is the only unconfigured
    /// representation; every configured reference is exactly
    /// `{provider_id, model}`.
    pub model: Option<PublicAgentModel>,
    /// Bound platform knowledge bases (grounded retrieval source of truth). The
    /// runtime bakes these into the scoped knowledge tool so a turn can never
    /// widen the base set.
    pub knowledge_base_ids: Vec<KnowledgeBaseId>,
    /// Grounded (strict) mode: only answer from the bound knowledge bases; when
    /// nothing is found, politely decline / suggest escalation — never fabricate.
    pub grounded_mode: bool,
    /// Service policy / 服务守则 (business scope, forbidden topics, compliance
    /// tone). Owner-authored; injected as a hard system directive. Free-text in
    /// P1 (structured policy is P2).
    pub service_policy: String,
    /// Frozen reusable configuration. Runtime policy remains authoritative:
    /// public-service tool clamps and grounded restrictions cannot be relaxed
    /// by a preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_preset: Option<nomifun_api_types::ResolvedPresetSnapshot>,
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
    pub fn new(name: &str, seq: u64) -> Self {
        assert!(seq > 0, "public-agent display sequence must be positive");
        Self {
            public_agent_id: PublicAgentId::new(),
            seq,
            name: name.to_owned(),
            greeting: String::new(),
            tone: String::new(),
            model: None,
            knowledge_base_ids: Vec::new(),
            grounded_mode: true,
            service_policy: String::new(),
            applied_preset: None,
            audit_retention_days: DEFAULT_AUDIT_RETENTION_DAYS,
            enabled: true,
            created_at: now_ms(),
        }
    }

    /// Validate cross-field invariants not expressible by typed ID serde.
    pub(crate) fn validate(&self) -> Result<(), AppError> {
        if self.name.trim().is_empty() {
            return Err(AppError::BadRequest("public-agent name must not be empty".into()));
        }
        if self.seq == 0 {
            return Err(AppError::BadRequest(
                "public-agent sequence must be positive".into(),
            ));
        }
        if let Some(model) = self.model.as_ref() {
            model.validate()?;
        }
        if self
            .applied_preset
            .as_ref()
            .is_some_and(|snapshot| snapshot.resolved_model.is_some())
        {
            return Err(AppError::BadRequest(
                "public-agent side store keeps Provider references only in the fixed model field"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_agent_has_enterprise_defaults() {
        let a = PublicAgentConfig::new("客服", 1);
        assert!(PublicAgentId::parse(a.public_agent_id.as_str()).is_ok());
        assert_eq!(a.name, "客服");
        assert!(a.grounded_mode, "grounded is the anti-hallucination default");
        assert!(a.enabled);
        assert_eq!(a.audit_retention_days, DEFAULT_AUDIT_RETENTION_DAYS);
        assert!(a.model.is_none());
        assert_eq!(a.seq, 1);
    }

    #[test]
    fn config_json_roundtrips_with_canonical_ids() {
        let provider_id = ProviderId::new();
        let kb_id = KnowledgeBaseId::new();
        let mut config = PublicAgentConfig::new("A", 1);
        config.model = Some(PublicAgentModel {
            provider_id: provider_id.clone(),
            model: "model-a".into(),
        });
        config.knowledge_base_ids = vec![kb_id.clone()];

        let decoded: PublicAgentConfig =
            serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        assert_eq!(decoded, config);
        let wire = serde_json::to_value(&decoded).unwrap();
        assert_eq!(
            wire["public_agent_id"],
            serde_json::json!(decoded.public_agent_id)
        );
        assert!(wire.get("id").is_none());
        assert_eq!(
            decoded.model.as_ref().map(|model| &model.provider_id),
            Some(&provider_id)
        );
        assert_eq!(decoded.knowledge_base_ids, vec![kb_id]);
    }

    #[test]
    fn config_json_rejects_noncanonical_entity_ids() {
        let canonical = serde_json::to_value(PublicAgentConfig::new("A", 1)).unwrap();
        for (field, invalid) in [
            (
                "public_agent_id",
                serde_json::json!("not-a-public-agent-id"),
            ),
            ("public_agent_id", serde_json::json!(42)),
        ] {
            let mut value = canonical.clone();
            value[field] = invalid;
            assert!(serde_json::from_value::<PublicAgentConfig>(value).is_err());
        }

        let mut legacy_generic_id = canonical.clone();
        legacy_generic_id["id"] = legacy_generic_id["public_agent_id"].clone();
        legacy_generic_id
            .as_object_mut()
            .unwrap()
            .remove("public_agent_id");
        assert!(serde_json::from_value::<PublicAgentConfig>(legacy_generic_id).is_err());

        let mut bad_provider = canonical.clone();
        bad_provider["model"] = serde_json::json!({
            "provider_id": "not-a-provider-id",
            "model": "model-a"
        });
        assert!(serde_json::from_value::<PublicAgentConfig>(bad_provider).is_err());

        let mut bad_kb = canonical;
        bad_kb["knowledge_base_ids"] =
            serde_json::json!(["not-a-knowledge-base-id"]);
        assert!(serde_json::from_value::<PublicAgentConfig>(bad_kb).is_err());
    }

    #[test]
    fn model_rejects_empty_id_sentinel_and_partial_configuration() {
        assert!(
            serde_json::from_value::<PublicAgentModel>(serde_json::json!({
                "provider_id": "",
                "model": "model-a"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<PublicAgentModel>(serde_json::json!({
                "model": "model-a"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<PublicAgentModel>(serde_json::json!({
                "provider_id": ProviderId::new(),
                "model": " model-a "
            }))
            .unwrap()
            .validate()
            .is_err()
        );
    }
}
