use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `model_profiles` table — the authoritative per-model
/// capability record. `tasks`/`traits`/`params` are JSON text (serialized
/// `ModelTask[]` / `ModelTrait[]` / object); the service layer (de)serializes
/// them into the api-types `ModelProfile`.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ModelProfileRow {
    pub provider_id: String,
    pub model: String,
    pub tasks: String,
    pub traits: String,
    pub params: String,
    pub source: String,
    pub updated_at: TimestampMs,
}

/// Upsert params: JSON strings pre-serialized by the caller.
#[derive(Debug, Clone)]
pub struct UpsertModelProfileParams<'a> {
    pub provider_id: &'a str,
    pub model: &'a str,
    pub tasks: &'a str,
    pub traits: &'a str,
    pub params: &'a str,
    pub source: &'a str,
}
