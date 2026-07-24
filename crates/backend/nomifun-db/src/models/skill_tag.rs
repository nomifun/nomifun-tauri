use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `skill_tags` table (user tag assignments per skill).
/// Built-in seed assignments live in skill-tags.json, merged at the route layer.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SkillTagRow {
    pub id: i64,
    pub skill_name: String,
    pub audience_tags: Option<String>,
    pub scenario_tags: Option<String>,
    pub updated_at: TimestampMs,
}

/// Upsert params: JSON-array strings (pre-serialized by the caller).
#[derive(Debug, Clone)]
pub struct UpsertSkillTagParams<'a> {
    pub skill_name: &'a str,
    pub audience_tags: Option<&'a str>,
    pub scenario_tags: Option<&'a str>,
}
