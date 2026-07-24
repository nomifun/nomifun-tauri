//! Runtime assertions for the clean v3 database lineage.
//!
//! Product tables use local `INTEGER PRIMARY KEY AUTOINCREMENT` row identities.
//! Cross-boundary identities live in explicitly named columns, while indexed
//! logical links replace SQLite foreign keys. This module is the executable
//! registry for that contract and provides a read-only orphan-audit skeleton.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::{Row, SqlitePool};

use crate::error::DbError;

pub(crate) const PRODUCT_TABLES: &[&str] = &[
    "acp_session",
    "agent_execution_attempts",
    "agent_execution_events",
    "agent_execution_participants",
    "agent_execution_step_dependencies",
    "agent_execution_steps",
    "agent_execution_template_participants",
    "agent_execution_templates",
    "agent_executions",
    "agent_metadata",
    "attachments",
    "channel_pairing_codes",
    "channel_plugins",
    "channel_sessions",
    "channel_users",
    "client_preferences",
    "companion_access_token",
    "connector_credentials",
    "conversation_artifacts",
    "conversation_creation_keys",
    "conversation_delivery_receipts",
    "conversation_execution_links",
    "conversation_mcp_servers",
    "conversations",
    "creation_tasks",
    "cron_job_runs",
    "cron_jobs",
    "idmm_interventions",
    "installation_identity",
    "knowledge_bases",
    "knowledge_binding_bases",
    "knowledge_bindings",
    "knowledge_tags",
    "mcp_servers",
    "message_correlations",
    "messages",
    "model_profiles",
    "oauth_tokens",
    "preset_agent_preferences",
    "preset_examples",
    "preset_knowledge_bases",
    "preset_knowledge_policy",
    "preset_localizations",
    "preset_model_preferences",
    "preset_skill_bindings",
    "preset_tag_bindings",
    "preset_tags",
    "preset_targets",
    "preset_user_state",
    "presets",
    "providers",
    "remote_agents",
    "requirement_display_sequence",
    "requirement_tags",
    "requirements",
    "skill_tags",
    "system_settings",
    "tag_settings",
    "terminal_scrollback",
    "terminal_sessions",
    "users",
    "webhooks",
    "workshop_assets",
    "workshop_canvases",
];

/// Business columns that carry a bare canonical UUIDv7 for every populated row.
const UUIDV7_BUSINESS_COLUMNS: &[(&str, &str)] = &[
    ("agent_execution_attempts", "attempt_id"),
    ("agent_execution_participants", "participant_id"),
    ("agent_execution_steps", "step_id"),
    (
        "agent_execution_template_participants",
        "template_participant_id",
    ),
    ("agent_execution_templates", "execution_template_id"),
    ("agent_executions", "execution_id"),
    ("agent_metadata", "agent_id"),
    ("attachments", "attachment_id"),
    ("channel_plugins", "channel_plugin_id"),
    ("channel_sessions", "channel_session_id"),
    ("channel_users", "channel_user_id"),
    ("connector_credentials", "credential_id"),
    ("conversation_artifacts", "conversation_artifact_id"),
    ("conversations", "conversation_id"),
    ("creation_tasks", "creation_task_id"),
    ("cron_job_runs", "cron_job_run_id"),
    ("cron_jobs", "cron_job_id"),
    ("idmm_interventions", "intervention_id"),
    ("knowledge_bases", "knowledge_base_id"),
    ("knowledge_bindings", "knowledge_binding_id"),
    ("mcp_servers", "mcp_server_id"),
    ("messages", "message_id"),
    ("preset_tags", "preset_tag_id"),
    ("presets", "preset_id"),
    ("providers", "provider_id"),
    ("remote_agents", "remote_agent_id"),
    ("requirements", "requirement_id"),
    ("terminal_sessions", "terminal_id"),
    ("users", "user_id"),
    ("webhooks", "webhook_id"),
    ("workshop_assets", "asset_id"),
    ("workshop_canvases", "canvas_id"),
];

/// Canonical UUIDv7 values owned by a managed side store rather than a
/// relational entity row in SQLite.
const UUIDV7_MANAGED_VALUE_COLUMNS: &[(&str, &str)] = &[("creation_tasks", "node_id")];

/// `_id` columns that are identities, operation tokens, platform handles, or
/// opaque remote handles rather than relational links. Every other physical
/// `_id` column must be present in [`LOGICAL_REFERENCES`].
const NON_REFERENCE_ID_COLUMNS: &[(&str, &str)] = &[
    ("acp_session", "acp_session_id"),
    ("agent_metadata", "agent_id"),
    ("agent_metadata", "yolo_id"),
    ("agent_execution_attempts", "attempt_id"),
    ("agent_execution_participants", "participant_id"),
    ("agent_execution_steps", "step_id"),
    (
        "agent_execution_template_participants",
        "template_participant_id",
    ),
    ("agent_execution_templates", "execution_template_id"),
    ("agent_executions", "execution_id"),
    ("attachments", "attachment_id"),
    ("channel_pairing_codes", "platform_user_id"),
    ("channel_plugins", "channel_plugin_id"),
    ("channel_sessions", "channel_session_id"),
    ("channel_sessions", "chat_id"),
    ("channel_users", "channel_user_id"),
    ("channel_users", "platform_user_id"),
    ("connector_credentials", "credential_id"),
    ("conversation_artifacts", "conversation_artifact_id"),
    ("conversation_delivery_receipts", "operation_id"),
    ("conversations", "conversation_id"),
    ("conversations", "channel_chat_id"),
    ("cron_job_runs", "cron_job_run_id"),
    ("cron_jobs", "cron_job_id"),
    ("creation_tasks", "creation_task_id"),
    ("creation_tasks", "node_id"),
    ("creation_tasks", "remote_task_id"),
    ("idmm_interventions", "intervention_id"),
    ("knowledge_bases", "knowledge_base_id"),
    ("knowledge_bindings", "knowledge_binding_id"),
    ("mcp_servers", "mcp_server_id"),
    ("messages", "message_id"),
    ("preset_tags", "preset_tag_id"),
    ("presets", "preset_id"),
    ("providers", "provider_id"),
    ("remote_agents", "remote_agent_id"),
    ("remote_agents", "device_id"),
    ("requirements", "requirement_id"),
    ("terminal_sessions", "terminal_id"),
    ("users", "user_id"),
    ("webhooks", "webhook_id"),
    ("workshop_assets", "asset_id"),
    ("workshop_canvases", "canvas_id"),
];

const PARTIAL_UNIQUE_INDEXES: &[PartialUniqueIndexContract] = &[
    PartialUniqueIndexContract {
        index_name: "uq_knowledge_bindings_target_workpath",
        table: "knowledge_bindings",
        columns: &["target_workpath"],
        predicate: "target_kind = 'workpath' AND target_workpath IS NOT NULL",
    },
    PartialUniqueIndexContract {
        index_name: "uq_knowledge_bindings_target_conversation_id",
        table: "knowledge_bindings",
        columns: &["target_conversation_id"],
        predicate: "target_kind = 'conversation' AND target_conversation_id IS NOT NULL",
    },
    PartialUniqueIndexContract {
        index_name: "uq_knowledge_bindings_target_terminal_id",
        table: "knowledge_bindings",
        columns: &["target_terminal_id"],
        predicate: "target_kind = 'terminal' AND target_terminal_id IS NOT NULL",
    },
    PartialUniqueIndexContract {
        index_name: "uq_knowledge_bindings_target_companion_id",
        table: "knowledge_bindings",
        columns: &["target_companion_id"],
        predicate: "target_kind = 'companion' AND target_companion_id IS NOT NULL",
    },
    PartialUniqueIndexContract {
        index_name: "uq_presets_catalog_source_key",
        table: "presets",
        columns: &["source_kind", "source_key"],
        predicate: "source_kind IN ('builtin', 'extension')",
    },
];

#[derive(Clone, Copy, Debug)]
struct PartialUniqueIndexContract {
    index_name: &'static str,
    table: &'static str,
    columns: &'static [&'static str],
    predicate: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LogicalReferenceKind {
    Text,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LogicalReferenceValueContract {
    Opaque,
    CanonicalUuidV7,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeletePolicy {
    Restrict,
    Cascade,
    SetNull,
    KeepHistory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RebuildPolicy {
    PreserveBusinessId,
    /// Preserve a canonical UUIDv7 token that scopes an internal protocol
    /// owner, even though the token is not an entity identity and has no
    /// parent row to remap.
    PreserveProtocolToken,
    ExternalOwner,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OrphanAuditPolicy {
    /// A live child must resolve to a valid parent.
    RequireParent,
    /// Historical rows intentionally retain the former parent value after the
    /// parent is deleted. Existing parents must still satisfy scope rules.
    AllowMissingHistoricalParent,
    /// The parent belongs to another store and cannot be audited by SQLite.
    ExternalOwner,
    /// There is deliberately no parent row. Validate only the value contract
    /// (currently canonical UUIDv7) and do not report the token as an orphan.
    ValidateValueOnly,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LogicalReference {
    pub child_table: &'static str,
    pub child_column: &'static str,
    pub parent_table: Option<&'static str>,
    pub parent_column: Option<&'static str>,
    pub kind: LogicalReferenceKind,
    pub value_contract: LogicalReferenceValueContract,
    pub nullable: bool,
    pub index_name: &'static str,
    pub delete_policy: DeletePolicy,
    pub rebuild_policy: RebuildPolicy,
    pub orphan_audit_policy: OrphanAuditPolicy,
    /// Optional child predicate for polymorphic columns.
    pub child_predicate: Option<&'static str>,
    /// Optional parent predicate for references to live rows in a soft-delete
    /// table. Expressions use the `parent` SQL alias.
    pub parent_predicate: Option<&'static str>,
    /// Optional aggregate-scope predicate. Expressions use the `child` and
    /// `parent` SQL aliases after the reference values have matched.
    pub aggregate_scope_predicate: Option<&'static str>,
}

impl LogicalReference {
    const fn with_orphan_audit_policy(mut self, policy: OrphanAuditPolicy) -> Self {
        self.orphan_audit_policy = policy;
        self
    }

    const fn with_child_predicate(mut self, predicate: &'static str) -> Self {
        self.child_predicate = Some(predicate);
        self
    }

    const fn with_parent_predicate(mut self, predicate: &'static str) -> Self {
        self.parent_predicate = Some(predicate);
        self
    }

    const fn with_aggregate_scope(mut self, predicate: &'static str) -> Self {
        self.aggregate_scope_predicate = Some(predicate);
        self
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct JsonLogicalReference {
    pub child_table: &'static str,
    pub child_column: &'static str,
    pub json_path: &'static str,
    pub value_sql: &'static str,
    pub parent_table: Option<&'static str>,
    pub parent_column: Option<&'static str>,
    pub kind: LogicalReferenceKind,
    pub value_contract: LogicalReferenceValueContract,
    pub index_name: &'static str,
    pub delete_policy: DeletePolicy,
    pub rebuild_policy: RebuildPolicy,
    pub orphan_audit_policy: OrphanAuditPolicy,
}

macro_rules! json_text_ref {
    ($table:literal, $column:literal, $path:literal, $sql:literal =>
     $parent_table:literal, $parent_column:literal, $index:literal, $delete:ident,
     $audit:ident) => {
        JsonLogicalReference {
            child_table: $table,
            child_column: $column,
            json_path: $path,
            value_sql: $sql,
            parent_table: Some($parent_table),
            parent_column: Some($parent_column),
            kind: LogicalReferenceKind::Text,
            value_contract: LogicalReferenceValueContract::CanonicalUuidV7,
            index_name: $index,
            delete_policy: DeletePolicy::$delete,
            rebuild_policy: RebuildPolicy::PreserveBusinessId,
            orphan_audit_policy: OrphanAuditPolicy::$audit,
        }
    };
}

macro_rules! json_external_ref {
    ($table:literal, $column:literal, $path:literal, $sql:literal, $index:literal,
     $delete:ident) => {
        JsonLogicalReference {
            child_table: $table,
            child_column: $column,
            json_path: $path,
            value_sql: $sql,
            parent_table: None,
            parent_column: None,
            kind: LogicalReferenceKind::Text,
            // Cross-store ownership prevents a SQLite parent-existence check,
            // but the identifier itself is still a NomiFun business ID and
            // must remain a canonical bare UUIDv7.
            value_contract: LogicalReferenceValueContract::CanonicalUuidV7,
            index_name: $index,
            delete_policy: DeletePolicy::$delete,
            rebuild_policy: RebuildPolicy::ExternalOwner,
            orphan_audit_policy: OrphanAuditPolicy::ExternalOwner,
        }
    };
}

const fn default_orphan_audit_policy(delete_policy: DeletePolicy) -> OrphanAuditPolicy {
    match delete_policy {
        DeletePolicy::KeepHistory => OrphanAuditPolicy::AllowMissingHistoricalParent,
        DeletePolicy::Restrict | DeletePolicy::Cascade | DeletePolicy::SetNull => {
            OrphanAuditPolicy::RequireParent
        }
    }
}

macro_rules! text_ref {
    ($child_table:literal, $child_column:literal => $parent_table:literal, $parent_column:literal,
     $nullable:expr, $index:literal, $delete:ident) => {
        LogicalReference {
            child_table: $child_table,
            child_column: $child_column,
            parent_table: Some($parent_table),
            parent_column: Some($parent_column),
            kind: LogicalReferenceKind::Text,
            value_contract: LogicalReferenceValueContract::CanonicalUuidV7,
            nullable: $nullable,
            index_name: $index,
            delete_policy: DeletePolicy::$delete,
            rebuild_policy: RebuildPolicy::PreserveBusinessId,
            orphan_audit_policy: default_orphan_audit_policy(DeletePolicy::$delete),
            child_predicate: None,
            parent_predicate: None,
            aggregate_scope_predicate: None,
        }
    };
}

macro_rules! external_ref {
    ($child_table:literal, $child_column:literal, $kind:ident, $nullable:expr,
     $value_contract:ident, $index:literal, $delete:ident) => {
        LogicalReference {
            child_table: $child_table,
            child_column: $child_column,
            parent_table: None,
            parent_column: None,
            kind: LogicalReferenceKind::$kind,
            value_contract: LogicalReferenceValueContract::$value_contract,
            nullable: $nullable,
            index_name: $index,
            delete_policy: DeletePolicy::$delete,
            rebuild_policy: RebuildPolicy::ExternalOwner,
            orphan_audit_policy: OrphanAuditPolicy::ExternalOwner,
            child_predicate: None,
            parent_predicate: None,
            aggregate_scope_predicate: None,
        }
    };
}

macro_rules! protocol_uuidv7_ref {
    ($table:literal, $column:literal, $index:literal, $delete:ident) => {
        LogicalReference {
            child_table: $table,
            child_column: $column,
            parent_table: None,
            parent_column: None,
            kind: LogicalReferenceKind::Text,
            value_contract: LogicalReferenceValueContract::CanonicalUuidV7,
            nullable: false,
            index_name: $index,
            delete_policy: DeletePolicy::$delete,
            rebuild_policy: RebuildPolicy::PreserveProtocolToken,
            orphan_audit_policy: OrphanAuditPolicy::ValidateValueOnly,
            child_predicate: None,
            parent_predicate: None,
            aggregate_scope_predicate: None,
        }
    };
}

/// Database and cross-store links owned by the application. Every entry names
/// its required index, delete policy and restore/clone policy. Parentless
/// entries are deliberate cross-store references; the database audit reports
/// them as externally owned instead of pretending SQLite can verify them.
pub(crate) const LOGICAL_REFERENCES: &[LogicalReference] = &[
    text_ref!("conversations", "user_id" => "users", "user_id", false, "idx_conversations_user_id", Cascade),
    text_ref!("conversations", "cron_job_id" => "cron_jobs", "cron_job_id", true, "idx_conversations_cron_job_id", SetNull),
    text_ref!("conversations", "preset_id" => "presets", "preset_id", true, "idx_conversations_preset_id", SetNull),
    text_ref!("conversations", "execution_template_id" => "agent_execution_templates", "execution_template_id", true, "idx_conversations_execution_template_id", SetNull),
    text_ref!("messages", "conversation_id" => "conversations", "conversation_id", false, "idx_messages_conversation_id", Cascade),
    text_ref!("messages", "msg_id" => "messages", "message_id", true, "idx_messages_msg_id", KeepHistory)
        .with_aggregate_scope("parent.conversation_id = child.conversation_id"),
    text_ref!("terminal_sessions", "user_id" => "users", "user_id", false, "idx_terminal_sessions_user_id", Cascade),
    text_ref!("agent_execution_templates", "user_id" => "users", "user_id", false, "idx_execution_templates_user_id", Cascade),
    text_ref!("agent_execution_templates", "primary_participant_id" => "agent_execution_template_participants", "template_participant_id", false, "idx_execution_templates_primary_participant_id", Restrict)
        .with_aggregate_scope("parent.template_id = child.execution_template_id"),
    text_ref!("agent_executions", "user_id" => "users", "user_id", false, "idx_agent_executions_user_id", Cascade),
    text_ref!("attachments", "requirement_id" => "requirements", "requirement_id", false, "idx_attachments_requirement_id", Cascade),
    text_ref!("channel_sessions", "channel_user_id" => "channel_users", "channel_user_id", false, "idx_channel_sessions_channel_user_id", Cascade),
    text_ref!("channel_sessions", "conversation_id" => "conversations", "conversation_id", true, "idx_channel_sessions_conversation_id", SetNull),
    text_ref!("channel_sessions", "channel_plugin_id" => "channel_plugins", "channel_plugin_id", true, "idx_channel_sessions_channel_plugin_id", SetNull),
    text_ref!("agent_execution_participants", "execution_id" => "agent_executions", "execution_id", false, "idx_execution_participants_execution_id", Cascade),
    text_ref!("agent_execution_participants", "source_agent_id" => "agent_metadata", "agent_id", false, "idx_execution_participants_source_agent_id", KeepHistory),
    text_ref!("agent_execution_participants", "preset_id" => "presets", "preset_id", true, "idx_execution_participants_preset_id", KeepHistory),
    text_ref!("agent_execution_participants", "provider_id" => "providers", "provider_id", true, "idx_execution_participants_provider_id", KeepHistory)
        .with_orphan_audit_policy(OrphanAuditPolicy::RequireParent)
        .with_child_predicate(
            "child.retired_in_revision IS NULL \
             AND EXISTS (\
                 SELECT 1 FROM agent_executions execution \
                 WHERE execution.execution_id = child.execution_id \
                   AND execution.status <> 'cancelled' \
                   AND execution.deleted_at IS NULL\
             )",
        ),
    text_ref!("agent_execution_steps", "execution_id" => "agent_executions", "execution_id", false, "idx_execution_steps_execution_id", Cascade),
    text_ref!("agent_execution_steps", "assigned_participant_id" => "agent_execution_participants", "participant_id", true, "idx_execution_steps_assigned_participant_id", Restrict)
        .with_aggregate_scope("parent.execution_id = child.execution_id"),
    text_ref!("agent_execution_attempts", "execution_id" => "agent_executions", "execution_id", false, "idx_execution_attempts_execution_id", Cascade),
    text_ref!("agent_execution_attempts", "step_id" => "agent_execution_steps", "step_id", false, "idx_execution_attempts_step_id", Cascade)
        .with_aggregate_scope("parent.execution_id = child.execution_id"),
    text_ref!("agent_execution_attempts", "participant_id" => "agent_execution_participants", "participant_id", true, "idx_execution_attempts_participant_id", KeepHistory)
        .with_aggregate_scope("parent.execution_id = child.execution_id"),
    text_ref!("agent_execution_events", "execution_id" => "agent_executions", "execution_id", false, "idx_execution_events_execution_id", Cascade),
    text_ref!("agent_execution_events", "step_id" => "agent_execution_steps", "step_id", true, "idx_execution_events_step_id", KeepHistory)
        .with_aggregate_scope("parent.execution_id = child.execution_id"),
    text_ref!("agent_execution_events", "attempt_id" => "agent_execution_attempts", "attempt_id", true, "idx_execution_events_attempt_id", KeepHistory)
        .with_aggregate_scope(
            "parent.execution_id = child.execution_id AND parent.step_id = child.step_id",
        ),
    text_ref!("agent_execution_events", "actor_id" => "users", "user_id", true, "idx_execution_events_actor_user_id", KeepHistory)
        .with_child_predicate("child.actor_type = 'user'"),
    text_ref!("agent_execution_events", "actor_id" => "conversations", "conversation_id", true, "idx_execution_events_actor_local_agent_id", KeepHistory)
        .with_child_predicate(
            "child.actor_type = 'agent' AND child.actor_conversation_id IS NOT NULL",
        )
        .with_aggregate_scope("parent.conversation_id = child.actor_conversation_id"),
    external_ref!(
        "agent_execution_events",
        "actor_id",
        Text,
        true,
        CanonicalUuidV7,
        "idx_execution_events_actor_external_agent_id",
        KeepHistory
    )
    .with_child_predicate(
        "child.actor_type = 'agent' \
         AND child.actor_conversation_id IS NULL \
         AND child.actor_id IS NOT NULL",
    ),
    text_ref!("agent_execution_events", "actor_conversation_id" => "conversations", "conversation_id", true, "idx_execution_events_actor_conversation_id", KeepHistory),
    text_ref!("agent_execution_events", "actor_attempt_id" => "agent_execution_attempts", "attempt_id", true, "idx_execution_events_actor_attempt_id", KeepHistory)
        .with_aggregate_scope("parent.execution_id = child.execution_id"),
    text_ref!("agent_execution_events", "on_behalf_of_user_id" => "users", "user_id", false, "idx_execution_events_on_behalf_of_user_id", KeepHistory),
    text_ref!("agent_execution_template_participants", "template_id" => "agent_execution_templates", "execution_template_id", false, "idx_template_participants_template_id", Cascade),
    text_ref!("agent_execution_template_participants", "source_agent_id" => "agent_metadata", "agent_id", false, "idx_template_participants_source_agent_id", Restrict),
    text_ref!("agent_execution_template_participants", "preset_id" => "presets", "preset_id", true, "idx_template_participants_preset_id", SetNull),
    text_ref!("agent_execution_template_participants", "provider_id" => "providers", "provider_id", true, "idx_template_participants_provider_id", Restrict),
    text_ref!("conversation_artifacts", "conversation_id" => "conversations", "conversation_id", false, "idx_conversation_artifacts_conversation_id", Cascade),
    text_ref!("conversation_artifacts", "cron_job_id" => "cron_jobs", "cron_job_id", true, "idx_conversation_artifacts_cron_job_id", SetNull),
    text_ref!("conversation_execution_links", "conversation_id" => "conversations", "conversation_id", false, "idx_conversation_execution_links_conversation_id", KeepHistory),
    text_ref!("conversation_execution_links", "execution_id" => "agent_executions", "execution_id", false, "idx_conversation_execution_links_execution_id", Cascade),
    text_ref!("conversation_execution_links", "step_id" => "agent_execution_steps", "step_id", true, "idx_conversation_execution_links_step_id", KeepHistory)
        .with_aggregate_scope("parent.execution_id = child.execution_id"),
    text_ref!("conversation_execution_links", "attempt_id" => "agent_execution_attempts", "attempt_id", true, "idx_conversation_execution_links_attempt_id", KeepHistory)
        .with_aggregate_scope(
            "parent.execution_id = child.execution_id AND parent.step_id = child.step_id",
        ),
    text_ref!("cron_jobs", "user_id" => "users", "user_id", false, "idx_cron_jobs_user_id", Cascade),
    text_ref!("cron_jobs", "preset_id" => "presets", "preset_id", true, "idx_cron_jobs_preset_id", SetNull),
    text_ref!("cron_jobs", "conversation_id" => "conversations", "conversation_id", true, "idx_cron_jobs_conversation_id", Cascade),
    text_ref!("cron_job_runs", "cron_job_id" => "cron_jobs", "cron_job_id", false, "idx_cron_job_runs_cron_job_id", Cascade),
    external_ref!("channel_plugins", "companion_id", Text, true, CanonicalUuidV7, "idx_channel_plugins_companion_id", SetNull),
    external_ref!("channel_plugins", "public_agent_id", Text, true, CanonicalUuidV7, "idx_channel_plugins_public_agent_id", SetNull),
    text_ref!("channel_users", "channel_plugin_id" => "channel_plugins", "channel_plugin_id", true, "idx_channel_users_channel_plugin_id", Cascade),
    text_ref!("creation_tasks", "canvas_id" => "workshop_canvases", "canvas_id", true, "idx_creation_tasks_canvas_id", SetNull),
    text_ref!("creation_tasks", "provider_id" => "providers", "provider_id", false, "idx_creation_tasks_provider_id", Restrict),
    text_ref!("idmm_interventions", "user_id" => "users", "user_id", false, "idx_idmm_interventions_user_id", Cascade),
    text_ref!("idmm_interventions", "target_id" => "conversations", "conversation_id", false, "idx_idmm_interventions_conversation_target_id", Cascade)
        .with_child_predicate("child.target_kind = 'conversation'")
        .with_aggregate_scope("parent.user_id = child.user_id"),
    text_ref!("idmm_interventions", "target_id" => "terminal_sessions", "terminal_id", false, "idx_idmm_interventions_terminal_target_id", Cascade)
        .with_child_predicate("child.target_kind = 'terminal'")
        .with_aggregate_scope("parent.user_id = child.user_id"),
    text_ref!("requirements", "owner_conversation_id" => "conversations", "conversation_id", true, "idx_requirements_owner_conversation_id", SetNull),
    text_ref!("requirements", "owner_terminal_id" => "terminal_sessions", "terminal_id", true, "idx_requirements_owner_terminal_id", SetNull),
    external_ref!("knowledge_bindings", "target_workpath", Text, true, Opaque, "uq_knowledge_bindings_target_workpath", Cascade),
    text_ref!("knowledge_bindings", "target_conversation_id" => "conversations", "conversation_id", true, "uq_knowledge_bindings_target_conversation_id", Cascade)
        .with_child_predicate("child.target_kind = 'conversation'"),
    text_ref!("knowledge_bindings", "target_terminal_id" => "terminal_sessions", "terminal_id", true, "uq_knowledge_bindings_target_terminal_id", Cascade)
        .with_child_predicate("child.target_kind = 'terminal'"),
    external_ref!("knowledge_bindings", "target_companion_id", Text, true, CanonicalUuidV7, "uq_knowledge_bindings_target_companion_id", Cascade),
    text_ref!("agent_execution_step_dependencies", "execution_id" => "agent_executions", "execution_id", false, "idx_execution_dependencies_execution_id", Cascade),
    text_ref!("agent_execution_step_dependencies", "blocker_step_id" => "agent_execution_steps", "step_id", false, "idx_execution_dependencies_blocker_step_id", Cascade)
        .with_aggregate_scope("parent.execution_id = child.execution_id"),
    text_ref!("agent_execution_step_dependencies", "blocked_step_id" => "agent_execution_steps", "step_id", false, "idx_execution_dependencies_blocked_step_id", Cascade)
        .with_aggregate_scope("parent.execution_id = child.execution_id"),
    text_ref!("channel_pairing_codes", "channel_plugin_id" => "channel_plugins", "channel_plugin_id", true, "idx_channel_pairing_codes_channel_plugin_id", Cascade),
    text_ref!("conversation_creation_keys", "user_id" => "users", "user_id", false, "idx_conversation_creation_keys_user_id", Cascade),
    text_ref!("conversation_creation_keys", "conversation_id" => "conversations", "conversation_id", false, "idx_conversation_creation_keys_conversation_id", Cascade),
    text_ref!("conversation_delivery_receipts", "message_id" => "messages", "message_id", false, "idx_delivery_receipts_message_id", KeepHistory),
    text_ref!("conversation_delivery_receipts", "conversation_id" => "conversations", "conversation_id", false, "idx_delivery_receipts_conversation_id", KeepHistory),
    text_ref!("conversation_delivery_receipts", "user_id" => "users", "user_id", false, "idx_delivery_receipts_user_id", KeepHistory),
    text_ref!("conversation_mcp_servers", "conversation_id" => "conversations", "conversation_id", false, "idx_conversation_mcp_servers_conversation_id", Cascade),
    text_ref!("conversation_mcp_servers", "mcp_server_id" => "mcp_servers", "mcp_server_id", false, "idx_conversation_mcp_servers_mcp_server_id", Cascade)
        .with_parent_predicate("parent.deleted_at IS NULL"),
    text_ref!("knowledge_binding_bases", "knowledge_binding_id" => "knowledge_bindings", "knowledge_binding_id", false, "idx_knowledge_binding_bases_knowledge_binding_id", Cascade),
    text_ref!("knowledge_binding_bases", "knowledge_base_id" => "knowledge_bases", "knowledge_base_id", false, "idx_knowledge_binding_bases_knowledge_base_id", Cascade),
    text_ref!("message_correlations", "conversation_id" => "conversations", "conversation_id", false, "idx_message_correlations_conversation_id", Cascade),
    // `turn_message_id` is the wire-scoped owner token supplied by the
    // streaming protocol. A continuation can reserve a correlation before
    // its root/turn message is projected, and some continuations intentionally
    // have no ordinary `messages.message_id` row at all. It is therefore a
    // protocol UUIDv7 token, not a parent reference to `messages`.
    protocol_uuidv7_ref!(
        "message_correlations",
        "turn_message_id",
        "idx_message_correlations_turn_message_id",
        KeepHistory
    ),
    // A correlation reserves message_id before the Message is projected. The
    // missing parent is therefore intentional until projection completes; if
    // the projection exists, it must remain inside the same Conversation.
    text_ref!("message_correlations", "message_id" => "messages", "message_id", false, "idx_message_correlations_message_id", KeepHistory)
        .with_aggregate_scope("parent.conversation_id = child.conversation_id"),
    text_ref!("model_profiles", "provider_id" => "providers", "provider_id", false, "idx_model_profiles_provider_id", Cascade),
    text_ref!("preset_agent_preferences", "preset_id" => "presets", "preset_id", false, "idx_preset_agent_preferences_preset_id", Cascade),
    text_ref!("preset_agent_preferences", "agent_id" => "agent_metadata", "agent_id", false, "idx_preset_agent_preferences_agent_id", Restrict),
    text_ref!("preset_examples", "preset_id" => "presets", "preset_id", false, "idx_preset_examples_preset_id", Cascade),
    text_ref!("preset_knowledge_bases", "preset_id" => "presets", "preset_id", false, "idx_preset_knowledge_bases_preset_id", Cascade),
    text_ref!("preset_knowledge_bases", "knowledge_base_id" => "knowledge_bases", "knowledge_base_id", false, "idx_preset_knowledge_bases_knowledge_base_id", Restrict),
    text_ref!("preset_localizations", "preset_id" => "presets", "preset_id", false, "idx_preset_localizations_preset_id", Cascade),
    text_ref!("preset_model_preferences", "preset_id" => "presets", "preset_id", false, "idx_preset_model_preferences_preset_id", Cascade),
    text_ref!("preset_model_preferences", "provider_id" => "providers", "provider_id", true, "idx_preset_model_preferences_provider_id", SetNull),
    text_ref!("preset_skill_bindings", "preset_id" => "presets", "preset_id", false, "idx_preset_skill_bindings_preset_id", Cascade),
    text_ref!("preset_tag_bindings", "preset_id" => "presets", "preset_id", false, "idx_preset_tag_bindings_preset_id", Cascade),
    text_ref!("preset_tag_bindings", "preset_tag_id" => "preset_tags", "preset_tag_id", false, "idx_preset_tag_bindings_preset_tag_id", Cascade)
        .with_aggregate_scope("parent.dimension = child.dimension"),
    text_ref!("preset_targets", "preset_id" => "presets", "preset_id", false, "idx_preset_targets_preset_id", Cascade),
    text_ref!("requirement_tags", "paused_requirement_id" => "requirements", "requirement_id", true, "idx_requirement_tags_paused_requirement_id", SetNull),
    text_ref!("tag_settings", "webhook_id" => "webhooks", "webhook_id", true, "idx_tag_settings_webhook_id", SetNull),
    text_ref!("acp_session", "conversation_id" => "conversations", "conversation_id", false, "idx_acp_session_conversation_id", Cascade),
    text_ref!("acp_session", "agent_id" => "agent_metadata", "agent_id", true, "idx_acp_session_agent_id", Restrict),
    external_ref!("companion_access_token", "companion_id", Text, false, CanonicalUuidV7, "idx_companion_access_token_companion_id", Cascade),
    text_ref!("installation_identity", "owner_user_id" => "users", "user_id", false, "idx_installation_identity_owner_user_id", Restrict),
    text_ref!("preset_knowledge_policy", "preset_id" => "presets", "preset_id", false, "idx_preset_knowledge_policy_preset_id", Cascade),
    text_ref!("preset_user_state", "preset_id" => "presets", "preset_id", false, "idx_preset_user_state_preset_id", Cascade),
    text_ref!("preset_user_state", "preferred_agent_id" => "agent_metadata", "agent_id", true, "idx_preset_user_state_preferred_agent_id", SetNull),
    text_ref!("terminal_scrollback", "terminal_id" => "terminal_sessions", "terminal_id", false, "idx_terminal_scrollback_terminal_id", Cascade),
];

/// Stable JSON paths that carry Provider or business identifiers. The SQL for
/// each entry yields one column named `value`, including one row per array
/// element where necessary.
pub(crate) const JSON_LOGICAL_REFERENCES: &[JsonLogicalReference] = &[
    json_text_ref!(
        "conversations", "model", "$.provider_id",
        "SELECT json_extract(model, '$.provider_id') AS value FROM conversations WHERE model IS NOT NULL" =>
        "providers", "provider_id", "idx_conversations_model_provider_id", Restrict, RequireParent
    ),
    json_text_ref!(
        "conversations", "execution_model_pool", "$.model.provider_id",
        "SELECT json_extract(execution_model_pool, '$.model.provider_id') AS value FROM conversations WHERE json_extract(execution_model_pool, '$.mode') = 'single'" =>
        "providers", "provider_id", "idx_conversations_execution_model_pool_json", SetNull, RequireParent
    ),
    json_text_ref!(
        "conversations", "execution_model_pool", "$.models[].provider_id",
        "SELECT json_extract(item.value, '$.provider_id') AS value FROM conversations, json_each(conversations.execution_model_pool, '$.models') item WHERE json_extract(conversations.execution_model_pool, '$.mode') = 'range'" =>
        "providers", "provider_id", "idx_conversations_execution_model_pool_json", SetNull, RequireParent
    ),
    json_text_ref!(
        "conversations", "extra", "$.idmm.fault_watch.bypass_model.provider_id",
        "SELECT json_extract(extra, '$.idmm.fault_watch.bypass_model.provider_id') AS value FROM conversations" =>
        "providers", "provider_id", "idx_conversations_extra_idmm_fault_provider_id", SetNull, RequireParent
    ),
    json_text_ref!(
        "conversations", "extra", "$.idmm.decision_watch.bypass_model.provider_id",
        "SELECT json_extract(extra, '$.idmm.decision_watch.bypass_model.provider_id') AS value FROM conversations" =>
        "providers", "provider_id", "idx_conversations_extra_idmm_decision_provider_id", SetNull, RequireParent
    ),
    json_text_ref!(
        "terminal_sessions", "idmm", "$.fault_watch.bypass_model.provider_id",
        "SELECT json_extract(idmm, '$.fault_watch.bypass_model.provider_id') AS value FROM terminal_sessions WHERE idmm IS NOT NULL" =>
        "providers", "provider_id", "idx_terminal_sessions_idmm_fault_provider_id", SetNull, RequireParent
    ),
    json_text_ref!(
        "terminal_sessions", "idmm", "$.decision_watch.bypass_model.provider_id",
        "SELECT json_extract(idmm, '$.decision_watch.bypass_model.provider_id') AS value FROM terminal_sessions WHERE idmm IS NOT NULL" =>
        "providers", "provider_id", "idx_terminal_sessions_idmm_decision_provider_id", SetNull, RequireParent
    ),
    json_text_ref!(
        "cron_jobs", "agent_config", "$.provider_id (agent_type=nomi)",
        "SELECT json_extract(agent_config, '$.provider_id') AS value FROM cron_jobs WHERE agent_type = 'nomi' AND agent_config IS NOT NULL" =>
        "providers", "provider_id", "idx_cron_jobs_nomi_provider_id", Restrict, RequireParent
    ),
    json_text_ref!(
        "knowledge_bases", "extra", "$.source.credentialRef",
        "SELECT json_extract(extra, '$.source.credentialRef') AS value FROM knowledge_bases WHERE json_extract(extra, '$.source.credentialRef') IS NOT NULL" =>
        "connector_credentials", "credential_id", "idx_knowledge_bases_extra_credential_ref", Restrict, RequireParent
    ),
    json_text_ref!(
        "workshop_assets", "origin", "$.provider_id",
        "SELECT json_extract(origin, '$.provider_id') AS value FROM workshop_assets WHERE origin IS NOT NULL" =>
        "providers", "provider_id", "idx_workshop_assets_origin_provider_id", KeepHistory, AllowMissingHistoricalParent
    ),
    json_text_ref!(
        "workshop_assets", "origin", "$.canvas_id",
        "SELECT json_extract(origin, '$.canvas_id') AS value FROM workshop_assets WHERE origin IS NOT NULL" =>
        "workshop_canvases", "canvas_id", "idx_workshop_assets_origin_canvas_id", KeepHistory, AllowMissingHistoricalParent
    ),
    json_text_ref!(
        "workshop_assets", "origin", "$.creation_task_id",
        "SELECT json_extract(origin, '$.creation_task_id') AS value FROM workshop_assets WHERE origin IS NOT NULL" =>
        "creation_tasks", "creation_task_id", "idx_workshop_assets_origin_creation_task_id", KeepHistory, AllowMissingHistoricalParent
    ),
    json_external_ref!(
        "workshop_assets", "origin", "$.node_id",
        "SELECT json_extract(origin, '$.node_id') AS value FROM workshop_assets WHERE origin IS NOT NULL",
        "idx_workshop_assets_origin_node_id", KeepHistory
    ),
    json_text_ref!(
        "creation_tasks", "result_asset_ids", "$[]",
        "SELECT item.value AS value FROM creation_tasks, json_each(creation_tasks.result_asset_ids) item" =>
        "workshop_assets", "asset_id", "idx_creation_tasks_result_asset_ids_json", SetNull, RequireParent
    ),
    json_text_ref!(
        "client_preferences", "value", "$ (idmm_backup_provider_id)",
        "SELECT value AS value FROM client_preferences WHERE key = 'idmm_backup_provider_id'" =>
        "providers", "provider_id", "idx_client_preferences_provider_key", Restrict, RequireParent
    ),
    json_text_ref!(
        "client_preferences", "value", "$.queue[].provider_id",
        "SELECT json_extract(item.value, '$.provider_id') AS value FROM client_preferences preference, json_each(preference.value, '$.queue') item WHERE preference.key = 'agent.model_failover' AND json_valid(preference.value)" =>
        "providers", "provider_id", "idx_client_preferences_provider_key", SetNull, RequireParent
    ),
    json_text_ref!(
        "client_preferences", "value", "$[].provider_id",
        "SELECT json_extract(item.value, '$.provider_id') AS value FROM client_preferences preference, json_each(preference.value) item WHERE preference.key = 'nomi.collaborationModels' AND json_valid(preference.value)" =>
        "providers", "provider_id", "idx_client_preferences_provider_key", SetNull, RequireParent
    ),
    json_text_ref!(
        "client_preferences", "value", "$.provider_id",
        "SELECT json_extract(value, '$.provider_id') AS value FROM client_preferences WHERE (key = 'nomi.defaultModel' OR key = 'knowledge.autogenModel' OR key = 'tools.imageGenerationModel' OR key = 'tools.speechToText' OR key LIKE 'channels.%.defaultModel') AND json_valid(value)" =>
        "providers", "provider_id", "idx_client_preferences_provider_key", SetNull, RequireParent
    ),
    json_text_ref!(
        "conversations", "extra", "$.remote_agent_id",
        "SELECT json_extract(extra, '$.remote_agent_id') AS value FROM conversations" =>
        "remote_agents", "remote_agent_id", "idx_conversations_extra_remote_agent_id", Restrict, RequireParent
    ),
    json_text_ref!(
        "conversations", "extra", "$.agent_id",
        "SELECT json_extract(extra, '$.agent_id') AS value FROM conversations" =>
        "agent_metadata", "agent_id", "idx_conversations_extra_agent_id", Restrict, RequireParent
    ),
    json_text_ref!(
        "conversations", "extra", "$.custom_agent_id",
        "SELECT json_extract(extra, '$.custom_agent_id') AS value FROM conversations" =>
        "agent_metadata", "agent_id", "idx_conversations_extra_custom_agent_id", Restrict, RequireParent
    ),
    json_external_ref!(
        "conversations", "extra", "$.companion_id",
        "SELECT json_extract(extra, '$.companion_id') AS value FROM conversations",
        "idx_conversations_extra_companion_id", KeepHistory
    ),
    json_external_ref!(
        "conversations", "extra", "$.public_agent_id",
        "SELECT json_extract(extra, '$.public_agent_id') AS value FROM conversations",
        "idx_conversations_extra_public_agent_id", KeepHistory
    ),
];

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct OrphanAuditFinding {
    pub child_table: String,
    pub child_column: String,
    pub parent_table: String,
    pub parent_column: String,
    pub count: i64,
    pub delete_policy: &'static str,
    pub rebuild_policy: &'static str,
}

/// Validate the structural v3 invariants and the logical-reference registry.
pub async fn validate_id_schema_contract(pool: &SqlitePool) -> Result<(), DbError> {
    let actual_tables: BTreeSet<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations'",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    let expected_tables: BTreeSet<String> = PRODUCT_TABLES.iter().map(|value| (*value).to_owned()).collect();
    if actual_tables != expected_tables {
        let missing = expected_tables.difference(&actual_tables).cloned().collect::<Vec<_>>();
        let extra = actual_tables.difference(&expected_tables).cloned().collect::<Vec<_>>();
        return Err(DbError::Init(format!(
            "v3 schema product-table registry mismatch; missing={missing:?}, extra={extra:?}"
        )));
    }

    for table in PRODUCT_TABLES {
        require_autoincrement_primary_key(pool, table).await?;
    }
    validate_no_physical_foreign_keys(pool).await?;
    validate_no_triggers(pool).await?;
    validate_no_row_id_columns(pool).await?;

    validate_business_id_registry(pool).await?;
    require_column(pool, "preset_tags", "preset_tag_id", "TEXT", true).await?;
    require_single_column_unique_index(pool, "preset_tags", "preset_tag_id").await?;
    require_column(pool, "preset_tags", "key", "TEXT", true).await?;
    require_single_column_unique_index(pool, "preset_tags", "key").await?;
    require_column(pool, "preset_tag_bindings", "preset_tag_id", "TEXT", true).await?;

    validate_logical_reference_registry(pool).await?;
    validate_logical_reference_coverage(pool).await?;
    validate_json_logical_reference_registry(pool).await?;
    require_workshop_asset_origin_id_contract(pool).await?;
    for contract in PARTIAL_UNIQUE_INDEXES {
        require_partial_unique_index(
            pool,
            contract.index_name,
            contract.table,
            contract.columns,
            contract.predicate,
        )
        .await?;
    }
    Ok(())
}

/// Validate every populated stable business ID, managed UUID value and
/// canonical logical-reference column in the v3 registry.
pub(crate) async fn validate_id_value_contract(pool: &SqlitePool) -> Result<(), DbError> {
    for (table, column) in UUIDV7_BUSINESS_COLUMNS {
        validate_uuidv7_column_values(pool, table, column, None).await?;
    }
    for (table, column) in UUIDV7_MANAGED_VALUE_COLUMNS {
        validate_uuidv7_column_values(pool, table, column, None).await?;
    }
    for reference in LOGICAL_REFERENCES {
        if reference.value_contract == LogicalReferenceValueContract::CanonicalUuidV7 {
            validate_uuidv7_column_values(
                pool,
                reference.child_table,
                reference.child_column,
                reference.child_predicate,
            )
            .await?;
        }
    }
    Ok(())
}

/// Validate the complete durable v3 ID data contract. Any failure identifies
/// the current managed dataset as incompatible; callers must quarantine/reset
/// the dataset rather than rewrite IDs.
pub async fn validate_id_data_contract(pool: &SqlitePool) -> Result<(), DbError> {
    validate_id_value_contract(pool).await?;
    validate_workshop_asset_origin_values(pool).await?;
    validate_creation_task_result_asset_ids(pool).await?;
    let findings = audit_logical_reference_orphans(pool).await?;
    if findings.is_empty() {
        return Ok(());
    }
    let details = findings
        .iter()
        .map(|finding| {
            format!(
                "{}.{} -> {}.{}: {} invalid/orphan value(s)",
                finding.child_table,
                finding.child_column,
                finding.parent_table,
                finding.parent_column,
                finding.count
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Err(DbError::Init(format!(
        "v3 ID data contract audit failed: {details}"
    )))
}

/// Read-only database orphan audit. Cross-store registry entries are skipped;
/// their owners must extend this skeleton with side-store inventory checks.
/// Keep-history and catalog-backed references permit an absent parent, but a
/// still-present parent must satisfy live-row and aggregate-scope predicates.
pub(crate) async fn audit_logical_reference_orphans(
    pool: &SqlitePool,
) -> Result<Vec<OrphanAuditFinding>, DbError> {
    let mut findings = Vec::new();
    for reference in LOGICAL_REFERENCES {
        if matches!(
            reference.orphan_audit_policy,
            OrphanAuditPolicy::ExternalOwner | OrphanAuditPolicy::ValidateValueOnly
        ) {
            audit_parentless_logical_reference_values(pool, reference, &mut findings).await?;
            continue;
        }
        let (Some(parent_table), Some(parent_column)) =
            (reference.parent_table, reference.parent_column)
        else {
            continue;
        };
        let child_predicate = reference
            .child_predicate
            .map(|value| format!(" AND ({value})"))
            .unwrap_or_default();
        let parent_predicate = reference
            .parent_predicate
            .map(|value| format!(" AND ({value})"))
            .unwrap_or_default();
        let aggregate_scope_predicate = reference
            .aggregate_scope_predicate
            .map(|value| format!(" AND ({value})"))
            .unwrap_or_default();
        let parent_exists = format!(
            "EXISTS (SELECT 1 FROM {parent_table} parent \
                     WHERE parent.{parent_column} = child.{child_column})",
            parent_table = quote_sqlite_identifier(parent_table),
            parent_column = quote_sqlite_identifier(parent_column),
            child_column = quote_sqlite_identifier(reference.child_column),
        );
        let valid_parent_exists = format!(
            "EXISTS (SELECT 1 FROM {parent_table} parent \
                     WHERE parent.{parent_column} = child.{child_column}\
                     {parent_predicate}{aggregate_scope_predicate})",
            parent_table = quote_sqlite_identifier(parent_table),
            parent_column = quote_sqlite_identifier(parent_column),
            child_column = quote_sqlite_identifier(reference.child_column),
        );
        let invalid_parent_predicate = match reference.orphan_audit_policy {
            OrphanAuditPolicy::RequireParent => format!("NOT {valid_parent_exists}"),
            OrphanAuditPolicy::AllowMissingHistoricalParent => {
                format!("{parent_exists} AND NOT {valid_parent_exists}")
            }
            OrphanAuditPolicy::ExternalOwner | OrphanAuditPolicy::ValidateValueOnly => {
                unreachable!("handled above")
            }
        };
        let sql = format!(
            "SELECT COUNT(*) FROM {child_table} child \
             WHERE child.{child_column} IS NOT NULL{child_predicate} \
               AND ({invalid_parent_predicate})",
            child_table = quote_sqlite_identifier(reference.child_table),
            child_column = quote_sqlite_identifier(reference.child_column),
        );
        let count: i64 = sqlx::query_scalar(&sql).fetch_one(pool).await?;
        if count > 0 {
            findings.push(OrphanAuditFinding {
                child_table: reference.child_table.to_owned(),
                child_column: reference.child_column.to_owned(),
                parent_table: parent_table.to_owned(),
                parent_column: parent_column.to_owned(),
                count,
                delete_policy: delete_policy_name(reference.delete_policy),
                rebuild_policy: rebuild_policy_name(reference.rebuild_policy),
            });
        }
    }
    audit_json_logical_reference_orphans(pool, &mut findings).await?;
    Ok(findings)
}

async fn audit_parentless_logical_reference_values(
    pool: &SqlitePool,
    reference: &LogicalReference,
    findings: &mut Vec<OrphanAuditFinding>,
) -> Result<(), DbError> {
    if reference.kind != LogicalReferenceKind::Text
        || reference.value_contract != LogicalReferenceValueContract::CanonicalUuidV7
    {
        return Ok(());
    }
    let child_predicate = reference
        .child_predicate
        .map(|value| format!(" AND ({value})"))
        .unwrap_or_default();
    let sql = format!(
        "SELECT child.{column} AS value \
         FROM {table} child \
         WHERE child.{column} IS NOT NULL{child_predicate}",
        table = quote_sqlite_identifier(reference.child_table),
        column = quote_sqlite_identifier(reference.child_column),
    );
    let values: Vec<String> = sqlx::query_scalar(&sql).fetch_all(pool).await?;
    let invalid = values
        .iter()
        .filter(|value| nomifun_common::validate_uuidv7(value).is_err())
        .count() as i64;
    if invalid > 0 {
        findings.push(OrphanAuditFinding {
            child_table: reference.child_table.to_owned(),
            child_column: reference.child_column.to_owned(),
            parent_table: match reference.orphan_audit_policy {
                OrphanAuditPolicy::ExternalOwner => "<external>",
                OrphanAuditPolicy::ValidateValueOnly => "<protocol-token>",
                OrphanAuditPolicy::RequireParent
                | OrphanAuditPolicy::AllowMissingHistoricalParent => {
                    unreachable!("parentless audit requires a parentless policy")
                }
            }
            .to_owned(),
            parent_column: "<none>".to_owned(),
            count: invalid,
            delete_policy: delete_policy_name(reference.delete_policy),
            rebuild_policy: rebuild_policy_name(reference.rebuild_policy),
        });
    }
    Ok(())
}

async fn require_autoincrement_primary_key(pool: &SqlitePool, table: &str) -> Result<(), DbError> {
    let columns = table_info(pool, table).await?;
    let Some(id) = columns.iter().find(|column| column.name == "id") else {
        return Err(DbError::Init(format!("v3 schema table {table} is missing id")));
    };
    if id.data_type != "INTEGER" || id.primary_key_position != 1 {
        return Err(DbError::Init(format!(
            "v3 schema {table}.id must be the single INTEGER primary key"
        )));
    }
    if columns.iter().filter(|column| column.primary_key_position > 0).count() != 1 {
        return Err(DbError::Init(format!(
            "v3 schema table {table} must not have a composite primary key"
        )));
    }
    let create_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?",
    )
    .bind(table)
    .fetch_one(pool)
    .await?;
    let normalized = normalize_sql(&create_sql);
    if !normalized.contains("ID INTEGER PRIMARY KEY AUTOINCREMENT") {
        return Err(DbError::Init(format!(
            "v3 schema table {table} must declare id INTEGER PRIMARY KEY AUTOINCREMENT"
        )));
    }
    Ok(())
}

async fn validate_no_physical_foreign_keys(pool: &SqlitePool) -> Result<(), DbError> {
    for table in PRODUCT_TABLES {
        let sql = format!("PRAGMA foreign_key_list({})", quote_sqlite_identifier(table));
        if sqlx::query(&sql).fetch_optional(pool).await?.is_some() {
            return Err(DbError::Init(format!(
                "v3 schema forbids physical foreign keys; found one on {table}"
            )));
        }
    }
    Ok(())
}

async fn validate_no_triggers(pool: &SqlitePool) -> Result<(), DbError> {
    let triggers: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_schema WHERE type = 'trigger' ORDER BY name")
            .fetch_all(pool)
            .await?;
    if !triggers.is_empty() {
        return Err(DbError::Init(format!(
            "v3 schema forbids database triggers; found {triggers:?}"
        )));
    }
    Ok(())
}

async fn validate_no_row_id_columns(pool: &SqlitePool) -> Result<(), DbError> {
    for table in PRODUCT_TABLES {
        for column in table_info(pool, table).await? {
            if column.name.ends_with("_row_id") {
                return Err(DbError::Init(format!(
                    "v3 schema forbids dual-key column {table}.{}",
                    column.name
                )));
            }
        }
    }
    Ok(())
}

async fn validate_business_id_registry(pool: &SqlitePool) -> Result<(), DbError> {
    let mut seen = BTreeSet::new();
    for (table, column) in UUIDV7_BUSINESS_COLUMNS {
        if !seen.insert((*table, *column)) {
            return Err(DbError::Init(format!(
                "v3 business-ID registry duplicates {table}.{column}"
            )));
        }
        require_column(pool, table, column, "TEXT", true).await?;
        require_single_column_unique_index(pool, table, column).await?;
        require_uuidv7_check(pool, table, column).await?;
    }
    for (table, column) in UUIDV7_MANAGED_VALUE_COLUMNS {
        require_column(pool, table, column, "TEXT", false).await?;
        require_uuidv7_check(pool, table, column).await?;
    }
    Ok(())
}

async fn validate_logical_reference_registry(pool: &SqlitePool) -> Result<(), DbError> {
    let mut seen_columns = BTreeSet::new();
    let mut seen_indexes = BTreeSet::new();
    for reference in LOGICAL_REFERENCES {
        let key = (
            reference.child_table,
            reference.child_column,
            reference.parent_table,
            reference.parent_column,
            reference.child_predicate,
        );
        if !seen_columns.insert(key) {
            return Err(DbError::Init(format!(
                "logical-reference registry duplicates {}.{} predicate {:?}",
                reference.child_table, reference.child_column, reference.child_predicate
            )));
        }
        if !seen_indexes.insert(reference.index_name) {
            return Err(DbError::Init(format!(
                "logical-reference registry duplicates index {}",
                reference.index_name
            )));
        }

        let expected_type = "TEXT";
        require_column(
            pool,
            reference.child_table,
            reference.child_column,
            expected_type,
            !reference.nullable,
        )
        .await?;
        require_index_prefix(
            pool,
            reference.index_name,
            reference.child_table,
            reference.child_column,
        )
        .await?;
        if reference.value_contract == LogicalReferenceValueContract::CanonicalUuidV7 {
            require_uuidv7_check(pool, reference.child_table, reference.child_column).await?;
        }
        if let (Some(parent_table), Some(parent_column)) =
            (reference.parent_table, reference.parent_column)
        {
            require_column(pool, parent_table, parent_column, expected_type, true).await?;
            require_unique_parent_identity(pool, parent_table, parent_column).await?;
        }
    }
    Ok(())
}

async fn validate_logical_reference_coverage(pool: &SqlitePool) -> Result<(), DbError> {
    let registered: BTreeSet<(&str, &str)> = LOGICAL_REFERENCES
        .iter()
        .map(|reference| (reference.child_table, reference.child_column))
        .collect();
    let exempt: BTreeSet<(&str, &str)> = NON_REFERENCE_ID_COLUMNS.iter().copied().collect();
    let mut missing = Vec::new();
    for table in PRODUCT_TABLES {
        for column in table_info(pool, table).await? {
            if column.name != "id"
                && column.name.ends_with("_id")
                && !registered.contains(&(*table, column.name.as_str()))
                && !exempt.contains(&(*table, column.name.as_str()))
            {
                missing.push(format!("{table}.{}", column.name));
            }
        }
    }
    if !missing.is_empty() {
        return Err(DbError::Init(format!(
            "v3 logical-reference registry is missing relationship-like columns {missing:?}"
        )));
    }
    Ok(())
}

async fn validate_json_logical_reference_registry(pool: &SqlitePool) -> Result<(), DbError> {
    let mut seen = BTreeSet::new();
    for reference in JSON_LOGICAL_REFERENCES {
        let key = (
            reference.child_table,
            reference.child_column,
            reference.json_path,
        );
        if !seen.insert(key) {
            return Err(DbError::Init(format!(
                "JSON logical-reference registry duplicates {}.{}:{}",
                reference.child_table, reference.child_column, reference.json_path
            )));
        }
        require_column(
            pool,
            reference.child_table,
            reference.child_column,
            "TEXT",
            false,
        )
        .await?;
        require_index_on_table(pool, reference.index_name, reference.child_table).await?;
        let expected_type = "TEXT";
        if let (Some(parent_table), Some(parent_column)) =
            (reference.parent_table, reference.parent_column)
        {
            require_column(pool, parent_table, parent_column, expected_type, true).await?;
            require_unique_parent_identity(pool, parent_table, parent_column).await?;
        }

        // Compile every registered extractor against the live SQLite build.
        // This catches misspelled paths/columns and unavailable JSON1 support.
        let sql = format!(
            "SELECT value FROM ({}) logical_reference LIMIT 0",
            reference.value_sql
        );
        sqlx::query(&sql).fetch_all(pool).await?;
    }
    Ok(())
}

async fn require_workshop_asset_origin_id_contract(
    pool: &SqlitePool,
) -> Result<(), DbError> {
    let create_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'workshop_assets'",
    )
    .fetch_one(pool)
    .await?;
    let normalized = normalize_sql(&create_sql);
    for retired_key in [
        "TASK_ID",
        "PROVIDERID",
        "CANVASID",
        "NODEID",
        "CREATIONTASKID",
    ] {
        let fragment = format!("JSON_TYPE(ORIGIN, '$.{retired_key}') IS NULL");
        if !normalized.contains(&fragment) {
            return Err(DbError::Init(format!(
                "v3 workshop_assets.origin contract permits unsupported field {retired_key}"
            )));
        }
    }
    for key in [
        "PROVIDER_ID",
        "CANVAS_ID",
        "NODE_ID",
        "CREATION_TASK_ID",
    ] {
        let fragments = [
            format!("JSON_TYPE(ORIGIN, '$.{key}') IS NULL"),
            format!("JSON_TYPE(ORIGIN, '$.{key}') = 'TEXT'"),
            format!("LENGTH(JSON_EXTRACT(ORIGIN, '$.{key}')) = 36"),
            format!(
                "JSON_EXTRACT(ORIGIN, '$.{key}') GLOB '????????-????-7???-[89AB]???-????????????'"
            ),
            format!(
                "REPLACE(JSON_EXTRACT(ORIGIN, '$.{key}'), '-', '') NOT GLOB '*[^0-9A-F]*'"
            ),
        ];
        for fragment in fragments {
            if !normalized.contains(&fragment) {
                return Err(DbError::Init(format!(
                    "v3 workshop_assets.origin {key} contract is missing CHECK fragment {fragment}"
                )));
            }
        }
    }
    Ok(())
}

async fn validate_workshop_asset_origin_values(pool: &SqlitePool) -> Result<(), DbError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT asset_id, origin FROM workshop_assets WHERE origin IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    for (asset_id, encoded) in rows {
        let value: serde_json::Value = serde_json::from_str(&encoded).map_err(|error| {
            DbError::Init(format!(
                "v3 workshop asset {asset_id} has invalid origin JSON: {error}"
            ))
        })?;
        let object = value.as_object().ok_or_else(|| {
            DbError::Init(format!(
                "v3 workshop asset {asset_id} origin must be a JSON object"
            ))
        })?;
        for retired_key in [
            "task_id",
            "providerId",
            "canvasId",
            "nodeId",
            "creationTaskId",
        ] {
            if object.contains_key(retired_key) {
                return Err(DbError::Init(format!(
                    "v3 workshop asset {asset_id} origin contains unsupported ID field {retired_key:?}"
                )));
            }
        }
        for key in [
            "provider_id",
            "canvas_id",
            "node_id",
            "creation_task_id",
        ] {
            let Some(value) = object.get(key) else {
                continue;
            };
            let value = value.as_str().ok_or_else(|| {
                DbError::Init(format!(
                    "v3 workshop asset {asset_id} origin.{key} must be omitted or a canonical UUIDv7 string"
                ))
            })?;
            nomifun_common::validate_uuidv7(value).map_err(|error| {
                DbError::Init(format!(
                    "v3 workshop asset {asset_id} origin.{key}={value:?} is invalid: {error}"
                ))
            })?;
        }
    }
    Ok(())
}

async fn validate_creation_task_result_asset_ids(pool: &SqlitePool) -> Result<(), DbError> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT creation_task_id, status, result_asset_ids FROM creation_tasks",
    )
    .fetch_all(pool)
    .await?;
    for (creation_task_id, status, encoded) in rows {
        let values: serde_json::Value = serde_json::from_str(&encoded).map_err(|error| {
            DbError::Init(format!(
                "v3 creation task {creation_task_id} has invalid result_asset_ids JSON: {error}"
            ))
        })?;
        let values = values.as_array().ok_or_else(|| {
            DbError::Init(format!(
                "v3 creation task {creation_task_id} result_asset_ids must be a JSON array"
            ))
        })?;
        if status == "succeeded" && values.is_empty() {
            return Err(DbError::Init(format!(
                "v3 creation task {creation_task_id} is succeeded but has no result assets"
            )));
        }
        if status != "succeeded" && !values.is_empty() {
            return Err(DbError::Init(format!(
                "v3 creation task {creation_task_id} is {status:?} but claims committed result assets"
            )));
        }
        let mut seen = BTreeSet::new();
        for value in values {
            let value = value.as_str().ok_or_else(|| {
                DbError::Init(format!(
                    "v3 creation task {creation_task_id} result_asset_ids must contain only canonical UUIDv7 strings"
                ))
            })?;
            nomifun_common::WorkshopAssetId::parse(value).map_err(|error| {
                DbError::Init(format!(
                    "v3 creation task {creation_task_id} result asset {value:?} is invalid: {error}"
                ))
            })?;
            if !seen.insert(value) {
                return Err(DbError::Init(format!(
                    "v3 creation task {creation_task_id} contains duplicate result asset {value}"
                )));
            }
            let origin: Option<String> =
                sqlx::query_scalar("SELECT origin FROM workshop_assets WHERE asset_id = ?")
                    .bind(value)
                    .fetch_optional(pool)
                    .await?
                    .flatten();
            let origin = origin.ok_or_else(|| {
                DbError::Init(format!(
                    "v3 creation task {creation_task_id} result asset {value} is missing or has no managed origin"
                ))
            })?;
            let owner = serde_json::from_str::<serde_json::Value>(&origin)
                .ok()
                .and_then(|origin| {
                    origin
                        .get("creation_task_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                });
            if owner.as_deref() != Some(creation_task_id.as_str()) {
                return Err(DbError::Init(format!(
                    "v3 creation task {creation_task_id} result asset {value} belongs to {:?}",
                    owner.as_deref()
                )));
            }
        }
    }
    Ok(())
}

async fn audit_json_logical_reference_orphans(
    pool: &SqlitePool,
    findings: &mut Vec<OrphanAuditFinding>,
) -> Result<(), DbError> {
    for reference in JSON_LOGICAL_REFERENCES {
        let sql = format!(
            "SELECT value, typeof(value) AS value_type \
             FROM ({}) logical_reference WHERE value IS NOT NULL",
            reference.value_sql
        );
        let rows = sqlx::query(&sql).fetch_all(pool).await?;
        let mut invalid = 0_i64;
        for row in rows {
            let value_type: String = row.try_get("value_type").map_err(DbError::Query)?;
            let parent_exists = {
                    if value_type != "text" {
                        invalid += 1;
                        continue;
                    }
                    let value: String = row.try_get("value").map_err(DbError::Query)?;
                    if value.trim().is_empty()
                        || (reference.value_contract
                            == LogicalReferenceValueContract::CanonicalUuidV7
                            && nomifun_common::validate_uuidv7(&value).is_err())
                    {
                        invalid += 1;
                        continue;
                    }
                    match (reference.parent_table, reference.parent_column) {
                        (Some(parent_table), Some(parent_column)) => {
                            let parent_sql = format!(
                                "SELECT EXISTS(SELECT 1 FROM {} WHERE {} = ?)",
                                quote_sqlite_identifier(parent_table),
                                quote_sqlite_identifier(parent_column),
                            );
                            sqlx::query_scalar::<_, bool>(&parent_sql)
                                .bind(value)
                                .fetch_one(pool)
                                .await?
                        }
                        _ => true,
                    }
            };
            if !parent_exists
                && reference.orphan_audit_policy == OrphanAuditPolicy::RequireParent
            {
                invalid += 1;
            }
        }
        if invalid > 0 {
            findings.push(OrphanAuditFinding {
                child_table: reference.child_table.to_owned(),
                child_column: format!("{}:{}", reference.child_column, reference.json_path),
                parent_table: reference.parent_table.unwrap_or("<external>").to_owned(),
                parent_column: reference.parent_column.unwrap_or("<external>").to_owned(),
                count: invalid,
                delete_policy: delete_policy_name(reference.delete_policy),
                rebuild_policy: rebuild_policy_name(reference.rebuild_policy),
            });
        }
    }
    Ok(())
}

async fn validate_uuidv7_column_values(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    predicate: Option<&str>,
) -> Result<(), DbError> {
    let predicate = predicate
        .map(|value| format!(" AND ({value})"))
        .unwrap_or_default();
    let sql = format!(
        "SELECT child.{column} FROM {table} child WHERE child.{column} IS NOT NULL{predicate}",
        table = quote_sqlite_identifier(table),
        column = quote_sqlite_identifier(column),
    );
    let values: Vec<String> = sqlx::query_scalar(&sql).fetch_all(pool).await?;
    for value in values {
        nomifun_common::validate_uuidv7(&value).map_err(|error| {
            DbError::Init(format!(
                "v3 business ID {table}.{column}={value:?} is invalid: {error}"
            ))
        })?;
    }
    Ok(())
}

async fn require_column(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    expected_type: &str,
    required: bool,
) -> Result<(), DbError> {
    let columns = table_info(pool, table).await?;
    let Some(actual) = columns.iter().find(|value| value.name == column) else {
        return Err(DbError::Init(format!(
            "v3 schema table {table} is missing column {column}"
        )));
    };
    if actual.data_type != expected_type {
        return Err(DbError::Init(format!(
            "v3 schema {table}.{column} must be {expected_type}, found {}",
            actual.data_type
        )));
    }
    if required && !actual.not_null && actual.primary_key_position == 0 {
        return Err(DbError::Init(format!(
            "v3 schema {table}.{column} must be NOT NULL"
        )));
    }
    Ok(())
}

async fn require_single_column_unique_index(
    pool: &SqlitePool,
    table: &str,
    column: &str,
) -> Result<(), DbError> {
    let indexes = index_columns(pool, table).await?;
    if !indexes
        .values()
        .any(|index| index.unique && index.columns.len() == 1 && index.columns[0] == column)
    {
        return Err(DbError::Init(format!(
            "v3 business ID {table}.{column} must have a single-column UNIQUE index"
        )));
    }
    Ok(())
}

async fn require_unique_parent_identity(
    pool: &SqlitePool,
    table: &str,
    column: &str,
) -> Result<(), DbError> {
    if column == "id" {
        let columns = table_info(pool, table).await?;
        if columns
            .iter()
            .any(|value| value.name == "id" && value.primary_key_position == 1)
        {
            return Ok(());
        }
    }
    require_single_column_unique_index(pool, table, column).await
}

async fn require_uuidv7_check(pool: &SqlitePool, table: &str, column: &str) -> Result<(), DbError> {
    let create_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?",
    )
    .bind(table)
    .fetch_one(pool)
    .await?;
    let normalized = normalize_sql(&create_sql);
    let column_name = column.to_ascii_uppercase();
    for fragment in [
        format!("LENGTH({column_name}) = 36"),
        format!("LOWER({column_name}) = {column_name}"),
        format!("{column_name} GLOB '????????-????-7???-[89AB]???-????????????'"),
        format!("REPLACE({column_name}, '-', '') NOT GLOB '*[^0-9A-F]*'"),
    ] {
        if !normalized.contains(&fragment) {
            return Err(DbError::Init(format!(
                "v3 business ID {table}.{column} is missing UUIDv7 CHECK fragment {fragment}"
            )));
        }
    }
    Ok(())
}

async fn require_index_prefix(
    pool: &SqlitePool,
    index_name: &str,
    table: &str,
    column: &str,
) -> Result<(), DbError> {
    let actual_table: Option<String> = sqlx::query_scalar(
        "SELECT tbl_name FROM sqlite_schema WHERE type = 'index' AND name = ?",
    )
    .bind(index_name)
    .fetch_optional(pool)
    .await?;
    if actual_table.as_deref() != Some(table) {
        return Err(DbError::Init(format!(
            "logical reference {table}.{column} requires index {index_name}"
        )));
    }
    let sql = format!("PRAGMA index_info({})", quote_sqlite_identifier(index_name));
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    let first = rows
        .iter()
        .min_by_key(|row| row.try_get::<i64, _>("seqno").unwrap_or(i64::MAX))
        .and_then(|row| row.try_get::<String, _>("name").ok());
    if first.as_deref() != Some(column) {
        return Err(DbError::Init(format!(
            "logical reference index {index_name} must start with {table}.{column}"
        )));
    }
    Ok(())
}

async fn require_index_on_table(
    pool: &SqlitePool,
    index_name: &str,
    table: &str,
) -> Result<(), DbError> {
    let actual_table: Option<String> = sqlx::query_scalar(
        "SELECT tbl_name FROM sqlite_schema WHERE type = 'index' AND name = ?",
    )
    .bind(index_name)
    .fetch_optional(pool)
    .await?;
    if actual_table.as_deref() != Some(table) {
        return Err(DbError::Init(format!(
            "JSON logical reference on {table} requires index {index_name}"
        )));
    }
    Ok(())
}

async fn require_partial_unique_index(
    pool: &SqlitePool,
    index_name: &str,
    table: &str,
    expected_columns: &[&str],
    predicate: &str,
) -> Result<(), DbError> {
    let sql = format!("PRAGMA index_list({})", quote_sqlite_identifier(table));
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    let Some(row) = rows
        .iter()
        .find(|row| row.try_get::<String, _>("name").ok().as_deref() == Some(index_name))
    else {
        return Err(DbError::Init(format!(
            "v3 schema {table}{expected_columns:?} requires partial UNIQUE index {index_name}"
        )));
    };
    let unique = row.try_get::<i64, _>("unique").map_err(DbError::Query)? != 0;
    let partial = row.try_get::<i64, _>("partial").map_err(DbError::Query)? != 0;
    if !unique || !partial {
        return Err(DbError::Init(format!(
            "v3 schema index {index_name} must be UNIQUE and partial"
        )));
    }

    let info_sql = format!("PRAGMA index_info({})", quote_sqlite_identifier(index_name));
    let mut columns = sqlx::query(&info_sql).fetch_all(pool).await?;
    columns.sort_by_key(|row| row.try_get::<i64, _>("seqno").unwrap_or(i64::MAX));
    let columns = columns
        .into_iter()
        .filter_map(|row| row.try_get::<String, _>("name").ok())
        .collect::<Vec<_>>();
    if columns
        != expected_columns
            .iter()
            .map(|column| (*column).to_owned())
            .collect::<Vec<_>>()
    {
        return Err(DbError::Init(format!(
            "v3 schema index {index_name} must uniquely index {table}{expected_columns:?} in order"
        )));
    }

    let create_sql: String =
        sqlx::query_scalar("SELECT sql FROM sqlite_schema WHERE type = 'index' AND name = ?")
            .bind(index_name)
            .fetch_one(pool)
            .await?;
    let normalized = normalize_sql(&create_sql);
    let actual_predicate = normalized
        .split_once(" WHERE ")
        .map(|(_, predicate)| predicate);
    let expected_predicate = normalize_sql(predicate);
    if actual_predicate != Some(expected_predicate.as_str()) {
        return Err(DbError::Init(format!(
            "v3 schema index {index_name} must use predicate {predicate}"
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct ColumnInfo {
    name: String,
    data_type: String,
    not_null: bool,
    primary_key_position: i64,
}

async fn table_info(pool: &SqlitePool, table: &str) -> Result<Vec<ColumnInfo>, DbError> {
    let sql = format!("PRAGMA table_info({})", quote_sqlite_identifier(table));
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            Ok(ColumnInfo {
                name: row.try_get("name").map_err(DbError::Query)?,
                data_type: row
                    .try_get::<String, _>("type")
                    .map_err(DbError::Query)?
                    .to_ascii_uppercase(),
                not_null: row.try_get::<i64, _>("notnull").map_err(DbError::Query)? != 0,
                primary_key_position: row.try_get("pk").map_err(DbError::Query)?,
            })
        })
        .collect::<Result<Vec<_>, DbError>>()?)
}

#[derive(Debug)]
struct IndexInfo {
    unique: bool,
    columns: Vec<String>,
}

async fn index_columns(pool: &SqlitePool, table: &str) -> Result<BTreeMap<String, IndexInfo>, DbError> {
    let sql = format!("PRAGMA index_list({})", quote_sqlite_identifier(table));
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    let mut indexes = BTreeMap::new();
    for row in rows {
        let name: String = row.try_get("name").map_err(DbError::Query)?;
        let unique = row.try_get::<i64, _>("unique").map_err(DbError::Query)? != 0;
        let info_sql = format!("PRAGMA index_info({})", quote_sqlite_identifier(&name));
        let mut columns = sqlx::query(&info_sql).fetch_all(pool).await?;
        columns.sort_by_key(|column| column.try_get::<i64, _>("seqno").unwrap_or(i64::MAX));
        let columns = columns
            .into_iter()
            .filter_map(|column| column.try_get::<String, _>("name").ok())
            .collect();
        indexes.insert(name, IndexInfo { unique, columns });
    }
    Ok(indexes)
}

fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

fn delete_policy_name(policy: DeletePolicy) -> &'static str {
    match policy {
        DeletePolicy::Restrict => "RESTRICT",
        DeletePolicy::Cascade => "CASCADE",
        DeletePolicy::SetNull => "SET_NULL",
        DeletePolicy::KeepHistory => "KEEP_HISTORY",
    }
}

fn rebuild_policy_name(policy: RebuildPolicy) -> &'static str {
    match policy {
        RebuildPolicy::PreserveBusinessId => "PRESERVE_BUSINESS_ID",
        RebuildPolicy::PreserveProtocolToken => "PRESERVE_PROTOCOL_TOKEN",
        RebuildPolicy::ExternalOwner => "EXTERNAL_OWNER",
    }
}

fn quote_sqlite_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    #[tokio::test]
    async fn clean_v3_baseline_satisfies_schema_contract() {
        let database = init_database_memory().await.expect("database");
        validate_id_schema_contract(database.pool()).await.expect("schema contract");
        assert!(
            audit_logical_reference_orphans(database.pool())
                .await
                .expect("orphan audit")
                .is_empty()
        );
    }

    #[test]
    fn external_agent_actor_is_registered_as_external_keep_history() {
        let reference = LOGICAL_REFERENCES
            .iter()
            .find(|reference| {
                reference.index_name == "idx_execution_events_actor_external_agent_id"
            })
            .expect("external-agent actor registry entry");
        assert_eq!(reference.child_table, "agent_execution_events");
        assert_eq!(reference.child_column, "actor_id");
        assert_eq!(reference.parent_table, None);
        assert_eq!(reference.parent_column, None);
        assert_eq!(reference.delete_policy, DeletePolicy::KeepHistory);
        assert_eq!(reference.rebuild_policy, RebuildPolicy::ExternalOwner);
        assert_eq!(
            reference.orphan_audit_policy,
            OrphanAuditPolicy::ExternalOwner
        );
        assert_eq!(
            reference.child_predicate,
            Some(
                "child.actor_type = 'agent' \
                 AND child.actor_conversation_id IS NULL \
                 AND child.actor_id IS NOT NULL"
            )
        );
    }

    #[test]
    fn message_correlation_turn_is_a_wire_protocol_owner_token() {
        let reference = LOGICAL_REFERENCES
            .iter()
            .find(|reference| reference.index_name == "idx_message_correlations_turn_message_id")
            .expect("message-correlation turn registry entry");
        assert_eq!(reference.child_table, "message_correlations");
        assert_eq!(reference.child_column, "turn_message_id");
        assert_eq!(reference.parent_table, None);
        assert_eq!(reference.parent_column, None);
        assert_eq!(
            reference.value_contract,
            LogicalReferenceValueContract::CanonicalUuidV7
        );
        assert_eq!(
            reference.rebuild_policy,
            RebuildPolicy::PreserveProtocolToken
        );
        assert_eq!(
            reference.orphan_audit_policy,
            OrphanAuditPolicy::ValidateValueOnly
        );
    }

    #[tokio::test]
    async fn message_correlation_audit_allows_unprojected_owner_but_rejects_cross_conversation_projection(
    ) {
        let database = init_database_memory().await.expect("database");
        let pool = database.pool();
        let owner: String = sqlx::query_scalar("SELECT user_id FROM users ORDER BY id LIMIT 1")
            .fetch_one(pool)
            .await
            .expect("owner");
        let conversation_a = nomifun_common::ConversationId::new();
        let conversation_b = nomifun_common::ConversationId::new();
        for (conversation_id, name) in [
            (&conversation_a, "correlation-a"),
            (&conversation_b, "correlation-b"),
        ] {
            sqlx::query(
                "INSERT INTO conversations \
                 (conversation_id, user_id, name, type, created_at, updated_at) \
                 VALUES (?, ?, ?, 'nomi', 1, 1)",
            )
            .bind(conversation_id.as_str())
            .bind(&owner)
            .bind(name)
            .execute(pool)
            .await
            .expect("conversation");
        }

        let projected_message_id = nomifun_common::MessageId::new();
        sqlx::query(
            "INSERT INTO messages \
             (message_id, conversation_id, type, content, created_at) \
             VALUES (?, ?, 'tool_call', '{}', 1)",
        )
        .bind(projected_message_id.as_str())
        .bind(conversation_b.as_str())
        .execute(pool)
        .await
        .expect("projected message");
        sqlx::query(
            "INSERT INTO message_correlations \
             (conversation_id, turn_message_id, message_type, correlation_key, message_id) \
             VALUES (?, ?, 'tool_call', 'cross-conversation-projection', ?)",
        )
        .bind(conversation_a.as_str())
        .bind(nomifun_common::MessageId::new().as_str())
        .bind(projected_message_id.as_str())
        .execute(pool)
        .await
        .expect("correlation fixture");

        let findings = audit_logical_reference_orphans(pool)
            .await
            .expect("correlation audit");
        assert!(findings.iter().any(|finding| {
            finding.child_table == "message_correlations"
                && finding.child_column == "message_id"
                && finding.count == 1
        }));
        assert!(findings.iter().all(|finding| {
            !(finding.child_table == "message_correlations"
                && finding.child_column == "turn_message_id")
        }));
    }

    #[tokio::test]
    async fn orphan_audit_allows_missing_history_but_rejects_wrong_aggregate_scope() {
        let database = init_database_memory().await.expect("database");
        let pool = database.pool();
        let execution_a = nomifun_common::AgentExecutionId::new();
        let execution_b = nomifun_common::AgentExecutionId::new();
        let owner: String = sqlx::query_scalar("SELECT user_id FROM users ORDER BY id LIMIT 1")
            .fetch_one(pool)
            .await
            .expect("owner");

        for execution_id in [&execution_a, &execution_b] {
            sqlx::query(
                "INSERT INTO agent_executions \
                 (execution_id, user_id, goal, status, plan_gate, adaptation_policy, \
                  decision_policy, delegation_policy, max_parallel, initial_plan_input, \
                  created_at, updated_at) \
                 VALUES (?, ?, 'scope audit', 'planning', 'automatic', 'fixed', \
                         'automatic', 'automatic', 1, '{}', 1, 1)",
            )
            .bind(execution_id.as_str())
            .bind(&owner)
            .execute(pool)
            .await
            .expect("execution");
        }

        let step_id = nomifun_common::generate_id();
        sqlx::query(
            "INSERT INTO agent_execution_steps \
             (step_id, execution_id, title, spec, kind, status, introduced_in_revision, created_at, updated_at) \
             VALUES (?, ?, 'step', '{}', 'agent', 'pending', 0, 1, 1)",
        )
        .bind(&step_id)
        .bind(execution_a.as_str())
        .execute(pool)
        .await
        .expect("step");
        sqlx::query(
            "INSERT INTO agent_execution_events \
             (execution_id, sequence, event_type, step_id, actor_type, \
              on_behalf_of_user_id, payload, created_at) \
             VALUES (?, 1, 'step_changed', ?, 'system', ?, '{}', 1)",
        )
        .bind(execution_b.as_str())
        .bind(&step_id)
        .bind(&owner)
        .execute(pool)
        .await
        .expect("cross-aggregate event");
        sqlx::query(
            "INSERT INTO agent_execution_events \
             (execution_id, sequence, event_type, actor_conversation_id, actor_type, \
              on_behalf_of_user_id, payload, created_at) \
             VALUES (?, 2, 'step_changed', ?, 'system', ?, '{}', 1)",
        )
        .bind(execution_b.as_str())
        .bind(nomifun_common::ConversationId::new().as_str())
        .bind(&owner)
        .execute(pool)
        .await
        .expect("historical actor reference");

        let findings = audit_logical_reference_orphans(pool).await.expect("orphan audit");
        assert!(
            findings.iter().any(|finding| {
                finding.child_table == "agent_execution_events"
                    && finding.child_column == "step_id"
                    && finding.count == 1
            }),
            "same-id references must remain in the owning execution aggregate"
        );
        assert!(
            findings.iter().all(|finding| {
                !(finding.child_table == "agent_execution_events"
                    && finding.child_column == "actor_conversation_id")
            }),
            "KEEP_HISTORY must not treat an intentionally absent parent as an orphan"
        );
    }

    #[tokio::test]
    async fn orphan_audit_treats_soft_deleted_parents_as_inactive() {
        let database = init_database_memory().await.expect("database");
        let pool = database.pool();
        let conversation_id = nomifun_common::ConversationId::new();
        let owner: String = sqlx::query_scalar("SELECT user_id FROM users ORDER BY id LIMIT 1")
            .fetch_one(pool)
            .await
            .expect("owner");
        sqlx::query(
            "INSERT INTO conversations \
             (conversation_id, user_id, name, type, created_at, updated_at) \
             VALUES (?, ?, 'soft-delete audit', 'nomi', 1, 1)",
        )
        .bind(conversation_id.as_str())
        .bind(owner)
        .execute(pool)
        .await
        .expect("conversation");
        let mcp_server_id = nomifun_common::generate_id();
        sqlx::query(
            "INSERT INTO mcp_servers \
             (mcp_server_id, name, enabled, transport_type, transport_config, last_test_status, \
              builtin, created_at, updated_at) \
             VALUES (?, 'audit-mcp', 0, 'stdio', '{}', 'disconnected', 0, 1, 1)",
        )
        .bind(&mcp_server_id)
        .execute(pool)
        .await
        .expect("MCP server");
        sqlx::query(
            "INSERT INTO conversation_mcp_servers \
             (conversation_id, mcp_server_id, sort_order) VALUES (?, ?, 0)",
        )
        .bind(conversation_id.as_str())
        .bind(&mcp_server_id)
        .execute(pool)
        .await
        .expect("MCP binding");
        sqlx::query("UPDATE mcp_servers SET deleted_at = 2 WHERE mcp_server_id = ?")
            .bind(&mcp_server_id)
            .execute(pool)
            .await
            .expect("soft delete");

        let findings = audit_logical_reference_orphans(pool).await.expect("orphan audit");
        assert!(findings.iter().any(|finding| {
            finding.child_table == "conversation_mcp_servers"
                && finding.child_column == "mcp_server_id"
                && finding.count == 1
        }));
    }

    #[tokio::test]
    async fn data_contract_rejects_succeeded_creation_without_committed_assets() {
        let database = init_database_memory().await.expect("database");
        let pool = database.pool();
        let provider_id = nomifun_common::ProviderId::new();
        sqlx::query(
            "INSERT INTO providers \
             (provider_id, platform, name, base_url, api_key_encrypted, created_at, updated_at) \
             VALUES (?, 'contract', 'Creation audit provider', 'https://example.invalid', '', 1, 1)",
        )
        .bind(provider_id.as_str())
        .execute(pool)
        .await
        .expect("provider");
        let creation_task_id = nomifun_common::CreationTaskId::new();
        sqlx::query(
            "INSERT INTO creation_tasks \
             (creation_task_id, provider_id, model, capability, params, status, \
              result_asset_ids, submitted_at, finished_at) \
             VALUES (?, ?, 'model', 't2i', '{}', 'succeeded', '[]', 1, 2)",
        )
        .bind(creation_task_id.as_str())
        .bind(provider_id.as_str())
        .execute(pool)
        .await
        .expect("invalid current-lineage fixture");

        let error = validate_id_data_contract(pool).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("succeeded but has no result assets")
        );
        let unchanged: String = sqlx::query_scalar(
            "SELECT status FROM creation_tasks WHERE creation_task_id = ?",
        )
        .bind(creation_task_id.as_str())
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(unchanged, "succeeded");
    }

    #[tokio::test]
    async fn json_business_reference_audit_rejects_integer_credential_ids() {
        let database = init_database_memory().await.expect("database");
        let pool = database.pool();
        let provider_id = nomifun_common::generate_id();
        sqlx::query(
            "INSERT INTO providers \
             (provider_id, platform, name, base_url, api_key_encrypted, created_at, updated_at) \
             VALUES (?, 'contract', 'JSON audit provider', 'https://example.invalid', '', 1, 1)",
        )
        .bind(&provider_id)
        .execute(pool)
        .await
        .expect("provider");
        sqlx::query(
            "INSERT INTO creation_tasks \
             (creation_task_id, provider_id, model, capability, params, status, submitted_at) \
             VALUES (?, ?, 'model', 'text', '{}', 'queued', 1)",
        )
        .bind(nomifun_common::generate_id())
        .bind(&provider_id)
        .execute(pool)
        .await
        .expect("creation task");
        sqlx::query(
            "INSERT INTO connector_credentials \
             (credential_id, kind, name, payload_encrypted, created_at, updated_at) \
             VALUES (?, 'contract', 'JSON audit credential', 'ciphertext', 1, 1)",
        )
        .bind(nomifun_common::generate_id())
        .execute(pool)
        .await
        .expect("credential");
        assert!(
            sqlx::query(
                "INSERT INTO workshop_assets \
                 (asset_id, kind, title, origin, created_at, updated_at) \
                 VALUES (?, 'image', 'legacy integer task reference', \
                         '{\"task_id\":1}', 1, 1)",
            )
            .bind(nomifun_common::generate_id())
            .execute(pool)
            .await
            .is_err(),
            "legacy origin.task_id must be rejected by the v3 schema"
        );
        sqlx::query(
            "INSERT INTO knowledge_bases \
             (knowledge_base_id, name, root_path, extra, created_at, updated_at) \
             VALUES (?, 'invalid integer credential reference', '/tmp/invalid-credential-ref', \
                     '{\"source\":{\"credentialRef\":1}}', 1, 1)",
        )
        .bind(nomifun_common::generate_id())
        .execute(pool)
        .await
        .expect("invalid JSON fixture");

        let findings = audit_logical_reference_orphans(pool)
            .await
            .expect("JSON reference audit");
        assert!(findings.iter().any(|finding| {
            finding.child_table == "knowledge_bases"
                && finding.child_column == "extra:$.source.credentialRef"
                && finding.count == 1
        }));
    }
}
