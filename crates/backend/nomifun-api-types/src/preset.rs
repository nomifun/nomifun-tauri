//! Contracts for reusable NomiFun presets and their execution snapshots.

use std::collections::HashMap;
use nomifun_common::KnowledgeBaseId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PresetSource { Builtin, User, Extension }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PresetTarget { Conversation, ExecutionStep, Companion, PublicCompanion, Cron }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PresetTagDimension { Audience, Scenario }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentPreference {
    #[serde(deserialize_with = "crate::serde_util::deserialize_agent_id")]
    pub agent_id: String,
    #[serde(default)] pub required: bool,
}

/// Provider-qualified model reference. `provider_id` and `model` are one fixed
/// pair; model-only catalog entries are not valid v3 API DTOs.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelPreference {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_provider_id"
    )]
    pub provider_id: Option<String>,
    #[serde(deserialize_with = "crate::serde_util::deserialize_model_name")]
    pub model: String,
    #[serde(default)] pub required: bool,
}

impl<'de> Deserialize<'de> for ModelPreference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(
                default,
                deserialize_with = "crate::serde_util::deserialize_optional_provider_id"
            )]
            provider_id: Option<String>,
            #[serde(deserialize_with = "crate::serde_util::deserialize_model_name")]
            model: String,
            #[serde(default)]
            required: bool,
        }

        let wire = Wire::deserialize(deserializer)?;
        let provider_id = wire.provider_id.ok_or_else(|| {
            serde::de::Error::custom("provider_id and model must be supplied together")
        })?;
        Ok(Self {
            provider_id: Some(provider_id),
            model: wire.model,
            required: wire.required,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillBinding {
    pub skill_name: String,
    #[serde(default)] pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnowledgeBaseBinding {
    pub knowledge_base_id: KnowledgeBaseId,
    #[serde(default)] pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PresetKnowledgePolicy {
    #[serde(default)] pub enabled: bool,
    #[serde(default = "default_knowledge_mode")] pub mode: String,
    #[serde(default)] pub writeback: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub eagerness: Option<String>,
    #[serde(default)] pub grounded: bool,
}

fn default_knowledge_mode() -> String { "inherit".to_string() }

impl Default for PresetKnowledgePolicy {
    fn default() -> Self {
        Self { enabled: false, mode: default_knowledge_mode(), writeback: false, eagerness: None, grounded: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetResponse {
    #[serde(deserialize_with = "crate::serde_util::deserialize_preset_id")]
    pub preset_id: String,
    pub revision: i64,
    pub source: PresetSource,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub source_key: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")] pub name_i18n: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub description: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")] pub description_i18n: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub routing_description: Option<String>,
    #[serde(default)] pub instructions: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")] pub instructions_i18n: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub avatar: Option<String>,
    #[serde(default)] pub fallback_allowed: bool,
    #[serde(default)] pub targets: Vec<PresetTarget>,
    #[serde(default)] pub agent_preferences: Vec<AgentPreference>,
    #[serde(default)] pub model_preferences: Vec<ModelPreference>,
    #[serde(default)] pub included_skills: Vec<SkillBinding>,
    #[serde(default)] pub excluded_auto_skills: Vec<String>,
    #[serde(default)] pub knowledge_policy: PresetKnowledgePolicy,
    #[serde(default)] pub knowledge_bases: Vec<KnowledgeBaseBinding>,
    #[serde(default)] pub examples: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")] pub examples_i18n: HashMap<String, Vec<String>>,
    #[serde(default, deserialize_with = "crate::serde_util::deserialize_uuidv7_vec")]
    pub audience_tag_ids: Vec<String>,
    #[serde(default, deserialize_with = "crate::serde_util::deserialize_uuidv7_vec")]
    pub scenario_tag_ids: Vec<String>,
    /// Readable catalog keys for in-process capability inference. They are
    /// derived from UUIDv7 bindings and never cross the public wire.
    #[serde(skip)]
    pub audience_tags: Vec<String>,
    #[serde(skip)]
    pub scenario_tags: Vec<String>,
    pub enabled: bool,
    pub auto_selectable: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_agent_id"
    )]
    pub preferred_agent_id: Option<String>,
    pub sort_order: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePresetRequest {
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_preset_id"
    )]
    pub preset_id: Option<String>,
    pub name: String,
    #[serde(default)] pub description: Option<String>,
    #[serde(default)] pub routing_description: Option<String>,
    #[serde(default)] pub instructions: String,
    #[serde(default)] pub avatar: Option<String>,
    #[serde(default)] pub fallback_allowed: bool,
    #[serde(default)] pub targets: Vec<PresetTarget>,
    #[serde(default)] pub agent_preferences: Vec<AgentPreference>,
    #[serde(default)] pub model_preferences: Vec<ModelPreference>,
    #[serde(default)] pub included_skills: Vec<SkillBinding>,
    #[serde(default)] pub excluded_auto_skills: Vec<String>,
    #[serde(default)] pub knowledge_policy: PresetKnowledgePolicy,
    #[serde(default)] pub knowledge_bases: Vec<KnowledgeBaseBinding>,
    #[serde(default)] pub examples: Vec<String>,
    #[serde(default)] pub examples_i18n: HashMap<String, Vec<String>>,
    #[serde(default, deserialize_with = "crate::serde_util::deserialize_uuidv7_vec")]
    pub audience_tag_ids: Vec<String>,
    #[serde(default, deserialize_with = "crate::serde_util::deserialize_uuidv7_vec")]
    pub scenario_tag_ids: Vec<String>,
    #[serde(default)] pub name_i18n: HashMap<String, String>,
    #[serde(default)] pub description_i18n: HashMap<String, String>,
    #[serde(default)] pub instructions_i18n: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpdatePresetRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub routing_description: Option<String>,
    pub instructions: Option<String>,
    pub avatar: Option<String>,
    pub fallback_allowed: Option<bool>,
    pub targets: Option<Vec<PresetTarget>>,
    pub agent_preferences: Option<Vec<AgentPreference>>,
    pub model_preferences: Option<Vec<ModelPreference>>,
    pub included_skills: Option<Vec<SkillBinding>>,
    pub excluded_auto_skills: Option<Vec<String>>,
    pub knowledge_policy: Option<PresetKnowledgePolicy>,
    pub knowledge_bases: Option<Vec<KnowledgeBaseBinding>>,
    pub examples: Option<Vec<String>>,
    pub examples_i18n: Option<HashMap<String, Vec<String>>>,
    #[serde(default, deserialize_with = "crate::serde_util::deserialize_optional_uuidv7_vec")]
    pub audience_tag_ids: Option<Vec<String>>,
    #[serde(default, deserialize_with = "crate::serde_util::deserialize_optional_uuidv7_vec")]
    pub scenario_tag_ids: Option<Vec<String>>,
    pub name_i18n: Option<HashMap<String, String>>,
    pub description_i18n: Option<HashMap<String, String>>,
    pub instructions_i18n: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SetPresetStateRequest {
    pub enabled: Option<bool>,
    pub auto_selectable: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_agent_id"
    )]
    pub preferred_agent_id: Option<String>,
    pub sort_order: Option<i32>,
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PresetOverrides {
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_agent_id"
    )]
    pub agent_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_provider_id"
    )]
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub instructions: Option<String>,
    #[serde(default)] pub include_skills: Vec<String>,
    #[serde(default)] pub exclude_skills: Vec<String>,
    pub knowledge_policy: Option<PresetKnowledgePolicy>,
    #[serde(default)]
    pub knowledge_base_ids: Option<Vec<KnowledgeBaseId>>,
}

impl<'de> Deserialize<'de> for PresetOverrides {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(
                default,
                deserialize_with = "crate::serde_util::deserialize_optional_agent_id"
            )]
            agent_id: Option<String>,
            #[serde(
                default,
                deserialize_with = "crate::serde_util::deserialize_optional_provider_id"
            )]
            provider_id: Option<String>,
            #[serde(
                default,
                deserialize_with = "crate::serde_util::deserialize_optional_model_name"
            )]
            model: Option<String>,
            instructions: Option<String>,
            #[serde(default)]
            include_skills: Vec<String>,
            #[serde(default)]
            exclude_skills: Vec<String>,
            knowledge_policy: Option<PresetKnowledgePolicy>,
            #[serde(default)]
            knowledge_base_ids: Option<Vec<KnowledgeBaseId>>,
        }

        let wire = Wire::deserialize(deserializer)?;
        crate::serde_util::validate_optional_provider_model_pair(
            wire.provider_id.as_deref(),
            wire.model.as_deref(),
        )
        .map_err(serde::de::Error::custom)?;
        Ok(Self {
            agent_id: wire.agent_id,
            provider_id: wire.provider_id,
            model: wire.model,
            instructions: wire.instructions,
            include_skills: wire.include_skills,
            exclude_skills: wire.exclude_skills,
            knowledge_policy: wire.knowledge_policy,
            knowledge_base_ids: wire.knowledge_base_ids,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvePresetRequest {
    pub target: PresetTarget,
    #[serde(default)] pub locale: Option<String>,
    #[serde(default)] pub overrides: PresetOverrides,
}

/// Persist this execution-time materialization with the target object. Later
/// preset edits must never mutate an existing snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResolvedPresetSnapshot {
    #[serde(deserialize_with = "crate::serde_util::deserialize_preset_id")]
    pub preset_id: String,
    pub preset_revision: i64,
    pub preset_name: String,
    pub target: PresetTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub routing_description: Option<String>,
    #[serde(default)] pub instructions: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_agent_id"
    )]
    pub resolved_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub resolved_agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub resolved_agent_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub resolved_model: Option<ModelPreference>,
    #[serde(default)] pub included_skills: Vec<String>,
    #[serde(default)] pub excluded_auto_skills: Vec<String>,
    #[serde(default)] pub knowledge_policy: PresetKnowledgePolicy,
    #[serde(default)]
    pub knowledge_base_ids: Vec<KnowledgeBaseId>,
    #[serde(default)] pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetTagResponse {
    #[serde(deserialize_with = "crate::serde_util::deserialize_uuidv7")]
    pub preset_tag_id: String,
    #[serde(deserialize_with = "crate::serde_util::deserialize_preset_tag_key")]
    pub key: String,
    pub dimension: PresetTagDimension,
    pub label: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")] pub label_i18n: HashMap<String, String>,
    pub sort_order: i32,
    pub builtin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePresetTagRequest { pub dimension: PresetTagDimension, pub label: String }

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpdatePresetTagRequest { pub label: Option<String>, pub sort_order: Option<i32> }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportPresetsRequest { pub presets: Vec<CreatePresetRequest> }

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImportPresetsResult {
    pub imported: usize,
    pub skipped: usize,
    pub failed: usize,
    #[serde(default)] pub errors: Vec<PresetImportError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetImportError { pub preset_id: String, pub error: String }

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PROVIDER_ID: &str = "018f1234-5678-7abc-8def-012345678990";
    const PRESET_ID: &str = "018f1234-5678-7abc-8def-012345678991";
    const NOMI_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000114";
    #[test]
    fn provider_qualified_model_round_trips() {
        let model = ModelPreference { provider_id: Some(PROVIDER_ID.into()), model: "gpt-5".into(), required: true };
        let value = serde_json::to_value(&model).unwrap();
        assert_eq!(value["provider_id"], PROVIDER_ID);
        assert_eq!(serde_json::from_value::<ModelPreference>(value).unwrap(), model);
    }
    #[test]
    fn target_names_are_stable_snake_case() {
        assert_eq!(serde_json::to_string(&PresetTarget::ExecutionStep).unwrap(), "\"execution_step\"");
    }

    #[test]
    fn model_preference_rejects_platform_key_as_provider_id() {
        let raw = json!({ "provider_id": "openai", "model": "gpt-5" });
        assert!(serde_json::from_value::<ModelPreference>(raw).is_err());
    }

    #[test]
    fn model_preference_requires_a_complete_provider_model_pair() {
        for raw in [
            json!({ "model": "gpt-5" }),
            json!({ "provider_id": PROVIDER_ID }),
            json!({ "provider_id": PROVIDER_ID, "model": "" }),
            json!({ "provider_id": PROVIDER_ID, "model": " gpt-5" }),
            json!({ "provider_id": PROVIDER_ID, "model": "gpt-5 " }),
        ] {
            assert!(serde_json::from_value::<ModelPreference>(raw).is_err());
        }
    }

    #[test]
    fn model_preference_rejects_unknown_fields() {
        let raw = json!({
            "provider_id": PROVIDER_ID,
            "model": "gpt-5",
            "provider": "openai"
        });
        assert!(serde_json::from_value::<ModelPreference>(raw).is_err());
    }

    #[test]
    fn create_preset_request_rejects_noncanonical_preset_id() {
        for preset_id in [
            json!(42),
            json!("word-creator"),
            json!("550e8400-e29b-41d4-a716-446655440000"),
            json!("0190F5FE-7C00-7A00-8000-000000000042"),
            json!("preset_0190f5fe-7c00-7a00-8000-000000000042"),
        ] {
            let raw = json!({ "preset_id": preset_id, "name": "General" });
            assert!(serde_json::from_value::<CreatePresetRequest>(raw).is_err());
        }
    }

    #[test]
    fn create_preset_request_rejects_removed_generic_id() {
        let raw = json!({ "id": PRESET_ID, "name": "General" });
        assert!(serde_json::from_value::<CreatePresetRequest>(raw).is_err());
    }

    #[test]
    fn preset_id_is_always_a_bare_uuid_v7() {
        for id in [PRESET_ID, "word-creator", "preset_0190f5fe-7c00-7a00-8abc-012345678901"] {
            let raw = json!({
                "preset_id": id,
                "preset_revision": 1,
                "preset_name": "General",
                "target": "conversation"
            });
            let parsed = serde_json::from_value::<ResolvedPresetSnapshot>(raw);
            assert_eq!(parsed.is_ok(), id == PRESET_ID);
        }
    }

    #[test]
    fn preset_response_rejects_noncanonical_and_removed_generic_id() {
        let valid = json!({
            "preset_id": PRESET_ID,
            "revision": 1,
            "source": "user",
            "name": "General",
            "enabled": true,
            "auto_selectable": false,
            "sort_order": 0
        });
        assert!(serde_json::from_value::<PresetResponse>(valid.clone()).is_ok());

        for preset_id in [
            json!(42),
            json!("word-creator"),
            json!("550e8400-e29b-41d4-a716-446655440000"),
            json!("0190F5FE-7C00-7A00-8000-000000000042"),
            json!("preset_0190f5fe-7c00-7a00-8000-000000000042"),
        ] {
            let mut raw = valid.clone();
            raw["preset_id"] = preset_id;
            assert!(serde_json::from_value::<PresetResponse>(raw).is_err());
        }

        let mut legacy = valid;
        legacy["id"] = legacy["preset_id"].take();
        legacy.as_object_mut().unwrap().remove("preset_id");
        assert!(serde_json::from_value::<PresetResponse>(legacy).is_err());
    }

    #[test]
    fn preset_overrides_reject_noncanonical_entity_ids() {
        let raw = json!({
            "target": "conversation",
            "overrides": {
                "provider_id": "openai",
                "knowledge_base_ids": ["knowledge-1"]
            }
        });
        assert!(serde_json::from_value::<ResolvePresetRequest>(raw).is_err());
    }

    #[test]
    fn knowledge_base_binding_uses_typed_canonical_id() {
        let id = KnowledgeBaseId::new();
        let binding: KnowledgeBaseBinding = serde_json::from_value(json!({
            "knowledge_base_id": id.as_str()
        }))
        .unwrap();
        let typed: &KnowledgeBaseId = &binding.knowledge_base_id;
        assert_eq!(typed, &id);

        assert!(serde_json::from_value::<KnowledgeBaseBinding>(json!({
            "knowledge_base_id": "kb_docs"
        }))
        .is_err());
    }

    #[test]
    fn agent_reference_uses_business_id_and_catalog_source_key_is_separate() {
        let raw = json!({ "agent_id": NOMI_AGENT_ID });
        let preference: AgentPreference = serde_json::from_value(raw).unwrap();
        assert_eq!(preference.agent_id, NOMI_AGENT_ID);

        let raw = json!({
            "preset_id": PRESET_ID,
            "revision": 1,
            "source": "builtin",
            "source_key": "agent_builtin_nomi",
            "name": "Nomi",
            "enabled": true,
            "auto_selectable": false,
            "sort_order": 0
        });
        let preset: PresetResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(preset.source_key.as_deref(), Some("agent_builtin_nomi"));

        for non_business_id in ["nomi", "agent_not-a-uuid"] {
            let malformed = json!({ "agent_id": non_business_id });
            assert!(serde_json::from_value::<AgentPreference>(malformed).is_err());
        }
    }

    #[test]
    fn preset_tag_response_separates_uuidv7_identity_from_catalog_key() {
        const PRESET_TAG_ID: &str = "0190f5fe-7c00-7a00-8000-000000000004";
        for key in ["office", "research-2", "vendor:tag"] {
            let raw = json!({
                "preset_tag_id": PRESET_TAG_ID,
                "key": key,
                "dimension": "scenario",
                "label": "Research",
                "sort_order": 0,
                "builtin": false
            });
            let tag: PresetTagResponse = serde_json::from_value(raw).unwrap();
            assert_eq!(tag.preset_tag_id, PRESET_TAG_ID);
            assert_eq!(tag.key, key);
        }
        for key in [
            "Research",
            "research/tag",
        ] {
            let raw = json!({
                "preset_tag_id": PRESET_TAG_ID,
                "key": key,
                "dimension": "scenario",
                "label": "Research",
                "sort_order": 0,
                "builtin": false
            });
            assert!(serde_json::from_value::<PresetTagResponse>(raw).is_err());
        }
        for invalid_id in ["research", "preset_tag_0190f5fe-7c00-7a00-8000-000000000004"] {
            let raw = json!({
                "preset_tag_id": invalid_id,
                "key": "research",
                "dimension": "scenario",
                "label": "Research",
                "sort_order": 0,
                "builtin": false
            });
            assert!(serde_json::from_value::<PresetTagResponse>(raw).is_err());
        }
    }

    #[test]
    fn canonical_provider_fixture_is_uuid_v7() {
        let model = ModelPreference {
            provider_id: Some(PROVIDER_ID.into()),
            model: "gpt-5".into(),
            required: false,
        };
        let value = serde_json::to_value(&model).unwrap();
        assert!(serde_json::from_value::<ModelPreference>(value).is_ok());
    }
}
