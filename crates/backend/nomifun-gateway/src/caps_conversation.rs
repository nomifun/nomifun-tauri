//! Conversation-domain capabilities (registry form): list / status / send /
//! create / update / delete. Creation is companion-only on this Agent-facing
//! surface. All self-protection guards from the legacy tool are preserved (no
//! self-injection, no self-model-change, no self-deletion), and nomi sessions
//! still get a model at creation via the shared resolution chain so downstream
//! consumers never see a model-less nomi conversation.

use std::sync::Arc;

use nomifun_api_types::{
    CreateConversationRequest, ListConversationsQuery, ListMessagesQuery, SendMessageRequest,
    UpdateConversationRequest,
};
use nomifun_common::{AgentType, AppError, CompanionId, ConversationId, ProviderWithModel, ProviderId, RemoteAgentId};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::{CallerCtx, GatewayDeps};
use crate::provider_support;
use crate::registry::{Capability, CapabilityMeta, DangerTier, Surface};
use crate::server::ok;

const DEFAULT_LIST_LIMIT: u32 = 50;
const DEFAULT_MESSAGE_LIMIT: u32 = 5;
/// Per-message content budget in status output — keeps a busy transcript from
/// blowing up the calling agent's context.
const MESSAGE_SNIPPET_CHARS: usize = 500;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListConversationsParams {
    /// Maximum number of conversations to return (default 50).
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ConversationStatusParams {
    /// The id of the conversation to inspect.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    conversation_id: ConversationId,
    /// How many recent messages to include (default 5, max 50).
    #[serde(default)]
    message_limit: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendToConversationParams {
    /// The id of the TARGET conversation (not your own).
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    conversation_id: ConversationId,
    /// The message or task prompt to inject.
    content: String,
    /// When true the message is hidden from the visible history (use for
    /// background task prompts, like AutoWork does).
    #[serde(default)]
    hidden: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateConversationParams {
    /// Optional display name for the new conversation.
    #[serde(default)]
    name: Option<String>,
    /// Agent type: "nomi" (default), "acp", or "remote". NOT for terminals — any
    /// terminal/shell intent must go through nomi_create_terminal instead.
    #[serde(default)]
    agent_type: Option<String>,
    /// ACP backend vendor when agent_type is "acp" (e.g. "claude", "codex", "gemini").
    #[serde(default)]
    backend: Option<String>,
    /// Exact provider/model pair for a nomi session. Omit to auto-resolve:
    /// your own companion model → first configured provider.
    #[serde(default)]
    model: Option<ConversationModelRefParam>,
    /// Registered remote-agent business id. Required when agent_type is "remote".
    #[serde(default)]
    #[schemars(schema_with = "crate::id_schema::optional_canonical_uuid_v7_schema")]
    remote_agent_id: Option<RemoteAgentId>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateConversationParams {
    /// The id of the conversation to update (from nomi_list_conversations).
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    conversation_id: ConversationId,
    /// New display name (omit to keep).
    #[serde(default)]
    name: Option<String>,
    /// Pin (true) or unpin (false) the conversation in the sidebar.
    #[serde(default)]
    pinned: Option<bool>,
    /// Exact replacement provider/model pair (nomi conversations only).
    #[serde(default)]
    model: Option<ConversationModelRefParam>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteConversationParams {
    /// The id of the conversation to delete. Confirm the target with the user
    /// before calling — deletion also kills its agent and cron bindings.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    conversation_id: ConversationId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WhoamiParams {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ConversationModelRefParam {
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    provider_id: ProviderId,
    #[serde(deserialize_with = "deserialize_model_name")]
    model: String,
}

impl From<ConversationModelRefParam> for ProviderWithModel {
    fn from(value: ConversationModelRefParam) -> Self {
        Self {
            provider_id: value.provider_id.into_string(),
            model: value.model,
            use_model: None,
        }
    }
}

fn deserialize_model_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty() || value.trim() != value {
        return Err(serde::de::Error::custom(
            "model must be a non-empty trimmed natural key",
        ));
    }
    Ok(value)
}

fn error_value(e: AppError) -> Value {
    json!({ "error": e.to_string() })
}

fn require_user(ctx: &CallerCtx) -> Result<&str, Value> {
    Ok(ctx.user_id.as_str())
}

fn require_companion_creator(ctx: &CallerCtx) -> Result<(), Value> {
    if ctx
        .companion_id
        .as_ref()
        .is_some()
    {
        Ok(())
    } else {
        Err(json!({
            "error": "conversation_creation_forbidden: top-level conversations may only be created by the user, scheduled jobs, or a companion; use nomi_delegate for multi-Agent work inside the current conversation"
        }))
    }
}

async fn list(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ListConversationsParams) -> Value {
    let user_id = match require_user(&ctx) {
        Ok(u) => u,
        Err(e) => return e,
    };
    let query = ListConversationsQuery {
        limit: Some(p.limit.unwrap_or(DEFAULT_LIST_LIMIT)),
        ..Default::default()
    };
    // Exclude the companion's own work-partner single sessions from the page + total.
    let resp = match deps.conversation_service.list(user_id, query, true).await {
        Ok(r) => r,
        Err(e) => return error_value(e),
    };
    let mut items = Vec::with_capacity(resp.items.len());
    for conv in resp.items {
        let runtime = deps
            .conversation_service
            .runtime_summary_for(conv.conversation_id.as_str())
            .await;
        let companion_id = match conv.extra.get("companion_id") {
            None => None,
            Some(Value::String(value)) => match CompanionId::parse(value) {
                Ok(id) => Some(id),
                Err(error) => {
                    return json!({"error": format!("conversation {} has invalid companion_id: {error}", conv.conversation_id)});
                }
            },
            Some(_) => {
                return json!({"error": format!("conversation {} has non-string companion_id", conv.conversation_id)});
            }
        };
        items.push(json!({
            "conversation_id": conv.conversation_id,
            "name": conv.name,
            "agent_type": conv.r#type,
            "status": conv.status,
            "runtime_state": runtime.state,
            "pending_confirmations": runtime.pending_confirmations,
            "source": conv.source,
            "pinned": conv.pinned,
            "is_companion_companion": conv.extra.get("companion_session").and_then(Value::as_bool).unwrap_or(false),
            "companion_id": companion_id,
            "is_self": ctx.conversation_id.as_ref().is_some_and(|id| conv.conversation_id.as_str() == id.as_str()),
            "modified_at": conv.modified_at,
        }));
    }
    ok(json!({ "total": resp.total, "conversations": items }))
}

async fn status(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: ConversationStatusParams) -> Value {
    let user_id = match require_user(&ctx) {
        Ok(u) => u,
        Err(e) => return e,
    };
    let id = p.conversation_id.as_str();
    let conv = match deps.conversation_service.get(user_id, id).await {
        Ok(c) => c,
        Err(e) => return error_value(e),
    };
    let runtime = deps.conversation_service.runtime_summary_for(id).await;
    let message_limit = p.message_limit.unwrap_or(DEFAULT_MESSAGE_LIMIT).clamp(1, 50);
    let messages = match deps
        .conversation_service
        .list_messages(
            user_id,
            id,
            ListMessagesQuery {
                page: Some(1),
                page_size: Some(message_limit),
                order: Some("desc".to_owned()),
                content_mode: None,
                cursor: None,
            },
        )
        .await
    {
        Ok(m) => m,
        Err(e) => return error_value(e),
    };
    let messages_json = match serde_json::to_value(&messages) {
        Ok(v) => truncate_message_contents(v),
        Err(e) => return json!({ "error": format!("failed to serialize messages: {e}") }),
    };
    ok(json!({
        "conversation_id": conv.conversation_id,
        "name": conv.name,
        "agent_type": conv.r#type,
        "status": conv.status,
        "runtime": runtime,
        "recent_messages": messages_json,
    }))
}

async fn send(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: SendToConversationParams) -> Value {
    let user_id = match require_user(&ctx) {
        Ok(u) => u.to_owned(),
        Err(e) => return e,
    };
    let id = p.conversation_id.into_string();
    if ctx.conversation_id.as_ref().is_some_and(|caller| id == caller.as_str()) {
        return json!({ "error": "self_injection_forbidden: you cannot send a message into your own conversation" });
    }
    let Some(operation_id) = ctx.operation_id.as_deref() else {
        return json!({
            "error": "missing_idempotency_key: conversation send requires an authenticated operation identity"
        });
    };
    let req = SendMessageRequest {
        content: p.content,
        files: vec![],
        inject_skills: vec![],
        hidden: p.hidden.unwrap_or(false),
        origin: Some("companion".into()),
        channel_platform: None,
    };
    match deps
        .conversation_service
        .send_message_with_idempotency_key(
            &user_id,
            &id,
            operation_id,
            req,
            &deps.runtime_registry,
        )
        .await
    {
        Ok(delivery) => ok(json!({
            "msg_id": delivery.message_id,
            "replayed": delivery.replayed,
            "completed": delivery.completed,
            "result_ok": delivery.result_ok,
            "result_text": delivery.result_text,
            "result_error": delivery.result_error,
            "note": "message accepted; the target session processes it asynchronously — use nomi_conversation_status to follow progress"
        })),
        Err(AppError::Conflict(m)) => json!({
            "error": format!("busy: the target conversation is already running a turn ({m}); check nomi_conversation_status and retry later")
        }),
        Err(e) => error_value(e),
    }
}

async fn create(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: CreateConversationParams) -> Value {
    if let Err(error) = require_companion_creator(&ctx) {
        return error;
    }
    let user_id = match require_user(&ctx) {
        Ok(u) => u.to_owned(),
        Err(e) => return e,
    };
    let agent_type_str = p.agent_type.unwrap_or_else(|| "nomi".to_owned());
    if agent_type_str == "terminal" {
        return json!({
            "error": "terminal sessions are not conversations: use nomi_create_terminal (preset shell | claude | codex | gemini) for any terminal/shell intent"
        });
    }
    let agent_type: AgentType = match serde_json::from_value(json!(agent_type_str)) {
        Ok(t) => t,
        Err(_) => return json!({ "error": format!("invalid agent_type '{agent_type_str}'") }),
    };
    let mut extra = json!({});
    if agent_type == AgentType::Remote {
        let Some(remote_agent_id) = p.remote_agent_id else {
            return json!({ "error": "remote_agent_id is required when agent_type is 'remote'" });
        };
        match deps.remote_agent_service.get(&remote_agent_id).await {
            Ok(remote) if remote.protocol == nomifun_common::RemoteAgentProtocol::OpenClaw => {}
            Ok(_) => {
                return json!({
                    "error": "remote_agent_id refers to an unsupported legacy protocol; create an OpenClaw remote agent"
                });
            }
            Err(error) => return json!({ "error": format!("invalid remote_agent_id: {error}") }),
        }
        extra["remote_agent_id"] = json!(remote_agent_id);
    } else if p.remote_agent_id.is_some() {
        return json!({ "error": "remote_agent_id is only valid when agent_type is 'remote'" });
    }
    if let Some(backend) = p.backend {
        extra["backend"] = json!(backend);
    }
    let mut model = None;
    let mut model_source = None;
    if agent_type == AgentType::Nomi {
        let requested_model = p.model.map(ProviderWithModel::from);
        match provider_support::resolve_nomi_model(&deps, &ctx, requested_model.as_ref()).await {
            Ok((m, source)) => {
                model = Some(m);
                model_source = Some(source);
            }
            Err(e) => return e,
        }
    } else if p.model.is_some() {
        return json!({ "error": "model is only valid when agent_type is 'nomi'" });
    }
    let req = CreateConversationRequest {
        r#type: agent_type,
        name: p.name,
        model,
        source: None,
        channel_chat_id: None,
        preset_id: None,
        preset_overrides: None,
        delegation_policy: Default::default(),
        execution_model_pool: None,
        decision_policy: Default::default(),
        execution_template_id: None,
        extra,
    };
    match deps.conversation_service.create(&user_id, req).await {
        Ok(resp) => ok(json!({
            "conversation_id": resp.conversation_id,
            "name": resp.name,
            "agent_type": resp.r#type,
            "model": resp.model,
            "model_source": model_source,
        })),
        Err(e) => error_value(e),
    }
}

/// Reflect the calling session's identity: which companion it is bound to, the
/// surface it arrived on, and the user scope. For an external partner this
/// answers "who am I acting as?"; for tests it proves companion binding reached
/// dispatch.
async fn whoami(_deps: Arc<GatewayDeps>, ctx: CallerCtx, _p: WhoamiParams) -> Value {
    ok(json!({
        "user_id": ctx.user_id,
        "companion_id": ctx.companion_id,
        "surface": format!("{:?}", ctx.surface()),
        "remote": ctx.remote,
        "channel_platform": ctx.channel_platform,
    }))
}

async fn update(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: UpdateConversationParams) -> Value {
    let user_id = match require_user(&ctx) {
        Ok(u) => u.to_owned(),
        Err(e) => return e,
    };
    let id = p.conversation_id.into_string();
    if p.name.is_none() && p.pinned.is_none() && p.model.is_none() {
        return json!({ "error": "nothing to update: provide at least one of name / pinned / model" });
    }
    let mut model = None;
    if let Some(requested_model) = p.model.map(ProviderWithModel::from) {
        if ctx.conversation_id.as_ref().is_some_and(|caller| id == caller.as_str()) {
            return json!({
                "error": "self_model_change_forbidden: changing your own conversation's model would terminate your current turn; the owner can change it from the desktop UI"
            });
        }
        match provider_support::resolve_explicit_model(&deps, requested_model).await {
            Ok(m) => model = Some(m),
            Err(e) => return e,
        }
    }
    let model_changed = model.is_some();
    let req = UpdateConversationRequest {
        name: p.name,
        pinned: p.pinned,
        model,
        delegation_policy: None,
        execution_model_pool: None,
        decision_policy: None,
        execution_template_id: None,
        extra: None,
    };
    match deps.conversation_service.update(&user_id, &id, req, &deps.runtime_registry).await {
        Ok(resp) => ok(json!({
            "conversation_id": resp.conversation_id,
            "name": resp.name,
            "pinned": resp.pinned,
            "model": resp.model,
            "note": model_changed.then_some(
                "model changed: any running task in that conversation was terminated; it restarts with the new model on the next message"
            ),
        })),
        Err(e) => error_value(e),
    }
}

async fn delete(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: DeleteConversationParams) -> Value {
    let user_id = match require_user(&ctx) {
        Ok(u) => u.to_owned(),
        Err(e) => return e,
    };
    let id = p.conversation_id.into_string();
    if ctx.conversation_id.as_ref().is_some_and(|caller| id == caller.as_str()) {
        return json!({ "error": "self_deletion_forbidden: you cannot delete your own conversation" });
    }
    match deps.conversation_service.delete(&user_id, &id).await {
        Ok(()) => ok(json!({ "deleted": id })),
        Err(e) => error_value(e),
    }
}

/// Cap every `content` string inside the serialized message list so a long
/// transcript cannot flood the calling agent.
fn truncate_message_contents(mut value: Value) -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(map) => {
                for (k, item) in map.iter_mut() {
                    if k == "content" {
                        if let Value::String(s) = item
                            && s.chars().count() > MESSAGE_SNIPPET_CHARS
                        {
                            let truncated: String = s.chars().take(MESSAGE_SNIPPET_CHARS).collect();
                            *item = Value::String(format!("{truncated}…[truncated]"));
                        } else {
                            walk(item);
                        }
                    } else {
                        walk(item);
                    }
                }
            }
            Value::Array(arr) => {
                for item in arr.iter_mut() {
                    walk(item);
                }
            }
            _ => {}
        }
    }
    walk(&mut value);
    value
}

pub(crate) fn register(out: &mut Vec<Capability>) {
    out.push(Capability::new::<ListConversationsParams, _, _>(
        CapabilityMeta::new(
            "nomi_list_conversations",
            "conversation",
            "List the desktop's conversations with their live runtime state.",
            DangerTier::Read,
        ),
        list,
    ));
    out.push(Capability::new::<ConversationStatusParams, _, _>(
        CapabilityMeta::new(
            "nomi_conversation_status",
            "conversation",
            "Runtime summary + the tail of a conversation's transcript (live progress snapshot).",
            DangerTier::Read,
        ),
        status,
    ));
    out.push(Capability::new::<SendToConversationParams, _, _>(
        CapabilityMeta::new(
            "nomi_send_to_conversation",
            "conversation",
            "Inject a message (or a hidden task prompt) into another session.",
            DangerTier::Write,
        ),
        send,
    ));
    out.push(Capability::new::<CreateConversationParams, _, _>(
        CapabilityMeta::new(
            "nomi_create_conversation",
            "conversation",
            "Open a fresh desktop session on behalf of the calling companion (nomi, acp, or remote OpenClaw). For remote sessions pass remote_agent_id from nomi_remote_agent_list; for multi-Agent work inside the current conversation, use nomi_delegate.",
            DangerTier::Write,
        ),
        create,
    ));
    out.push(Capability::new::<UpdateConversationParams, _, _>(
        CapabilityMeta::new(
            "nomi_update_conversation",
            "conversation",
            "Rename / pin / change model of a conversation (not your own model).",
            DangerTier::Write,
        ),
        update,
    ));
    out.push(Capability::new::<DeleteConversationParams, _, _>(
        CapabilityMeta::new(
            "nomi_delete_conversation",
            "conversation",
            "Delete a conversation (cascades: agent kill, cron unbind, knowledge unmount). Confirm first.",
            DangerTier::Destructive,
        )
        .deny_on(&[Surface::Channel]),
        delete,
    ));
    out.push(Capability::new::<WhoamiParams, _, _>(
        CapabilityMeta::new(
            "nomi_whoami",
            "conversation",
            "Identity of the calling session: the bound companion id, surface (Desktop/Channel/Remote), and user. Lets an external partner confirm which companion it is acting as.",
            DangerTier::Read,
        ),
        whoami,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::UserId;

    #[test]
    fn truncate_caps_long_content_strings() {
        let long = "x".repeat(2000);
        let v = json!({"items": [{"content": long, "other": "keep"}]});
        let out = truncate_message_contents(v);
        let content = out["items"][0]["content"].as_str().unwrap();
        assert!(content.chars().count() < 600);
        assert!(content.ends_with("…[truncated]"));
        assert_eq!(out["items"][0]["other"], "keep");
    }

    #[test]
    fn truncate_keeps_short_content_untouched() {
        let v = json!({"content": "short"});
        let out = truncate_message_contents(v);
        assert_eq!(out["content"], "short");
    }

    #[test]
    fn remote_create_params_accepts_canonical_remote_agent_id() {
        let params: CreateConversationParams = serde_json::from_value(json!({
            "agent_type": "remote",
            "remote_agent_id": "0190f5fe-7c00-7a00-8abc-012345678901"
        }))
        .unwrap();

        assert_eq!(params.agent_type.as_deref(), Some("remote"));
        assert_eq!(
            params.remote_agent_id.as_ref().map(RemoteAgentId::as_str),
            Some("0190f5fe-7c00-7a00-8abc-012345678901")
        );
    }

    #[test]
    fn conversation_params_require_typed_ids_and_fixed_model_object() {
        assert!(
            serde_json::from_value::<ConversationStatusParams>(json!({
                "conversation_id": "conversation-1"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<CreateConversationParams>(json!({
                "agent_type": "nomi",
                "provider_id": "0190f5fe-7c00-7a00-8abc-012345678904",
                "model": "model-a"
            }))
            .is_err()
        );
        let params: CreateConversationParams = serde_json::from_value(json!({
            "agent_type": "nomi",
            "model": {
                "provider_id": "0190f5fe-7c00-7a00-8abc-012345678904",
                "model": "model-a"
            }
        }))
        .unwrap();
        assert_eq!(
            params.model.as_ref().map(|model| model.provider_id.as_str()),
            Some("0190f5fe-7c00-7a00-8abc-012345678904")
        );
        assert!(
            serde_json::from_value::<CreateConversationParams>(json!({
                "agent_type": "nomi",
                "model": {
                    "provider_id": "0190f5fe-7c00-7a00-8abc-012345678904",
                    "model": "model-a",
                    "use_model": "retired-alias"
                }
            }))
            .is_err()
        );
    }

    #[test]
    fn top_level_creation_requires_a_companion_identity() {
        let plain = CallerCtx {
            conversation_id: Some(
                ConversationId::parse("0190f5fe-7c00-7a00-8abc-012345678901")
                    .unwrap(),
            ),
            user_id: UserId::parse("0190f5fe-7c00-7a00-8abc-012345678902")
                .unwrap(),
            ..Default::default()
        };
        let error = require_companion_creator(&plain).unwrap_err();
        assert!(error["error"]
            .as_str()
            .is_some_and(|message| message.contains("conversation_creation_forbidden")));

        let companion = CallerCtx {
            companion_id: Some(
                CompanionId::parse("0190f5fe-7c00-7a00-8abc-012345678903")
                    .unwrap(),
            ),
            ..plain
        };
        assert!(require_companion_creator(&companion).is_ok());
    }
}
