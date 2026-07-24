//! Extended SCHEDULING / AUTONOMY gateway tools — surface service methods that
//! the base `caps_cron`, `caps_requirement`, `caps_autowork`, and `caps_idmm`
//! files do NOT already register.
//!
//! Each handler follows the established pattern: typed `*Params` (single source
//! of schema + deserialization), `(Arc<GatewayDeps>, CallerCtx, P) -> Value` or
//! `(Arc<GatewayDeps>, P) -> Value`, `crate::server::ok` for success, structured
//! `json!({"error":…)` on failure.

use std::sync::Arc;

use nomifun_api_types::IdmmTargetKind;
use nomifun_common::{CronJobId, ProviderId, RequirementId};
use nomifun_cron::types::cron_job_to_response;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::caps_idmm::{
    parse_target_id as parse_idmm_target_id, verify_target as verify_idmm_target,
};
use crate::deps::{CallerCtx, GatewayDeps};
use crate::id_schema::{CanonicalEntityId, SessionTargetKind};
use crate::registry::{Capability, CapabilityMeta, DangerTier, Surface};
use crate::server::ok;

// CRON DOMAIN (extensions)
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CronGetJobParams {
    /// The id of the cron job to retrieve (from nomi_cron_list).
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    cron_job_id: CronJobId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CronRunNowParams {
    /// The id of the cron job to trigger immediately.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    cron_job_id: CronJobId,
}

async fn cron_get_job(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: CronGetJobParams) -> Value {
    match deps
        .cron_service
        .get_job(ctx.user_id.as_str(), p.cron_job_id.as_str())
        .await
    {
        Ok(job) => ok(cron_job_to_response(&job)),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn cron_run_now(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: CronRunNowParams) -> Value {
    let Some(operation_id) = ctx.operation_id.as_deref() else {
        return json!({
            "error": "cron run-now requires a transport-authenticated operation identity"
        });
    };
    let operation_id = format!("gateway:{operation_id}");
    match deps
        .cron_service
        .run_now(
            ctx.user_id.as_str(),
            p.cron_job_id.as_str(),
            &operation_id,
        )
        .await
    {
        Ok(resp) => ok(json!({
            "triggered": true,
            "conversation_id": resp.conversation_id,
        })),
        Err(e) => json!({"error": e.to_string()}),
    }
}

// REQUIREMENT DOMAIN (extensions)
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RequirementGetParams {
    /// Stable business id of the requirement to fetch.
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    requirement_id: RequirementId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RequirementListTagsParams {
    // Intentionally empty — tags() takes no arguments.
    // A unit struct would also work, but an empty object is friendlier to
    // schema consumers.
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RequirementGetBoardParams {
    /// The tag whose kanban board to retrieve.
    tag: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RequirementResumeTagParams {
    /// The tag to resume (un-pause).
    tag: String,
    /// Re-queue ALL failed requirements in the tag back to pending (default false).
    #[serde(default)]
    requeue_failed: bool,
    /// Re-queue these specific failed requirement ids back to pending.
    #[serde(default)]
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_array_schema")]
    requeue_ids: Vec<RequirementId>,
}

async fn requirement_get(deps: Arc<GatewayDeps>, p: RequirementGetParams) -> Value {
    match deps
        .requirement_service
        .get(p.requirement_id.as_str())
        .await
    {
        Ok(req) => ok(req),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn requirement_list_tags(deps: Arc<GatewayDeps>, _p: RequirementListTagsParams) -> Value {
    match deps.requirement_service.tags().await {
        Ok(tags) => ok(tags),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn requirement_get_board(deps: Arc<GatewayDeps>, p: RequirementGetBoardParams) -> Value {
    match deps.requirement_service.board(&p.tag).await {
        Ok(board) => ok(board),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn requirement_resume_tag(deps: Arc<GatewayDeps>, p: RequirementResumeTagParams) -> Value {
    // Mirror the REST route: if `requeue_failed`, collect all failed ids from
    // the board and merge with explicit ids.
    let mut requeue_ids = p
        .requeue_ids
        .into_iter()
        .map(RequirementId::into_string)
        .collect::<Vec<_>>();
    if p.requeue_failed {
        match deps.requirement_service.board(&p.tag).await {
            Ok(board) => {
                requeue_ids.extend(board.failed.into_iter().map(|r| r.requirement_id));
            }
            Err(e) => return json!({"error": e.to_string()}),
        }
    }
    if let Err(e) = deps.requirement_service.resume_tag(&p.tag, &requeue_ids).await {
        return json!({"error": e.to_string()});
    }
    // Return the updated tag summary (same as the REST route).
    match deps.requirement_service.tags().await {
        Ok(tags) => {
            let summary = tags.into_iter().find(|t| t.tag == p.tag);
            ok(json!({
                "resumed": true,
                "requeued_count": requeue_ids.len(),
                "tag_summary": summary,
            }))
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

// IDMM DOMAIN (extensions)
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IdmmGetLogParams {
    /// Target kind: "conversation" or "terminal".
    kind: SessionTargetKind,
    /// The conversation id or terminal id to inspect.
    target_id: CanonicalEntityId,
    /// Maximum rows to return (default 50, clamped to 1..=500).
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IdmmGetActivityParams {
    /// Maximum rows to return (default 50, clamped to 1..=500).
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IdmmInterveneParams {
    /// Target kind: "conversation" or "terminal".
    kind: SessionTargetKind,
    /// The conversation id or terminal id to intervene on.
    target_id: CanonicalEntityId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IdmmGetSettingsParams {
    // No parameters — global settings.
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IdmmSetSettingsParams {
    /// Backup provider/model pair. Omit to leave unchanged.
    #[serde(default)]
    backup_model: Option<IdmmBackupModelParam>,
    /// Explicitly clear the backup pair.
    #[serde(default)]
    clear_backup_model: bool,
    /// Default steering prompt injected into new IDMM supervision configs.
    #[serde(default)]
    default_steering_prompt: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IdmmBackupModelParam {
    #[schemars(schema_with = "crate::id_schema::canonical_uuid_v7_schema")]
    provider_id: ProviderId,
    #[serde(deserialize_with = "deserialize_model_name")]
    model: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IdmmClearLogParams {
    /// Target kind: "conversation" or "terminal".
    kind: SessionTargetKind,
    /// The conversation id or terminal id whose log to clear.
    target_id: CanonicalEntityId,
}

// --- Helpers ---------------------------------------------------------------

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

// --- Handlers --------------------------------------------------------------

async fn idmm_get_log(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: IdmmGetLogParams) -> Value {
    let kind = IdmmTargetKind::from(p.kind);
    let target_id = match parse_idmm_target_id(kind, p.target_id.into_string()) {
        Ok(target_id) => target_id,
        Err(error) => return error,
    };
    if let Some(err) = verify_idmm_target(&deps, &ctx, kind, &target_id).await {
        return err;
    }
    let limit = p.limit.unwrap_or(50).clamp(1, 500);
    match deps.idmm_service.log(ctx.user_id.as_str(), kind, &target_id, limit).await {
        Ok(records) => ok(records),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn idmm_get_activity(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: IdmmGetActivityParams) -> Value {
    if nomifun_common::UserId::parse(ctx.user_id.as_str()).is_err() {
        return json!({"error": "missing caller user identity"});
    }
    let limit = p.limit.unwrap_or(50).clamp(1, 500);
    match deps.idmm_service.recent_activity(ctx.user_id.as_str(), limit).await {
        Ok(records) => ok(records),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn idmm_intervene(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: IdmmInterveneParams) -> Value {
    let kind = IdmmTargetKind::from(p.kind);
    let target_id = match parse_idmm_target_id(kind, p.target_id.into_string()) {
        Ok(target_id) => target_id,
        Err(error) => return error,
    };
    if let Some(err) = verify_idmm_target(&deps, &ctx, kind, &target_id).await {
        return err;
    }
    match deps
        .idmm_service
        .intervene_now(ctx.user_id.as_str(), kind, &target_id)
        .await
    {
        Ok(()) => {
            // Return the updated state (same as the REST route).
            match deps
                .idmm_service
                .build_state(ctx.user_id.as_str(), kind, &target_id)
                .await
            {
                Ok(state) => ok(json!({
                    "intervened": true,
                    "state": state,
                })),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn idmm_get_settings(deps: Arc<GatewayDeps>, _p: IdmmGetSettingsParams) -> Value {
    match deps.idmm_service.get_settings().await {
        Ok(settings) => ok(settings),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn idmm_set_settings(deps: Arc<GatewayDeps>, p: IdmmSetSettingsParams) -> Value {
    // Read current settings and overlay provided fields (same partial-update
    // semantics as the REST route).
    let mut settings = match deps.idmm_service.get_settings().await {
        Ok(s) => s,
        Err(e) => return json!({"error": e.to_string()}),
    };
    if p.clear_backup_model && p.backup_model.is_some() {
        return json!({"error":"backup_model and clear_backup_model cannot both be set"});
    }
    if p.clear_backup_model {
        settings.backup_provider_id = None;
        settings.backup_model = None;
    } else if let Some(backup) = p.backup_model {
        settings.backup_provider_id = Some(backup.provider_id.into_string());
        settings.backup_model = Some(backup.model);
    }
    if let Some(prompt) = p.default_steering_prompt {
        settings.default_steering_prompt = prompt;
    }

    match deps.idmm_service.set_settings(&settings).await {
        Ok(()) => ok(settings),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn idmm_clear_log(deps: Arc<GatewayDeps>, ctx: CallerCtx, p: IdmmClearLogParams) -> Value {
    let kind = IdmmTargetKind::from(p.kind);
    let target_id = match parse_idmm_target_id(kind, p.target_id.into_string()) {
        Ok(target_id) => target_id,
        Err(error) => return error,
    };
    if let Some(err) = verify_idmm_target(&deps, &ctx, kind, &target_id).await {
        return err;
    }
    match deps.idmm_service.clear_log(ctx.user_id.as_str(), kind, &target_id).await {
        Ok(count) => json!({"result": format!("cleared {count} intervention records")}),
        Err(e) => json!({"error": e.to_string()}),
    }
}

// REGISTRATION
/// Register the scheduling/autonomy extension capabilities.
pub(crate) fn register(out: &mut Vec<Capability>) {
    // -- Cron extensions --------------------------------------------------
    out.push(Capability::new::<CronGetJobParams, _, _>(
        CapabilityMeta::new(
            "nomi_cron_get_job",
            "cron",
            "Get a single cron job by id (full detail including schedule, next/last run, error).",
            DangerTier::Read,
        ),
        cron_get_job,
    ));
    out.push(Capability::new::<CronRunNowParams, _, _>(
        CapabilityMeta::new(
            "nomi_cron_run_now",
            "cron",
            "Trigger a cron job to execute immediately (out-of-schedule one-shot run).",
            DangerTier::Write,
        ),
        cron_run_now,
    ));

    // -- Requirement extensions -------------------------------------------
    out.push(Capability::new::<RequirementGetParams, _, _>(
        CapabilityMeta::new(
            "nomi_requirement_get",
            "requirement",
            "Fetch a single requirement by requirement_id (full detail including attachments, timestamps, status).",
            DangerTier::Read,
        )
        .instance_owner(),
        |deps, _ctx, p| requirement_get(deps, p),
    ));
    out.push(Capability::new::<RequirementListTagsParams, _, _>(
        CapabilityMeta::new(
            "nomi_requirement_list_tags",
            "requirement",
            "List all AutoWork tags with per-status counts, paused state, and totals.",
            DangerTier::Read,
        )
        .instance_owner(),
        |deps, _ctx, p| requirement_list_tags(deps, p),
    ));
    out.push(Capability::new::<RequirementGetBoardParams, _, _>(
        CapabilityMeta::new(
            "nomi_requirement_get_board",
            "requirement",
            "Get the kanban board view for a tag (requirements grouped by status column).",
            DangerTier::Read,
        )
        .instance_owner(),
        |deps, _ctx, p| requirement_get_board(deps, p),
    ));
    out.push(Capability::new::<RequirementResumeTagParams, _, _>(
        CapabilityMeta::new(
            "nomi_requirement_resume_tag",
            "requirement",
            "Resume a paused AutoWork tag and optionally re-queue failed requirements back to pending.",
            DangerTier::Write,
        )
        .instance_owner(),
        |deps, _ctx, p| requirement_resume_tag(deps, p),
    ));

    // -- IDMM extensions --------------------------------------------------
    out.push(Capability::new::<IdmmGetLogParams, _, _>(
        CapabilityMeta::new(
            "nomi_idmm_get_log",
            "idmm",
            "Read the persisted intervention log for a conversation or terminal (most-recent-first).",
            DangerTier::Read,
        ),
        idmm_get_log,
    ));
    out.push(Capability::new::<IdmmGetActivityParams, _, _>(
        CapabilityMeta::new(
            "nomi_idmm_get_activity",
            "idmm",
            "Read the caller's cross-session intervention feed (their targets only, most-recent-first).",
            DangerTier::Read,
        ),
        idmm_get_activity,
    ));
    out.push(Capability::new::<IdmmInterveneParams, _, _>(
        CapabilityMeta::new(
            "nomi_idmm_intervene",
            "idmm",
            "Force one IDMM supervision pass now (manual 'act now') and return the resulting state.",
            DangerTier::Write,
        ),
        idmm_intervene,
    ));
    out.push(Capability::new::<IdmmGetSettingsParams, _, _>(
        CapabilityMeta::new(
            "nomi_idmm_get_settings",
            "idmm",
            "Read global IDMM settings (backup provider/model, default steering prompt).",
            DangerTier::Read,
        )
        .instance_owner(),
        |deps, _ctx, p| idmm_get_settings(deps, p),
    ));
    out.push(Capability::new::<IdmmSetSettingsParams, _, _>(
        CapabilityMeta::new(
            "nomi_idmm_set_settings",
            "idmm",
            "Update global IDMM settings (backup provider/model, default steering prompt). Partial update: omitted fields keep their current value.",
            DangerTier::Sensitive,
        )
        .instance_owner(),
        |deps, _ctx, p| idmm_set_settings(deps, p),
    ));
    out.push(Capability::new::<IdmmClearLogParams, _, _>(
        CapabilityMeta::new(
            "nomi_idmm_clear_log",
            "idmm",
            "Clear all persisted intervention records for a conversation or terminal. Irreversible.",
            DangerTier::Destructive,
        )
        .deny_on(&[Surface::Channel]),
        idmm_clear_log,
    ));
}

// --- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::InterventionRecord;
    use nomifun_common::IdmmInterventionId;
    use serde_json::json;

    const REQUIREMENT_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";

    #[test]
    fn requirement_get_uses_named_wire_field() {
        let params: RequirementGetParams =
            serde_json::from_value(json!({"requirement_id": REQUIREMENT_ID}))
                .expect("requirement_id should deserialize");
        assert_eq!(params.requirement_id.as_str(), REQUIREMENT_ID);

        assert!(
            serde_json::from_value::<RequirementGetParams>(json!({"id": REQUIREMENT_ID}))
                .is_err(),
            "the generic id wire field must not be accepted"
        );
        assert!(
            serde_json::from_value::<RequirementGetParams>(json!({
                "id": REQUIREMENT_ID,
                "requirement_id": REQUIREMENT_ID
            }))
            .is_err(),
            "legacy and canonical fields must not coexist"
        );
    }

    #[test]
    fn requirement_get_schema_uses_named_wire_field() {
        let schema = serde_json::to_value(schemars::schema_for!(RequirementGetParams))
            .expect("requirement get schema should serialize");
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("requirement get schema should have properties");

        assert!(properties.contains_key("requirement_id"));
        assert!(!properties.contains_key("id"));
        assert!(
            schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| required.iter().any(|field| field == "requirement_id")),
            "requirement_id must be required"
        );
    }

    #[test]
    fn idmm_gateway_result_uses_named_intervention_id() {
        let intervention_id = IdmmInterventionId::new();
        let result = ok(InterventionRecord {
            intervention_id: intervention_id.clone(),
            target_kind: "conversation".into(),
            target_id: "0190f5fe-7c00-7a00-8000-000000000001".into(),
            watch: "decision".into(),
            at: 1,
            stall_class: "decision".into(),
            tier_used: "rule".into(),
            category: None,
            action: "wait".into(),
            detail: None,
            outcome: "skipped".into(),
            reason: None,
            confidence: None,
            bypass_model: None,
        });
        assert_eq!(
            result["result"]["intervention_id"],
            intervention_id.as_str()
        );
        assert!(result["result"].get("id").is_none());
    }
}
