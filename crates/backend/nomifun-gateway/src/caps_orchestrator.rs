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
//! choreography so a single tool call sets up a run from EXPLICIT params. The
//! tool takes `{goal, work_dir?, model_range?, autonomy?}` directly. `model_range`
//! defaults to `Auto` (expanded HERE to every enabled `(provider, model)` pair);
//! `work_dir` defaults to `None` (a temp dir); `autonomy` defaults to
//! **`supervised`** — an MCP/agent caller auto-runs (it has no orchestration Tab to
//! approve a plan from). An explicit `autonomy: "interactive"` still parks the run
//! at `awaiting_plan_approval` and returns a relay message for the 主管 instead of
//! starting. It drives the workspace-less
//! [`create_adhoc`](nomifun_orchestrator::RunService::create_adhoc) path.
//!
//! ## Path A — bind the run to the CALLING conversation
//! (conversation-native orchestration v2). When the master agent invokes this tool
//! from inside a conversation, the calling conversation id (`ctx.conversation_id`)
//! is parsed to `lead_conv_id` and the run is linked back to that conversation via
//! [`ConversationService::link_orchestrator_run`] (merge `extra.orchestrator_run_id`
//! + broadcast `conversation.listChanged`), so the FE lights up that conversation's
//! orchestration canvas entry. An MCP / no-session caller has an empty
//! `conversation_id` ⇒ `lead_conv_id: None` and no write-back — the run is still
//! created. The link is best-effort: a link failure only `warn!`s, never failing the
//! already-persisted run. The two read tools project the rich `RunDetail` down to a
//! compact, LLM-friendly shape (run status + per-task title/status, and on result
//! the per-task `output_summary`).
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
    CreateAdhocRunRequest, FleetMember, ModelRange, ModelRef, PlannedTask, RunDetail, derive_capability,
};
use nomifun_common::generate_prefixed_id;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::deps::GatewayDeps;
use crate::registry::{Capability, CapabilityMeta, DangerTier, Surface};
use crate::server::{ok, require_user};
use crate::tools_provider::{ProviderSummary, load_provider_summaries};

/// Orchestration is a DESKTOP master-agent feature. External callers (the network
/// Remote front door, `Surface::Remote`) must not create or inspect runs:
/// `nomi_run_status` / `nomi_run_result` take a bare `run_id` with no ownership
/// predicate, so advertising/dispatching them externally would let one companion's
/// token read ANY run's status/output, and `nomi_run_create` is a write that
/// synthesizes a fleet from this desktop's providers. Hard-deny the whole domain
/// on Remote so it is neither advertised (filtered out of `tool_specs`) NOR
/// dispatchable (a guessed call is Denied, not just hidden), while staying fully
/// available on the trusted Desktop surface. (Mirrors the per-surface `deny_on`
/// curation used elsewhere.)
const ORCHESTRATOR_DENY_SURFACES: &[Surface] = &[Surface::Remote];

// ── param structs (single source: schema + runtime) ──────────────────────

/// Create and kick off an orchestration run from EXPLICIT params: the goal plus
/// optional `work_dir`, `model_range`, and `autonomy`. Nothing is read FROM the
/// calling conversation, but when invoked from inside one the run is linked back
/// to it (Path A — `lead_conv_id` + `extra.orchestrator_run_id`).
#[derive(Deserialize, JsonSchema)]
struct RunCreateParams {
    /// The high-level goal to decompose into tasks and execute.
    goal: String,
    /// Working directory the run + its workers execute in. Omit for a temporary
    /// directory (no persistent workspace).
    #[serde(default)]
    work_dir: Option<String>,
    /// The model range to synthesize the fleet from, tagged by `mode`:
    /// `{"mode":"single","model":{"provider_id":..,"model":..}}`,
    /// `{"mode":"range","models":[{"provider_id":..,"model":..}, ..]}`, or
    /// `{"mode":"auto"}`. Omit (or pass `auto`) to use every enabled model on
    /// this desktop (expanded server-side).
    #[serde(default)]
    model_range: Option<Value>,
    /// Autonomy mode: "supervised" (the default — the run plans then runs without
    /// a human gate, since an MCP/agent caller has no orchestration Tab to approve
    /// from), "interactive" (parks at `awaiting_plan_approval` and returns a relay
    /// message instead of starting), or "autonomous". Omit for the supervised
    /// default.
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

/// One task in a `nomi_spawn` flat fan-out.
#[derive(Deserialize, JsonSchema)]
struct SpawnTaskParam {
    /// Short descriptive name (becomes the task/node title on the canvas).
    name: String,
    /// The instruction for this sub-agent.
    prompt: String,
    /// Optional restricted role: searcher/reviewer (read-only tools) or
    /// verifier (read-only + Bash). Omit for full tools.
    #[serde(default)]
    role: Option<String>,
}

/// Fan several INDEPENDENT sub-agent tasks out in parallel — no planner LLM, no
/// approval gate; each task runs as a visible worker on the orchestration canvas.
#[derive(Deserialize, JsonSchema)]
struct SpawnParams {
    /// 1-8 independent tasks to run in parallel.
    tasks: Vec<SpawnTaskParam>,
    /// When true (and ≥2 tasks), append a read-only synthesis task that
    /// consolidates all outputs and flags conflicts.
    #[serde(default)]
    synthesize: Option<bool>,
}

// ── re-adjust (supervision) param structs ─────────────────────────────────

/// Incrementally re-strategize a run: a natural-language `intent` the lead LLM
/// reconciles as KEEP / DROP / NEW over the existing tasks (completed work is
/// preserved). The primary re-strategy lever after a failure.
#[derive(Deserialize, JsonSchema)]
struct RunAdjustParams {
    /// The run to adjust (from nomi_run_create / nomi_spawn).
    run_id: String,
    /// What to change, in natural language (e.g. "换一种方式重做失败的安全扫描节点，
    /// 保留已完成的其它节点").
    intent: String,
}

/// Re-run ONE node (and cascade-reset its already-settled downstream), e.g. to
/// retry a failed node — optionally after changing its model/prompt via
/// `nomi_task_config`.
#[derive(Deserialize, JsonSchema)]
struct TaskRerunParams {
    run_id: String,
    /// The task/node id (from nomi_run_status).
    task_id: String,
}

/// Override a node's model and/or preset requirement before re-running it — e.g.
/// route a failing node to a stronger/different model.
#[derive(Deserialize, JsonSchema)]
struct TaskConfigParams {
    run_id: String,
    task_id: String,
    /// Provider id to route this node to (pair with `model`). Omit to keep current.
    #[serde(default)]
    provider_id: Option<String>,
    /// Model name for this node (pair with `provider_id`). Omit to keep current.
    #[serde(default)]
    model: Option<String>,
    /// A preset requirement appended to this node's brief (separate from its spec).
    #[serde(default)]
    preset_prompt: Option<String>,
}

/// Cancel a run and terminate its in-flight sub-agents.
#[derive(Deserialize, JsonSchema)]
struct RunCancelParams {
    run_id: String,
}

// ── handlers ──────────────────────────────────────────────────────────────

async fn create(deps: Arc<GatewayDeps>, ctx: crate::deps::CallerCtx, p: RunCreateParams) -> Value {
    let user = match require_user(&ctx) {
        Ok(u) => u.to_owned(),
        Err(e) => return e,
    };

    // 1. Resolve the explicit model range. Omitted ⇒ `Auto`; a present value is
    //    parsed from the tagged JSON (a malformed tag is a clean error, not a
    //    panic — mirrors the old lead-extra tolerant parse).
    let model_range = match resolve_model_range(p.model_range) {
        Ok(r) => r,
        Err(e) => return e,
    };

    // 1b. When the lead OMITS model_range (⇒ Auto), prefer the curated「主模型 +
    //     协作模型」range the homepage stashed on the calling conversation's
    //     `extra.orchestrator_model_range` (deterministic — never relayed through
    //     the LLM). An explicit tool arg (MCP caller) still wins; a missing /
    //     malformed extra value falls through to Auto.
    let model_range = if matches!(model_range, ModelRange::Auto) {
        read_conversation_model_range(&deps, &user, &ctx.conversation_id)
            .await
            .unwrap_or(model_range)
    } else {
        model_range
    };

    // The 主模型 = the FIRST model of the curated/explicit range; it becomes the
    // run's lead/planner (RunService floats its fleet member to the front so
    // `pick_lead` selects it). Captured BEFORE Auto-expansion so an uncurated Auto
    // run keeps `None` (engine's positional default — zero behavior change).
    let lead_model: Option<ModelRef> = match &model_range {
        ModelRange::Single { model } => Some(model.clone()),
        ModelRange::Range { models } => models.first().cloned(),
        ModelRange::Auto => None,
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

    // Path A: bind the run to the CALLING conversation (the master agent's chat)
    // when there is one. Parse the id once: empty ⇒ no lead (MCP / no-session),
    // non-numeric ⇒ None (never panic). The link write-back below is gated on the
    // same non-empty id.
    let lead_conv_id = parse_lead_conv_id(&ctx.conversation_id);

    // Autonomy: pass the caller's explicit value straight through; when omitted,
    // `create_adhoc` applies its own `supervised` default so the run AUTO-RUNS and
    // the 主管 can follow it to completion IN-TURN (poll `nomi_run_status` /
    // `nomi_run_result` and summarize) — the same engaged experience as `nomi_spawn`.
    // A conversation-native run no longer forces `interactive`: the human
    // plan-approval gate lives ONLY at the 编排 Tab front door
    // (`routes.rs::create_adhoc_run`, which sets `interactive` explicitly on the
    // request), so an in-chat fan-out never strands the 主管 at an unattended approval
    // park (that was the「主 main 失联」bug). An explicit `autonomy: "interactive"`
    // from the caller still parks — handled by the message branch below and the
    // engine-start gate in `spawn_plan_and_start`.
    let autonomy = p.autonomy;

    // Build the ad-hoc request from the EXPLICIT params: work_dir straight from the
    // arg, resolved autonomy, lead_conv_id = the parsed calling-conversation id.
    let req =
        build_adhoc_request(p.goal, p.work_dir, model_range, autonomy, role_members, lead_conv_id, lead_model);

    // 4. Create: synthesize the fleet from the model range + park in `planning`.
    let run = match deps.orchestrator_run_service.create_adhoc(&user, req).await {
        Ok(run) => run,
        Err(e) => return json!({ "error": e.to_string() }),
    };

    // Path A link write-back: associate the calling conversation with this run
    // (merge `extra.orchestrator_run_id` + broadcast `conversation.listChanged`) so
    // the FE lights up that conversation's orchestration canvas entry. Best-effort:
    // the run is already persisted, so a link failure only `warn!`s — it must NOT
    // fail the created run. An empty `ctx.conversation_id` is a no-op inside
    // `link_orchestrator_run`; we still skip the call to avoid a needless round-trip.
    if !ctx.conversation_id.is_empty() {
        if let Err(e) = deps
            .conversation_service
            .link_orchestrator_run(&ctx.conversation_id, &run.id)
            .await
        {
            tracing::warn!(
                error = %e,
                conversation_id = %ctx.conversation_id,
                run_id = %run.id,
                "failed to link orchestration run to calling conversation; run still created"
            );
        }
    }
    // 5. PLAN.
    //
    // Path A (a linked lead conversation = the desktop 智能编排 entry): the calling
    // conversation is SUBSCRIBED to the run over WS (its canvas streams `leadThinking`
    // + status), so plan in the BACKGROUND — mirror the Tab front door via
    // `spawn_plan_and_start` — and return the tool IMMEDIATELY. Blocking here is what
    // caused 会话9: `plan()` on a slow/weak lead model takes tens of seconds; the
    // SYNCHRONOUS tool call blocked the lead turn so long the weak model kept
    // re-invoking `nomi_run_create` every ~60s → multiple orphaned `planning` runs +
    // a 200s+ "stuck" turn with no visible progress. Returning at once keeps the lead
    // turn short and lets the canvas show planning live. With the default `supervised`
    // autonomy the run AUTO-STARTS once planned and the 主管 stays engaged — it follows
    // the SAME run_id via `nomi_run_status`/`nomi_run_result` to completion and
    // summarizes (no unattended approval park); `spawn_plan_and_start` starts the
    // engine for it. An explicit `interactive` run still parks for approval, and the
    // relay message then tells the 主管 to ask the user to approve first, THEN keep
    // polling — so it is never「失联」either way.
    if lead_conv_id.is_some() {
        let interactive = run.autonomy == "interactive";
        nomifun_orchestrator::spawn_plan_and_start(
            deps.orchestrator_run_service.clone(),
            deps.orchestrator_run_engine.as_ref().clone(),
            run.id.clone(),
            run.autonomy.clone(),
        );
        return ok(json!({
            "run_id": run.id,
            "status": "planning",
            "message": planning_started_message(interactive),
        }));
    }

    // Pure MCP / no-session caller: NO WS subscription, so keep the ONE-SHOT
    // synchronous choreography — the tool RESULT must carry the post-plan status +
    // task_count for the calling agent (steps 6-8 below).
    if let Err(e) = deps.orchestrator_run_service.plan(&run.id).await {
        return json!({ "error": format!("run {} created but planning failed: {e}", run.id) });
    }

    // 6. Read the post-plan detail ONCE: it tells us the resulting status (did the
    //    autonomy gate park the run?) and the planned task count (for the relay
    //    message). The run exists (we just created + planned it); a read error is
    //    non-fatal — we fall back to the create-time status and an empty task list.
    let (status, task_count) = match deps.orchestrator_run_service.get_detail(&run.id).await {
        Ok(detail) => (detail.run.status, detail.tasks.len()),
        Err(_) => (run.status.clone(), 0),
    };
    let awaiting = is_awaiting_approval(&status);

    // 7. Start the execution loop ONLY when the run is not awaiting approval. An
    //    explicit `interactive` run must NOT auto-start — it waits for the user to
    //    approve the plan (the `approve` route then starts the engine). All other
    //    autonomy levels (incl. the supervised default) start immediately
    //    (idempotent; restarts any existing loop).
    if !awaiting {
        deps.orchestrator_run_engine.start(run.id.clone());
    }

    // 8. Return. When the run parked at `awaiting_plan_approval` (explicit
    //    interactive), include a `message` instructing the 主管 to relay to the
    //    user that a team for `task_count` subtasks was drafted and is pending
    //    approval. Otherwise (the run is running) return the bare run id + status.
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

// ── explicit-param resolution + Auto expansion ────────────────────────────

/// Resolve the explicit `model_range` arg into a [`ModelRange`].
///
/// - Omitted (`None`) ⇒ [`ModelRange::Auto`] — "use every enabled model" (the
///   handler expands it to a concrete `Range` via [`expand_auto_range`]).
/// - Present ⇒ parsed from the tagged JSON. An unparseable value (bad/absent
///   `mode` tag) is a clean error, not a panic.
///
/// `Auto` is returned verbatim here — its expansion to a concrete `Range` needs
/// provider access and happens in [`expand_auto_range`] at the handler.
fn resolve_model_range(model_range: Option<Value>) -> Result<ModelRange, Value> {
    match model_range {
        None => Ok(ModelRange::Auto),
        Some(v) => serde_json::from_value(v).map_err(|e| {
            json!({
                "error": format!("model_range is malformed ({e}); expected one of mode=single|range|auto")
            })
        }),
    }
}

/// Read the curated「主模型 + 协作模型」range the homepage stashed on the lead
/// conversation's `extra.orchestrator_model_range`. This is the deterministic
/// channel from the FE picker into the run (the lead agent never has to pass a
/// `model_range`). Returns `None` — falling back to `Auto` — for every soft
/// failure: no calling conversation, an unreadable conversation, an absent key,
/// or a value that does not parse as a [`ModelRange`].
async fn read_conversation_model_range(
    deps: &Arc<GatewayDeps>,
    user_id: &str,
    conversation_id: &str,
) -> Option<ModelRange> {
    if conversation_id.is_empty() {
        return None;
    }
    let conv = deps
        .conversation_service
        .get(user_id, conversation_id)
        .await
        .ok()?;
    let raw = conv.extra.get("orchestrator_model_range")?;
    match serde_json::from_value::<ModelRange>(raw.clone()) {
        Ok(range) => Some(range),
        Err(e) => {
            tracing::warn!(
                conversation_id,
                error = %e,
                "orchestrator_model_range on conversation extra is malformed; falling back to Auto"
            );
            None
        }
    }
}

/// Build the [`CreateAdhocRunRequest`] from the EXPLICIT params plus an optional
/// lead conversation (Path A). `work_dir` comes straight from the arg; `autonomy`
/// is passed through untouched so an omitted value falls to `create_adhoc`'s own
/// `supervised` default; `lead_conv_id` is the parsed calling-conversation id
/// (`None` for MCP / no-session callers). The model range is already expanded (no
/// `Auto`) and the role members already built.
fn build_adhoc_request(
    goal: String,
    work_dir: Option<String>,
    model_range: ModelRange,
    autonomy: Option<String>,
    role_members: Vec<FleetMember>,
    lead_conv_id: Option<i64>,
    lead_model: Option<ModelRef>,
) -> CreateAdhocRunRequest {
    CreateAdhocRunRequest {
        goal,
        work_dir,
        model_range,
        pinned_roles: vec![],
        role_members,
        // Pass the explicit arg through: omitted ⇒ `create_adhoc` applies its own
        // `supervised` default (an MCP/agent caller has no Tab to approve from);
        // an explicit `interactive` still parks at `awaiting_plan_approval`.
        autonomy,
        // Serial loop (P1): parallelism is not yet a gateway-exposed knob.
        max_parallel: None,
        // Path A: bind the run to the calling conversation (the master agent's
        // chat) when there is one, so the FE lights up that conversation's
        // orchestration canvas entry. `None` for MCP / no-session callers.
        lead_conv_id,
        // The 主模型 (planner/lead) when the range is curated/explicit; `None` for
        // an uncurated Auto run (engine keeps its positional default).
        lead_model,
    }
}

/// Parse `ctx.conversation_id` (a `String`, empty when there is no calling
/// conversation) into a lead conversation id. An empty id ⇒ `None` (MCP /
/// no-session caller — regress to the no-lead behavior); a non-empty id is parsed
/// with `.parse().ok()`, so a non-numeric value degrades to `None` rather than
/// panicking.
fn parse_lead_conv_id(conversation_id: &str) -> Option<i64> {
    if conversation_id.is_empty() {
        None
    } else {
        conversation_id.parse::<i64>().ok()
    }
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

// ── awaiting-approval relay (explicit interactive path) ────────────────────

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

/// The 主管-facing relay message for a Path-A run whose planning was kicked off in
/// the BACKGROUND (the calling conversation watches it live). Keeps the 主管 ENGAGED
/// rather than stopping it: it must poll the SAME run_id to completion and summarize
/// — mirroring the `nomi_spawn` follow-up contract, so the main chat never goes
/// silent (the「主 main 失联」bug). It still guards against re-creating the run (会话9).
/// `interactive` (explicit caller value only) prepends the approval-relay step, since
/// such a run parks at `awaiting_plan_approval` before it can execute.
fn planning_started_message(interactive: bool) -> String {
    if interactive {
        "编排已创建，正在后台拆解为任务图(可在右侧编排画布实时查看规划过程)。这是需人工批准的编排：规划完成后会停在「待批准」——请告知用户在编排面板点「批准执行」或直接回复批准，不要再次创建编排。批准后子任务会自动执行，全部完成或失败时系统会自动把结果回执给你，届时你再向用户汇总——你无需轮询等待。".to_string()
    } else {
        "编排已创建(run_id 见返回)，正在后台拆解为任务图并会自动开跑(可在右侧编排画布实时查看每个子 agent 的状态与产出)。请告诉用户已在后台并行执行、进度见画布，然后结束本轮——你无需轮询等待：全部完成或失败时系统会自动把结果回执给你，届时你再向用户汇总。不要再次创建编排；用户若中途询问进度，可用 nomi_run_status 查一次。".to_string()
    }
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

/// `nomi_spawn` 的任务数上限：对齐进程内 Spawn（5）与其全局并发上限（8）的量级，
/// 超过应改用 nomi_run_create 让 planner 规划分批。
const MAX_SPAWN_TASKS: usize = 8;

/// Flat fan-out (`nomi_spawn`): create a run, link it to the calling
/// conversation, persist the caller's EXPLICIT task list via `plan_flat` (no
/// planner LLM), and start the engine — all with `supervised` autonomy so it
/// runs immediately (no approval park; that gate is for planner-drafted teams).
/// Each task becomes a real worker conversation, visible on the orchestration
/// canvas (status / live transcript / output per agent).
async fn spawn(deps: Arc<GatewayDeps>, ctx: crate::deps::CallerCtx, p: SpawnParams) -> Value {
    let user = match require_user(&ctx) {
        Ok(u) => u.to_owned(),
        Err(e) => return e,
    };
    if p.tasks.is_empty() {
        return json!({ "error": "nomi_spawn requires at least one task" });
    }
    if p.tasks.len() > MAX_SPAWN_TASKS {
        return json!({ "error": format!("too many tasks: {} (max {MAX_SPAWN_TASKS}); use nomi_run_create for larger goals", p.tasks.len()) });
    }

    // 模型解析：会话策展的「主模型+协作模型」优先，缺省 Auto 全量展开（与 create 一致）。
    let model_range = read_conversation_model_range(&deps, &user, &ctx.conversation_id)
        .await
        .unwrap_or(ModelRange::Auto);
    let lead_model: Option<ModelRef> = match &model_range {
        ModelRange::Single { model } => Some(model.clone()),
        ModelRange::Range { models } => models.first().cloned(),
        ModelRange::Auto => None,
    };
    let model_range = if matches!(model_range, ModelRange::Auto) {
        let summaries = match load_provider_summaries(&deps).await {
            Ok(s) => s,
            Err(e) => return e,
        };
        match expand_auto_range(&summaries) {
            Ok(r) => r,
            Err(e) => return e,
        }
    } else {
        model_range
    };

    let lead_conv_id = parse_lead_conv_id(&ctx.conversation_id);
    let n = p.tasks.len();
    let goal = format!(
        "并行执行 {n} 个子任务：{}",
        p.tasks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join("、")
    );
    // 扁平扇出恒 supervised：即时并行、无审批门。绝不显式传 interactive——
    // nomi_spawn 的任务是调用方显式给出的，park 在批准门会回归进程内 Spawn 的
    // 静默卡死体验（人工审批门只在编排 Tab 前门保留）。
    let req = build_adhoc_request(
        goal,
        None,
        model_range,
        Some("supervised".to_string()),
        Vec::new(),
        lead_conv_id,
        lead_model,
    );

    let run = match deps.orchestrator_run_service.create_adhoc(&user, req).await {
        Ok(run) => run,
        Err(e) => return json!({ "error": e.to_string() }),
    };

    // Path A link write-back（与 create 相同的 best-effort 语义）。
    if !ctx.conversation_id.is_empty() {
        if let Err(e) = deps
            .conversation_service
            .link_orchestrator_run(&ctx.conversation_id, &run.id)
            .await
        {
            tracing::warn!(
                error = %e,
                conversation_id = %ctx.conversation_id,
                run_id = %run.id,
                "failed to link flat fan-out run to calling conversation; run still created"
            );
        }
    }

    // 组装扁平任务（+ 可选只读综合节点，语义对齐进程内 Spawn 的 synthesize）。
    let synthesize = p.synthesize.unwrap_or(false);
    let mut tasks: Vec<PlannedTask> = p
        .tasks
        .into_iter()
        .map(|t| PlannedTask {
            title: t.name,
            spec: t.prompt,
            task_profile: None,
            depends_on: vec![],
            member_index: None,
            rationale: None,
            role: t.role,
            kind: "agent".to_string(),
            pattern_config: None,
        })
        .collect();
    if synthesize && n >= 2 {
        tasks.push(PlannedTask {
            title: "综合汇总".to_string(),
            spec: "综合上游各子任务的产出为一份结论，显式标注子任务之间的冲突或分歧（无则写「无」）。"
                .to_string(),
            task_profile: None,
            depends_on: (0..n).collect(),
            member_index: None,
            rationale: None,
            // 只读综合（对齐进程内 Spawn 的 reviewer 语义；worker 端收缩工具）。
            role: Some("reviewer".to_string()),
            kind: "synthesis".to_string(),
            pattern_config: None,
        });
    }

    // 后台 plan_flat → engine.start（fail-soft），工具立即返回 —— 画布经 WS 实时
    // 点亮（与 create 的 Path A spawn_plan_and_start 同样的不阻塞 lead turn 设计）。
    nomifun_orchestrator::spawn_plan_flat_and_start(
        deps.orchestrator_run_service.clone(),
        deps.orchestrator_run_engine.as_ref().clone(),
        run.id.clone(),
        tasks,
    );
    ok(json!({
        "run_id": run.id,
        "status": "running",
        "task_count": n,
        "message": "子任务已在编排画布并行执行，用户能实时看到每个子 agent 的状态与产出。请告诉用户已在后台并行执行、进度见右侧画布，然后结束本轮——你无需轮询等待：全部完成或失败时系统会自动把结果回执给你再汇总。不要重复创建编排；用户若中途询问进度，可用 nomi_run_status 查一次。",
    }))
}

// ── re-adjust (supervision) handlers ──────────────────────────────────────

/// Incremental re-strategy (KEEP/DROP/NEW). Mirrors the Tab `adjust_run` route:
/// engine.adjust → (re)start the loop if the reconciled run is running with no
/// live loop. Completed work is preserved.
async fn run_adjust(deps: Arc<GatewayDeps>, ctx: crate::deps::CallerCtx, p: RunAdjustParams) -> Value {
    let user = match require_user(&ctx) {
        Ok(u) => u.to_owned(),
        Err(e) => return e,
    };
    if p.intent.trim().is_empty() {
        return json!({ "error": "intent must not be empty" });
    }
    match deps
        .orchestrator_run_engine
        .adjust(&deps.orchestrator_run_service, &user, &p.run_id, &p.intent)
        .await
    {
        Ok(run) => {
            if run.status == "running" && !deps.orchestrator_run_engine.is_running(&p.run_id) {
                deps.orchestrator_run_engine.start(p.run_id.clone());
            }
            ok(json!({
                "run_id": run.id,
                "status": run.status,
                "message": "已按你的意图增量调整编排（保留已完成节点、重做/新增其余）。调整后会继续执行，完成或失败时自动回执——无需轮询。",
            }))
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// Re-run one node (+ cascade-reset its downstream). Mirrors the Tab `rerun_task`
/// route. Retry a failed node — optionally after `nomi_task_config` changes it.
async fn task_rerun(deps: Arc<GatewayDeps>, ctx: crate::deps::CallerCtx, p: TaskRerunParams) -> Value {
    let user = match require_user(&ctx) {
        Ok(u) => u.to_owned(),
        Err(e) => return e,
    };
    match deps
        .orchestrator_run_engine
        .rerun_task(&deps.orchestrator_run_service, &user, &p.run_id, &p.task_id)
        .await
    {
        Ok(run) => {
            if run.status == "running" && !deps.orchestrator_run_engine.is_running(&p.run_id) {
                deps.orchestrator_run_engine.start(p.run_id.clone());
            }
            ok(json!({
                "run_id": run.id,
                "status": run.status,
                "message": "已重跑该节点并级联重置其下游；继续执行、完成/失败自动回执。",
            }))
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// Change a node's model / preset requirement. Mirrors the Tab `set_task_config`
/// route (takes effect on next dispatch; for a settled node follow with rerun).
async fn task_config(deps: Arc<GatewayDeps>, ctx: crate::deps::CallerCtx, p: TaskConfigParams) -> Value {
    let user = match require_user(&ctx) {
        Ok(u) => u.to_owned(),
        Err(e) => return e,
    };
    match deps
        .orchestrator_run_service
        .set_task_config(&user, &p.run_id, &p.task_id, p.provider_id, p.model, p.preset_prompt)
        .await
    {
        Ok(()) => ok(json!({
            "run_id": p.run_id,
            "task_id": p.task_id,
            "message": "已更新该节点的模型/预置要求。pending 节点下次派发即生效；已结算节点请再调 nomi_task_rerun 使其以新配置重跑。",
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// Cancel a run + terminate its in-flight sub-agents. Mirrors the Tab `cancel_run`
/// route (engine.stop + run_service.cancel).
async fn run_cancel(deps: Arc<GatewayDeps>, ctx: crate::deps::CallerCtx, p: RunCancelParams) -> Value {
    if let Err(e) = require_user(&ctx) {
        return e;
    }
    deps.orchestrator_run_engine.stop(&p.run_id);
    match deps.orchestrator_run_service.cancel(&p.run_id).await {
        Ok(()) => ok(json!({
            "run_id": p.run_id,
            "status": "cancelled",
            "message": "已取消该编排，在飞子任务已终止。",
        })),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── result projections (RunDetail → compact LLM-friendly shape) ───────────

/// Run status + per-task {id, title, status, attempt, conversation_id, last_error}
/// — enough for the supervising lead to diagnose (which node, how many attempts,
/// why it failed) and decide a re-strategy.
fn project_status(detail: &RunDetail) -> Value {
    json!({
        "run_id": detail.run.id,
        "status": detail.run.status,
        "tasks": detail
            .tasks
            .iter()
            .map(|t| json!({
                "id": t.id,
                "title": t.title,
                "status": t.status,
                "attempt": t.attempt,
                "conversation_id": t.conversation_id,
                "last_error": t.last_error,
            }))
            .collect::<Vec<_>>(),
    })
}

/// Run status + summary + per-task {id, title, status, output_summary, attempt,
/// last_error}. While not terminal, `status` reflects the in-flight state; failed
/// nodes carry `last_error` so the lead can explain / re-strategize.
fn project_result(detail: &RunDetail) -> Value {
    json!({
        "run_id": detail.run.id,
        "status": detail.run.status,
        "summary": detail.run.summary,
        "tasks": detail
            .tasks
            .iter()
            .map(|t| json!({
                "id": t.id,
                "title": t.title,
                "status": t.status,
                "output_summary": t.output_summary,
                "attempt": t.attempt,
                "last_error": t.last_error,
            }))
            .collect::<Vec<_>>(),
    })
}

// ── registration ─────────────────────────────────────────────────────────

/// Register the orchestration-domain capabilities.
pub(crate) fn register(out: &mut Vec<Capability>) {
    // 1. Create + kick off a run (write). Desktop-only: deny on Remote (the reads
    //    take a bare run_id with no ownership predicate, so the whole domain is
    //    Desktop-only).
    out.push(Capability::new::<RunCreateParams, _, _>(
        CapabilityMeta::new(
            "nomi_run_create",
            "orchestrator",
            "Create and run an orchestration job from a goal: decompose it into a task DAG over a model range and propose a team to execute it. Params: goal (required), work_dir (optional dir; omit for a temp dir), model_range (optional; {mode:single|range|auto} — omit for all enabled models), autonomy (optional; defaults to `supervised` = plan then run automatically; pass `interactive` to park at `awaiting_plan_approval` and get a relay message instead). Returns the run id and status.",
            DangerTier::Write,
        )
        .deny_on(ORCHESTRATOR_DENY_SURFACES),
        |deps, ctx, p| create(deps, ctx, p),
    ));

    // 1b. Flat fan-out（write）：跳过 planner 的并行子 agent 扇出，画布可视化、
    //     supervised 即跑。与 create 同域同 deny：Desktop-only。
    out.push(Capability::new::<SpawnParams, _, _>(
        CapabilityMeta::new(
            "nomi_spawn",
            "orchestrator",
            "Run several INDEPENDENT sub-agent tasks in parallel with live visualization: each task becomes a visible worker on the orchestration canvas (the user sees per-agent status, live transcript, and output). No planner, no approval gate — starts immediately. Params: tasks (1-8 of {name, prompt, role?}; role searcher/reviewer = read-only tools, verifier = read-only+Bash, omit = full tools), synthesize (optional; true appends a read-only consolidation task). For complex goals needing decomposition or dependencies use nomi_run_create instead. Returns run_id; follow up with nomi_run_status / nomi_run_result.",
            DangerTier::Write,
        )
        .deny_on(ORCHESTRATOR_DENY_SURFACES),
        |deps, ctx, p| spawn(deps, ctx, p),
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

    // 4. Supervision / re-adjust (write, Desktop-only): let the master agent
    //    RE-STRATEGIZE a run after a failure instead of only observing it — adjust
    //    the plan, re-run a node, change a node's model, or cancel. Same domain
    //    deny (Remote) as the rest.
    out.push(Capability::new::<RunAdjustParams, _, _>(
        CapabilityMeta::new(
            "nomi_run_adjust",
            "orchestrator",
            "Incrementally re-strategize a run from a natural-language intent (the lead reconciles it as keep/drop/new over existing tasks, preserving completed work). The primary lever to recover from a failure — e.g. redo the failed node a different way while keeping the rest. Params: run_id, intent. Returns run status.",
            DangerTier::Write,
        )
        .deny_on(ORCHESTRATOR_DENY_SURFACES),
        |deps, ctx, p| run_adjust(deps, ctx, p),
    ));
    out.push(Capability::new::<TaskRerunParams, _, _>(
        CapabilityMeta::new(
            "nomi_task_rerun",
            "orchestrator",
            "Re-run one node of a run (and cascade-reset its already-settled downstream) — e.g. retry a failed node, optionally after changing its model/prompt via nomi_task_config. Params: run_id, task_id (from nomi_run_status).",
            DangerTier::Write,
        )
        .deny_on(ORCHESTRATOR_DENY_SURFACES),
        |deps, ctx, p| task_rerun(deps, ctx, p),
    ));
    out.push(Capability::new::<TaskConfigParams, _, _>(
        CapabilityMeta::new(
            "nomi_task_config",
            "orchestrator",
            "Override a node's model (provider_id + model) and/or preset requirement before re-running it — e.g. route a failing node to a stronger model, then nomi_task_rerun. Params: run_id, task_id, provider_id?, model?, preset_prompt?.",
            DangerTier::Write,
        )
        .deny_on(ORCHESTRATOR_DENY_SURFACES),
        |deps, ctx, p| task_config(deps, ctx, p),
    ));
    out.push(Capability::new::<RunCancelParams, _, _>(
        CapabilityMeta::new(
            "nomi_run_cancel",
            "orchestrator",
            "Cancel a run and terminate its in-flight sub-agents. Params: run_id.",
            DangerTier::Write,
        )
        .deny_on(ORCHESTRATOR_DENY_SURFACES),
        |deps, ctx, p| run_cancel(deps, ctx, p),
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

    // ── resolve_model_range: explicit arg → ModelRange (omitted ⇒ Auto) ──────

    #[test]
    fn resolve_model_range_omitted_is_auto() {
        // No model_range arg ⇒ Auto (the handler then expands it to every enabled
        // model). This is the "无 conversation_service.get" default path: the range
        // comes from the (absent) param, NOT from a calling conversation's extra.
        let range = resolve_model_range(None).expect("omitted → Auto");
        assert!(matches!(range, ModelRange::Auto), "omitted model_range → Auto");
    }

    #[test]
    fn resolve_model_range_explicit_range_passes_through() {
        let v = json!({"mode": "range", "models": [
            {"provider_id": "p1", "model": "m1"},
            {"provider_id": "p2", "model": "m2"}
        ]});
        let range = resolve_model_range(Some(v)).expect("parses explicit range");
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
    fn resolve_model_range_explicit_single_passes_through() {
        let v = json!({"mode": "single", "model": {"provider_id": "ps", "model": "ms"}});
        let range = resolve_model_range(Some(v)).expect("parses single");
        assert!(matches!(range, ModelRange::Single { .. }));
    }

    #[test]
    fn resolve_model_range_explicit_auto_is_auto() {
        let v = json!({"mode": "auto"});
        let range = resolve_model_range(Some(v)).expect("parses auto");
        assert!(matches!(range, ModelRange::Auto), "explicit auto returned verbatim");
    }

    #[test]
    fn resolve_model_range_malformed_is_clean_error() {
        // Present but unparseable (bad tag) → a clear "malformed" error, not a panic.
        let v = json!({"mode": "nonsense"});
        let err = resolve_model_range(Some(v)).expect_err("must error on malformed range");
        let msg = err["error"].as_str().unwrap_or("");
        assert!(msg.contains("malformed"), "got: {msg}");
    }

    // ── build_adhoc_request: EXPLICIT params → CreateAdhocRunRequest ──────────

    #[test]
    fn build_adhoc_request_uses_explicit_params_and_threads_lead_conv() {
        // The explicit-param contract: goal/work_dir/model_range/autonomy/role_members
        // map straight onto the request; lead_conv_id is whatever the caller threads
        // in (Path A: the parsed calling-conversation id).
        let range = ModelRange::Range {
            models: vec![ModelRef { provider_id: "p1".into(), model: "m1".into() }],
        };
        let req = build_adhoc_request(
            "ship it".into(),
            Some("/tmp/proj".into()),
            range,
            Some("supervised".into()),
            vec![],
            Some(909),
            Some(ModelRef { provider_id: "p1".into(), model: "m1".into() }),
        );
        assert_eq!(req.goal, "ship it");
        assert_eq!(req.work_dir.as_deref(), Some("/tmp/proj"));
        assert!(matches!(req.model_range, ModelRange::Range { .. }), "explicit range preserved");
        assert_eq!(req.autonomy.as_deref(), Some("supervised"), "autonomy passed through");
        assert_eq!(req.lead_conv_id, Some(909), "lead_conv_id threaded through");
        assert_eq!(
            req.lead_model.as_ref().map(|m| m.model.as_str()),
            Some("m1"),
            "lead_model (主模型) threaded through"
        );
        assert!(req.pinned_roles.is_empty());
        assert!(req.max_parallel.is_none());
    }

    #[test]
    fn build_adhoc_request_none_lead_conv_stays_none() {
        // No calling conversation (MCP / no-session) ⇒ lead_conv_id None: behaves
        // exactly as before this Path-A wiring (regression guard).
        let req = build_adhoc_request(
            "ship it".into(),
            Some("/tmp/proj".into()),
            ModelRange::Range { models: vec![] },
            Some("supervised".into()),
            vec![],
            None,
            None,
        );
        assert!(req.lead_conv_id.is_none(), "lead_conv_id must be None (no lead conversation)");
    }

    #[test]
    fn build_adhoc_request_omitted_autonomy_defers_to_service_default() {
        // Omitted autonomy ⇒ None passed through, so create_adhoc applies its own
        // `supervised` default (an MCP/agent caller has no Tab to approve from).
        let req = build_adhoc_request(
            "goal".into(),
            None,
            ModelRange::Range { models: vec![] },
            None,
            vec![],
            None,
            None,
        );
        assert!(req.autonomy.is_none(), "omitted autonomy → None (service default applies)");
        assert!(req.work_dir.is_none(), "omitted work_dir → None (temp dir)");
        assert!(req.lead_conv_id.is_none());
    }

    // A conversation-native `nomi_run_create` now keeps the 主管 ENGAGED instead of
    // stopping it (the fix for「主 main 失联」): the background-planning relay message
    // must tell the 主管 to poll the same run to completion and summarize — the
    // `nomi_spawn` follow-up contract — and must NOT tell it to just wait. It still
    // guards against re-creating the run (会话9).
    #[test]
    fn planning_started_message_defers_to_auto_report() {
        // Dispatch-and-ack: the run auto-runs and the engine auto-REPORTS the result
        // back to the lead conversation on completion/failure — so the 主管 must NOT
        // be told to busy-poll; it dispatches, acks, and waits for the auto-report.
        let auto = planning_started_message(false);
        assert!(auto.contains("自动"), "must mention the auto-report: {auto}");
        assert!(auto.contains("回执"), "must mention the receipt back to the lead: {auto}");
        assert!(auto.contains("无需轮询"), "must tell the 主管 NOT to busy-poll: {auto}");
        assert!(auto.contains("不要再次创建编排"), "must still guard against re-creating (会话9): {auto}");
        assert!(!auto.contains("持续用"), "must NOT instruct busy-polling: {auto}");
        assert!(!auto.contains("耐心等待"), "must NOT tell the 主管 to just wait idly: {auto}");
        // On-demand progress stays available.
        assert!(auto.contains("nomi_run_status"), "keeps nomi_run_status as an on-demand option: {auto}");

        // Interactive: still relays the approval step, but after approval the result
        // is also auto-reported — no busy-poll.
        let inter = planning_started_message(true);
        assert!(inter.contains("批准"), "interactive must relay the approval step: {inter}");
        assert!(inter.contains("自动"), "interactive must still auto-report after approval: {inter}");
        assert!(inter.contains("无需轮询"), "interactive must NOT instruct busy-polling: {inter}");
    }

    // The deterministic homepage channel: the exact `extra.orchestrator_model_range`
    // Value shape the FE stashes (a tagged range with 主模型 first) must parse to a
    // `ModelRange` — this is what `read_conversation_model_range` does — and its
    // first model is the 主模型 the handler lifts into `lead_model`.
    #[test]
    fn extra_orchestrator_model_range_value_parses_main_first() {
        let raw = json!({
            "mode": "range",
            "models": [
                { "provider_id": "p_main", "model": "opus" },
                { "provider_id": "p_collab", "model": "haiku" }
            ]
        });
        let range: ModelRange =
            serde_json::from_value(raw).expect("the stashed extra range must parse");
        let first = match &range {
            ModelRange::Range { models } => {
                assert_eq!(models.len(), 2, "主模型 + 1 协作模型");
                models.first().cloned()
            }
            _ => panic!("expected a range"),
        };
        assert_eq!(
            first.map(|m| m.model),
            Some("opus".to_string()),
            "models[0] is the 主模型 → becomes lead_model"
        );
    }

    // ── parse_lead_conv_id: ctx.conversation_id (String) → Option<i64> ────────

    #[test]
    fn parse_lead_conv_id_non_empty_numeric_is_some() {
        // A real calling conversation (Path A): the numeric id parses to Some(id),
        // which the handler threads onto the request as the run's lead.
        assert_eq!(parse_lead_conv_id("909"), Some(909));
    }

    #[test]
    fn parse_lead_conv_id_empty_is_none() {
        // Empty conversation_id (MCP / no-session caller) ⇒ None: regress to today's
        // behavior — no lead conversation, nothing written back.
        assert_eq!(parse_lead_conv_id(""), None);
    }

    #[test]
    fn parse_lead_conv_id_non_numeric_is_none_not_panic() {
        // A non-empty but non-numeric id must NOT panic (`.parse().ok()` swallows the
        // error) — it degrades to None, so the run is still created without a lead.
        assert_eq!(parse_lead_conv_id("not-a-number"), None);
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
    /// surface (the trusted surface; all are Read/Write — never hard-denied there),
    /// with names within the 42-char style budget.
    #[test]
    fn orchestrator_tools_registered_and_visible_on_desktop() {
        let reg = Registry::global();
        for name in ["nomi_run_create", "nomi_spawn", "nomi_run_status", "nomi_run_result", "nomi_run_adjust", "nomi_task_rerun", "nomi_task_config", "nomi_run_cancel"] {
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
    /// dispatchable on the external Remote front door (the reads take a bare run_id
    /// with no ownership
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
        for name in ["nomi_run_create", "nomi_spawn", "nomi_run_status", "nomi_run_result", "nomi_run_adjust", "nomi_task_rerun", "nomi_task_config", "nomi_run_cancel"] {
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

    // ── awaiting-approval relay (explicit interactive path) ──────────────────

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
