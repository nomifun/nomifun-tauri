//! 智能编排 (orchestration) domain capabilities (registry form): create an
//! orchestration run from a goal + fleet, inspect its task DAG status, and read
//! the aggregated result once the run completes.
//!
//! Backed by:
//! - `nomifun_orchestrator::RunService` — the run control-plane
//!   (`create` snapshots the fleet + parks in `planning`; `plan` decomposes the
//!   goal into a task DAG + assignments + flips to `running`; `get_detail` reads
//!   the run + tasks + deps + assignments).
//! - `nomifun_orchestrator::RunEngine` — the serial execution loop; `start`
//!   spawns (or restarts) the loop that drives ready tasks to completion.
//!
//! `nomi_run_create` performs the create → plan → (conditionally) start
//! choreography so a single tool call sets up a run from the calling conversation.
//! As of the conversation-native redesign (P1) it takes ONLY `{goal, autonomy?}`
//! and pulls everything else — `work_dir`, `model_range`, `lead_conv_id` — from
//! the CALLING conversation's `extra` (the "orchestration lead" context), then
//! drives the workspace-less
//! [`create_adhoc`](nomifun_orchestrator::RunService::create_adhoc) path. As of
//! P6 Task 1 the lead path defaults autonomy to **`interactive`**: the run parks
//! at `awaiting_plan_approval` and the engine is NOT started until the user
//! approves the plan (the tool returns that status + a relay message for the
//! 主管). Other autonomy levels start immediately. The two read tools project the
//! rich `RunDetail` down to a compact, LLM-friendly shape (run status + per-task
//! title/status, and on result the per-task `output_summary`).
//!
//! ## `ModelRange::Auto` expansion (Task 3 decision)
//! `RunService::create_adhoc` rejects an unexpanded `Auto` — it has no provider
//! access (its struct holds only run/fleet/ws repos + a planner + an emitter). The
//! gateway DOES (`GatewayDeps::provider_repo`, surfaced via
//! [`load_provider_summaries`](crate::tools_provider::load_provider_summaries),
//! already filtered to enabled providers × enabled models). So we expand `Auto`
//! → a concrete `Range` of every enabled `(provider, model)` pair HERE, in the
//! caps layer, before calling `create_adhoc`. `Single`/`Range` pass through
//! verbatim.

use std::sync::Arc;

use nomifun_api_types::{
    CreateAdhocRunRequest, FleetMember, ModelRange, ModelRef, RunDetail, UpdateConversationRequest,
    derive_capability,
};
use nomifun_common::generate_prefixed_id;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::GatewayDeps;
use crate::registry::{Capability, CapabilityMeta, DangerTier, Surface};
use crate::server::{ok, require_user};
use crate::tools_provider::{ProviderSummary, load_provider_summaries};

/// Orchestration is a DESKTOP master-agent feature: the "lead" conversation that
/// drives a run lives on a local trusted desktop session (it carries the
/// `model_range` / `work_dir` context `nomi_run_create` reads). External callers
/// (the network Remote front door, `Surface::Remote`) are NOT orchestration leads
/// and must not create or inspect runs — `nomi_run_status` / `nomi_run_result`
/// take a bare `run_id` with no ownership predicate, so advertising/dispatching
/// them externally would let one companion's token read ANY run's status/output.
/// Hard-deny the whole domain on Remote so it is neither advertised (filtered out
/// of `tool_specs`) NOR dispatchable (a guessed call is Denied, not just hidden),
/// while staying fully available on the trusted Desktop surface where the lead
/// runs. (Mirrors the per-surface `deny_on` curation used elsewhere.)
const ORCHESTRATOR_DENY_SURFACES: &[Surface] = &[Surface::Remote];

// ── param structs (single source: schema + runtime) ──────────────────────

/// Create and kick off an orchestration run from the calling conversation's
/// context. The conversation must be an orchestration "lead" (its `extra` carries
/// a `model_range`); `work_dir` / `lead_conv_id` / `model_range` are read from
/// there, so the tool only needs the goal (and, optionally, an autonomy override).
#[derive(Deserialize, JsonSchema)]
struct RunCreateParams {
    /// The high-level goal to decompose into tasks and execute.
    goal: String,
    /// Autonomy mode: "interactive" (the default for conversation-native runs —
    /// the run parks at `awaiting_plan_approval` for the user to approve the plan
    /// in the 编排面板 before any worker runs), "supervised", or "autonomous".
    /// Omit for the interactive default.
    #[serde(default)]
    autonomy: Option<String>,
}

/// Inspect a run's current status and the status of each of its tasks.
#[derive(Deserialize, JsonSchema)]
struct RunStatusParams {
    /// The run id (from nomi_run_create).
    run_id: String,
}

/// Read a run's aggregated result: the run summary and each task's output
/// summary. While the run is still executing, `status` reflects that.
#[derive(Deserialize, JsonSchema)]
struct RunResultParams {
    /// The run id (from nomi_run_create).
    run_id: String,
}

// ── handlers ──────────────────────────────────────────────────────────────

async fn create(deps: Arc<GatewayDeps>, ctx: crate::deps::CallerCtx, p: RunCreateParams) -> Value {
    let user = match require_user(&ctx) {
        Ok(u) => u.to_owned(),
        Err(e) => return e,
    };
    if ctx.conversation_id.is_empty() {
        return json!({ "error": "missing caller conversation identity (NOMI_GW_MCP_CONVERSATION_ID)" });
    }

    // 1. Read the calling ("lead") conversation's context.
    let conv = match deps.conversation_service.get(&user, &ctx.conversation_id).await {
        Ok(c) => c,
        Err(e) => return json!({ "error": e.to_string() }),
    };
    let (work_dir, model_range) = match parse_lead_extra(&conv.extra) {
        Ok(pair) => pair,
        Err(e) => return e,
    };

    // 2. Load provider summaries once: needed to (a) expand `Auto` to a concrete
    //    `Range`, (b) map an assistant's preferred model NAME → a (provider_id,
    //    model) within the run's range, and (c) fill `description` on both the
    //    assistant-backed AND the bare model members. (Cheap: one provider list.)
    let summaries = match load_provider_summaries(&deps).await {
        Ok(s) => s,
        Err(e) => return e,
    };

    // Expand `Auto` to a concrete `Range` (RunService::create_adhoc rejects an
    // unexpanded Auto). Single/Range pass through unchanged.
    let model_range = if matches!(model_range, ModelRange::Auto) {
        match expand_auto_range(&summaries) {
            Ok(r) => r,
            Err(e) => return e,
        }
    } else {
        model_range
    };

    // The concrete (provider_id, model) pairs this run may execute over. An
    // assistant whose preferred models are all OUTSIDE this set is skipped (we
    // never force a model on a run); a member's description is looked up here too.
    let range_pairs = range_pairs(&model_range);

    // 3. Build the assistant-backed role members: for each ENABLED assistant whose
    //    preferred model falls in range, fold its persona (read_rule, fail-soft) /
    //    skills / model into an enriched FleetMember. Fail-soft on a list error —
    //    a run with just the bare model members is still valid.
    let role_members = build_assistant_members(&deps, &summaries, &range_pairs).await;

    let lead_conv_id = ctx.conversation_id.parse::<i64>().ok();
    let req = CreateAdhocRunRequest {
        goal: p.goal,
        work_dir,
        model_range,
        pinned_roles: vec![],
        role_members,
        // Conversation-native lead runs default to `interactive` (P6 Task 1): the
        // 主管 proposes the team + DAG and the run parks at
        // `awaiting_plan_approval` for the user to approve in the 编排面板. An
        // explicit autonomy arg overrides this; `create_adhoc`'s own default
        // (supervised) is NOT used here.
        autonomy: Some(lead_autonomy(p.autonomy)),
        // Serial loop (P1): parallelism is not yet a gateway-exposed knob.
        max_parallel: None,
        lead_conv_id,
    };

    // 3. Create: synthesize the fleet from the model range + park in `planning`.
    let run = match deps.orchestrator_run_service.create_adhoc(&user, req).await {
        Ok(run) => run,
        Err(e) => return json!({ "error": e.to_string() }),
    };
    // 4. Plan: decompose the goal → task DAG + assignments, then apply the
    //    autonomy gate. An `interactive` run (the conversation-native default)
    //    parks at `awaiting_plan_approval`; every other level flips to `running`.
    if let Err(e) = deps.orchestrator_run_service.plan(&run.id).await {
        return json!({ "error": format!("run {} created but planning failed: {e}", run.id) });
    }

    // 5. Read the post-plan detail ONCE: it tells us the resulting status (did the
    //    autonomy gate park the run?) and the planned task count (for the relay
    //    message). The run exists (we just created + planned it); a read error is
    //    non-fatal — we fall back to the create-time status and an empty task list.
    let (status, task_count) = match deps.orchestrator_run_service.get_detail(&run.id).await {
        Ok(detail) => (detail.run.status, detail.tasks.len()),
        Err(_) => (run.status.clone(), 0),
    };
    let awaiting = is_awaiting_approval(&status);

    // 6. Start the execution loop ONLY when the run is not awaiting approval. An
    //    `interactive` run must NOT auto-start — it waits for the user to approve
    //    the plan in the 编排面板 (the `approve` route then starts the engine).
    //    All other autonomy levels start immediately (idempotent; restarts any
    //    existing loop).
    if !awaiting {
        deps.orchestrator_run_engine.start(run.id.clone());
    }

    // 7. Write the run id back into the lead conversation's `extra` so the
    //    frontend DAG can locate this run later (P2). `ConversationService::update`
    //    MERGES `extra` (top-level keys overwritten, others preserved), so this
    //    does not clobber `workspace` / `model_range` / etc. Best-effort: a
    //    write-back failure is logged but does not fail the (already-created) run.
    let update = UpdateConversationRequest {
        name: None,
        pinned: None,
        model: None,
        extra: Some(json!({ "orchestrator_run_id": run.id })),
    };
    if let Err(e) = deps
        .conversation_service
        .update(&user, &ctx.conversation_id, update, &deps.task_manager)
        .await
    {
        tracing::warn!(
            run_id = %run.id,
            lead_conv_id = %ctx.conversation_id,
            error = %e,
            "failed to write orchestrator_run_id back to lead conversation extra"
        );
    }

    // 8. Return. When the run parked at `awaiting_plan_approval`, include a
    //    `message` instructing the 主管 to relay to the user that a team for
    //    `task_count` subtasks was drafted and is pending approval in the 编排面板.
    //    Otherwise (the run is running) return the bare run id + status.
    if awaiting {
        ok(json!({
            "run_id": run.id,
            "status": status,
            "task_count": task_count,
            "message": awaiting_plan_message(task_count),
        }))
    } else {
        ok(json!({ "run_id": run.id, "status": status }))
    }
}

// ── lead-conversation context parsing + Auto expansion ────────────────────

/// Read the run's `work_dir` + `model_range` out of a lead conversation's `extra`.
///
/// - `work_dir` ← `extra.workspace` (string, optional → `None` when absent/empty).
/// - `model_range` ← `extra.model_range` (the tagged [`ModelRange`] JSON). Absent
///   or unparseable ⇒ a clear error: this conversation is not an orchestration
///   lead (it never picked a model range), so it cannot drive a run.
///
/// `Auto` is returned verbatim here — its expansion to a concrete `Range` needs
/// provider access and happens in [`expand_auto_range`] at the handler.
fn parse_lead_extra(extra: &Value) -> Result<(Option<String>, ModelRange), Value> {
    let work_dir = extra
        .get("workspace")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let model_range: ModelRange = match extra.get("model_range") {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
            json!({
                "error": format!("this conversation's model_range is malformed ({e}); it cannot drive an orchestration run")
            })
        })?,
        None => {
            return Err(json!({
                "error": "this conversation is not an orchestration lead: it has no model_range in its context. Start the run from a conversation configured with a model range (single / range / auto)."
            }));
        }
    };
    Ok((work_dir, model_range))
}

/// Expand `ModelRange::Auto` into a concrete `Range` of every ENABLED provider ×
/// its enabled models (the summaries are already `model_enabled`-filtered). An
/// empty result (no provider/model configured) is a clear error rather than an
/// empty run.
fn expand_auto_range(summaries: &[ProviderSummary]) -> Result<ModelRange, Value> {
    let models: Vec<ModelRef> = summaries
        .iter()
        .filter(|p| p.enabled)
        .flat_map(|p| {
            p.models.iter().map(move |m| ModelRef {
                provider_id: p.id.clone(),
                model: m.clone(),
            })
        })
        .collect();
    if models.is_empty() {
        return Err(json!({
            "error": "auto model range selected, but no provider/model is enabled on this desktop. Configure one in Settings → Providers (or pick a concrete model range) before starting a run."
        }));
    }
    Ok(ModelRange::Range { models })
}

// ── interactive default + awaiting-approval relay (P6 Task 1) ──────────────

/// The autonomy a conversation-native ("lead") run uses. Decision (P6 Task 1):
/// multi-agent runs created from a conversation default to **`interactive`** —
/// the 主管 proposes the team + task DAG and the run parks at
/// `awaiting_plan_approval` for the user to approve in the 编排面板 before any
/// worker dispatches. An EXPLICIT autonomy override (a non-blank tool arg) is
/// honored verbatim (trimmed); an omitted or blank value falls back to the
/// interactive default.
///
/// NOTE: this is the LEAD-path default only. `RunService::create_adhoc`'s own
/// `DEFAULT_AUTONOMY` stays `"supervised"` (other callers / the e2e rely on it);
/// the interactive default is applied HERE, at the caps layer, so only the
/// conversation-native lead path goes interactive by default.
fn lead_autonomy(autonomy: Option<String>) -> String {
    autonomy
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| "interactive".to_string())
}

/// Whether a run status means "parked, waiting for the user to approve the plan".
/// The choreography must NOT `engine.start` such a run — it waits for `approve`.
fn is_awaiting_approval(status: &str) -> bool {
    status == "awaiting_plan_approval"
}

/// The 主管-facing relay message for a run that parked at `awaiting_plan_approval`:
/// it instructs the lead LLM to tell the user that a team for `task_count`
/// subtasks was drafted and is pending approval in the 编排面板. The concrete
/// count is interpolated so the LLM relays the real number.
fn awaiting_plan_message(task_count: usize) -> String {
    format!(
        "已拟定 {task_count} 个子任务的团队，待你在编排面板批准后开始执行。请把这一情况转达给用户，并等待其批准。"
    )
}

// ── assistant → role member resolution (P4 Task 2) ─────────────────────────

/// The set of concrete `(provider_id, model)` pairs a run may execute over,
/// extracted from the (already-expanded) `Single`/`Range` model range. An
/// assistant whose preferred model is not one of these is skipped.
fn range_pairs(range: &ModelRange) -> Vec<(String, String)> {
    match range {
        ModelRange::Single { model } => vec![(model.provider_id.clone(), model.model.clone())],
        ModelRange::Range { models } => models
            .iter()
            .map(|m| (m.provider_id.clone(), m.model.clone()))
            .collect(),
        // Auto is expanded before this is called; treat as empty defensively.
        ModelRange::Auto => Vec::new(),
    }
}

/// The minimal assistant data the role-member builder needs (decoupled from the
/// async `AssistantService` so the build logic is pure + unit-testable).
struct AssistantData {
    id: String,
    name: String,
    description: Option<String>,
    /// The assistant's preferred model NAMES, in priority order.
    models: Vec<String>,
    enabled_skills: Vec<String>,
    disabled_builtin_skills: Vec<String>,
    audience_tags: Vec<String>,
    scenario_tags: Vec<String>,
    /// Persona/rule text (already read server-side via `read_rule`); empty → None.
    persona: String,
}

/// Resolve an assistant's preferred model to the FIRST `(provider_id, model)`
/// that is BOTH (a) one of the assistant's preferred model names and (b) present
/// in the run's range. Returns `None` when the assistant has no model in range —
/// the caller SKIPS it (we never force a model on a run).
///
/// `range_pairs` is the run's concrete pairs (provider_id, model). A model NAME
/// can map to several providers; we honor the assistant's priority order, and
/// for a given preferred name pick the first range pair that uses it.
fn resolve_assistant_model(
    preferred_models: &[String],
    range_pairs: &[(String, String)],
) -> Option<(String, String)> {
    for want in preferred_models {
        if let Some(pair) = range_pairs.iter().find(|(_, model)| model == want) {
            return Some(pair.clone());
        }
    }
    None
}

/// Build one enriched [`FleetMember`] from an assistant + its resolved in-range
/// model. Folds the persona (fail-soft → `None` on empty), skills, description,
/// and a conservative derived capability profile into the snapshot member so the
/// orchestrator worker (Task 3) reads everything from the snapshot with no
/// assistant-crate dependency.
fn derive_role_member(a: &AssistantData, provider_id: String, model: String) -> FleetMember {
    let persona = a.persona.trim();
    FleetMember {
        id: generate_prefixed_id("rmbr"),
        agent_id: a.id.clone(),
        provider_id: Some(provider_id),
        model: Some(model),
        role_hint: Some(a.name.clone()),
        capability_profile: Some(derive_capability(
            &a.audience_tags,
            &a.scenario_tags,
            a.description.as_deref(),
            !a.enabled_skills.is_empty(),
        )),
        constraints: None,
        // Re-densified by the merge in `create_adhoc`; a placeholder here.
        sort_order: 0,
        description: a.description.clone(),
        system_prompt: if persona.is_empty() { None } else { Some(persona.to_string()) },
        enabled_skills: a.enabled_skills.clone(),
        disabled_builtin_skills: a.disabled_builtin_skills.clone(),
    }
}

/// Pure core: turn the ENABLED assistants into enriched role members, skipping
/// any whose preferred models are all out of the run's range. Unit-tested
/// directly; the async wrapper supplies the assistant list + personas.
fn build_role_members_from_assistants(
    assistants: &[AssistantData],
    range_pairs: &[(String, String)],
) -> Vec<FleetMember> {
    assistants
        .iter()
        .filter_map(|a| {
            let (provider_id, model) = resolve_assistant_model(&a.models, range_pairs)?;
            Some(derive_role_member(a, provider_id, model))
        })
        .collect()
}

/// Async wrapper: list the ENABLED assistants, read each one's persona
/// (`read_rule`, default locale, fail-soft → empty), and build the role members.
///
/// Also emits "description decorations" for the bare model-range members: a
/// bare member (empty `agent_id`) carrying the model's user-authored
/// `description` for each range pair that has one. The `create_adhoc` merge puts
/// role members first + dedups by `(provider, model, agent_id)`, so each
/// decoration WINS over the plain range-built member with the same key — this is
/// how the bare members get descriptions for the planner WITHOUT duplicating
/// routing targets (P3 still works: it reads descriptions from the provider rows,
/// and `member.description` is purely additive).
///
/// **Fail-soft on a list error** — descriptions/personas are an enrichment, not a
/// hard requirement; a run with just the bare model members is still valid. A
/// `read_rule` error for a single assistant degrades that assistant's persona to
/// empty (`None` system_prompt), never failing the whole build.
async fn build_assistant_members(
    deps: &GatewayDeps,
    summaries: &[ProviderSummary],
    range_pairs: &[(String, String)],
) -> Vec<FleetMember> {
    // Description decorations for the bare model members, derived from the
    // providers' user-authored model_descriptions. Only emitted for range pairs
    // that actually carry a non-blank description.
    let mut out: Vec<FleetMember> = range_pairs
        .iter()
        .filter_map(|(pid, model)| {
            let desc = summaries
                .iter()
                .find(|p| &p.id == pid)
                .and_then(|p| p.model_descriptions.get(model))
                .map(|d| d.trim())
                .filter(|d| !d.is_empty())?;
            Some(FleetMember {
                id: generate_prefixed_id("rmbr"),
                agent_id: String::new(),
                provider_id: Some(pid.clone()),
                model: Some(model.clone()),
                role_hint: None,
                capability_profile: None,
                constraints: None,
                sort_order: 0,
                description: Some(desc.to_string()),
                system_prompt: None,
                enabled_skills: Vec::new(),
                disabled_builtin_skills: Vec::new(),
            })
        })
        .collect();

    let responses = match deps.assistant_service.list().await {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!(error = %e, "failed to list assistants for orchestration role members; using bare model members only");
            return out;
        }
    };

    let mut data: Vec<AssistantData> = Vec::new();
    for r in responses.into_iter().filter(|r| r.enabled) {
        // Read the persona server-side (default locale → None). Fail-soft.
        let persona = deps
            .assistant_service
            .read_rule(&r.id, None)
            .await
            .unwrap_or_default();
        data.push(AssistantData {
            id: r.id,
            name: r.name,
            description: r.description,
            models: r.models,
            enabled_skills: r.enabled_skills,
            disabled_builtin_skills: r.disabled_builtin_skills,
            audience_tags: r.audience_tags,
            scenario_tags: r.scenario_tags,
            persona,
        });
    }

    out.extend(build_role_members_from_assistants(&data, range_pairs));
    out
}

async fn status(deps: Arc<GatewayDeps>, p: RunStatusParams) -> Value {
    match deps.orchestrator_run_service.get_detail(&p.run_id).await {
        Ok(detail) => ok(project_status(&detail)),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn result(deps: Arc<GatewayDeps>, p: RunResultParams) -> Value {
    match deps.orchestrator_run_service.get_detail(&p.run_id).await {
        Ok(detail) => ok(project_result(&detail)),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── result projections (RunDetail → compact LLM-friendly shape) ───────────

/// Run status + per-task {id, title, status}.
fn project_status(detail: &RunDetail) -> Value {
    json!({
        "run_id": detail.run.id,
        "status": detail.run.status,
        "tasks": detail
            .tasks
            .iter()
            .map(|t| json!({ "id": t.id, "title": t.title, "status": t.status }))
            .collect::<Vec<_>>(),
    })
}

/// Run status + summary + per-task {title, output_summary}. When the run is not
/// yet terminal, `status` reflects the in-flight state (e.g. "running"); the
/// summary / output fields are simply whatever has been persisted so far.
fn project_result(detail: &RunDetail) -> Value {
    json!({
        "run_id": detail.run.id,
        "status": detail.run.status,
        "summary": detail.run.summary,
        "tasks": detail
            .tasks
            .iter()
            .map(|t| json!({ "title": t.title, "output_summary": t.output_summary }))
            .collect::<Vec<_>>(),
    })
}

// ── registration ─────────────────────────────────────────────────────────

/// Register the orchestration-domain capabilities.
pub(crate) fn register(out: &mut Vec<Capability>) {
    // 1. Create + kick off a run (write). Desktop-only: deny on Remote (an
    //    external caller is never an orchestration lead).
    out.push(Capability::new::<RunCreateParams, _, _>(
        CapabilityMeta::new(
            "nomi_run_create",
            "orchestrator",
            "Create an orchestration run from THIS conversation's context: decompose the goal into a task DAG over the conversation's chosen model range and propose a team to execute it. Only works in an orchestration-lead conversation (one with a model_range). Defaults to interactive: the run parks awaiting your plan approval in the 编排面板 (returns status `awaiting_plan_approval` + a message to relay to the user) rather than auto-running. Returns the run id and status.",
            DangerTier::Write,
        )
        .deny_on(ORCHESTRATOR_DENY_SURFACES),
        |deps, ctx, p| create(deps, ctx, p),
    ));

    // 2. Run status (read). Desktop-only: deny on Remote — the read takes a bare
    //    run_id with no ownership predicate, so it must not be reachable externally.
    out.push(Capability::new::<RunStatusParams, _, _>(
        CapabilityMeta::new(
            "nomi_run_status",
            "orchestrator",
            "Get an orchestration run's current status and each task's id, title, and status.",
            DangerTier::Read,
        )
        .deny_on(ORCHESTRATOR_DENY_SURFACES),
        |deps, _ctx, p| status(deps, p),
    ));

    // 3. Run result (read). Desktop-only: deny on Remote (same bare-run_id reason).
    out.push(Capability::new::<RunResultParams, _, _>(
        CapabilityMeta::new(
            "nomi_run_result",
            "orchestrator",
            "Read an orchestration run's aggregated result: the run summary and each task's output summary. While still running, status reflects the in-flight state.",
            DangerTier::Read,
        )
        .deny_on(ORCHESTRATOR_DENY_SURFACES),
        |deps, _ctx, p| result(deps, p),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Registry, Surface};

    fn summary(id: &str, enabled: bool, models: &[&str]) -> ProviderSummary {
        ProviderSummary {
            id: id.to_owned(),
            name: format!("name-{id}"),
            platform: "openai".to_owned(),
            enabled,
            models: models.iter().map(|m| m.to_string()).collect(),
            model_descriptions: std::collections::HashMap::new(),
        }
    }

    // ── parse_lead_extra: reads work_dir + model_range from a lead conv's extra ──

    #[test]
    fn parse_lead_extra_reads_workspace_and_range() {
        let extra = json!({
            "workspace": "/x/proj",
            "model_range": {"mode": "range", "models": [
                {"provider_id": "p1", "model": "m1"},
                {"provider_id": "p2", "model": "m2"}
            ]}
        });
        let (work_dir, range) = parse_lead_extra(&extra).expect("parses");
        assert_eq!(work_dir.as_deref(), Some("/x/proj"));
        match range {
            ModelRange::Range { models } => {
                assert_eq!(models.len(), 2);
                assert_eq!(models[0].provider_id, "p1");
                assert_eq!(models[1].model, "m2");
            }
            other => panic!("expected range, got {other:?}"),
        }
    }

    #[test]
    fn parse_lead_extra_single_range_and_no_workspace() {
        // No `workspace` key → work_dir None; single model range parses.
        let extra = json!({
            "model_range": {"mode": "single", "model": {"provider_id": "ps", "model": "ms"}}
        });
        let (work_dir, range) = parse_lead_extra(&extra).expect("parses");
        assert!(work_dir.is_none(), "absent workspace → None");
        assert!(matches!(range, ModelRange::Single { .. }));
    }

    #[test]
    fn parse_lead_extra_blank_workspace_is_none() {
        let extra = json!({
            "workspace": "   ",
            "model_range": {"mode": "auto"}
        });
        let (work_dir, range) = parse_lead_extra(&extra).expect("parses");
        assert!(work_dir.is_none(), "blank workspace → None");
        assert!(matches!(range, ModelRange::Auto), "auto returned verbatim");
    }

    #[test]
    fn parse_lead_extra_missing_model_range_is_clean_error() {
        // A conversation that never picked a model range is not a lead → clean error.
        let extra = json!({ "workspace": "/x" });
        let err = parse_lead_extra(&extra).expect_err("must error without model_range");
        let msg = err["error"].as_str().unwrap_or("");
        assert!(
            msg.contains("not an orchestration lead"),
            "error must explain the conversation is not a lead, got: {msg}"
        );
    }

    #[test]
    fn parse_lead_extra_malformed_model_range_is_clean_error() {
        // Present but unparseable (bad tag) → a clear "malformed" error, not a panic.
        let extra = json!({ "model_range": {"mode": "nonsense"} });
        let err = parse_lead_extra(&extra).expect_err("must error on malformed range");
        let msg = err["error"].as_str().unwrap_or("");
        assert!(msg.contains("malformed"), "got: {msg}");
    }

    // ── expand_auto_range: Auto → concrete Range of enabled (provider, model) ──

    #[test]
    fn expand_auto_lists_enabled_models() {
        let summaries = vec![
            summary("p1", true, &["a", "b"]),
            summary("off", false, &["x"]), // disabled provider excluded
            summary("p2", true, &["c"]),
        ];
        let range = expand_auto_range(&summaries).expect("expands");
        match range {
            ModelRange::Range { models } => {
                // p1×{a,b} + p2×{c} = 3 pairs; the disabled provider is excluded.
                assert_eq!(models.len(), 3, "two enabled providers' models only");
                let pairs: Vec<(&str, &str)> = models
                    .iter()
                    .map(|m| (m.provider_id.as_str(), m.model.as_str()))
                    .collect();
                assert!(pairs.contains(&("p1", "a")));
                assert!(pairs.contains(&("p1", "b")));
                assert!(pairs.contains(&("p2", "c")));
                assert!(!pairs.iter().any(|(p, _)| *p == "off"), "disabled excluded");
            }
            other => panic!("expected range, got {other:?}"),
        }
    }

    #[test]
    fn expand_auto_empty_is_clean_error() {
        // Only a disabled provider (and an enabled-but-model-less one) → no models.
        let summaries = vec![summary("off", false, &["a"]), summary("empty", true, &[])];
        let err = expand_auto_range(&summaries).expect_err("must error with no enabled models");
        let msg = err["error"].as_str().unwrap_or("");
        assert!(msg.contains("no provider/model is enabled"), "got: {msg}");
    }

    /// The three orchestration tools are registered and visible on the Desktop
    /// surface (the trusted surface where the lead runs; all are Read/Write —
    /// never hard-denied there), with names within the 42-char style budget.
    #[test]
    fn orchestrator_tools_registered_and_visible_on_desktop() {
        let reg = Registry::global();
        for name in ["nomi_run_create", "nomi_run_status", "nomi_run_result"] {
            assert!(
                reg.contains(name),
                "orchestrator tool {name} is not registered"
            );
            assert!(
                reg.tool_visible(Surface::Desktop, name),
                "orchestrator tool {name} must be visible on the Desktop surface"
            );
            assert!(
                name.len() <= 42,
                "orchestrator tool name {name} exceeds the 42-char budget ({} chars)",
                name.len()
            );
        }
    }

    /// The orchestration domain is DESKTOP-only: it must NOT be advertised or
    /// dispatchable on the external Remote front door (a Remote companion is never
    /// an orchestration lead, and the reads take a bare run_id with no ownership
    /// predicate). `deny_on(Remote)` makes the tools invisible to `tool_specs`
    /// (advertisement) AND yields `Decision::Deny` at dispatch (a guessed call is
    /// denied, not just hidden) — while staying available on Desktop.
    #[test]
    fn orchestrator_tools_absent_on_remote_surface() {
        let reg = Registry::global();
        let remote: Vec<&str> = reg
            .tool_specs(Surface::Remote)
            .iter()
            .map(|s| s.name)
            .collect();
        for name in ["nomi_run_create", "nomi_run_status", "nomi_run_result"] {
            // Not advertised on the Remote surface.
            assert!(
                !remote.contains(&name),
                "orchestrator tool {name} must NOT be advertised on the Remote surface"
            );
            // Not visible (the dispatch gate Denies it, not merely hidden).
            assert!(
                !reg.tool_visible(Surface::Remote, name),
                "orchestrator tool {name} must be denied on the Remote surface"
            );
            // …but still available on the trusted Desktop surface (the lead).
            assert!(
                reg.tool_visible(Surface::Desktop, name),
                "orchestrator tool {name} must remain visible on the Desktop surface"
            );
        }
    }

    // ── P4 Task 2: assistant → role member resolution ─────────────────────

    fn assistant_data(id: &str, name: &str, models: &[&str], persona: &str) -> AssistantData {
        AssistantData {
            id: id.to_string(),
            name: name.to_string(),
            description: Some(format!("{name} 描述")),
            models: models.iter().map(|m| m.to_string()).collect(),
            enabled_skills: vec!["web_search".to_string()],
            disabled_builtin_skills: vec!["browser".to_string()],
            audience_tags: vec!["developer".to_string()],
            scenario_tags: vec!["coding".to_string()],
            persona: persona.to_string(),
        }
    }

    // resolve_assistant_model: honors the assistant's model priority and picks
    // the first preferred model that is present in the run's range.
    #[test]
    fn resolve_assistant_model_picks_first_in_range() {
        let range = vec![
            ("p1".to_string(), "m1".to_string()),
            ("p2".to_string(), "m2".to_string()),
        ];
        // Prefers "m2" (in range) over "mX" (not in range): first preferred-in-range wins.
        let got = resolve_assistant_model(&["mX".to_string(), "m2".to_string()], &range);
        assert_eq!(got, Some(("p2".to_string(), "m2".to_string())));

        // No preferred model is in range → None (caller skips the assistant).
        let none = resolve_assistant_model(&["mZ".to_string()], &range);
        assert_eq!(none, None);

        // No preferred models at all → None.
        assert_eq!(resolve_assistant_model(&[], &range), None);
    }

    // (KEYSTONE, pure) build_role_members_from_assistants: an assistant whose
    // preferred model is in range becomes an enriched member (agent_id=id,
    // role_hint=name, system_prompt=persona, enabled_skills, description, derived
    // capability); an assistant whose models are all out of range is SKIPPED.
    #[test]
    fn build_role_members_in_range_enriched_out_of_range_skipped() {
        let range = vec![("p1".to_string(), "m1".to_string())];
        let assistants = vec![
            assistant_data("asst_in", "研究员", &["m1"], "你是一名研究员"),
            // out of range: prefers m9, which is not in the run's range.
            assistant_data("asst_out", "写手", &["m9"], "你是一名写手"),
        ];

        let members = build_role_members_from_assistants(&assistants, &range);
        assert_eq!(members.len(), 1, "only the in-range assistant becomes a member");
        let m = &members[0];
        assert_eq!(m.agent_id, "asst_in", "agent_id = assistant id");
        assert_eq!(m.role_hint.as_deref(), Some("研究员"), "role_hint = assistant name");
        assert_eq!(m.provider_id.as_deref(), Some("p1"));
        assert_eq!(m.model.as_deref(), Some("m1"), "resolved to the in-range model");
        assert_eq!(m.system_prompt.as_deref(), Some("你是一名研究员"), "persona folded in");
        assert_eq!(m.enabled_skills, vec!["web_search"]);
        assert_eq!(m.disabled_builtin_skills, vec!["browser"]);
        assert_eq!(m.description.as_deref(), Some("研究员 描述"));
        assert!(m.id.starts_with("rmbr_"), "minted rmbr id: {}", m.id);
        // Derived capability: coding from the scenario tag, tools=true (has skills).
        let cap = m.capability_profile.as_ref().expect("capability derived");
        assert!(cap.strengths.contains(&"coding".to_string()), "coding from tag: {:?}", cap.strengths);
        assert!(cap.tools, "has skills → tools true");
    }

    // A blank/whitespace persona folds to None (fail-soft), not an empty string.
    #[test]
    fn build_role_member_blank_persona_is_none() {
        let range = vec![("p1".to_string(), "m1".to_string())];
        let assistants = vec![assistant_data("asst_x", "X", &["m1"], "   ")];
        let members = build_role_members_from_assistants(&assistants, &range);
        assert_eq!(members.len(), 1);
        assert!(members[0].system_prompt.is_none(), "blank persona → None");
    }

    // ── P6 Task 1: conversation-native runs default to `interactive` ──────────

    // The conversation-native lead path (nomi_run_create) defaults autonomy to
    // `interactive` when the caller omits it — so the 主管's plan parks at
    // `awaiting_plan_approval` for the user to approve in the 编排面板, rather
    // than auto-starting. An EXPLICIT autonomy override is honored verbatim;
    // a blank/whitespace value is treated as absent → the interactive default.
    #[test]
    fn lead_autonomy_defaults_to_interactive() {
        // Omitted → interactive (the conversation-native default).
        assert_eq!(lead_autonomy(None), "interactive", "omitted autonomy → interactive");
        // Blank/whitespace → treated as absent → interactive.
        assert_eq!(lead_autonomy(Some("   ".to_string())), "interactive", "blank → interactive");
        // Explicit overrides are honored verbatim (trimmed).
        assert_eq!(lead_autonomy(Some("supervised".to_string())), "supervised");
        assert_eq!(lead_autonomy(Some("autonomous".to_string())), "autonomous");
        assert_eq!(lead_autonomy(Some(" interactive ".to_string())), "interactive", "trimmed");
    }

    // When a run parks at `awaiting_plan_approval`, the tool return must carry the
    // awaiting status AND a 主管-facing message instructing it to tell the user a
    // team of N subtasks was drafted, pending approval in the 编排面板. The task
    // count is interpolated so the LLM relays the concrete number.
    #[test]
    fn awaiting_message_names_task_count_and_panel() {
        let msg = awaiting_plan_message(3);
        assert!(msg.contains('3'), "message must name the task count (3): {msg}");
        assert!(msg.contains("批准"), "message must mention approval: {msg}");
        assert!(
            msg.contains("编排面板"),
            "message must point the user at the 编排面板: {msg}"
        );
    }

    // A run that did NOT park (e.g. supervised/autonomous → `running`) is not an
    // awaiting state, so the choreography must START the engine for it.
    #[test]
    fn awaiting_status_predicate_only_for_awaiting() {
        assert!(is_awaiting_approval("awaiting_plan_approval"));
        assert!(!is_awaiting_approval("running"));
        assert!(!is_awaiting_approval("planning"));
    }
}
