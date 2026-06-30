use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `providers` table.
///
/// JSON fields (models, capabilities, model_protocols, model_descriptions,
/// model_enabled, model_health, bedrock_config) are stored as TEXT in SQLite
/// and deserialized by the service layer.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Provider {
    pub id: String,
    pub platform: String,
    pub name: String,
    pub base_url: String,
    pub api_key_encrypted: String,
    /// JSON array of model ID strings.
    pub models: String,
    pub enabled: bool,
    /// JSON array of capability objects.
    pub capabilities: String,
    pub context_limit: Option<i64>,
    /// JSON object: model_id -> protocol string.
    pub model_protocols: Option<String>,
    /// JSON object: model_id -> description string (user-authored).
    pub model_descriptions: Option<String>,
    /// JSON object: model_id -> bool.
    pub model_enabled: Option<String>,
    /// JSON object: model_id -> health status object.
    pub model_health: Option<String>,
    /// JSON object: Bedrock-specific configuration.
    pub bedrock_config: Option<String>,
    /// When true, base_url is treated as a complete endpoint URL.
    /// The system will NOT append paths like /v1/chat/completions.
    pub is_full_url: bool,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
