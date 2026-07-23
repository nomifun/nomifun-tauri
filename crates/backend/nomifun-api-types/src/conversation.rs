use nomifun_common::{
    AgentType, ConversationSource, ConversationStatus, DecisionPolicy, DelegationPolicy,
    MessagePosition, MessageStatus, MessageType, PaginatedResult, ProviderWithModel, TimestampMs,
};
use serde::{Deserialize, Serialize};

use crate::McpServerId;
use crate::webhook::double_option;

/// Per-MCP snapshot status stored in `conversation.extra`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationMcpStatusKind {
    Loaded,
    Failed,
    Unsupported,
}

/// A single MCP item shown in the conversation-scoped MCP list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationMcpStatus {
    pub mcp_server_id: McpServerId,
    pub name: String,
    pub status: ConversationMcpStatusKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ── Request types ──────────────────────────────────────────────────

/// Body for `POST /api/conversations`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateConversationRequest {
    pub r#type: AgentType,
    pub name: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_provider_with_model"
    )]
    pub model: Option<ProviderWithModel>,
    pub source: Option<ConversationSource>,
    pub channel_chat_id: Option<String>,
    /// Reusable launch configuration. The server resolves this reference and
    /// persists immutable lineage/snapshot columns during creation.
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_preset_id"
    )]
    pub preset_id: Option<String>,
    #[serde(default)]
    pub preset_overrides: Option<crate::PresetOverrides>,
    #[serde(default)]
    pub delegation_policy: DelegationPolicy,
    #[serde(default)]
    pub execution_model_pool: Option<crate::ExecutionModelPool>,
    #[serde(default)]
    pub decision_policy: DecisionPolicy,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_execution_template_id"
    )]
    pub execution_template_id: Option<String>,
    pub extra: serde_json::Value,
}

/// Body for `PATCH /api/conversations/:id`.
///
/// All fields optional — only supplied fields are applied.
/// `extra` uses merge semantics (patch, not replace).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateConversationRequest {
    pub name: Option<String>,
    pub pinned: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_provider_with_model"
    )]
    pub model: Option<ProviderWithModel>,
    pub delegation_policy: Option<DelegationPolicy>,
    #[serde(default, deserialize_with = "double_option")]
    pub execution_model_pool: Option<Option<crate::ExecutionModelPool>>,
    pub decision_policy: Option<DecisionPolicy>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_execution_template_patch"
    )]
    pub execution_template_id: Option<Option<String>>,
    pub extra: Option<serde_json::Value>,
}

/// Body for `POST /api/conversations/clone`.
///
/// Despite the name, this endpoint no longer supports cloning from an
/// existing conversation — it's kept as a distinct route because multiple
/// call sites pass a pre-built `CreateConversationRequest` payload shape.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloneConversationRequest {
    pub conversation: CreateConversationRequest,
}

/// Body for `POST /api/conversations/:id/messages`.
///
/// `msg_id` is server-generated — clients must not provide one.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessageRequest {
    pub content: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub inject_skills: Vec<String>,
    #[serde(default)]
    pub hidden: bool,
    /// Originator of this message when it was NOT typed by the human owner:
    /// `"companion"` (gateway injection), `"cron"`, `"autowork"`, `"idmm"`.
    /// `None` means a real user sent it. Echoed into the
    /// `message.userCreated` broadcast so downstream consumers (e.g. the companion
    /// memory collector) can tell agent-initiated messages from owner speech.
    #[serde(default)]
    pub origin: Option<String>,
    /// Per-turn IM channel platform marker (`"telegram"`/`"lark"`/…) for a turn
    /// that originated from a channel. `None` for owner/desktop turns. Takes
    /// precedence over the conversation's static `extra.channel_platform`, so a
    /// companion's single session (now shared by the desktop bubble, the chat
    /// tab, AND every IM channel) can still tag each individual IM turn for the
    /// floating window's remote-turn rendering without baking the platform into
    /// the conversation row.
    #[serde(default)]
    pub channel_platform: Option<String>,
}

/// Response for `POST /api/conversations/:id/messages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageResponse {
    #[serde(deserialize_with = "crate::serde_util::deserialize_message_id")]
    pub msg_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRuntimeStateKind {
    Idle,
    Starting,
    Running,
    WaitingConfirmation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationRuntimeSummary {
    pub state: ConversationRuntimeStateKind,
    pub can_send_message: bool,
    pub has_runtime: bool,
    pub runtime_status: Option<ConversationStatus>,
    pub is_processing: bool,
    pub pending_confirmations: usize,
    /// Wall-clock start (epoch ms) of the currently-running turn, when
    /// `is_processing` is true. Sourced from the turn-claim time so the
    /// frontend's elapsed-time indicator survives view unmount/remount
    /// (tab switch, session switch) and reconnection — analogous to how the
    /// thinking bubble anchors to the persisted `message.created_at`.
    /// `None` when idle. Additive/backward-compatible.
    #[serde(default)]
    pub processing_started_at: Option<i64>,
}

// ── Query types ────────────────────────────────────────────────────

/// Query parameters for `GET /api/conversations`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListConversationsQuery {
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_conversation_id"
    )]
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub source: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_cron_job_id"
    )]
    pub cron_job_id: Option<String>,
    pub pinned: Option<bool>,
}

/// Query parameters for `GET /api/conversations/:id/messages`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListMessagesQuery {
    pub page: Option<u32>,
    pub page_size: Option<u32>,
    pub order: Option<String>,
    pub content_mode: Option<String>,
    /// Opaque keyset cursor `"<created_at_ms>:<id>"`. When present, returns the
    /// window of messages strictly OLDER than the cursor, newest-first, and
    /// `page`/`page_size` offset pagination is ignored. Used by the chat UI to
    /// incrementally load an ever-growing companion session without fetching the
    /// whole transcript. The first ("load latest") page omits it.
    #[serde(default, deserialize_with = "deserialize_optional_message_cursor")]
    pub cursor: Option<String>,
}

/// Body for
/// `PATCH /api/conversations/:conversation_id/artifacts/:conversation_artifact_id`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateConversationArtifactRequest {
    pub status: ConversationArtifactStatus,
}

/// Query parameters for `GET /api/messages/search`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchMessagesQuery {
    pub keyword: String,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
}

// ── Response types ─────────────────────────────────────────────────

/// Full conversation object returned in API responses.
///
/// `model` is the canonical top-level field **only for `AgentType::Nomi`**.
/// For every other agent type, `model` is always `None` here and the client
/// should read agent-specific model/mode fields out of `extra` (e.g. ACP uses
/// `extra.current_model_id` / `extra.current_mode_id`). See
/// `docs/superpowers/specs/2026-05-12-conversation-type-aware-model-design.md`.
///
/// `Option<T>` fields use `skip_serializing_if = "Option::is_none"` so the
/// serialized JSON omits the key entirely when the value is absent. This
/// keeps the wire shape tight and matches what the frontend mapper already
/// tolerates (`'model' in r` guard handles missing keys).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationResponse {
    #[serde(deserialize_with = "crate::serde_util::deserialize_conversation_id")]
    pub conversation_id: String,
    pub name: String,
    pub r#type: AgentType,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_provider_with_model"
    )]
    pub model: Option<ProviderWithModel>,
    pub status: ConversationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ConversationRuntimeSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ConversationSource>,
    pub pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_at: Option<TimestampMs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_chat_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_preset_id"
    )]
    pub preset_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset_snapshot: Option<crate::ResolvedPresetSnapshot>,
    pub delegation_policy: DelegationPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_model_pool: Option<crate::ExecutionModelPool>,
    pub decision_policy: DecisionPolicy,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_execution_template_id"
    )]
    pub execution_template_id: Option<String>,
    /// Current Agent collaboration projected from `conversation_execution_links`.
    /// These fields are read-only and are never stored on the conversation.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_execution_id"
    )]
    pub linked_execution_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_execution_step_id"
    )]
    pub execution_step_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_execution_attempt_id"
    )]
    pub execution_attempt_id: Option<String>,
    pub created_at: TimestampMs,
    pub modified_at: TimestampMs,
    pub extra: serde_json::Value,
}

/// Paginated list of conversations.
pub type ConversationListResponse = PaginatedResult<ConversationResponse>;

/// Single message object returned in API responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageResponse {
    #[serde(deserialize_with = "crate::serde_util::deserialize_message_id")]
    pub message_id: String,
    #[serde(deserialize_with = "crate::serde_util::deserialize_conversation_id")]
    pub conversation_id: String,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_message_id"
    )]
    pub msg_id: Option<String>,
    pub r#type: MessageType,
    pub content: serde_json::Value,
    pub position: Option<MessagePosition>,
    pub status: Option<MessageStatus>,
    pub hidden: bool,
    pub created_at: TimestampMs,
}

/// Paginated list of messages.
pub type MessageListResponse = PaginatedResult<MessageResponse>;

/// Response for `GET /api/conversations/active-count`.
#[derive(Debug, Serialize)]
pub struct ActiveCountResponse {
    pub count: usize,
}

/// Artifact kind discriminant for conversation-bound UI artifacts.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationArtifactKind {
    CronTrigger,
    SkillSuggest,
}

/// Durable artifact state exposed to the client.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationArtifactStatus {
    Active,
    Pending,
    Dismissed,
    Saved,
}

/// Artifact object returned by conversation artifact APIs and websocket events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConversationArtifactResponse {
    /// Stable Conversation Artifact business identity.
    #[serde(deserialize_with = "crate::serde_util::deserialize_uuidv7")]
    pub conversation_artifact_id: String,
    #[serde(deserialize_with = "crate::serde_util::deserialize_conversation_id")]
    pub conversation_id: String,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_cron_job_id"
    )]
    pub cron_job_id: Option<String>,
    pub kind: ConversationArtifactKind,
    pub status: ConversationArtifactStatus,
    pub payload: serde_json::Value,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// List of conversation artifacts for a single conversation.
pub type ConversationArtifactListResponse = Vec<ConversationArtifactResponse>;

/// A single item from cross-conversation message search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSearchItem {
    #[serde(deserialize_with = "crate::serde_util::deserialize_message_id")]
    pub message_id: String,
    pub message_type: String,
    pub message_created_at: TimestampMs,
    pub preview_text: String,
    pub conversation: ConversationResponse,
}

/// Paginated search results for messages.
pub type MessageSearchResponse = PaginatedResult<MessageSearchItem>;

fn deserialize_optional_execution_template_patch<'de, D>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<Option<String>> = double_option(deserializer)?;
    value
        .map(|value| {
            value
                .map(|value| {
                    nomifun_common::AgentExecutionTemplateId::parse(value.clone())
                        .map(|_| value)
                        .map_err(serde::de::Error::custom)
                })
                .transpose()
        })
        .transpose()
}

fn deserialize_optional_message_cursor<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|cursor| {
            if cursor.is_empty() {
                return Ok(cursor);
            }
            let (created_at, message_id) = cursor.split_once(':').ok_or_else(|| {
                serde::de::Error::custom("message cursor must use '<created_at>:<message_id>'")
            })?;
            let _: i64 = created_at.parse().map_err(|_| {
                serde::de::Error::custom("message cursor created_at must be an integer")
            })?;
            nomifun_common::MessageId::try_from(message_id).map_err(serde::de::Error::custom)?;
            Ok(cursor)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PROVIDER_ID_1: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const PROVIDER_ID_2: &str = "0190f5fe-7c00-7a00-8000-000000000002";
    const CRON_JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678941";
    const EXECUTION_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const MESSAGE_ID_1: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const MESSAGE_ID_2: &str = "0190f5fe-7c00-7a00-8000-000000000002";
    const TEMPLATE_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    // ── CreateConversationRequest ───────────────────────────────────

    #[test]
    fn deserialize_create_request_full() {
        let raw = json!({
            "type": "acp",
            "name": "Code Review",
            "model": { "provider_id": PROVIDER_ID_1, "model": "claude-sonnet-4-20250514" },
            "source": "nomifun",
            "channel_chat_id": "user:123",
            "extra": { "workspace": "/project" }
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.r#type, AgentType::Acp);
        assert_eq!(req.name.as_deref(), Some("Code Review"));
        assert_eq!(req.model.unwrap().model, "claude-sonnet-4-20250514");
        assert_eq!(req.source, Some(ConversationSource::Nomifun));
        assert_eq!(req.channel_chat_id.as_deref(), Some("user:123"));
        assert_eq!(req.extra["workspace"], "/project");
    }

    #[test]
    fn deserialize_create_request_minimal() {
        let raw = json!({
            "type": "acp",
            "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
            "extra": {}
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.r#type, AgentType::Acp);
        assert!(req.name.is_none());
        assert!(req.source.is_none());
        assert!(req.channel_chat_id.is_none());
    }

    #[test]
    fn conversation_request_references_require_canonical_entity_ids() {
        let valid = json!({
            "type": "nomi",
            "execution_template_id": TEMPLATE_ID,
            "extra": {}
        });
        assert!(serde_json::from_value::<CreateConversationRequest>(valid).is_ok());

        for invalid in [json!("template-1"), json!(1)] {
            let value = json!({
                "type": "nomi",
                "execution_template_id": invalid,
                "extra": {}
            });
            assert!(serde_json::from_value::<CreateConversationRequest>(value).is_err());
        }

        assert!(
            serde_json::from_value::<UpdateConversationRequest>(json!({
                "execution_template_id": null
            }))
            .is_ok()
        );
        assert!(
            serde_json::from_value::<UpdateConversationRequest>(json!({
                "execution_template_id": "1"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ListConversationsQuery>(json!({
                "cron_job_id": "cron-1"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ListConversationsQuery>(json!({
                "cursor": "42"
            }))
            .is_err()
        );
    }

    #[test]
    fn conversation_requests_reject_unknown_legacy_fields() {
        assert!(
            serde_json::from_value::<CreateConversationRequest>(json!({
                "type": "nomi",
                "executionTemplateId": TEMPLATE_ID,
                "extra": {}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<SendMessageRequest>(json!({
                "content": "hello",
                "msgId": MESSAGE_ID_1
            }))
            .is_err()
        );
    }

    #[test]
    fn deserialize_create_request_without_model() {
        let raw = json!({
            "type": "acp",
            "extra": {}
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.r#type, AgentType::Acp);
        assert!(req.model.is_none());
    }

    #[test]
    fn conversation_model_pool_uses_the_agent_execution_tagged_contract() {
        let raw = json!({
            "type": "nomi",
            "execution_model_pool": {
                "mode": "range",
                "models": [{ "provider_id": PROVIDER_ID_1, "model": "model-1" }]
            },
            "extra": {}
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(
            req.execution_model_pool,
            Some(crate::ExecutionModelPool::Range {
                models: vec![crate::ExecutionModelRef {
                    provider_id: PROVIDER_ID_1.into(),
                    model: "model-1".into(),
                }],
            })
        );
    }

    #[test]
    fn conversation_model_pool_rejects_the_removed_array_alias() {
        let raw = json!({
            "type": "nomi",
            "execution_model_pool": [
                { "provider_id": PROVIDER_ID_1, "model": "model-1" }
            ],
            "extra": {}
        });
        assert!(serde_json::from_value::<CreateConversationRequest>(raw).is_err());
    }

    #[test]
    fn conversation_model_references_reject_noncanonical_provider_id() {
        for provider_id in [
            json!(42),
            json!("openai"),
            json!("550e8400-e29b-41d4-a716-446655440000"),
            json!("0190F5FE-7C00-7A00-8000-000000000001"),
            json!("provider_0190f5fe-7c00-7a00-8000-000000000001"),
        ] {
            assert!(
                serde_json::from_value::<CreateConversationRequest>(json!({
                    "type": "nomi",
                    "model": { "provider_id": provider_id.clone(), "model": "model-1" },
                    "extra": {}
                }))
                .is_err()
            );
            assert!(
                serde_json::from_value::<UpdateConversationRequest>(json!({
                    "model": { "provider_id": provider_id.clone(), "model": "model-1" }
                }))
                .is_err()
            );
            assert!(
                serde_json::from_value::<CreateConversationRequest>(json!({
                    "type": "nomi",
                    "execution_model_pool": {
                        "mode": "single",
                        "model": { "provider_id": provider_id, "model": "model-1" }
                    },
                    "extra": {}
                }))
                .is_err()
            );
        }
    }

    #[test]
    fn deserialize_create_request_missing_type() {
        let raw = json!({
            "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
            "extra": {}
        });
        assert!(serde_json::from_value::<CreateConversationRequest>(raw).is_err());
    }

    #[test]
    fn deserialize_create_request_missing_extra() {
        let raw = json!({
            "type": "acp",
            "model": { "provider_id": PROVIDER_ID_1, "model": "m1" }
        });
        assert!(serde_json::from_value::<CreateConversationRequest>(raw).is_err());
    }

    #[test]
    fn deserialize_create_request_invalid_type() {
        let raw = json!({
            "type": "invalid_type",
            "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
            "extra": {}
        });
        assert!(serde_json::from_value::<CreateConversationRequest>(raw).is_err());
    }

    // ── UpdateConversationRequest ───────────────────────────────────

    #[test]
    fn deserialize_update_request_partial() {
        let raw = json!({ "name": "New Name" });
        let req: UpdateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name.as_deref(), Some("New Name"));
        assert!(req.pinned.is_none());
        assert!(req.model.is_none());
        assert!(req.extra.is_none());
    }

    #[test]
    fn deserialize_update_request_all_fields() {
        let raw = json!({
            "name": "Updated",
            "pinned": true,
            "model": { "provider_id": PROVIDER_ID_2, "model": "new-model" },
            "extra": { "workspace": "/new" }
        });
        let req: UpdateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name.as_deref(), Some("Updated"));
        assert_eq!(req.pinned, Some(true));
        assert!(req.model.is_some());
        assert_eq!(req.extra.as_ref().unwrap()["workspace"], "/new");
    }

    #[test]
    fn deserialize_update_request_empty() {
        let raw = json!({});
        let req: UpdateConversationRequest = serde_json::from_value(raw).unwrap();
        assert!(req.name.is_none());
        assert!(req.pinned.is_none());
        assert!(req.model.is_none());
        assert!(req.extra.is_none());
    }

    #[test]
    fn deserialize_update_model_pool_distinguishes_omitted_from_clear() {
        let clear: UpdateConversationRequest =
            serde_json::from_value(json!({ "execution_model_pool": null })).unwrap();
        assert_eq!(clear.execution_model_pool, Some(None));

        let automatic: UpdateConversationRequest = serde_json::from_value(json!({
            "execution_model_pool": { "mode": "automatic" }
        }))
        .unwrap();
        assert_eq!(
            automatic.execution_model_pool,
            Some(Some(crate::ExecutionModelPool::Automatic))
        );
    }

    #[test]
    fn deserialize_update_artifact_request() {
        let raw = json!({ "status": "dismissed" });
        let req: UpdateConversationArtifactRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.status, ConversationArtifactStatus::Dismissed);
    }

    // ── CloneConversationRequest ────────────────────────────────────

    #[test]
    fn deserialize_clone_request() {
        let raw = json!({
            "conversation": {
                "type": "acp",
                "model": { "provider_id": PROVIDER_ID_1, "model": "m1" },
                "extra": {}
            }
        });
        let req: CloneConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.conversation.r#type, AgentType::Acp);
    }

    // ── ListConversationsQuery ──────────────────────────────────────

    #[test]
    fn deserialize_list_query_full() {
        let raw = json!({
            "cursor": "0190f5fe-7c00-7a00-8000-000000000099",
            "limit": 10,
            "source": "telegram",
            "cron_job_id": CRON_JOB_ID,
            "pinned": true
        });
        let q: ListConversationsQuery = serde_json::from_value(raw).unwrap();
        assert_eq!(
            q.cursor.as_deref(),
            Some("0190f5fe-7c00-7a00-8000-000000000099")
        );
        assert_eq!(q.limit, Some(10));
        assert_eq!(q.source.as_deref(), Some("telegram"));
        assert_eq!(q.cron_job_id.as_deref(), Some(CRON_JOB_ID));
        assert_eq!(q.pinned, Some(true));
    }

    #[test]
    fn deserialize_list_query_empty() {
        let raw = json!({});
        let q: ListConversationsQuery = serde_json::from_value(raw).unwrap();
        assert!(q.cursor.is_none());
        assert!(q.limit.is_none());
        assert!(q.source.is_none());
        assert!(q.cron_job_id.is_none());
        assert!(q.pinned.is_none());
    }

    // ── ListMessagesQuery ───────────────────────────────────────────

    #[test]
    fn deserialize_messages_query_defaults() {
        let raw = json!({});
        let q: ListMessagesQuery = serde_json::from_value(raw).unwrap();
        assert!(q.page.is_none());
        assert!(q.page_size.is_none());
        assert!(q.order.is_none());
        assert!(q.content_mode.is_none());
    }

    #[test]
    fn deserialize_messages_query_with_values() {
        let expected_cursor = format!("1000:{MESSAGE_ID_1}");
        let raw = json!({
            "page": 2,
            "page_size": 30,
            "order": "ASC",
            "content_mode": "compact",
            "cursor": expected_cursor
        });
        let q: ListMessagesQuery = serde_json::from_value(raw).unwrap();
        assert_eq!(q.page, Some(2));
        assert_eq!(q.page_size, Some(30));
        assert_eq!(q.order.as_deref(), Some("ASC"));
        assert_eq!(q.content_mode.as_deref(), Some("compact"));
        assert_eq!(q.cursor.as_deref(), Some(expected_cursor.as_str()));
    }

    #[test]
    fn deserialize_messages_query_rejects_noncanonical_cursor_message_id() {
        for cursor in ["1000:1", "1000:msg-1", "not-a-cursor"] {
            assert!(
                serde_json::from_value::<ListMessagesQuery>(json!({ "cursor": cursor })).is_err()
            );
        }
        assert!(
            serde_json::from_value::<ListMessagesQuery>(json!({ "cursor": "" })).is_ok()
        );
    }

    // ── SearchMessagesQuery ─────────────────────────────────────────

    #[test]
    fn deserialize_search_query() {
        let raw = json!({ "keyword": "rust", "page": 1, "page_size": 20 });
        let q: SearchMessagesQuery = serde_json::from_value(raw).unwrap();
        assert_eq!(q.keyword, "rust");
        assert_eq!(q.page, Some(1));
        assert_eq!(q.page_size, Some(20));
    }

    #[test]
    fn deserialize_search_query_missing_keyword() {
        let raw = json!({ "page": 1 });
        assert!(serde_json::from_value::<SearchMessagesQuery>(raw).is_err());
    }

    // ── ConversationResponse ────────────────────────────────────────

    #[test]
    fn serialize_conversation_response_snake_case() {
        let resp = ConversationResponse {
            conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into(),
            name: "Test".into(),
            r#type: AgentType::Acp,
            model: Some(ProviderWithModel {
                provider_id: PROVIDER_ID_1.into(),
                model: "m1".into(),
                use_model: None,
            }),
            status: ConversationStatus::Pending,
            runtime: None,
            source: Some(ConversationSource::Nomifun),
            pinned: false,
            pinned_at: None,
            channel_chat_id: None,
            created_at: 1712345678000,
            modified_at: 1712345678000,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            delegation_policy: Default::default(),
            execution_model_pool: None,
            decision_policy: Default::default(),
            execution_template_id: None,
            linked_execution_id: Some(EXECUTION_ID.into()),
            execution_step_id: None,
            execution_attempt_id: None,
            extra: json!({ "workspace": "/project" }),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            json["conversation_id"],
            "0190f5fe-7c00-7a00-8abc-012345678901"
        );
        assert!(json.get("id").is_none());
        assert_eq!(json["type"], "acp");
        assert_eq!(json["status"], "pending");
        assert_eq!(json["source"], "nomifun");
        assert_eq!(json["created_at"], 1712345678000_i64);
        assert_eq!(json["modified_at"], 1712345678000_i64);
        assert_eq!(json["extra"]["workspace"], "/project");
        assert_eq!(json["linked_execution_id"], EXECUTION_ID);
        assert!(
            json.get("active_execution_id").is_none(),
            "the hard-cut wire contract must not retain the misleading legacy field"
        );
        // Verify snake_case keys
        assert!(json.get("channelChatId").is_none());
        assert!(json.get("createdAt").is_none());
        assert!(json.get("pinnedAt").is_none());
        // Null-valued Option fields must be omitted from JSON.
        assert!(json.get("pinned_at").is_none(), "pinned_at None should be omitted");
        assert!(
            json.get("channel_chat_id").is_none(),
            "channel_chat_id None should be omitted"
        );
    }

    #[test]
    fn serialize_conversation_response_omits_none_keys() {
        let resp = ConversationResponse {
            conversation_id: "0190f5fe-7c00-7a00-8abc-012345678902".into(),
            name: "Test".into(),
            r#type: AgentType::Acp,
            model: None,
            status: ConversationStatus::Pending,
            runtime: None,
            source: None,
            pinned: false,
            pinned_at: None,
            channel_chat_id: None,
            created_at: 1,
            modified_at: 1,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            delegation_policy: Default::default(),
            execution_model_pool: None,
            decision_policy: Default::default(),
            execution_template_id: None,
            linked_execution_id: None,
            execution_step_id: None,
            execution_attempt_id: None,
            extra: json!({}),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("model").is_none(), "model None should be omitted");
        assert!(json.get("source").is_none(), "source None should be omitted");
        assert!(json.get("pinned_at").is_none(), "pinned_at None should be omitted");
        assert!(
            json.get("channel_chat_id").is_none(),
            "channel_chat_id None should be omitted"
        );
        // Non-optional fields still present.
        assert_eq!(
            json["conversation_id"],
            "0190f5fe-7c00-7a00-8abc-012345678902"
        );
        assert!(json.get("id").is_none());
        assert_eq!(json["type"], "acp");
        assert_eq!(json["pinned"], false);
    }

    #[test]
    fn conversation_response_roundtrip() {
        let resp = ConversationResponse {
            conversation_id: "0190f5fe-7c00-7a00-8abc-012345678903".into(),
            name: "Round".into(),
            r#type: AgentType::Acp,
            model: None,
            status: ConversationStatus::Running,
            runtime: None,
            source: None,
            pinned: true,
            pinned_at: Some(1712345678000),
            channel_chat_id: Some("group:42".into()),
            created_at: 1000,
            modified_at: 2000,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            delegation_policy: Default::default(),
            execution_model_pool: None,
            decision_policy: Default::default(),
            execution_template_id: None,
            linked_execution_id: None,
            execution_step_id: None,
            execution_attempt_id: None,
            extra: json!({}),
        };
        let serialized = serde_json::to_string(&resp).unwrap();
        let deserialized: ConversationResponse = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.conversation_id, resp.conversation_id);
        assert!(deserialized.pinned);
        assert_eq!(deserialized.pinned_at, Some(1712345678000));
        assert_eq!(deserialized.channel_chat_id.as_deref(), Some("group:42"));
    }

    #[test]
    fn conversation_response_rejects_removed_generic_id() {
        let raw = json!({
            "id": "0190f5fe-7c00-7a00-8abc-012345678903",
            "name": "Legacy",
            "type": "acp",
            "status": "running",
            "pinned": false,
            "delegation_policy": "automatic",
            "decision_policy": "automatic",
            "created_at": 1000,
            "modified_at": 2000,
            "extra": {}
        });
        assert!(serde_json::from_value::<ConversationResponse>(raw).is_err());
    }

    #[test]
    fn conversation_response_rejects_noncanonical_conversation_id() {
        let valid = json!({
            "conversation_id": "0190f5fe-7c00-7a00-8abc-012345678903",
            "name": "Conversation",
            "type": "acp",
            "status": "running",
            "pinned": false,
            "delegation_policy": "automatic",
            "decision_policy": "automatic",
            "created_at": 1000,
            "modified_at": 2000,
            "extra": {}
        });
        for conversation_id in [
            json!(42),
            json!("conversation-1"),
            json!("550e8400-e29b-41d4-a716-446655440000"),
            json!("0190F5FE-7C00-7A00-8ABC-012345678903"),
            json!("conversation_0190f5fe-7c00-7a00-8abc-012345678903"),
        ] {
            let mut raw = valid.clone();
            raw["conversation_id"] = conversation_id;
            assert!(serde_json::from_value::<ConversationResponse>(raw).is_err());
        }
    }

    // ── MessageResponse ─────────────────────────────────────────────

    #[test]
    fn serialize_message_response_snake_case() {
        let resp = MessageResponse {
            message_id: MESSAGE_ID_1.into(),
            conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into(),
            msg_id: Some(MESSAGE_ID_2.into()),
            r#type: MessageType::Text,
            content: json!({ "content": "Hello" }),
            position: Some(MessagePosition::Right),
            status: Some(MessageStatus::Finish),
            hidden: false,
            created_at: 1712345678000,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["message_id"], MESSAGE_ID_1);
        assert!(json.get("id").is_none());
        assert_eq!(json["conversation_id"], "0190f5fe-7c00-7a00-8abc-012345678901");
        assert_eq!(json["msg_id"], MESSAGE_ID_2);
        assert_eq!(json["type"], "text");
        assert_eq!(json["position"], "right");
        assert_eq!(json["status"], "finish");
        assert_eq!(json["hidden"], false);
        assert_eq!(json["created_at"], 1712345678000_i64);
        // Verify no camelCase leaks
        assert!(json.get("conversationId").is_none());
        assert!(json.get("msgId").is_none());
        assert!(json.get("createdAt").is_none());
    }

    #[test]
    fn message_response_roundtrip() {
        let resp = MessageResponse {
            message_id: MESSAGE_ID_2.into(),
            conversation_id: "0190f5fe-7c00-7a00-8abc-012345678902".into(),
            msg_id: None,
            r#type: MessageType::ToolCall,
            content: json!({ "callId": "c1", "name": "bash" }),
            position: Some(MessagePosition::Left),
            status: None,
            hidden: true,
            created_at: 5000,
        };
        let serialized = serde_json::to_string(&resp).unwrap();
        let deserialized: MessageResponse = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.r#type, MessageType::ToolCall);
        assert!(deserialized.hidden);
        assert!(deserialized.msg_id.is_none());
        assert!(deserialized.status.is_none());
    }

    #[test]
    fn message_response_rejects_removed_generic_id() {
        let raw = json!({
            "id": MESSAGE_ID_2,
            "conversation_id": "0190f5fe-7c00-7a00-8abc-012345678902",
            "type": "text",
            "content": { "content": "Legacy" },
            "hidden": false,
            "created_at": 5000
        });
        assert!(serde_json::from_value::<MessageResponse>(raw).is_err());
    }

    // ── MessageSearchItem ───────────────────────────────────────────

    #[test]
    fn serialize_search_item_snake_case() {
        let item = MessageSearchItem {
            message_id: MESSAGE_ID_1.into(),
            message_type: "text".into(),
            message_created_at: 1712345678000,
            preview_text: "matched snippet".into(),
            conversation: ConversationResponse {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into(),
                name: "Code Review".into(),
                r#type: AgentType::Acp,
                model: None,
                status: ConversationStatus::Finished,
                runtime: None,
                source: None,
                pinned: false,
                pinned_at: None,
                channel_chat_id: None,
                created_at: 1712345678000,
                modified_at: 1712345678000,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                delegation_policy: Default::default(),
                execution_model_pool: None,
                decision_policy: Default::default(),
                execution_template_id: None,
                linked_execution_id: None,
                execution_step_id: None,
                execution_attempt_id: None,
                extra: json!({}),
            },
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["message_id"], MESSAGE_ID_1);
        assert_eq!(json["message_type"], "text");
        assert_eq!(json["message_created_at"], 1712345678000_i64);
        assert_eq!(json["preview_text"], "matched snippet");
        assert_eq!(
            json["conversation"]["conversation_id"],
            "0190f5fe-7c00-7a00-8abc-012345678901"
        );
        assert!(json["conversation"].get("id").is_none());
        assert_eq!(json["conversation"]["name"], "Code Review");
        // Verify no camelCase leaks
        assert!(json.get("messageId").is_none());
        assert!(json.get("messageType").is_none());
        assert!(json.get("previewText").is_none());
    }

    #[test]
    fn search_item_roundtrip() {
        let item = MessageSearchItem {
            message_id: MESSAGE_ID_2.into(),
            message_type: "tips".into(),
            message_created_at: 9000,
            preview_text: "some content preview".into(),
            conversation: ConversationResponse {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678903".into(),
                name: "Search Test".into(),
                r#type: AgentType::Acp,
                model: None,
                status: ConversationStatus::Finished,
                runtime: None,
                source: None,
                pinned: false,
                pinned_at: None,
                channel_chat_id: None,
                created_at: 9000,
                modified_at: 9000,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                delegation_policy: Default::default(),
                execution_model_pool: None,
                decision_policy: Default::default(),
                execution_template_id: None,
                linked_execution_id: None,
                execution_step_id: None,
                execution_attempt_id: None,
                extra: json!({}),
            },
        };
        let serialized = serde_json::to_string(&item).unwrap();
        let deserialized: MessageSearchItem = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.message_id, MESSAGE_ID_2);
        assert_eq!(deserialized.message_type, "tips");
        assert_eq!(deserialized.preview_text, "some content preview");
        assert_eq!(deserialized.conversation.name, "Search Test");
    }

    // ── SendMessageRequest ──────────────────────────────────────────

    #[test]
    fn deserialize_send_message_full() {
        let raw = json!({
            "content": "Review this code",
            "files": ["/tmp/a.rs"],
            "inject_skills": ["security-review"],
            "hidden": true
        });
        let req: SendMessageRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.content, "Review this code");
        assert_eq!(req.files, vec!["/tmp/a.rs"]);
        assert_eq!(req.inject_skills, vec!["security-review"]);
        assert!(req.hidden);
    }

    #[test]
    fn deserialize_send_message_minimal() {
        let raw = json!({ "content": "Hi" });
        let req: SendMessageRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.content, "Hi");
        assert!(req.files.is_empty());
        assert!(req.inject_skills.is_empty());
        assert!(!req.hidden);
    }

    #[test]
    fn deserialize_send_message_missing_content() {
        let raw = json!({});
        assert!(serde_json::from_value::<SendMessageRequest>(raw).is_err());
    }

    #[test]
    fn deserialize_send_message_rejects_client_msg_id() {
        // Message identity is server-owned; stale/forged client fields fail
        // closed instead of being silently normalized.
        let raw = json!({ "content": "Hi", "msg_id": "client-supplied" });
        assert!(serde_json::from_value::<SendMessageRequest>(raw).is_err());
    }

    // ── Paginated type aliases ──────────────────────────────────────

    #[test]
    fn conversation_list_response_serialization() {
        let list: ConversationListResponse = PaginatedResult {
            items: vec![ConversationResponse {
                conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into(),
                name: "Test".into(),
                r#type: AgentType::Acp,
                model: None,
                status: ConversationStatus::Pending,
                runtime: None,
                source: None,
                pinned: false,
                pinned_at: None,
                channel_chat_id: None,
                created_at: 1000,
                modified_at: 1000,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                delegation_policy: Default::default(),
                execution_model_pool: None,
                decision_policy: Default::default(),
                execution_template_id: None,
                linked_execution_id: None,
                execution_step_id: None,
                execution_attempt_id: None,
                extra: json!({}),
            }],
            total: 1,
            has_more: false,
        };
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json["items"].as_array().unwrap().len(), 1);
        assert_eq!(json["total"], 1);
        assert_eq!(json["has_more"], false);
    }

    #[test]
    fn message_list_response_serialization() {
        let list: MessageListResponse = PaginatedResult {
            items: vec![],
            total: 0,
            has_more: false,
        };
        let json = serde_json::to_value(&list).unwrap();
        assert!(json["items"].as_array().unwrap().is_empty());
        assert_eq!(json["total"], 0);
    }

    #[test]
    fn message_search_response_serialization() {
        let resp: MessageSearchResponse = PaginatedResult {
            items: vec![MessageSearchItem {
                message_id: MESSAGE_ID_1.into(),
                message_type: "text".into(),
                message_created_at: 5000,
                preview_text: "matched".into(),
                conversation: ConversationResponse {
                    conversation_id: "0190f5fe-7c00-7a00-8abc-012345678903".into(),
                    name: "Conv".into(),
                    r#type: AgentType::Acp,
                    model: None,
                    status: ConversationStatus::Finished,
                    runtime: None,
                    source: None,
                    pinned: false,
                    pinned_at: None,
                    channel_chat_id: None,
                    created_at: 5000,
                    modified_at: 5000,
                    preset_id: None,
                    preset_revision: None,
                    preset_snapshot: None,
                    delegation_policy: Default::default(),
                    execution_model_pool: None,
                    decision_policy: Default::default(),
                    execution_template_id: None,
                    linked_execution_id: None,
                    execution_step_id: None,
                    execution_attempt_id: None,
                    extra: json!({}),
                },
            }],
            total: 1,
            has_more: false,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["items"][0]["message_id"], MESSAGE_ID_1);
        assert_eq!(
            json["items"][0]["conversation"]["conversation_id"],
            "0190f5fe-7c00-7a00-8abc-012345678903"
        );
        assert_eq!(json["items"][0]["preview_text"], "matched");
        assert_eq!(json["total"], 1);
    }

    #[test]
    fn serialize_conversation_artifact_response() {
        let conversation_artifact_id = "0190f5fe-7c00-7a00-8abc-012345678951";
        let artifact = ConversationArtifactResponse {
            conversation_artifact_id: conversation_artifact_id.into(),
            conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into(),
            cron_job_id: Some(CRON_JOB_ID.into()),
            kind: ConversationArtifactKind::SkillSuggest,
            status: ConversationArtifactStatus::Active,
            payload: json!({
                "cron_job_id": CRON_JOB_ID,
                "name": "daily-report",
                "description": "Daily report",
                "skillContent": "---\nname: daily-report\n---\nUse it.",
            }),
            created_at: 1000,
            updated_at: 2000,
        };

        let raw = serde_json::to_value(&artifact).unwrap();
        assert_eq!(raw["conversation_artifact_id"], conversation_artifact_id);
        assert!(raw.get("artifact_id").is_none());
        assert!(raw.get("id").is_none());
        assert_eq!(raw["kind"], "skill_suggest");
        assert_eq!(raw["status"], "active");
        assert_eq!(raw["payload"]["name"], "daily-report");

        let decoded: ConversationArtifactResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(decoded.conversation_artifact_id, conversation_artifact_id);
    }

    #[test]
    fn conversation_artifact_response_rejects_noncanonical_and_legacy_ids() {
        let valid = json!({
            "conversation_artifact_id": "0190f5fe-7c00-7a00-8abc-012345678951",
            "conversation_id": "0190f5fe-7c00-7a00-8abc-012345678901",
            "cron_job_id": CRON_JOB_ID,
            "kind": "skill_suggest",
            "status": "active",
            "payload": {},
            "created_at": 1000,
            "updated_at": 2000
        });

        for invalid_id in [
            json!(42),
            json!("artifact_0190f5fe-7c00-7a00-8abc-012345678951"),
            json!("0190F5FE-7C00-7A00-8ABC-012345678951"),
            json!("550e8400-e29b-41d4-a716-446655440000"),
        ] {
            let mut raw = valid.clone();
            raw["conversation_artifact_id"] = invalid_id;
            assert!(serde_json::from_value::<ConversationArtifactResponse>(raw).is_err());
        }

        for legacy_field in ["artifact_id", "id"] {
            let mut raw = valid.clone();
            let value = raw
                .as_object_mut()
                .unwrap()
                .remove("conversation_artifact_id")
                .unwrap();
            raw[legacy_field] = value;
            assert!(serde_json::from_value::<ConversationArtifactResponse>(raw).is_err());
        }
    }
}
