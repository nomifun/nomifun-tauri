//! Relational row and aggregate models for reusable presets.

use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PresetRow {
    pub id: i64,
    pub preset_id: String,
    pub source_kind: String,
    pub source_key: Option<String>,
    pub revision: i64,
    pub name: String,
    pub description: Option<String>,
    pub routing_description: Option<String>,
    pub instructions: String,
    pub avatar: Option<String>,
    pub fallback_allowed: bool,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PresetLocalizationRow {
    pub id: i64,
    pub preset_id: String,
    pub locale: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub routing_description: Option<String>,
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PresetAgentPreferenceRow {
    pub id: i64,
    pub preset_id: String,
    pub agent_id: String,
    pub rank: i64,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PresetModelPreferenceRow {
    pub id: i64,
    pub preset_id: String,
    pub provider_id: Option<String>,
    pub model: String,
    pub rank: i64,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PresetSkillBindingRow {
    pub id: i64,
    pub preset_id: String,
    pub skill_name: String,
    pub binding: String,
    pub required: bool,
    pub sort_order: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PresetKnowledgePolicyRow {
    pub id: i64,
    pub preset_id: String,
    pub enabled: bool,
    pub mode: String,
    pub writeback: bool,
    pub eagerness: Option<String>,
    pub grounded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PresetKnowledgeBaseRow {
    pub id: i64,
    pub preset_id: String,
    pub knowledge_base_id: String,
    pub sort_order: i64,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PresetExampleRow {
    pub id: i64,
    pub preset_id: String,
    pub locale: String,
    pub sort_order: i64,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PresetTagBindingRow {
    pub id: i64,
    pub preset_id: String,
    pub preset_tag_id: String,
    /// Readable catalog key loaded from `preset_tags`; it is not stored on
    /// the binding row and is never used as the logical relationship.
    pub key: String,
    pub dimension: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PresetUserStateRow {
    pub id: i64,
    pub preset_id: String,
    pub enabled: bool,
    pub auto_selectable: bool,
    pub preferred_agent_id: Option<String>,
    pub sort_order: i32,
    pub last_used_at: Option<TimestampMs>,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PresetTagRow {
    pub id: i64,
    pub preset_tag_id: String,
    pub key: String,
    pub dimension: String,
    pub label: String,
    pub sort_order: i32,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, Default)]
pub struct PresetRecord {
    pub preset: Option<PresetRow>,
    pub localizations: Vec<PresetLocalizationRow>,
    pub targets: Vec<String>,
    pub agent_preferences: Vec<PresetAgentPreferenceRow>,
    pub model_preferences: Vec<PresetModelPreferenceRow>,
    pub skill_bindings: Vec<PresetSkillBindingRow>,
    pub knowledge_policy: Option<PresetKnowledgePolicyRow>,
    pub knowledge_bases: Vec<PresetKnowledgeBaseRow>,
    pub examples: Vec<PresetExampleRow>,
    pub tag_bindings: Vec<PresetTagBindingRow>,
    pub user_state: Option<PresetUserStateRow>,
}

#[derive(Debug, Clone)]
pub struct PresetWriteParams {
    pub preset_id: String,
    pub source_kind: String,
    pub source_key: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub routing_description: Option<String>,
    pub instructions: String,
    pub avatar: Option<String>,
    pub fallback_allowed: bool,
    /// locale, name, description, routing_description, instructions
    pub localizations: Vec<(String, Option<String>, Option<String>, Option<String>, Option<String>)>,
    pub targets: Vec<String>,
    /// agent_id, required
    pub agent_preferences: Vec<(String, bool)>,
    /// provider_id, model, required
    pub model_preferences: Vec<(Option<String>, String, bool)>,
    /// skill_name, binding, required
    pub skill_bindings: Vec<(String, String, bool)>,
    /// enabled, mode, writeback, eagerness, grounded
    pub knowledge_policy: (bool, String, bool, Option<String>, bool),
    /// knowledge_base_id, required
    pub knowledge_bases: Vec<(String, bool)>,
    /// locale, prompt
    pub examples: Vec<(String, String)>,
    /// preset_tag_id, dimension
    pub tag_bindings: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct UpsertPresetStateParams {
    pub preset_id: String,
    pub enabled: bool,
    pub auto_selectable: bool,
    pub preferred_agent_id: Option<String>,
    pub sort_order: i32,
    pub last_used_at: Option<TimestampMs>,
}

#[derive(Debug, Clone)]
pub struct CreatePresetTagParams<'a> {
    pub preset_tag_id: &'a str,
    pub key: &'a str,
    pub dimension: &'a str,
    pub label: &'a str,
    pub sort_order: i32,
}

#[derive(Debug, Clone, Default)]
pub struct UpdatePresetTagParams<'a> {
    pub label: Option<&'a str>,
    pub sort_order: Option<i32>,
}
