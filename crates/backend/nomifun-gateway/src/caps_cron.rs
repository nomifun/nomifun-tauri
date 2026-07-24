//! Cron-domain capabilities (registry form). Create/update reuse the
//! `ICronService` implementation behind the `[CRON_*]` text protocol, so a
//! gateway session gets the same context derivation (agent type / model from
//! the bound conversation) and bind-back behavior as the in-chat protocol.

use std::sync::Arc;

use nomifun_api_types::{ListCronJobsQuery, UpdateConversationRequest};
use nomifun_common::{AgentType, ConversationId, CronJobId};
use nomifun_conversation::response_middleware::{
    CronCreateParams as SvcCronCreate, CronUpdateParams as SvcCronUpdate, ICronService,
};
use nomifun_cron::types::cron_job_to_response;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::{CallerCtx, GatewayDeps};
use crate::registry::{Capability, CapabilityMeta, DangerTier};
use crate::server::ok;
use crate::provider_support;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CronListParams {
    /// Restrict to jobs bound to one conversation (default: all jobs).
    #[serde(default)]
    #[schemars(schema_with = "crate::id_schema::optional_canonical_uuid_v7_schema")]
    conversation_id: Option<ConversationId>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CronCreateParams {
    /// Short human-readable job name.
    name: String,
    /// Standard 5-field cron expression, e.g. "0 9 * * *" for daily 09:00.
    cron: String,
    /// Human-readable description of the schedule (e.g. "every day at 9am").
    #[serde(default)]
    description: Option<String>,
    /// The prompt message sent to the agent on every trigger.
    message: String,
    /// Conversation to run the job in (default: the calling conversation).
    #[serde(default)]
    #[schemars(schema_with = "crate::id_schema::optional_canonical_uuid_v7_schema")]
    conversation_id: Option<ConversationId>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CronUpdateParams {
    /// The id of the cron job to update (from nomi_cron_list).
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    cron_job_id: CronJobId,
    /// New job name (full replacement; pass the existing value to keep it).
    name: String,
    /// New cron expression (full replacement).
    cron: String,
    /// New human-readable schedule description.
    #[serde(default)]
    description: Option<String>,
    /// New trigger message (full replacement).
    message: String,
    /// Conversation the job is bound to (default: the calling conversation).
    #[serde(default)]
    #[schemars(schema_with = "crate::id_schema::optional_canonical_uuid_v7_schema")]
    conversation_id: Option<ConversationId>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CronDeleteParams {
    /// The id of the cron job to delete. Confirm the target with the user first.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    cron_job_id: CronJobId,
}

/// Duplicate-create guard: an ACTIVE job in the same conversation with the same
/// (trimmed) name or the exact same (trimmed) message counts as a duplicate.
fn is_duplicate_job(existing_name: &str, existing_message: &str, new_name: &str, new_message: &str) -> bool {
    existing_name.trim().eq_ignore_ascii_case(new_name.trim()) || existing_message.trim() == new_message.trim()
}

async fn list(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: CronListParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({ "error": "missing caller user identity" });
    }
    let query = ListCronJobsQuery {
        conversation_id: p.conversation_id.map(ConversationId::into_string),
    };
    match deps.cron_service.list_jobs(ctx.user_id.as_str(), &query).await {
        Ok(jobs) => ok(jobs.iter().map(cron_job_to_response).collect::<Vec<_>>()),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn create(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: CronCreateParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({ "error": "missing caller user identity" });
    }
    let target_conversation_id = p
        .conversation_id
        .map(ConversationId::into_string)
        .or_else(|| ctx.conversation_id.clone().map(nomifun_common::ConversationId::into_string));
    let Some(target_conversation_id) = target_conversation_id else {
        return json!({ "error": "missing required field: conversation_id" });
    };
    let target_conversation = target_conversation_id.clone();

    // -- duplicate guard ----------------------------------------------
    match deps
        .cron_service
        .list_jobs(ctx.user_id.as_str(), &ListCronJobsQuery {
            conversation_id: Some(target_conversation_id),
        })
        .await
    {
        Ok(jobs) => {
            if let Some(existing) = jobs
                .iter()
                .find(|j| j.enabled && is_duplicate_job(&j.name, &j.message, &p.name, &p.message))
            {
                return ok(json!({
                    "duplicate": true,
                    "existing_job": cron_job_to_response(existing),
                    "note": "an ACTIVE cron job with the same name or message already exists in this conversation —nothing was created. Use nomi_cron_update to modify it; only create a second job if the owner explicitly asked for a duplicate this turn."
                }));
            }
        }
        Err(e) => return json!({ "error": e.to_string() }),
    }

    // -- model guard (nomi conversations only) ------------------------
    let mut model_note: Option<String> = None;
    match deps.conversation_service.get(ctx.user_id.as_str(), &target_conversation).await {
        Ok(conv) => {
            let model_missing = conv.model.as_ref().is_none_or(|model| model.validate().is_err());
            if conv.r#type == AgentType::Nomi && model_missing {
                match provider_support::resolve_nomi_model(&deps, &ctx, None).await {
                    Ok((m, source)) => {
                        let req = UpdateConversationRequest {
                            name: None,
                            pinned: None,
                            model: Some(m.clone()),
                            delegation_policy: None,
                            execution_model_pool: None,
                            decision_policy: None,
                            execution_template_id: None,
                            extra: None,
                        };
                        if let Err(e) = deps
                            .conversation_service
                            .update(ctx.user_id.as_str(), &target_conversation, req, &deps.runtime_registry)
                            .await
                        {
                            return json!({ "error": format!("failed to persist auto-selected model onto the bound conversation: {e}") });
                        }
                        model_note = Some(format!(
                            "the bound conversation had no model configured; auto-selected {}/{} (source: {source}) and saved it onto the conversation —mention this to the owner",
                            m.provider_id, m.model
                        ));
                    }
                    Err(e) => return e,
                }
            }
        }
        Err(e) => {
            return json!({
                "error": format!("cannot create the cron job: the bound conversation '{target_conversation}' is not accessible ({e}); a job bound to a missing conversation would never run")
            });
        }
    }

    let params = SvcCronCreate {
        name: p.name,
        schedule: p.cron,
        schedule_description: p.description.unwrap_or_default(),
        message: p.message,
    };
    let result = ICronService::create_job(deps.cron_service.as_ref(), ctx.user_id.as_str(), &target_conversation, &params).await;
    if result.success {
        ok(json!({ "message": result.message, "model_note": model_note }))
    } else {
        json!({ "error": result.message })
    }
}

async fn update(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: CronUpdateParams) -> Value {
    let Some(target_conversation) = p
        .conversation_id
        .map(ConversationId::into_string)
        .or_else(|| ctx.conversation_id.clone().map(nomifun_common::ConversationId::into_string))
    else {
        return json!({ "error": "missing required field: conversation_id" });
    };
    let params = SvcCronUpdate {
        job_id: p.cron_job_id.into_string(),
        name: p.name,
        schedule: p.cron,
        schedule_description: p.description.unwrap_or_default(),
        message: p.message,
    };
    command_result(ICronService::update_job(deps.cron_service.as_ref(), ctx.user_id.as_str(), &target_conversation, &params).await)
}

async fn delete(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: CronDeleteParams) -> Value {
    command_result(
        ICronService::delete_job(
            deps.cron_service.as_ref(),
            ctx.user_id.as_str(),
            p.cron_job_id.as_str(),
        )
        .await,
    )
}

fn command_result(result: nomifun_conversation::response_middleware::CronCommandResult) -> Value {
    if result.success {
        json!({ "result": result.message })
    } else {
        json!({ "error": result.message })
    }
}

pub(crate) fn register(out: &mut Vec<Capability>) {
    out.push(Capability::new::<CronListParams, _, _>(
        CapabilityMeta::new(
            "nomi_cron_list",
            "cron",
            "List scheduled cron jobs (all jobs by default; pass conversation_id to filter to one session).",
            DangerTier::Read,
        ),
        list,
    ));
    out.push(Capability::new::<CronCreateParams, _, _>(
        CapabilityMeta::new(
            "nomi_cron_create",
            "cron",
            "Schedule a recurring prompt (cron). Binds to conversation_id or the calling conversation; guards against duplicates and model-less nomi sessions.",
            DangerTier::Write,
        ),
        create,
    ));
    out.push(Capability::new::<CronUpdateParams, _, _>(
        CapabilityMeta::new(
            "nomi_cron_update",
            "cron",
            "Update a cron job (full replacement of name/cron/message).",
            DangerTier::Write,
        ),
        update,
    ));
    out.push(Capability::new::<CronDeleteParams, _, _>(
        CapabilityMeta::new(
            "nomi_cron_delete",
            "cron",
            "Delete a cron job. Confirm the target with the user first.",
            DangerTier::Destructive,
        ),
        delete,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Registry, Surface};

    const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
    const CRON_JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678902";

    #[test]
    fn duplicate_when_name_matches_ignoring_case_and_whitespace() {
        assert!(is_duplicate_job("Daily Report", "msg a", "  daily report ", "msg b"));
    }

    #[test]
    fn duplicate_when_message_matches_exactly_after_trim() {
        assert!(is_duplicate_job("job a", " summarize inbox ", "job b", "summarize inbox"));
    }

    #[test]
    fn not_duplicate_when_both_differ() {
        assert!(!is_duplicate_job("job a", "message a", "job b", "message b"));
    }

    #[test]
    fn message_comparison_is_case_sensitive() {
        assert!(!is_duplicate_job("job a", "Do The Thing", "job b", "do the thing"));
    }

    #[test]
    fn cron_params_require_named_bare_uuidv7_ids() {
        let update: CronUpdateParams = serde_json::from_value(json!({
            "cron_job_id": CRON_JOB_ID,
            "name": "Daily",
            "cron": "0 9 * * *",
            "message": "Run",
            "conversation_id": CONVERSATION_ID
        }))
        .unwrap();
        assert_eq!(update.cron_job_id.as_str(), CRON_JOB_ID);
        assert_eq!(
            update.conversation_id.as_ref().map(ConversationId::as_str),
            Some(CONVERSATION_ID)
        );

        let delete: CronDeleteParams =
            serde_json::from_value(json!({"cron_job_id": CRON_JOB_ID})).unwrap();
        assert_eq!(delete.cron_job_id.as_str(), CRON_JOB_ID);

        for invalid in [
            json!({"cron_job_id": 1}),
            json!({"cron_job_id": "1"}),
            json!({"cron_job_id": "cron_0190f5fe-7c00-7a00-8abc-012345678902"}),
            json!({"job_id": CRON_JOB_ID}),
            json!({"id": CRON_JOB_ID}),
        ] {
            assert!(
                serde_json::from_value::<CronDeleteParams>(invalid).is_err(),
                "invalid cron locator must be rejected"
            );
        }
    }

    #[test]
    fn cron_update_and_delete_schemas_expose_only_cron_job_id() {
        let specs = Registry::global().tool_specs(Surface::Desktop);
        for name in ["nomi_cron_update", "nomi_cron_delete"] {
            let spec = specs
                .iter()
                .find(|spec| spec.name == name)
                .expect("cron tool registered");
            let properties = spec
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .expect("cron tool properties");
            let schema = properties.get("cron_job_id").expect("cron_job_id schema");
            assert!(!properties.contains_key("job_id"), "{name}");
            assert!(!properties.contains_key("id"), "{name}");
            assert_eq!(schema.get("type"), Some(&json!("string")), "{name}");
            assert!(schema.get("pattern").and_then(Value::as_str).is_some(), "{name}");
        }
    }
}
