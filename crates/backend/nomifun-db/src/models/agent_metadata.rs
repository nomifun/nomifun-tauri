//! Row models and parameter structs for the `agent_metadata` table.
//!
//! JSON-encoded columns (`agent_source_info`, `args`, `env`,
//! `native_skills_dirs`, `behavior_policy`, plus the ACP handshake
//! snapshots) stay as opaque strings at this layer. The ai-agent crate
//! owns the schema of these payloads and decodes them on read.

use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `agent_metadata` table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AgentMetadataRow {
    pub id: i64,
    /// Bare UUIDv7 business ID for every agent row.
    pub agent_id: String,
    pub icon: Option<String>,
    pub name: String,
    pub name_i18n: Option<String>,
    pub description: Option<String>,
    pub description_i18n: Option<String>,

    pub backend: Option<String>,
    pub agent_type: String,
    pub agent_source: String,
    pub agent_source_info: Option<String>,
    /// Stable catalog/install lineage, such as `agent_builtin_claude`.
    ///
    /// This is deliberately separate from `agent_id`; the latter is always a
    /// bare UUIDv7, including for builtin rows.
    pub source_key: Option<String>,

    pub enabled: bool,

    pub command: Option<String>,
    pub args: Option<String>,
    pub env: Option<String>,
    pub native_skills_dirs: Option<String>,

    pub behavior_policy: Option<String>,
    /// Native mode id that Nomi's legacy `yolo` / `yoloNoSandbox`
    /// aliases resolve to before calling `session/set_mode`. `None`
    /// means the backend has no yolo equivalent and the alias should
    /// pass through unchanged.
    pub yolo_id: Option<String>,

    pub agent_capabilities: Option<String>,
    pub auth_methods: Option<String>,
    pub config_options: Option<String>,
    pub available_modes: Option<String>,
    pub available_models: Option<String>,
    pub available_commands: Option<String>,

    /// Display ordering key — smaller values appear first.
    pub sort_order: i64,

    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Insert / upsert parameters for the full row.
///
/// JSON fields are pre-serialized strings; the caller is responsible for
/// encoding. `source_key` is catalog-owned: custom inserts leave it `NULL`,
/// while the repository preserves existing builtin lineage on conflict.
#[derive(Debug, Clone)]
pub struct UpsertAgentMetadataParams<'a> {
    pub agent_id: &'a str,
    pub icon: Option<&'a str>,
    pub name: &'a str,
    pub name_i18n: Option<&'a str>,
    pub description: Option<&'a str>,
    pub description_i18n: Option<&'a str>,
    pub backend: Option<&'a str>,
    pub agent_type: &'a str,
    pub agent_source: &'a str,
    pub agent_source_info: Option<&'a str>,
    pub enabled: bool,
    pub command: Option<&'a str>,
    pub args: Option<&'a str>,
    pub env: Option<&'a str>,
    pub native_skills_dirs: Option<&'a str>,
    pub behavior_policy: Option<&'a str>,
    pub yolo_id: Option<&'a str>,
    pub agent_capabilities: Option<&'a str>,
    pub auth_methods: Option<&'a str>,
    pub config_options: Option<&'a str>,
    pub available_modes: Option<&'a str>,
    pub available_models: Option<&'a str>,
    pub available_commands: Option<&'a str>,
    pub sort_order: i64,
}

/// Partial update applied after an ACP initialize/authenticate handshake.
///
/// Every field is `Option<Option<&str>>` so the caller can distinguish
/// "leave untouched" (outer `None`) from "clear to NULL" (inner `None`).
#[derive(Debug, Clone, Default)]
pub struct UpdateAgentHandshakeParams<'a> {
    pub agent_capabilities: Option<Option<&'a str>>,
    pub auth_methods: Option<Option<&'a str>>,
    pub config_options: Option<Option<&'a str>>,
    pub available_modes: Option<Option<&'a str>>,
    pub available_models: Option<Option<&'a str>>,
    pub available_commands: Option<Option<&'a str>>,
}
