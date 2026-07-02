//! 主管规划 (PlanProducer): turn a goal + a fleet member snapshot into an
//! executable task DAG ([`PlannedDag`]).
//!
//! [`LlmPlanProducer`] does one structured one-shot LLM call against a "lead"
//! model: it builds a planning prompt, asks the model for a strict-JSON
//! `{"tasks":[...]}` object, and parses it via [`parse_plan`].
//!
//! [`parse_plan`] is the heart of testability and is **fail-soft**: it extracts
//! the first JSON object from the raw model text (stripping ```json fences and
//! surrounding prose), parses it into a [`PlannedDag`], and on ANY failure
//! (no JSON, bad shape, empty `tasks`) logs a `warn!` and returns a single-task
//! fallback DAG built from the goal — so the Run engine always has something
//! executable.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use nomifun_ai_agent::{
    resolve_provider_config, streaming_completion_text_or_reasoning, user_message,
    DeltaKind,
};
use nomifun_api_types::{FleetMember, PlannedDag, PlannedTask, RunTask, RunTaskDep};
use nomifun_common::{AppError, ProviderWithModel};
use nomifun_db::IProviderRepository;
use nomifun_db::models::Provider;

/// How many tokens the planner may use for its DAG completion. Sized generously
/// because reasoning models (e.g. StepFun `step-*`) spend a large share of the
/// budget "thinking" BEFORE emitting the JSON — at 4096 the reasoning phase could
/// exhaust the budget (`finish_reason=length`) and truncate the plan (会话10). The
/// conversation entry plans in the background, so the extra latency is not on the
/// critical path.
const PLAN_MAX_TOKENS: u32 = 8192;

/// Non-silent notice pushed to the lead-thinking stream when planning FALLS BACK to
/// the single-task DAG (the lead did not emit a parseable plan). Lets the user see
/// the auto-decomposition failed instead of a mysterious single node.
const PLAN_FALLBACK_NOTICE: &str = "\n\n⚠️ 未能将目标自动拆解为多任务(规划模型未产出有效的任务结构)。已回退为单任务并停在待批准——建议细化目标后重新规划,或改用推理更强的模型。";

/// Max length of the fallback task title derived from the goal.
const FALLBACK_TITLE_LEN: usize = 60;

/// Which lead-thinking stream channel a delta belongs to, mirroring the
/// `nomifun_ai_agent::DeltaKind` the provider layer emits. `Text` = the visible
/// plan-JSON draft (a progress heartbeat — the frontend shows "拟稿中…", never the
/// raw JSON); `Reasoning` = the model's readable reasoning (`ThinkingDelta`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeadDeltaKind {
    /// A visible-answer (plan-JSON draft) delta.
    Text,
    /// A reasoning (thinking) delta.
    Reasoning,
}

impl From<DeltaKind> for LeadDeltaKind {
    fn from(k: DeltaKind) -> Self {
        match k {
            DeltaKind::Text => LeadDeltaKind::Text,
            DeltaKind::Reasoning => LeadDeltaKind::Reasoning,
        }
    }
}

/// A lead-thinking sink: a shared, thread-safe callback the planner invokes with
/// every streamed lead delta (`kind`, `delta`) so the engine can fan it out over
/// WebSocket (see `OrchestratorRunEventEmitter::emit_lead_thinking`). Shared
/// (`Arc<dyn Fn + Send + Sync>`) rather than `&mut FnMut` so it survives the
/// boxed `async_trait` future and the engine-side throttle (which carries its own
/// interior-mutable buffer). A `None` sink means "do not stream" — the planner
/// then takes the byte-identical `one_shot_completion` path (zero behavior change).
pub type LeadThinkingSink = Arc<dyn Fn(LeadDeltaKind, &str) + Send + Sync>;

// ── Merge throttle (防 WS 洪泛) ─────────────────────────────────────────────
//
// Provider token streams arrive one tiny delta at a time (often a few chars).
// Emitting a WebSocket frame per delta would flood the bus and the frontend.
// [`LeadThinkingThrottle`] COALESCES deltas per `kind` and only flushes an
// aggregated chunk to `emit_lead_thinking` when EITHER threshold trips:
//   - at least [`THROTTLE_FLUSH_INTERVAL`] has elapsed since this kind last
//     flushed (keeps the stream feeling live without per-token spam), OR
//   - the buffered chunk reached [`THROTTLE_FLUSH_CHARS`] (bounds frame size).
// The caller MUST call [`LeadThinkingThrottle::flush`] once after the completion
// returns to emit any residual buffered bytes — NOTHING is ever dropped.

/// Min wall-clock between two flushes of the SAME kind (time-based coalescing).
const THROTTLE_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(80);

/// Char count that forces a flush regardless of elapsed time (size-based cap).
const THROTTLE_FLUSH_CHARS: usize = 48;

/// Per-kind coalescing buffer + last-flush instant. Guarded by a `Mutex` because
/// the sink is an `Arc<dyn Fn>` (shared, non-`mut`) that the streaming completion
/// may call from any task.
#[derive(Default)]
struct ThrottleBuf {
    /// Pending reasoning bytes not yet flushed.
    reasoning: String,
    /// Pending text (plan-JSON draft) bytes not yet flushed.
    text: String,
    /// Last flush time for reasoning / text respectively (`None` = never flushed).
    reasoning_last: Option<std::time::Instant>,
    text_last: Option<std::time::Instant>,
}

/// Coalesces lead-thinking deltas and fans them out as merged chunks via
/// `emit_lead_thinking`, throttled to avoid WebSocket flooding (see the module
/// comment above). Build one per lead call ([`new`](Self::new)), hand its
/// [`sink`](Self::sink) to the planner, then [`flush`](Self::flush) the residue
/// after the call returns.
pub struct LeadThinkingThrottle {
    emitter: crate::events::OrchestratorRunEventEmitter,
    run_id: String,
    phase: String,
    buf: Arc<std::sync::Mutex<ThrottleBuf>>,
}

impl LeadThinkingThrottle {
    /// Build a throttle bound to `(run_id, phase)` that fans out via `emitter`.
    pub fn new(
        emitter: crate::events::OrchestratorRunEventEmitter,
        run_id: impl Into<String>,
        phase: impl Into<String>,
    ) -> Self {
        Self {
            emitter,
            run_id: run_id.into(),
            phase: phase.into(),
            buf: Arc::new(std::sync::Mutex::new(ThrottleBuf::default())),
        }
    }

    /// The [`LeadThinkingSink`] to hand the planner. Each call appends to the
    /// per-kind buffer and emits a merged chunk when a threshold trips.
    pub fn sink(&self) -> LeadThinkingSink {
        let emitter = self.emitter.clone();
        let run_id = self.run_id.clone();
        let phase = self.phase.clone();
        let buf = Arc::clone(&self.buf);
        Arc::new(move |kind: LeadDeltaKind, delta: &str| {
            if delta.is_empty() {
                return;
            }
            let now = std::time::Instant::now();
            // Compute (under the lock) whether a flush is due and, if so, take the
            // coalesced chunk + its wire kind. Then DROP the guard before the
            // broadcast (the bus does its own locking — never nest the two).
            let to_emit: Option<(&'static str, String)> = {
                let mut guard = match buf.lock() {
                    Ok(g) => g,
                    // A poisoned lock means a prior panic mid-stream; skip rather
                    // than re-panic (lead thinking is best-effort observability).
                    Err(_) => return,
                };
                match kind {
                    LeadDeltaKind::Reasoning => {
                        guard.reasoning.push_str(delta);
                        let due = guard
                            .reasoning_last
                            .map(|t| now.duration_since(t) >= THROTTLE_FLUSH_INTERVAL)
                            .unwrap_or(true)
                            || guard.reasoning.chars().count() >= THROTTLE_FLUSH_CHARS;
                        if due {
                            guard.reasoning_last = Some(now);
                            Some(("reasoning", std::mem::take(&mut guard.reasoning)))
                        } else {
                            None
                        }
                    }
                    LeadDeltaKind::Text => {
                        guard.text.push_str(delta);
                        let due = guard
                            .text_last
                            .map(|t| now.duration_since(t) >= THROTTLE_FLUSH_INTERVAL)
                            .unwrap_or(true)
                            || guard.text.chars().count() >= THROTTLE_FLUSH_CHARS;
                        if due {
                            guard.text_last = Some(now);
                            Some(("text", std::mem::take(&mut guard.text)))
                        } else {
                            None
                        }
                    }
                }
            };
            if let Some((wire_kind, chunk)) = to_emit {
                emitter.emit_lead_thinking(&run_id, &phase, wire_kind, Some(&chunk), None, false);
            }
        })
    }

    /// Flush any residual buffered bytes (both kinds). MUST be called once after
    /// the lead completion returns so trailing deltas are never dropped.
    pub fn flush(&self) {
        let (reasoning, text) = {
            let mut guard = match self.buf.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            (std::mem::take(&mut guard.reasoning), std::mem::take(&mut guard.text))
        };
        if !reasoning.is_empty() {
            self.emitter.emit_lead_thinking(&self.run_id, &self.phase, "reasoning", Some(&reasoning), None, false);
        }
        if !text.is_empty() {
            self.emitter.emit_lead_thinking(&self.run_id, &self.phase, "text", Some(&text), None, false);
        }
    }
}

/// Per-model user-authored descriptions, keyed by `(provider_id, model)`.
///
/// Built from the providers' `model_descriptions` JSON (Task 1) and threaded
/// into the planning prompt so the lead model can pick the best-matching model
/// per task. A missing key means "no description" (rendered as `-`).
type DescriptionMap = HashMap<(String, String), String>;


/// Produces a task DAG from a goal. The Run engine consumes the result.
#[async_trait]
pub trait PlanProducer: Send + Sync {
    /// 把目标拆成任务 DAG。`members` 是 fleet 成员快照(供按 index 分派)。
    ///
    /// `sink`(B2): an OPTIONAL lead-thinking sink. When `Some`, the producer
    /// streams the lead's reasoning/draft deltas through it (the engine fans them
    /// out over WebSocket); when `None`, the producer takes the byte-identical
    /// non-streaming path — behavior is EXACTLY as before the sink existed.
    async fn produce(
        &self,
        goal: &str,
        members: &[FleetMember],
        sink: Option<&LeadThinkingSink>,
    ) -> Result<PlannedDag, AppError>;

    /// UC-3a: intelligently RE-ADJUST an in-progress run from a free-form INTENT.
    /// The lead (one-shot, no persistent session) sees the intent + the CURRENT
    /// run state (`tasks` + `deps`) and JUDGES, per task, whether to KEEP the
    /// completed work or re-decompose, emitting an [`AdjustedPlan`]. `members` is
    /// the run's fleet snapshot (used only to derive the lead model — assignment
    /// of the resulting NEW tasks is the service's job). **Fail-soft to an ERROR**
    /// (never a fallback): a garbled plan returns a `BadRequest` so the caller
    /// leaves the run untouched.
    ///
    /// `sink`(B2): same OPTIONAL lead-thinking sink as [`produce`](Self::produce).
    /// As of B4 the production call site streams via the engine's `phase="adjust"`
    /// sink — the adjust LLM call now runs in `compute_adjusted_plan` OUTSIDE the
    /// per-run lock (the lock only wraps `apply_adjusted_plan`'s pure-DB reconcile),
    /// so streaming here never extends lock hold time.
    ///
    /// Default impl errors — only the production [`LlmPlanProducer`] and the test
    /// producers that exercise `adjust` override it (most planners only `produce`).
    async fn adjust(
        &self,
        _intent: &str,
        _tasks: &[RunTask],
        _deps: &[RunTaskDep],
        _members: &[FleetMember],
        _sink: Option<&LeadThinkingSink>,
    ) -> Result<AdjustedPlan, AppError> {
        Err(AppError::BadRequest(
            "this PlanProducer does not support adjust".to_string(),
        ))
    }

    /// B2: synthesize a coherent RUN-LEVEL summary from a completed run's task
    /// outputs in ONE lead-model one-shot call (NO persistent session — exactly
    /// the shape of [`produce`](Self::produce)). `tasks_digest` is a compact,
    /// already-truncated rendering of the run goal + every task's title/role/
    /// status/output (built by the engine via [`build_summary_user_prompt`]);
    /// `members` is the run's fleet snapshot, used ONLY to derive the lead model
    /// (the same `pick_lead` contract the planner uses). Returns the synthesized
    /// summary text.
    ///
    /// `sink`(B2): same OPTIONAL lead-thinking sink as [`produce`](Self::produce).
    /// The engine wires a `phase="summarize"` sink here (the summarize call already
    /// runs OUTSIDE the per-run lock, so streaming it is safe).
    ///
    /// Default impl ERRORS — only the production [`LlmPlanProducer`] (and the
    /// summary tests' stub producers) override it. The engine treats ANY error
    /// (or a blank result) as a signal to FALL BACK to the mechanical
    /// `aggregate_summary` concat: the summarization is best-effort observability
    /// and MUST NEVER fail or block a run.
    async fn summarize(
        &self,
        _goal: &str,
        _tasks_digest: &str,
        _members: &[FleetMember],
        _sink: Option<&LeadThinkingSink>,
    ) -> Result<String, AppError> {
        Err(AppError::BadRequest(
            "this PlanProducer does not support summarize".to_string(),
        ))
    }
}

/// Production planner: a single structured LLM call against a "lead" model
/// yields a [`PlannedDag`] JSON, parsed fail-soft via [`parse_plan`].
///
/// Holds the `provider_repo` plus the `encryption_key` and `workspace` that
/// [`resolve_provider_config`] needs to materialize a `Config` from the
/// `lead` provider row (mirrors `nomifun-idmm`'s `LiveCompleter`).
pub struct LlmPlanProducer {
    provider_repo: Arc<dyn IProviderRepository>,
    encryption_key: [u8; 32],
    workspace: PathBuf,
    lead: ProviderWithModel,
}

impl LlmPlanProducer {
    /// Build a planner against the `lead` model. `encryption_key` / `workspace`
    /// are required to resolve the provider config for the LLM call (the brief
    /// signature is `new(provider_repo, lead)`; these two are adapted in to
    /// satisfy `resolve_provider_config`, matching the IDMM sidecar pattern).
    pub fn new(
        provider_repo: Arc<dyn IProviderRepository>,
        encryption_key: [u8; 32],
        workspace: impl Into<PathBuf>,
        lead: ProviderWithModel,
    ) -> Self {
        Self {
            provider_repo,
            encryption_key,
            workspace: workspace.into(),
            lead,
        }
    }
}

/// Pick the planning "lead" model from the fleet members.
///
/// The app wires `LlmPlanProducer` with an EMPTY placeholder `lead`
/// (`provider_id:""`, `model:""`), which `resolve_provider_config` rejects
/// before `parse_plan`'s fail-soft can ever run — so every real run would stall
/// in `planning`. The real provider+model live on the fleet members, so derive
/// the lead from the FIRST member that carries BOTH a non-empty `provider_id`
/// AND a non-empty `model` (mirroring the Nomi-engine member contract in
/// `worker.rs`). If no member qualifies, fall back to the construction-time
/// `lead` override.
fn pick_lead(members: &[FleetMember], fallback: &ProviderWithModel) -> ProviderWithModel {
    for m in members {
        if let (Some(pid), Some(model)) = (m.provider_id.as_ref(), m.model.as_ref()) {
            if !pid.is_empty() && !model.is_empty() {
                return ProviderWithModel {
                    provider_id: pid.clone(),
                    model: model.clone(),
                    use_model: Some(model.clone()),
                };
            }
        }
    }
    fallback.clone()
}

#[async_trait]
impl PlanProducer for LlmPlanProducer {
    async fn produce(
        &self,
        goal: &str,
        members: &[FleetMember],
        sink: Option<&LeadThinkingSink>,
    ) -> Result<PlannedDag, AppError> {
        // Derive the lead from the fleet members; `self.lead` is the
        // construction-time override/fallback only (the app wires it empty).
        let lead = pick_lead(members, &self.lead);

        // The model to plan with: prefer the explicit use_model alias, else model.
        let model = lead.use_model.as_deref().unwrap_or(&lead.model);

        let cfg = resolve_provider_config(
            &self.provider_repo,
            &self.encryption_key,
            &lead.provider_id,
            model,
            self.workspace.as_path(),
        )
        .await?;

        // Build the (provider_id, model) → description map so the prompt can
        // surface each member's user-authored model description. Fetch every
        // provider once via `list()` (cheaper than N `find_by_id` calls and the
        // member set is small), then decode each provider's `model_descriptions`
        // JSON fail-soft. A repo error here MUST NOT fail the plan — descriptions
        // are an optimization, so degrade to an empty map (all `desc=-`).
        let descriptions = match self.provider_repo.list().await {
            Ok(providers) => build_description_map(&providers, members),
            Err(err) => {
                tracing::warn!(error = %err, "failed to list providers for plan descriptions; planning without them");
                DescriptionMap::new()
            }
        };

        let user = build_plan_user_prompt(goal, members, &descriptions);
        // With a sink, stream the lead deltas through it (the returned text is STILL
        // just the TextDelta concat — byte-identical to one_shot); without a sink,
        // take the non-streaming one-shot path (zero change).
        let raw = run_lead_completion(&cfg, PLAN_SYSTEM, user, PLAN_MAX_TOKENS, sink).await?;
        if let Some(dag) = parse_plan_opt(&raw) {
            return Ok(dag);
        }

        // The lead did NOT emit a parseable task DAG — common with weak / low-
        // reasoning "flash" models. Surface a NON-SILENT warning through the lead-
        // thinking stream (best-effort) so the user learns the auto-decomposition
        // failed rather than silently seeing one node, then degrade to the single-
        // task fallback so the engine still has something. The conversation entry's
        // `interactive` default then PARKS the run for approval — a failed plan is
        // never auto-executed.
        //
        // **No retry.** A second synchronous LLM call here doubled planning latency;
        // on the conversation path (a SYNCHRONOUS `nomi_run_create` tool call) that
        // blocked the lead turn long enough for a slow/weak model to re-invoke the
        // tool every ~60s → 会话9's multiple orphaned `planning` runs + a 200s+
        // "stuck" turn. A single attempt bounds latency; the conversation path also
        // now plans in the BACKGROUND (see caps_orchestrator) so it never blocks.
        tracing::warn!(raw_len = raw.len(), "planner output unparseable; using single-task fallback DAG");
        if let Some(sink) = sink {
            (sink)(LeadDeltaKind::Text, PLAN_FALLBACK_NOTICE);
        }
        Ok(fallback_dag(goal))
    }

    async fn adjust(
        &self,
        intent: &str,
        tasks: &[RunTask],
        deps: &[RunTaskDep],
        members: &[FleetMember],
        sink: Option<&LeadThinkingSink>,
    ) -> Result<AdjustedPlan, AppError> {
        // Same one-shot lead-call shape as `produce`: derive the lead from the
        // fleet snapshot (the app wires `self.lead` empty), resolve its config,
        // and ask for the adjusted-plan JSON. There is NO persistent session — a
        // single structured completion, exactly like planning.
        let lead = pick_lead(members, &self.lead);
        let model = lead.use_model.as_deref().unwrap_or(&lead.model);
        let cfg = resolve_provider_config(
            &self.provider_repo,
            &self.encryption_key,
            &lead.provider_id,
            model,
            self.workspace.as_path(),
        )
        .await?;

        let user = build_adjust_user_prompt(intent, tasks, deps);
        // B2: sink-aware (the production caller passes None until B4 moves this LLM
        // call out of the per-run lock; honoring a Some sink here is correct + free).
        let raw = run_lead_completion(&cfg, ADJUST_SYSTEM, user, PLAN_MAX_TOKENS, sink).await?;

        // Fail-soft TO AN ERROR (never a fallback): a garbled adjusted plan must
        // not mangle the existing run — the caller surfaces the BadRequest.
        parse_adjusted_plan(&raw)
    }

    async fn summarize(
        &self,
        goal: &str,
        tasks_digest: &str,
        members: &[FleetMember],
        sink: Option<&LeadThinkingSink>,
    ) -> Result<String, AppError> {
        // Same one-shot lead-call shape as `produce`/`adjust`: derive the lead
        // from the fleet snapshot (the app wires `self.lead` empty), resolve its
        // config, and ask for a synthesized run summary. NO persistent session —
        // a single completion. The engine wraps this call fail-soft: any error
        // here just means it falls back to the mechanical `aggregate_summary`.
        let lead = pick_lead(members, &self.lead);
        let model = lead.use_model.as_deref().unwrap_or(&lead.model);
        let cfg = resolve_provider_config(
            &self.provider_repo,
            &self.encryption_key,
            &lead.provider_id,
            model,
            self.workspace.as_path(),
        )
        .await?;

        let user = build_summary_user_prompt(goal, tasks_digest);
        // B2: sink-aware. The summarize call already runs OUTSIDE the per-run lock,
        // so streaming it (phase="summarize") is safe; None keeps the one-shot path.
        let raw = run_lead_completion(&cfg, SUMMARY_SYSTEM, user, SUMMARY_MAX_TOKENS, sink).await?;

        // Trim and hand the synthesized text back. A blank result is left for the
        // engine to detect (it falls back to the mechanical concat on blank too).
        Ok(raw.trim().to_string())
    }
}

/// Run one lead-model completion for the planner, honoring an optional lead-
/// thinking `sink`. Uses [`streaming_completion_text_or_reasoning`]: it forwards
/// BOTH `Text` and `Reasoning` deltas to the sink (so the canvas shows the planning
/// process) and — crucially — RETURNS the reasoning text when the model left
/// `content` empty. Reasoning models like StepFun `step-*` emit their answer (incl.
/// the requested plan JSON) only in the reasoning channel; the old text-only drain
/// returned `""` for them → the planner fell back to a single-task DAG (会话10).
/// `parse_plan`/`parse_adjusted_plan` then extract the JSON from whichever buffer
/// carried it.
async fn run_lead_completion(
    cfg: &nomifun_ai_agent::nomi_config::config::Config,
    system: &str,
    user: String,
    max_tokens: u32,
    sink: Option<&LeadThinkingSink>,
) -> Result<String, AppError> {
    streaming_completion_text_or_reasoning(
        cfg,
        system,
        vec![user_message(user)],
        max_tokens,
        |kind, delta| {
            if let Some(sink) = sink {
                (sink)(kind.into(), delta);
            }
        },
    )
    .await
}

/// How many tokens the lead may use for its one-shot run-summary completion.
/// Smaller than the planning budget — a summary is prose, not a structured DAG.
const SUMMARY_MAX_TOKENS: u32 = 1024;

/// System prompt for B2 run-level summarization: instruct the lead to SYNTHESIZE
/// (not concatenate) a coherent summary of a finished multi-agent run from its
/// task outputs. Plain prose out — no JSON, no fences.
pub const SUMMARY_SYSTEM: &str = "You are the lead agent of a multi-agent fleet writing the FINAL summary of a run that just completed. \
You are given the run's GOAL and a digest of every task (its title, role, status, and a short output summary). \
SYNTHESIZE a single coherent summary of what the run accomplished against the goal — do NOT merely concatenate the task outputs. \
Resolve overlaps, weave the per-task results into a connected whole, lead with the overall outcome, then the key deliverables/findings, and note anything unfinished or skipped. \
Write in the SAME language as the goal. Be concise (a few short paragraphs or a tight bullet list). \
Output ONLY the summary prose — no preamble, no JSON, no markdown code fences.";

/// Build the `summarize` user message: the run GOAL followed by the engine-built
/// task digest. The digest is already compact + truncated (the engine renders it
/// via the run's task rows), so this just frames it for the lead.
pub fn build_summary_user_prompt(goal: &str, tasks_digest: &str) -> String {
    let mut out = String::new();
    out.push_str("GOAL:\n");
    out.push_str(goal.trim());
    out.push_str("\n\nTASKS (one per line — title | role | status | output):\n");
    if tasks_digest.trim().is_empty() {
        out.push_str("(no task outputs)\n");
    } else {
        out.push_str(tasks_digest.trim_end());
        out.push('\n');
    }
    out.push_str("\nWrite the synthesized run summary now.");
    out
}

/// System prompt: instruct the model to output ONLY a strict-JSON task DAG.
const PLAN_SYSTEM: &str = "You are a planning supervisor for a multi-agent fleet. \
Decompose the user's GOAL into an executable task DAG and output ONLY a single JSON object — \
no prose, no explanation, no markdown fences. \
The JSON object MUST have exactly this shape:\n\
{\"tasks\":[{\"title\":string,\"spec\":string,\"role\":string,\"kind\":string?,\"pattern_config\":string?,\"task_profile\":{\"kind\":string,\"needs_vision\":bool,\"needs_long_context\":bool,\"needs_high_reasoning\":bool,\"bulk\":bool}?,\"depends_on\":[int],\"member_index\":int?,\"rationale\":string?}]}\n\
Rules:\n\
- \"depends_on\" lists the 0-based indices of EARLIER tasks (smaller index) this task depends on; the graph MUST be acyclic.\n\
- \"member_index\" is the 0-based index into the provided MEMBERS list, if you want to pre-assign the task to a member; omit it to let the engine route automatically.\n\
- Each member row carries a \"desc\" column: the user-authored description of that member's model. PREFER the member whose \"desc\" best matches the task and set \"member_index\" accordingly; \"desc=-\" means no description is available.\n\
- MATCH THE MODEL TO TASK DIFFICULTY (性价比 / cost-effectiveness): you are the 主模型 designing which model runs each task from the user's range. Assign cheaper/faster models to simple, mechanical, well-specified, or bulk tasks, and reserve the stronger/pricier models for hard, ambiguous, or reasoning-heavy tasks — do NOT route every task to the strongest model. Judge each member's relative cost/capability from its \"desc\" and \"strengths\"; the FIRST member is the 主模型 (a capable default when unsure). The goal is the best overall result per unit cost, not maximum power on every node.\n\
- \"role\" is a SHORT Chinese role name naming the kind of work this task is (例如 规划/前端/后端/测试/设计/文档/研究). Give every task a role so the roles a run used can later be distilled into reusable assistants. Keep it to 2–4 字; reuse the same role name across tasks of the same kind.\n\
- \"kind\" is the task's EXECUTION MODE; omit it (or use \"agent\") for a normal single-agent task — this is the DEFAULT and should be the vast majority of tasks. The other values are:\n\
  - \"synthesis\": a task that MERGES/synthesizes its dependency tasks' outputs into one coherent final result. Use it for a closing step like 「综合/合并上述产出，写出最终的 X」: set \"kind\":\"synthesis\" and make \"depends_on\" list every task whose output it should merge. A synthesis task needs no tools of its own — it reasons over the upstream results you give it.\n\
  - \"verify\": a NO-AGENT aggregator that VALIDATES an earlier task's result by majority/quorum vote of independent skeptics, then GATES the work that depends on it. Use it when a result must be checked before downstream work proceeds (correctness-critical output, a plan/spec others will build on). To set up a verify gate emit, for the task T you want to validate:\n\
    1) N independent SKEPTIC tasks (N usually 3) — each is a normal \"kind\":\"agent\" task that \"depends_on\":[T] and whose \"spec\" instructs it to CRITICALLY and INDEPENDENTLY evaluate T's result and OUTPUT ONLY a strict-JSON verdict: {\\\"pass\\\": true|false, \\\"critique\\\": \\\"<one-line reason>\\\"}. Phrase each skeptic's spec a little differently so they don't all make the same mistake.\n\
    2) ONE \"kind\":\"verify\" task that \"depends_on\" ALL N skeptics. Its \"pattern_config\" carries the vote policy as a JSON string: \"{\\\"vote\\\":\\\"majority\\\"}\" (default — pass iff > half pass), \"{\\\"vote\\\":\\\"unanimous\\\"}\" (pass iff every skeptic passes), or \"{\\\"vote\\\":{\\\"threshold\\\":K}}\" (pass iff at least K pass). The verify task runs NO agent — the engine tallies the skeptics' verdicts itself — so give it an empty/short spec.\n\
    3) The downstream work that must only run on a PASS \"depends_on\" the verify task. On a FAIL verdict the engine SKIPS that downstream automatically (it never runs unvalidated).\n\
  - \"judge\": a NO-AGENT aggregator that PICKS THE BEST among M candidate results by averaging/ranking the scores of N independent judges. Use it to choose one winner among alternatives (e.g. several candidate designs/drafts/approaches). To set up a judge contest emit:\n\
    1) M CANDIDATE tasks (usually a fan-out group of \"kind\":\"agent\" siblings) — each produces ONE alternative. Their ORDER matters: candidate i is index i in every judge's ballot.\n\
    2) N independent JUDGE tasks (N usually 3) — each is a normal \"kind\":\"agent\" task that \"depends_on\" ALL M candidates and whose \"spec\" instructs it to SCORE EVERY candidate (0.0–1.0, higher = better) and OUTPUT ONLY a strict-JSON ballot scoring all M, e.g. {\\\"scores\\\":[0.8,0.3,0.6]} (array indexed by candidate order) or {\\\"scores\\\":{\\\"0\\\":0.8,\\\"1\\\":0.3,\\\"2\\\":0.6}} (object keyed by candidate index). Phrase each judge's spec a little differently so they don't all weigh the same way.\n\
    3) ONE \"kind\":\"judge\" task that \"depends_on\" ALL N judges. Its \"pattern_config\" carries the aggregation policy as a JSON string: \"{\\\"aggregate\\\":\\\"mean\\\"}\" (default — average each candidate's scores across judges; winner = highest mean) or \"{\\\"aggregate\\\":\\\"borda\\\"}\" (each judge RANKS the candidates by its scores, award M-1…0 Borda points, sum across judges; winner = highest total). Optionally add \"{\\\"candidates\\\":M}\" to pin the candidate count. The judge task runs NO agent — the engine aggregates the ballots itself — so give it an empty/short spec. It REPORTS the winning candidate index in its output (downstream can build on the winner).\n\
  - \"loop\": a NO-AGENT controller that RE-RUNS one BODY task in place, iterating until a stop condition is met OR a HARD iteration cap is hit. Use it for iterative refinement — keep improving/retrying ONE task until it is good enough (e.g. 「反复打磨这段文案直到没有可改之处」, 「重试直到测试通过」). To set up a loop emit EXACTLY two tasks:\n\
    1) a BODY \"kind\":\"agent\" task that does one round of the work. Its \"spec\" should produce output that can be re-run/refined each round; it sees its own previous round's output as upstream context.\n\
    2) ONE \"kind\":\"loop\" task that \"depends_on\":[BODY] (the body is its ONLY dependency). Its \"pattern_config\" is a JSON string carrying a REQUIRED hard cap and a stop criterion: \"{\\\"max_iter\\\":N,\\\"stop\\\":{...}}\". \"max_iter\" (a small N like 3–5) is the HARD upper bound — the loop ALWAYS stops at the cap even if the criterion never fires (this guarantees termination). \"stop\" is one of: \"{\\\"kind\\\":\\\"max_iter\\\"}\" (stop only at the cap), \"{\\\"kind\\\":\\\"predicate\\\",\\\"done_marker\\\":\\\"DONE\\\"}\" (stop early once the body output contains the marker text, or strict JSON {\\\"done\\\":true}; instruct the body to emit the marker when it judges itself finished), \"{\\\"kind\\\":\\\"dry\\\",\\\"quiet_rounds\\\":K}\" (stop early once K consecutive rounds produce the SAME body output — no further change), or \"{\\\"kind\\\":\\\"approved\\\"}\" (a VERDICT-GATED loop — stop early once the body's output PASSES a self-check: instruct the body to SELF-ASSESS its round and emit a strict-JSON verdict {\\\"pass\\\":true|false} (or end with a PASS/FAIL marker); the loop keeps iterating until the body's own verdict PASSES, capped by max_iter). The loop task runs NO agent — the engine re-dispatches the body and evaluates the stop condition itself — so give it an empty/short spec. Downstream work \"depends_on\" the LOOP task (NOT the body), so it waits for the whole iteration to finish.\n\
- FAN-OUT (parallel variants / shards) is expressed by PLANNING, NOT a special kind: when a step benefits from doing the same work in parallel (e.g. N independent drafts, N shards of a corpus, N candidate approaches), emit MULTIPLE sibling tasks that all have \"kind\":\"agent\" and SHARE the same \"pattern_config\" group tag — a JSON string like \"{\\\"group\\\":\\\"<label>\\\"}\" (e.g. \"{\\\"group\\\":\\\"drafts\\\"}\"). Then add ONE downstream task (usually \"kind\":\"synthesis\") that \"depends_on\" ALL of those siblings to combine them. The engine runs the siblings in parallel automatically.\n\
- COMPOSING PATTERNS: the kinds above are building blocks — COMBINE them when the goal calls for it. Each pattern is just a task plus its \"depends_on\" edges, so you compose by CHAINING one pattern's aggregator/result into the next pattern's inputs. Reach for a composition only when the goal genuinely needs it (do not nest patterns gratuitously). The most useful compositions:\n\
  - FAN-OUT → JUDGE → SYNTHESIS (explore alternatives, pick the best, then build on it): emit M candidate \"kind\":\"agent\" siblings sharing a \"{\\\"group\\\":\\\"candidates\\\"}\" tag; N judge \"kind\":\"agent\" tasks each \"depends_on\" ALL M candidates emitting a \"{\\\"scores\\\":[..]}\" ballot; ONE \"kind\":\"judge\" task \"depends_on\" ALL N judges (it REPORTS the winning candidate index); then a closing \"kind\":\"synthesis\" (or plain \"kind\":\"agent\") task \"depends_on\" the judge that takes the winner and produces the final deliverable.\n\
  - VERIFY-GATE → DOWNSTREAM (validate a result before anything builds on it): emit the task T to validate, then N skeptic \"kind\":\"agent\" tasks each \"depends_on\":[T] emitting a \"{\\\"pass\\\":bool}\" verdict, then ONE \"kind\":\"verify\" task \"depends_on\" ALL N skeptics with a \"{\\\"vote\\\":...}\" policy, then the downstream work (a \"kind\":\"synthesis\" merge or a plain \"kind\":\"agent\" build step) \"depends_on\" the VERIFY task — on a FAIL verdict the engine SKIPS that downstream automatically so unvalidated work never runs.\n\
  - LOOP WITH AN INTERNAL CHECK (iterate until the result is good enough): emit a BODY \"kind\":\"agent\" task that BOTH produces a round of work AND self-assesses it, ending its output with a strict-JSON verdict {\\\"pass\\\":true|false} (or a PASS/FAIL marker); then ONE \"kind\":\"loop\" task \"depends_on\":[BODY] with \"pattern_config\":\"{\\\"max_iter\\\":N,\\\"stop\\\":{\\\"kind\\\":\\\"approved\\\"}}\" — the loop re-runs the body until ITS OWN verdict PASSES, bounded by the max_iter hard cap (the body sees its prior round's output, so it refines until it approves itself or the cap stops it). Downstream \"depends_on\" the LOOP.\n\
- \"pattern_config\" is a raw JSON STRING (or omit it). It carries the fan-out \"group\" tag, a verify task's \"vote\" policy, a judge task's \"aggregate\" policy, OR a loop task's \"max_iter\"+\"stop\" criterion (see above); leave it out for ordinary tasks.\n\
- \"task_profile\", \"member_index\" and \"rationale\" are optional.\n\
- \"title\" is a short imperative label; \"spec\" is the full instruction the worker agent will execute.\n\
- Keep the plan minimal but complete: one task if the goal is atomic, several with dependencies if it must be staged. Do NOT over-use synthesis/fan-out/verify/judge/loop — reach for them only when the goal genuinely benefits from merging multiple outputs, parallel variants, validating a result before building on it, choosing the best among alternatives, or iteratively refining a single result until it stops improving.\n\
Output the JSON object and nothing else.";

/// Build the `(provider_id, model) → description` map for the prompt.
///
/// For each distinct `provider_id` referenced by a member, decode that
/// provider's `model_descriptions` JSON (`{model_id: description}`) and record
/// the description for every `(provider_id, model)` a member actually uses.
///
/// **Fail-soft on every axis** — descriptions are an optimization, never a hard
/// dependency:
/// - a provider with no row, `model_descriptions == None`, or the Task-1 default
///   `"{}"` contributes nothing;
/// - a malformed `model_descriptions` JSON is skipped (no entries) with a warn,
///   not propagated as an error;
/// - a blank/whitespace-only description is dropped (treated as "no description").
fn build_description_map(providers: &[Provider], members: &[FleetMember]) -> DescriptionMap {
    // Index providers by id for O(1) lookup as we walk the members.
    let by_id: HashMap<&str, &Provider> = providers.iter().map(|p| (p.id.as_str(), p)).collect();

    // Decode each referenced provider's model_descriptions once, fail-soft.
    let mut decoded: HashMap<&str, HashMap<String, String>> = HashMap::new();
    let mut out = DescriptionMap::new();

    for m in members {
        let (Some(pid), Some(model)) = (m.provider_id.as_deref(), m.model.as_deref()) else {
            continue;
        };
        if pid.is_empty() || model.is_empty() {
            continue;
        }

        // Lazily decode this provider's descriptions JSON the first time we see it.
        let table = decoded.entry(pid).or_insert_with(|| {
            let Some(provider) = by_id.get(pid) else {
                return HashMap::new();
            };
            let raw = provider.model_descriptions.as_deref().unwrap_or("{}");
            match serde_json::from_str::<HashMap<String, String>>(raw) {
                Ok(map) => map,
                Err(err) => {
                    tracing::warn!(
                        provider_id = pid,
                        error = %err,
                        "provider model_descriptions is not a JSON object; ignoring"
                    );
                    HashMap::new()
                }
            }
        });

        if let Some(desc) = table.get(model) {
            let trimmed = desc.trim();
            if !trimmed.is_empty() {
                out.insert((pid.to_string(), model.to_string()), trimmed.to_string());
            }
        }
    }
    out
}

/// Build the user message: the goal plus a compact member roster.
fn build_plan_user_prompt(
    goal: &str,
    members: &[FleetMember],
    descriptions: &DescriptionMap,
) -> String {
    let mut out = String::new();
    out.push_str("GOAL:\n");
    out.push_str(goal);
    out.push_str("\n\nMEMBERS (index, agent_id, role_hint, strengths, desc):\n");
    if members.is_empty() {
        out.push_str("(none — plan without pre-assigning member_index)\n");
    } else {
        for (i, m) in members.iter().enumerate() {
            let role = m.role_hint.as_deref().unwrap_or("-");
            let strengths = m
                .capability_profile
                .as_ref()
                .map(|p| p.strengths.join("/"))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "-".to_string());
            // Description column. PRIMARY source (P4 Task 3, Change 3): the
            // member's own `description` — Task 2 populates it for assistant-backed
            // members (the assistant's description) and decorates bare model-range
            // members that have a provider model description. FALLBACK (P3): the
            // `(provider_id, model)` → provider-`model_descriptions` map, kept for
            // bare members whose `description` was not decorated (no provider desc
            // OR an older snapshot without the field). Missing on both → "-".
            let member_desc = m.description.as_deref().map(str::trim).filter(|s| !s.is_empty());
            let desc = member_desc.unwrap_or_else(|| match (m.provider_id.as_deref(), m.model.as_deref()) {
                (Some(pid), Some(model)) => descriptions
                    .get(&(pid.to_string(), model.to_string()))
                    .map(String::as_str)
                    .unwrap_or("-"),
                _ => "-",
            });
            out.push_str(&format!(
                "{i}. {} | role={role} | strengths={strengths} | desc={desc}\n",
                m.agent_id
            ));
        }
    }
    out.push_str("\nReturn ONLY the JSON task DAG.");
    out
}

/// Parse the raw model text into a [`PlannedDag`], **fail-soft**.
///
/// Strips ```json/``` fences and surrounding prose, locates the first balanced
/// `{...}` JSON object, and deserializes it. On ANY failure — no JSON object,
/// malformed JSON, wrong shape, or an empty `tasks` array — logs a `warn!` and
/// returns a single-task fallback DAG derived from `goal` (so the engine always
/// has an executable plan).
pub fn parse_plan(raw: &str, goal: &str) -> PlannedDag {
    match parse_plan_opt(raw) {
        Some(dag) => dag,
        None => {
            tracing::warn!(
                raw_len = raw.len(),
                "planner output unparseable/empty (no valid JSON task DAG); using fallback DAG"
            );
            fallback_dag(goal)
        }
    }
}

/// Parse the raw model text into a [`PlannedDag`], returning `None` on ANY failure
/// (no JSON object, malformed JSON, wrong shape, or an empty `tasks` array) —
/// **without** the goal-derived fallback. This is the non-fallback core so callers
/// that need to KNOW planning failed (to retry, or to warn the user) can branch on
/// the `None`; [`parse_plan`] wraps this with the single-task fallback so the engine
/// always has an executable plan.
pub fn parse_plan_opt(raw: &str) -> Option<PlannedDag> {
    match extract_json_object(raw).and_then(|json| serde_json::from_str::<PlannedDag>(&json).ok()) {
        Some(dag) if !dag.tasks.is_empty() => Some(dag),
        _ => None,
    }
}

/// Extract the first balanced top-level `{...}` substring from `raw`,
/// after stripping any markdown code fences. Returns `None` if no balanced
/// object is found. Quote/escape aware so braces inside strings don't confuse
/// the brace counter.
fn extract_json_object(raw: &str) -> Option<String> {
    // Strip code fences first; the model is told not to use them, but be robust.
    let cleaned = raw.replace("```json", "").replace("```JSON", "").replace("```", "");

    let bytes = cleaned.as_bytes();
    let start = cleaned.find('{')?;

    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(cleaned[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Single-task fallback DAG: the whole goal as one task assigned to member 0.
fn fallback_dag(goal: &str) -> PlannedDag {
    PlannedDag {
        tasks: vec![PlannedTask {
            title: truncate_title(goal),
            spec: goal.to_string(),
            task_profile: None,
            depends_on: vec![],
            member_index: Some(0),
            rationale: Some("fallback: planner output unparseable".to_string()),
            role: None,
            kind: "agent".to_string(),
            pattern_config: None,
        }],
    }
}

/// Truncate the goal into a short title (~`FALLBACK_TITLE_LEN` chars),
/// respecting char boundaries (CJK-safe).
fn truncate_title(goal: &str) -> String {
    let trimmed = goal.trim();
    if trimmed.chars().count() <= FALLBACK_TITLE_LEN {
        return trimmed.to_string();
    }
    let truncated: String = trimmed.chars().take(FALLBACK_TITLE_LEN).collect();
    format!("{truncated}…")
}

// ===========================================================================
// UC-3a: conversation-driven intelligent re-adjust (adjust + reconcile).
//
// The user expresses a free-form INTENT against an EXISTING run; the lead model
// (one-shot, no persistent session) sees the intent + the CURRENT run state and
// JUDGES — per task — whether to KEEP the completed work or RE-DECOMPOSE. It
// outputs an "adjusted plan" whose nodes are EITHER `{"keep":"<existing_id>"}`
// (preserve that task + its output) OR a NEW task. The service then RECONCILEs
// the current plan to the adjusted one (see `RunService::adjust`).
// ===========================================================================

/// How many chars of a task's `output_summary` to surface in the adjust prompt.
const ADJUST_OUTPUT_SUMMARY_LEN: usize = 300;

/// A dependency reference inside an adjusted-plan NEW task. Untagged so the lead
/// may write EITHER an existing kept `task_id` (a JSON string) OR a 0-based index
/// into the adjusted plan's `tasks` array pointing at a NEW task (a JSON int).
/// The reconcile step resolves both to concrete task ids.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
#[serde(untagged)]
pub enum AdjustedDepRef {
    /// An existing task id the adjusted plan KEPT (a string).
    Kept(String),
    /// A 0-based index into the adjusted plan's `tasks` array (a NEW task).
    NewIndex(usize),
}

/// One NEW task in an adjusted plan: a task the lead chose to ADD (or to replace
/// a dropped one with). Mirrors the create-time task fields. `kind` defaults to
/// `"agent"` (zero-regression); the engine treats any unknown kind as agent.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AdjustedNewTask {
    pub title: String,
    pub spec: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default = "default_adjusted_kind")]
    pub kind: String,
    #[serde(default)]
    pub pattern_config: Option<String>,
    /// Refs to the nodes this NEW task depends on (kept ids and/or new indices).
    #[serde(default)]
    pub depends_on: Vec<AdjustedDepRef>,
}

fn default_adjusted_kind() -> String {
    "agent".to_string()
}

/// One node of an adjusted plan: untagged so the lead may write EITHER
/// `{"keep":"<existing_task_id>"}` (preserve that task + its completed work) OR a
/// full NEW task object. Serde tries `Keep` first (it has the distinctive `keep`
/// key); a node without `keep` is parsed as a `New` task.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum AdjustedNode {
    /// Keep an existing task (by id) — its status/output/conversation/assignment
    /// are preserved unchanged in reconcile.
    Keep {
        /// The existing `task_id` to preserve.
        keep: String,
    },
    /// A brand-new task to insert (pending) + route + wire.
    New(AdjustedNewTask),
}

/// The adjusted plan the lead produces for `adjust`: an ordered list of nodes,
/// each KEEPing an existing task or describing a NEW one. Existing tasks NOT
/// kept are dropped in reconcile.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AdjustedPlan {
    pub tasks: Vec<AdjustedNode>,
}

/// System prompt for `adjust`: hand the lead the user's INTENT + the CURRENT run
/// state and EMPOWER it to judge, per task, whether to keep the completed work or
/// re-decompose. Output is ONLY the adjusted-plan JSON.
pub const ADJUST_SYSTEM: &str = "You are the lead agent re-adjusting an in-progress multi-agent orchestration. \
You are given the user's INTENT (a free-form instruction) and the CURRENT state of the run: every task with its id, title, spec, role, kind, status, a short output summary (if any), and its dependencies. \
Your job is to produce an ADJUSTED PLAN of the task DAG that best serves the intent. \
Output ONLY a single JSON object — no prose, no explanation, no markdown fences. \
The JSON object MUST have exactly this shape:\n\
{\"tasks\":[ <node> ]}\n\
where each <node> is EITHER\n\
  {\"keep\":\"<existing_task_id>\"}  — preserve that existing task AND its already-completed work (status/output/assignment) untouched, OR\n\
  {\"title\":string,\"spec\":string,\"role\":string?,\"kind\":string?,\"pattern_config\":string?,\"depends_on\":[<ref>]}  — a NEW task to add,\n\
where each <ref> in \"depends_on\" is EITHER an existing KEPT task_id (a JSON string) OR a 0-based index (a JSON integer) into THIS \"tasks\" array pointing at a NEW task earlier in the list.\n\
YOU JUDGE, per task, based on the intent AND the current delivery state:\n\
- KEEP a completed task whose work STILL serves the intent — do NOT waste finished work by re-doing it. Reference it as {\"keep\":\"<id>\"} and (if other nodes build on it) by its id in their \"depends_on\".\n\
- RE-DECOMPOSE / REPLACE what the intent changes: drop the now-obsolete tasks (simply do NOT keep them) and add NEW tasks describing the corrected work.\n\
- ADD new tasks the intent introduces, wiring their \"depends_on\" to the kept upstream work and/or to earlier new tasks.\n\
- A task you neither keep nor replace is DROPPED — only keep what genuinely still helps.\n\
You are NOT constrained to a fixed policy: decide freely how much to preserve vs. rebuild so the resulting DAG delivers the user's intent with the least wasted work.\n\
\"role\" is a SHORT Chinese role name (例如 规划/前端/后端/测试/设计/文档/研究) for a NEW task. \"kind\" is the NEW task's execution mode; omit it (or use \"agent\") for a normal single-agent task (the default and the vast majority). The other kinds (\"synthesis\"/\"verify\"/\"judge\"/\"loop\") and their \"pattern_config\" follow the same conventions as the planner: synthesis merges its dependencies' outputs; a fan-out group is sibling agent tasks sharing \"pattern_config\":\"{\\\"group\\\":\\\"<label>\\\"}\"; a verify gate is N skeptic agent tasks → a \"kind\":\"verify\" task (\"{\\\"vote\\\":...}\") that SKIPS its dependents on a FAIL; a judge contest is M candidate siblings → N judge agents → a \"kind\":\"judge\" task (\"{\\\"aggregate\\\":...}\") reporting the winner; a loop is a BODY agent task → a \"kind\":\"loop\" task \"depends_on\":[body] (\"{\\\"max_iter\\\":N,\\\"stop\\\":{...}}\", where stop can be \"{\\\"kind\\\":\\\"approved\\\"}\" — iterate until the body self-verifies PASS, capped by max_iter). You may also COMPOSE them (fan-out→judge→synthesis, verify-gate→downstream, loop-with-a-self-checking-body) by chaining one pattern's result into the next via \"depends_on\". Reach for any of them only when the intent genuinely benefits.\n\
\"title\" is a short imperative label; \"spec\" is the full instruction the worker will execute.\n\
The graph MUST be acyclic: a NEW task's integer \"depends_on\" indices must point EARLIER in the \"tasks\" array.\n\
Output the JSON object and nothing else.";

/// Build the `adjust` user message: the INTENT + a compact serialization of the
/// CURRENT run state (one line per task). `output_summary` is truncated to
/// [`ADJUST_OUTPUT_SUMMARY_LEN`] chars (CJK-safe) so a long deliverable does not
/// blow the prompt budget. Dependencies are listed by the depended-on task ids
/// so the lead can reference them with `{"keep":"<id>"}` + id-string deps.
pub fn build_adjust_user_prompt(
    intent: &str,
    tasks: &[RunTask],
    deps: &[RunTaskDep],
) -> String {
    let mut out = String::new();
    out.push_str("INTENT:\n");
    out.push_str(intent.trim());
    out.push_str("\n\nCURRENT RUN STATE (one task per line — id | title | role | kind | status | depends_on=[ids] | output):\n");
    if tasks.is_empty() {
        out.push_str("(no tasks yet)\n");
    } else {
        for t in tasks {
            // The ids this task depends on (blocker → this task).
            let dep_ids: Vec<&str> = deps
                .iter()
                .filter(|d| d.blocked_task_id == t.id)
                .map(|d| d.blocker_task_id.as_str())
                .collect();
            let role = t.role.as_deref().unwrap_or("-");
            let summary = t
                .output_summary
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(truncate_summary)
                .unwrap_or_else(|| "-".to_string());
            out.push_str(&format!(
                "{} | {} | role={role} | kind={} | status={} | depends_on={:?} | output={summary}\n",
                t.id, t.title, t.kind, t.status, dep_ids
            ));
        }
    }
    out.push_str("\nReturn ONLY the adjusted-plan JSON object.");
    out
}

/// Truncate a task's `output_summary` to [`ADJUST_OUTPUT_SUMMARY_LEN`] chars,
/// CJK-safe, collapsing inner newlines so it stays on one prompt line.
fn truncate_summary(summary: &str) -> String {
    let one_line: String = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= ADJUST_OUTPUT_SUMMARY_LEN {
        return one_line;
    }
    let truncated: String = one_line.chars().take(ADJUST_OUTPUT_SUMMARY_LEN).collect();
    format!("{truncated}…")
}

/// Parse the raw lead text into an [`AdjustedPlan`], **fail-soft to an ERROR**
/// (NOT a fallback DAG). Unlike [`parse_plan`] — which degrades to a single-task
/// fallback so the engine always has SOMETHING to run — `adjust` mutates an
/// EXISTING run with real completed work, so a garbled adjusted plan must NOT be
/// guessed at: it returns a `BadRequest` the caller surfaces, leaving the run
/// untouched. Strips ```json fences + surrounding prose (reusing
/// [`extract_json_object`]). An empty `tasks` array is rejected (an adjust that
/// keeps + adds nothing would silently wipe the run). Unknown `kind` on a new
/// task is left verbatim (the engine maps it to `agent`).
pub fn parse_adjusted_plan(raw: &str) -> Result<AdjustedPlan, AppError> {
    let json = extract_json_object(raw).ok_or_else(|| {
        AppError::BadRequest("主 agent 调整计划无法解析(未找到 JSON);运行未改动".to_string())
    })?;
    let plan: AdjustedPlan = serde_json::from_str(&json).map_err(|e| {
        AppError::BadRequest(format!("主 agent 调整计划格式无效({e});运行未改动"))
    })?;
    if plan.tasks.is_empty() {
        return Err(AppError::BadRequest(
            "主 agent 调整计划为空(既未保留也未新增);运行未改动".to_string(),
        ));
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::TaskProfile;

    /// Fixed 2-task DAG mock proving the [`PlanProducer`] trait shape. Reused by
    /// the Run engine (Task 6) to drive the scheduler without a live LLM.
    struct MockPlanProducer;

    #[async_trait]
    impl PlanProducer for MockPlanProducer {
        async fn produce(&self, _goal: &str, _members: &[FleetMember], _sink: Option<&LeadThinkingSink>) -> Result<PlannedDag, AppError> {
            Ok(PlannedDag {
                tasks: vec![
                    PlannedTask {
                        title: "Gather".to_string(),
                        spec: "collect sources".to_string(),
                        task_profile: None,
                        depends_on: vec![],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    },
                    PlannedTask {
                        title: "Synthesize".to_string(),
                        spec: "write the report".to_string(),
                        task_profile: None,
                        depends_on: vec![0],
                        member_index: Some(1),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    },
                ],
            })
        }

        async fn adjust(
            &self,
            _intent: &str,
            _tasks: &[RunTask],
            _deps: &[RunTaskDep],
            _members: &[FleetMember],
            _sink: Option<&LeadThinkingSink>,
        ) -> Result<AdjustedPlan, AppError> {
            Ok(AdjustedPlan { tasks: vec![] })
        }
    }

    #[tokio::test]
    async fn mock_plan_producer_returns_fixed_two_task_dag() {
        let planner: Arc<dyn PlanProducer> = Arc::new(MockPlanProducer);
        let dag = planner.produce("anything", &[], None).await.expect("mock never errors");

        assert_eq!(dag.tasks.len(), 2);
        assert_eq!(dag.tasks[0].title, "Gather");
        assert!(dag.tasks[0].depends_on.is_empty());
        assert_eq!(dag.tasks[1].title, "Synthesize");
        assert_eq!(dag.tasks[1].depends_on, vec![0]);
        assert_eq!(dag.tasks[1].member_index, Some(1));
    }

    // ── B2: lead-thinking sink contract ──────────────────────────────────────
    //
    // A producer that mirrors `LlmPlanProducer`'s sink/None branching against a
    // CAPTURED fake delta stream (no live provider — `create_provider` has no
    // mock hook, so the LLM-call layer is exercised in provider_config's tests).
    // It proves the two load-bearing invariants B2 must hold:
    //   1. when a sink is present, every reasoning + text delta reaches it (kind-
    //      tagged), and
    //   2. the parsed `PlannedDag` is IDENTICAL on the Some-sink and None paths
    //      (fail-soft / parse behavior is unchanged by streaming — the returned
    //      text is the TextDelta concat either way).
    struct SinkEchoPlanProducer {
        /// The (kind, delta) stream the producer "receives" from the model.
        stream: Vec<(LeadDeltaKind, &'static str)>,
    }

    #[async_trait]
    impl PlanProducer for SinkEchoPlanProducer {
        async fn produce(
            &self,
            goal: &str,
            _members: &[FleetMember],
            sink: Option<&LeadThinkingSink>,
        ) -> Result<PlannedDag, AppError> {
            // Mirror `run_lead_completion`: forward every delta to a present sink,
            // assemble the answer from TEXT deltas ONLY (reasoning is not part of
            // the answer — exactly the `streaming_completion_kinded` contract).
            let mut text = String::new();
            for (kind, delta) in &self.stream {
                if let Some(sink) = sink {
                    (sink)(*kind, delta);
                }
                if *kind == LeadDeltaKind::Text {
                    text.push_str(delta);
                }
            }
            // The assembled text is the plan JSON; parse it fail-soft (same as prod).
            Ok(parse_plan(&text, goal))
        }
    }

    // 1) With a sink, the producer forwards every reasoning + text delta to it,
    //    kind-tagged and in order.
    #[tokio::test]
    async fn sink_receives_every_delta_kind_tagged() {
        let producer = SinkEchoPlanProducer {
            stream: vec![
                (LeadDeltaKind::Reasoning, "let me plan… "),
                (LeadDeltaKind::Text, r#"{"tasks":[{"title":"A",""#),
                (LeadDeltaKind::Reasoning, "one task suffices "),
                (LeadDeltaKind::Text, r#"spec":"do A","depends_on":[]}]}"#),
            ],
        };
        let captured: Arc<std::sync::Mutex<Vec<(LeadDeltaKind, String)>>> =
            Arc::new(std::sync::Mutex::new(vec![]));
        let cap = Arc::clone(&captured);
        let sink: LeadThinkingSink = Arc::new(move |kind, delta: &str| {
            cap.lock().unwrap().push((kind, delta.to_string()));
        });

        let dag = producer.produce("Do A", &[], Some(&sink)).await.expect("parses");

        let got = captured.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                (LeadDeltaKind::Reasoning, "let me plan… ".to_string()),
                (LeadDeltaKind::Text, r#"{"tasks":[{"title":"A",""#.to_string()),
                (LeadDeltaKind::Reasoning, "one task suffices ".to_string()),
                (LeadDeltaKind::Text, r#"spec":"do A","depends_on":[]}]}"#.to_string()),
            ]
        );
        // The plan parsed from the TEXT deltas only (reasoning excluded).
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].title, "A");
    }

    // 2) The parsed DAG is byte-for-byte the same whether a sink is present or not
    //    — streaming changes nothing about WHAT is produced (fail-soft invariant).
    #[tokio::test]
    async fn sink_some_and_none_produce_identical_dag() {
        let stream = vec![
            (LeadDeltaKind::Reasoning, "thinking "),
            (LeadDeltaKind::Text, r#"{"tasks":[{"title":"Build","#),
            (LeadDeltaKind::Text, r#""spec":"build it","depends_on":[]},"#),
            (LeadDeltaKind::Text, r#"{"title":"Test","spec":"test it","depends_on":[0]}]}"#),
        ];
        let none_dag = SinkEchoPlanProducer { stream: stream.clone() }
            .produce("Ship", &[], None)
            .await
            .expect("None path parses");

        let sink: LeadThinkingSink = Arc::new(|_, _| {});
        let some_dag = SinkEchoPlanProducer { stream }
            .produce("Ship", &[], Some(&sink))
            .await
            .expect("Some path parses");

        // Same task count, titles, specs, deps — the streaming path is observationally
        // identical for the produced plan.
        assert_eq!(none_dag.tasks.len(), some_dag.tasks.len());
        for (a, b) in none_dag.tasks.iter().zip(some_dag.tasks.iter()) {
            assert_eq!(a.title, b.title);
            assert_eq!(a.spec, b.spec);
            assert_eq!(a.depends_on, b.depends_on);
            assert_eq!(a.kind, b.kind);
        }
        assert_eq!(none_dag.tasks.len(), 2);
        assert_eq!(none_dag.tasks[0].title, "Build");
        assert_eq!(none_dag.tasks[1].depends_on, vec![0]);
    }

    // ── B2: merge-throttle (防 WS 洪泛) ───────────────────────────────────────

    /// Recording broadcaster mirroring the events.rs test helper — captures the
    /// fanned-out leadThinking frames so we can assert coalescing + no loss.
    struct RecordingBroadcaster {
        events: std::sync::Mutex<Vec<nomifun_api_types::WebSocketMessage<serde_json::Value>>>,
    }
    impl nomifun_realtime::EventBroadcaster for RecordingBroadcaster {
        fn broadcast(&self, event: nomifun_api_types::WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(event);
        }
    }

    // A size-triggered flush coalesces many small deltas into ONE frame (≥48 chars),
    // and flush() emits the residue so NOTHING is dropped. The reassembled text
    // across all frames equals the concatenation of the input deltas.
    #[test]
    fn throttle_coalesces_by_size_and_flush_emits_residue() {
        let bc = Arc::new(RecordingBroadcaster { events: std::sync::Mutex::new(vec![]) });
        let emitter = crate::events::OrchestratorRunEventEmitter::new(bc.clone());
        let throttle = LeadThinkingThrottle::new(emitter, "run_x", "plan");
        let sink = throttle.sink();

        // 12 reasoning deltas of 5 chars = 60 chars > 48 → at least one size flush,
        // then a residual flush() picks up whatever's left.
        for _ in 0..12 {
            sink(LeadDeltaKind::Reasoning, "abcde");
        }
        throttle.flush();

        let events = bc.events.lock().unwrap();
        // Every frame is reasoning/plan, delta-only, done=false.
        let mut reassembled = String::new();
        for e in events.iter() {
            assert_eq!(e.name, "orchestrator.run.leadThinking");
            assert_eq!(e.data["run_id"], "run_x");
            assert_eq!(e.data["phase"], "plan");
            assert_eq!(e.data["kind"], "reasoning");
            assert_eq!(e.data["done"], false);
            reassembled.push_str(e.data["delta"].as_str().expect("delta str"));
        }
        // NOTHING dropped: the reassembled stream equals all input.
        assert_eq!(reassembled, "abcde".repeat(12));
        // It did coalesce (fewer frames than the 12 input deltas).
        assert!(events.len() < 12, "deltas were coalesced, got {} frames", events.len());
    }

    // Text and reasoning are buffered + flushed on SEPARATE channels (a text delta
    // never bleeds into a reasoning frame and vice versa).
    #[test]
    fn throttle_keeps_text_and_reasoning_separate() {
        let bc = Arc::new(RecordingBroadcaster { events: std::sync::Mutex::new(vec![]) });
        let emitter = crate::events::OrchestratorRunEventEmitter::new(bc.clone());
        let throttle = LeadThinkingThrottle::new(emitter, "run_y", "summarize");
        let sink = throttle.sink();

        sink(LeadDeltaKind::Reasoning, "RR");
        sink(LeadDeltaKind::Text, "TT");
        throttle.flush();

        let events = bc.events.lock().unwrap();
        let reasoning: String = events
            .iter()
            .filter(|e| e.data["kind"] == "reasoning")
            .map(|e| e.data["delta"].as_str().unwrap())
            .collect();
        let text: String = events
            .iter()
            .filter(|e| e.data["kind"] == "text")
            .map(|e| e.data["delta"].as_str().unwrap())
            .collect();
        assert_eq!(reasoning, "RR");
        assert_eq!(text, "TT");
        // phase is carried through.
        assert!(events.iter().all(|e| e.data["phase"] == "summarize"));
    }

    // An empty delta is a no-op (no frame), and flush() on an empty throttle emits
    // nothing — the residue flush never invents content.
    #[test]
    fn throttle_ignores_empty_and_empty_flush_is_noop() {
        let bc = Arc::new(RecordingBroadcaster { events: std::sync::Mutex::new(vec![]) });
        let emitter = crate::events::OrchestratorRunEventEmitter::new(bc.clone());
        let throttle = LeadThinkingThrottle::new(emitter, "run_z", "plan");
        let sink = throttle.sink();
        sink(LeadDeltaKind::Text, "");
        throttle.flush();
        assert!(bc.events.lock().unwrap().is_empty(), "empty deltas produce no frames");
    }

    #[test]
    fn parse_plan_accepts_bare_valid_json() {
        let raw = r#"{"tasks":[
            {"title":"Research","spec":"find sources","depends_on":[],"member_index":0},
            {"title":"Write","spec":"synthesize","depends_on":[0],"member_index":1,
             "task_profile":{"kind":"writing","needs_vision":false,"needs_long_context":true,"needs_high_reasoning":true,"bulk":false}}
        ]}"#;
        let dag = parse_plan(raw, "Research and write a report");

        assert_eq!(dag.tasks.len(), 2);
        assert_eq!(dag.tasks[0].title, "Research");
        assert_eq!(dag.tasks[0].member_index, Some(0));
        assert_eq!(dag.tasks[1].depends_on, vec![0]);
        let profile: &TaskProfile = dag.tasks[1].task_profile.as_ref().expect("profile decoded");
        assert_eq!(profile.kind, "writing");
        assert!(profile.needs_long_context);
        assert!(profile.needs_high_reasoning);
        assert!(!profile.bulk);
    }

    #[test]
    fn parse_plan_strips_json_fences() {
        let raw = "```json\n{\"tasks\":[{\"title\":\"One\",\"spec\":\"do it\",\"depends_on\":[]}]}\n```";
        let dag = parse_plan(raw, "goal");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].title, "One");
    }

    // 迁移 023: a plan whose closing task is kind="synthesis" parses, and its
    // pattern_config (fan-out group tag on a sibling) round-trips. Sibling agent
    // tasks sharing a "group" tag + a synthesis task depending on all of them is
    // the 1a fan-out → synthesis shape.
    #[test]
    fn parse_plan_accepts_kind_synthesis_and_fanout_group() {
        let raw = r#"{"tasks":[
            {"title":"Draft A","spec":"write variant A","depends_on":[],"kind":"agent","pattern_config":"{\"group\":\"drafts\"}"},
            {"title":"Draft B","spec":"write variant B","depends_on":[],"kind":"agent","pattern_config":"{\"group\":\"drafts\"}"},
            {"title":"Merge","spec":"combine the two drafts into the final","depends_on":[0,1],"kind":"synthesis"}
        ]}"#;
        let dag = parse_plan(raw, "produce a doc via parallel drafts");
        assert_eq!(dag.tasks.len(), 3);
        // The two fan-out siblings are agent tasks sharing the same group tag.
        assert_eq!(dag.tasks[0].kind, "agent");
        assert_eq!(dag.tasks[1].kind, "agent");
        assert_eq!(dag.tasks[0].pattern_config.as_deref(), Some("{\"group\":\"drafts\"}"));
        assert_eq!(dag.tasks[1].pattern_config.as_deref(), Some("{\"group\":\"drafts\"}"));
        // The closing task is synthesis and depends on BOTH siblings.
        assert_eq!(dag.tasks[2].kind, "synthesis");
        assert_eq!(dag.tasks[2].depends_on, vec![0, 1]);
        assert!(dag.tasks[2].pattern_config.is_none());
    }

    // UC-1b: a verify plan — a task to validate, N skeptic agent tasks each
    // depending on it, and a `verify` aggregator depending on all skeptics with a
    // vote policy in pattern_config — parses, and the verify kind + vote policy
    // round-trip. parse_plan stays fail-soft for the kind (it is kept as-is; the
    // engine recognizes "verify").
    #[test]
    fn parse_plan_accepts_verify_skeptics_and_aggregator() {
        let raw = r#"{"tasks":[
            {"title":"Build","spec":"build the feature","depends_on":[],"kind":"agent"},
            {"title":"Skeptic 1","spec":"evaluate; output {\"pass\":bool}","depends_on":[0],"kind":"agent"},
            {"title":"Skeptic 2","spec":"evaluate; output {\"pass\":bool}","depends_on":[0],"kind":"agent"},
            {"title":"Skeptic 3","spec":"evaluate; output {\"pass\":bool}","depends_on":[0],"kind":"agent"},
            {"title":"Gate","spec":"tally","depends_on":[1,2,3],"kind":"verify","pattern_config":"{\"vote\":\"majority\"}"},
            {"title":"Deploy","spec":"ship it","depends_on":[4],"kind":"agent"}
        ]}"#;
        let dag = parse_plan(raw, "build, verify, deploy");
        assert_eq!(dag.tasks.len(), 6);
        // The three skeptics are plain agent tasks depending on Build.
        for i in 1..=3 {
            assert_eq!(dag.tasks[i].kind, "agent", "skeptic {i} is an agent task");
            assert_eq!(dag.tasks[i].depends_on, vec![0]);
        }
        // The aggregator is `verify`, depends on all three skeptics, carries the
        // vote policy in pattern_config.
        let gate = &dag.tasks[4];
        assert_eq!(gate.kind, "verify");
        assert_eq!(gate.depends_on, vec![1, 2, 3]);
        assert_eq!(gate.pattern_config.as_deref(), Some("{\"vote\":\"majority\"}"));
        // Downstream gates on the verify task.
        assert_eq!(dag.tasks[5].depends_on, vec![4]);
    }

    // An unknown kind stays as-is here (parse_plan does not normalize kinds); the
    // engine is what treats anything other than the known kinds as agent. This
    // pins the fail-soft contract (parse keeps the string, no error).
    #[test]
    fn parse_plan_keeps_unknown_kind_verbatim_fail_soft() {
        let raw = r#"{"tasks":[{"title":"X","spec":"do X","depends_on":[],"kind":"totally-unknown"}]}"#;
        let dag = parse_plan(raw, "goal");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].kind, "totally-unknown", "parse keeps the kind verbatim (fail-soft)");
    }

    // ZERO-REGRESSION: a legacy plan WITHOUT any `kind` field parses with every
    // task defaulting to "agent" — the current single-agent behavior is unchanged
    // for any pre-023 plan.
    #[test]
    fn parse_plan_legacy_without_kind_defaults_all_agent() {
        let raw = r#"{"tasks":[
            {"title":"Research","spec":"find sources","depends_on":[],"member_index":0},
            {"title":"Write","spec":"synthesize","depends_on":[0],"member_index":1}
        ]}"#;
        let dag = parse_plan(raw, "Research and write a report");
        assert_eq!(dag.tasks.len(), 2);
        for t in &dag.tasks {
            assert_eq!(t.kind, "agent", "missing kind must default to agent (zero regression)");
            assert!(t.pattern_config.is_none());
        }
    }

    // The fallback DAG (planner output unparseable) is an `agent` task — patterns
    // never appear on the safety fallback.
    #[test]
    fn fallback_dag_task_is_agent_kind() {
        let dag = parse_plan("not json at all", "Build a thing");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].kind, "agent", "fallback task must be a plain agent task");
        assert!(dag.tasks[0].pattern_config.is_none());
    }

    // The system prompt must TEACH the synthesis kind + the fan-out grouping
    // convention, otherwise the lead model never emits them. Assert both the
    // schema mentions `kind`/`pattern_config` and the rules name synthesis + group.
    #[test]
    fn plan_system_teaches_synthesis_and_fanout() {
        assert!(PLAN_SYSTEM.contains("\"kind\""), "schema must mention kind: {PLAN_SYSTEM}");
        assert!(
            PLAN_SYSTEM.contains("\"pattern_config\""),
            "schema must mention pattern_config: {PLAN_SYSTEM}"
        );
        assert!(PLAN_SYSTEM.contains("synthesis"), "rules must teach synthesis: {PLAN_SYSTEM}");
        assert!(
            PLAN_SYSTEM.contains("FAN-OUT") || PLAN_SYSTEM.contains("fan-out"),
            "rules must teach fan-out: {PLAN_SYSTEM}"
        );
        assert!(PLAN_SYSTEM.contains("group"), "rules must teach the group tag: {PLAN_SYSTEM}");
    }

    // UC-1b: the system prompt must TEACH the verify pattern — the `verify` kind,
    // the skeptic JSON verdict shape, and the vote policies — otherwise the lead
    // model never emits a verify gate.
    #[test]
    fn plan_system_teaches_verify_pattern() {
        assert!(PLAN_SYSTEM.contains("verify"), "rules must teach the verify kind: {PLAN_SYSTEM}");
        assert!(
            PLAN_SYSTEM.contains("SKEPTIC") || PLAN_SYSTEM.contains("skeptic"),
            "rules must mention skeptic tasks: {PLAN_SYSTEM}"
        );
        assert!(PLAN_SYSTEM.contains("\\\"pass\\\""), "rules must teach the pass verdict shape: {PLAN_SYSTEM}");
        assert!(PLAN_SYSTEM.contains("vote"), "rules must teach the vote policy: {PLAN_SYSTEM}");
        for kw in ["majority", "unanimous", "threshold"] {
            assert!(PLAN_SYSTEM.contains(kw), "rules must teach vote policy '{kw}': {PLAN_SYSTEM}");
        }
    }

    // UC-1c: the system prompt must TEACH the judge pattern — the `judge` kind,
    // the per-candidate `scores` ballot shape, and the mean/borda aggregate
    // policies — otherwise the lead model never emits a judge contest.
    #[test]
    fn plan_system_teaches_judge_pattern() {
        assert!(PLAN_SYSTEM.contains("judge"), "rules must teach the judge kind: {PLAN_SYSTEM}");
        assert!(
            PLAN_SYSTEM.contains("CANDIDATE") || PLAN_SYSTEM.contains("candidate"),
            "rules must mention candidate tasks: {PLAN_SYSTEM}"
        );
        assert!(PLAN_SYSTEM.contains("\\\"scores\\\""), "rules must teach the scores ballot shape: {PLAN_SYSTEM}");
        assert!(PLAN_SYSTEM.contains("aggregate"), "rules must teach the aggregate policy: {PLAN_SYSTEM}");
        for kw in ["mean", "borda"] {
            assert!(PLAN_SYSTEM.contains(kw), "rules must teach aggregate policy '{kw}': {PLAN_SYSTEM}");
        }
    }

    // UC-1d: the system prompt must TEACH the loop pattern — the `loop` kind, the
    // REQUIRED `max_iter` hard cap, and the three stop kinds (max_iter/predicate/
    // dry) — otherwise the lead model never emits a bounded loop.
    #[test]
    fn plan_system_teaches_loop_pattern() {
        assert!(PLAN_SYSTEM.contains("loop"), "rules must teach the loop kind: {PLAN_SYSTEM}");
        assert!(PLAN_SYSTEM.contains("max_iter"), "rules must teach the hard cap: {PLAN_SYSTEM}");
        assert!(
            PLAN_SYSTEM.contains("BODY") || PLAN_SYSTEM.contains("body"),
            "rules must mention the body task: {PLAN_SYSTEM}"
        );
        assert!(PLAN_SYSTEM.contains("\\\"stop\\\""), "rules must teach the stop criterion: {PLAN_SYSTEM}");
        for kw in ["predicate", "dry", "quiet_rounds", "done_marker"] {
            assert!(PLAN_SYSTEM.contains(kw), "rules must teach loop stop kw '{kw}': {PLAN_SYSTEM}");
        }
    }

    // B4: the system prompt must TEACH the verdict-gated loop stop — the
    // `{"kind":"approved"}` stop + that the body should self-assess and emit a
    // pass/fail verdict — so the lead can plan an "iterate until approved" loop.
    #[test]
    fn plan_system_teaches_verdict_gated_loop_stop() {
        assert!(
            PLAN_SYSTEM.contains("approved"),
            "rules must teach the approved (verdict-gated) loop stop: {PLAN_SYSTEM}"
        );
        // The body must be told to emit a pass/fail verdict the loop reads.
        assert!(
            PLAN_SYSTEM.contains("\\\"pass\\\""),
            "rules must teach the body emits a pass verdict: {PLAN_SYSTEM}"
        );
        assert!(
            PLAN_SYSTEM.contains("SELF-ASSESS") || PLAN_SYSTEM.contains("self-assess"),
            "rules must tell the body to self-assess: {PLAN_SYSTEM}"
        );
        // max_iter must stay the named hard backstop even in the verdict-gated case.
        assert!(PLAN_SYSTEM.contains("max_iter"), "max_iter hard cap still taught: {PLAN_SYSTEM}");
    }

    // B4: the system prompt must TEACH deep pattern COMPOSITION — the three named
    // compositions (fan-out→judge→synthesis, verify-gate→downstream, loop with an
    // internal self-check) — so the lead reliably emits composed structures rather
    // than only single patterns.
    #[test]
    fn plan_system_teaches_pattern_composition() {
        assert!(
            PLAN_SYSTEM.contains("COMPOSING PATTERNS") || PLAN_SYSTEM.contains("COMPOSITION"),
            "rules must have a composition section: {PLAN_SYSTEM}"
        );
        // fan-out → judge → synthesis.
        assert!(
            PLAN_SYSTEM.contains("FAN-OUT → JUDGE → SYNTHESIS"),
            "rules must teach fan-out→judge→synthesis: {PLAN_SYSTEM}"
        );
        // verify-gate → downstream.
        assert!(
            PLAN_SYSTEM.contains("VERIFY-GATE → DOWNSTREAM"),
            "rules must teach verify-gate→downstream: {PLAN_SYSTEM}"
        );
        // loop with an internal check.
        assert!(
            PLAN_SYSTEM.contains("LOOP WITH AN INTERNAL CHECK"),
            "rules must teach loop-with-internal-check: {PLAN_SYSTEM}"
        );
    }

    // B4: the ADJUST prompt must ALSO teach composition + the verdict-gated loop
    // stop, so a conversational re-adjust can emit composed structures.
    #[test]
    fn adjust_system_teaches_composition_and_verdict_loop() {
        assert!(
            ADJUST_SYSTEM.contains("COMPOSE") || ADJUST_SYSTEM.contains("compose"),
            "adjust must teach composing patterns: {ADJUST_SYSTEM}"
        );
        assert!(
            ADJUST_SYSTEM.contains("fan-out→judge→synthesis"),
            "adjust must name the fan-out→judge→synthesis composition: {ADJUST_SYSTEM}"
        );
        assert!(
            ADJUST_SYSTEM.contains("approved"),
            "adjust must teach the verdict-gated (approved) loop stop: {ADJUST_SYSTEM}"
        );
    }

    // UC-1d: a loop plan — a BODY agent task + a `loop` controller depending only
    // on the body, carrying max_iter + a stop criterion in pattern_config, plus a
    // downstream task gated on the LOOP (not the body) — parses, and the loop kind
    // + config round-trip. parse_plan stays fail-soft (kept verbatim; the engine
    // recognizes "loop").
    #[test]
    fn parse_plan_accepts_loop_body_controller_and_downstream() {
        let raw = r#"{"tasks":[
            {"title":"Refine","spec":"improve the draft one round; emit DONE when finished","depends_on":[],"kind":"agent"},
            {"title":"Loop","spec":"iterate","depends_on":[0],"kind":"loop","pattern_config":"{\"max_iter\":5,\"stop\":{\"kind\":\"predicate\",\"done_marker\":\"DONE\"}}"},
            {"title":"Publish","spec":"publish the refined draft","depends_on":[1],"kind":"agent"}
        ]}"#;
        let dag = parse_plan(raw, "iteratively refine then publish");
        assert_eq!(dag.tasks.len(), 3);
        // The body is a plain agent task with no deps.
        assert_eq!(dag.tasks[0].kind, "agent");
        assert!(dag.tasks[0].depends_on.is_empty());
        // The controller is `loop`, depends ONLY on the body, carries the config.
        let ctrl = &dag.tasks[1];
        assert_eq!(ctrl.kind, "loop");
        assert_eq!(ctrl.depends_on, vec![0], "loop depends only on the body");
        assert_eq!(
            ctrl.pattern_config.as_deref(),
            Some("{\"max_iter\":5,\"stop\":{\"kind\":\"predicate\",\"done_marker\":\"DONE\"}}")
        );
        // Downstream gates on the LOOP controller, not the body.
        assert_eq!(dag.tasks[2].depends_on, vec![1], "downstream waits for the loop, not the body");
    }

    // UC-1c: a judge plan — M candidate agent tasks (a fan-out group), N judge
    // agent tasks each depending on ALL M candidates, and one `judge` aggregator
    // depending on all N judges with an aggregate policy in pattern_config —
    // parses, and the judge kind + aggregate policy round-trip. parse_plan stays
    // fail-soft for the kind (kept as-is; the engine recognizes "judge").
    #[test]
    fn parse_plan_accepts_judge_candidates_judges_and_aggregator() {
        let raw = r#"{"tasks":[
            {"title":"Candidate A","spec":"design approach A","depends_on":[],"kind":"agent","pattern_config":"{\"group\":\"candidates\"}"},
            {"title":"Candidate B","spec":"design approach B","depends_on":[],"kind":"agent","pattern_config":"{\"group\":\"candidates\"}"},
            {"title":"Candidate C","spec":"design approach C","depends_on":[],"kind":"agent","pattern_config":"{\"group\":\"candidates\"}"},
            {"title":"Judge 1","spec":"score every candidate; output {\"scores\":[..]}","depends_on":[0,1,2],"kind":"agent"},
            {"title":"Judge 2","spec":"score every candidate; output {\"scores\":[..]}","depends_on":[0,1,2],"kind":"agent"},
            {"title":"Judge 3","spec":"score every candidate; output {\"scores\":[..]}","depends_on":[0,1,2],"kind":"agent"},
            {"title":"Pick","spec":"aggregate ballots","depends_on":[3,4,5],"kind":"judge","pattern_config":"{\"aggregate\":\"borda\"}"}
        ]}"#;
        let dag = parse_plan(raw, "pick the best design");
        assert_eq!(dag.tasks.len(), 7);
        // The three candidates are plain agent tasks sharing the fan-out group.
        for i in 0..=2 {
            assert_eq!(dag.tasks[i].kind, "agent", "candidate {i} is an agent task");
            assert!(dag.tasks[i].depends_on.is_empty(), "candidates are independent");
        }
        // The three judges are agent tasks depending on ALL candidates.
        for i in 3..=5 {
            assert_eq!(dag.tasks[i].kind, "agent", "judge {i} is an agent task");
            assert_eq!(dag.tasks[i].depends_on, vec![0, 1, 2], "judge {i} scores all candidates");
        }
        // The aggregator is `judge`, depends on all three judges, carries the
        // aggregate policy in pattern_config.
        let pick = &dag.tasks[6];
        assert_eq!(pick.kind, "judge");
        assert_eq!(pick.depends_on, vec![3, 4, 5]);
        assert_eq!(pick.pattern_config.as_deref(), Some("{\"aggregate\":\"borda\"}"));
    }

    #[test]
    fn parse_plan_extracts_json_wrapped_in_prose() {
        let raw = "Sure! Here is the plan you asked for:\n\n\
            {\"tasks\":[{\"title\":\"Alpha\",\"spec\":\"step\",\"depends_on\":[]}]}\n\n\
            Let me know if you'd like changes.";
        let dag = parse_plan(raw, "goal");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].title, "Alpha");
    }

    #[test]
    fn parse_plan_handles_braces_inside_strings() {
        // A literal "}" inside a string value must not prematurely close the object.
        let raw = r#"{"tasks":[{"title":"Use {braces}","spec":"emit a } char","depends_on":[]}]}"#;
        let dag = parse_plan(raw, "goal");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].title, "Use {braces}");
        assert_eq!(dag.tasks[0].spec, "emit a } char");
    }

    #[test]
    fn parse_plan_falls_back_on_garbage() {
        let dag = parse_plan("I'm sorry, I cannot help with that.", "Build a rocket");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].title, "Build a rocket");
        assert_eq!(dag.tasks[0].spec, "Build a rocket");
        assert_eq!(dag.tasks[0].member_index, Some(0));
        assert!(dag.tasks[0].depends_on.is_empty());
        assert_eq!(
            dag.tasks[0].rationale.as_deref(),
            Some("fallback: planner output unparseable")
        );
    }

    // parse_plan_opt is the NON-fallback core: it returns None on every failure mode
    // (so `produce` can retry / warn instead of silently falling back — the 会话6 fix),
    // and Some only for a parseable, non-empty task DAG.
    #[test]
    fn parse_plan_opt_returns_none_on_every_failure_mode() {
        assert!(parse_plan_opt("I'm sorry, I cannot help.").is_none(), "prose/garbage → None");
        assert!(parse_plan_opt(r#"{"tasks":[]}"#).is_none(), "empty tasks → None");
        assert!(parse_plan_opt(r#"{"tasks":[{"title":"x" "#).is_none(), "malformed JSON → None");
        assert!(parse_plan_opt("").is_none(), "empty string → None");
    }

    #[test]
    fn parse_plan_opt_returns_some_for_valid_dag() {
        let raw = r#"{"tasks":[{"title":"A","spec":"a","depends_on":[]},{"title":"B","spec":"b","depends_on":[0]}]}"#;
        let dag = parse_plan_opt(raw).expect("valid multi-task DAG → Some");
        assert_eq!(dag.tasks.len(), 2);
        assert_eq!(dag.tasks[1].depends_on, vec![0]);
    }

    #[test]
    fn parse_plan_falls_back_on_empty_tasks() {
        let dag = parse_plan(r#"{"tasks":[]}"#, "Some goal");
        assert_eq!(dag.tasks.len(), 1, "empty tasks must degrade to fallback");
        assert_eq!(dag.tasks[0].title, "Some goal");
    }

    #[test]
    fn parse_plan_falls_back_on_malformed_json() {
        // Unterminated object → no balanced match → fallback.
        let dag = parse_plan(r#"{"tasks":[{"title":"x" "#, "Goal text");
        assert_eq!(dag.tasks.len(), 1);
        assert_eq!(dag.tasks[0].spec, "Goal text");
    }

    #[test]
    fn parse_plan_truncates_long_goal_title() {
        let goal = "x".repeat(200);
        let dag = parse_plan("not json", &goal);
        // 60 chars + ellipsis.
        assert_eq!(dag.tasks[0].title.chars().count(), FALLBACK_TITLE_LEN + 1);
        assert!(dag.tasks[0].title.ends_with('…'));
        // spec keeps the full goal.
        assert_eq!(dag.tasks[0].spec, goal);
    }

    #[test]
    fn truncate_title_is_cjk_safe() {
        let goal = "目标".repeat(50); // 100 CJK chars
        let title = truncate_title(&goal);
        assert_eq!(title.chars().count(), FALLBACK_TITLE_LEN + 1);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn build_plan_user_prompt_includes_goal_and_members() {
        let member = FleetMember {
            id: "fm_1".to_string(),
            agent_id: "agent_research".to_string(),
            provider_id: None,
            model: None,
            role_hint: Some("researcher".to_string()),
            capability_profile: Some(nomifun_api_types::CapabilityProfile {
                strengths: vec!["search".to_string(), "synthesis".to_string()],
                modalities: vec!["text".to_string()],
                tools: true,
                reasoning: "high".to_string(),
                cost_tier: "premium".to_string(),
                speed_tier: "medium".to_string(),
            }),
            constraints: None,
            sort_order: 0,
            description: None,
            system_prompt: None,
            enabled_skills: Vec::new(),
            disabled_builtin_skills: Vec::new(),
        };
        let prompt = build_plan_user_prompt("Research X", &[member], &DescriptionMap::new());
        assert!(prompt.contains("Research X"));
        assert!(prompt.contains("0. agent_research"));
        assert!(prompt.contains("role=researcher"));
        assert!(prompt.contains("search/synthesis"));
        // No description available for this member → desc column is the "-" sentinel.
        assert!(prompt.contains("desc=-"), "missing-description members get desc=-: {prompt}");
    }

    #[test]
    fn build_plan_user_prompt_handles_no_members() {
        let prompt = build_plan_user_prompt("Solo goal", &[], &DescriptionMap::new());
        assert!(prompt.contains("Solo goal"));
        assert!(prompt.contains("none"));
    }

    // (P5 Task 1, d) The planner must be INSTRUCTED to emit a short Chinese role
    // per task. The instruction lives in the system prompt's JSON schema + rules;
    // assert both the `role` key in the schema and a rule naming example roles, so
    // the LLM actually produces it (otherwise nothing precipitates downstream).
    #[test]
    fn plan_system_instructs_role_per_task() {
        // The JSON shape the model is told to return includes "role".
        assert!(
            PLAN_SYSTEM.contains("\"role\""),
            "PLAN_SYSTEM JSON schema must include the role field: {PLAN_SYSTEM}"
        );
        // A rule names short Chinese example roles so the model emits sensible ones.
        for kw in ["规划", "前端", "后端", "测试", "设计"] {
            assert!(
                PLAN_SYSTEM.contains(kw),
                "PLAN_SYSTEM should mention example role '{kw}': {PLAN_SYSTEM}"
            );
        }
    }

    // 主模型 选模性价比: the planner must be told to weigh task DIFFICULTY against
    // model cost/capability (assign cheap models to easy tasks, strong models to
    // hard ones) rather than defaulting every node to the strongest model.
    #[test]
    fn plan_system_teaches_cost_difficulty_tradeoff() {
        assert!(
            PLAN_SYSTEM.contains("DIFFICULTY") || PLAN_SYSTEM.contains("difficulty"),
            "PLAN_SYSTEM must teach matching model to task difficulty: {PLAN_SYSTEM}"
        );
        assert!(
            PLAN_SYSTEM.contains("性价比"),
            "PLAN_SYSTEM must frame the cost-effectiveness (性价比) goal: {PLAN_SYSTEM}"
        );
    }

    // (a) build_plan_user_prompt surfaces a member's model description in the
    // desc= column when the (provider_id, model) → description map carries one,
    // so the planner can read it and pick the best-matching model.
    #[test]
    fn build_plan_user_prompt_includes_model_description() {
        let member = member_with(Some("prov_x"), Some("model-x"));
        let mut descriptions = DescriptionMap::new();
        descriptions.insert(
            ("prov_x".to_string(), "model-x".to_string()),
            "擅长前端与可视化".to_string(),
        );
        let prompt = build_plan_user_prompt("Build a UI", &[member], &descriptions);
        assert!(
            prompt.contains("desc=擅长前端与可视化"),
            "description must surface in the desc= column: {prompt}"
        );
    }

    // (Change 3) `member.description` is the PRIMARY desc source (Task 2 fills it
    // for assistant-backed and decorated bare members). It is shown even when the
    // P3 provider-description map has no entry for the member.
    #[test]
    fn build_plan_user_prompt_prefers_member_description() {
        let mut member = member_with(Some("prov_x"), Some("model-x"));
        member.description = Some("研究型助手，擅长检索与综述".to_string());
        // Empty P3 map — member.description alone must drive the desc= column.
        let prompt = build_plan_user_prompt("Research X", &[member], &DescriptionMap::new());
        assert!(
            prompt.contains("desc=研究型助手，擅长检索与综述"),
            "member.description must surface as the desc= column: {prompt}"
        );
    }

    // member.description WINS over the P3 provider-description map when both are
    // present (the member snapshot is the authoritative source now).
    #[test]
    fn build_plan_user_prompt_member_description_overrides_provider_map() {
        let mut member = member_with(Some("prov_x"), Some("model-x"));
        member.description = Some("助手自述描述".to_string());
        let mut descriptions = DescriptionMap::new();
        descriptions.insert(
            ("prov_x".to_string(), "model-x".to_string()),
            "模型卡描述（应被覆盖）".to_string(),
        );
        let prompt = build_plan_user_prompt("goal", &[member], &descriptions);
        assert!(prompt.contains("desc=助手自述描述"), "member.description wins: {prompt}");
        assert!(
            !prompt.contains("模型卡描述"),
            "provider-map desc must not appear when member.description is set: {prompt}"
        );
    }

    // A blank member.description falls back to the P3 provider-description map
    // (so bare members still get the model-card description via the fallback).
    #[test]
    fn build_plan_user_prompt_blank_member_description_falls_back_to_provider_map() {
        let mut member = member_with(Some("prov_x"), Some("model-x"));
        member.description = Some("   ".to_string()); // whitespace-only → ignored
        let mut descriptions = DescriptionMap::new();
        descriptions.insert(
            ("prov_x".to_string(), "model-x".to_string()),
            "模型卡描述".to_string(),
        );
        let prompt = build_plan_user_prompt("goal", &[member], &descriptions);
        assert!(
            prompt.contains("desc=模型卡描述"),
            "blank member.description must fall back to the provider map: {prompt}"
        );
    }

    /// Build a minimal `FleetMember` carrying the given provider/model.
    fn member_with(provider_id: Option<&str>, model: Option<&str>) -> FleetMember {
        FleetMember {
            id: "fm".to_string(),
            agent_id: "agent".to_string(),
            provider_id: provider_id.map(str::to_string),
            model: model.map(str::to_string),
            role_hint: None,
            capability_profile: None,
            constraints: None,
            sort_order: 0,
            description: None,
            system_prompt: None,
            enabled_skills: Vec::new(),
            disabled_builtin_skills: Vec::new(),
        }
    }

    #[test]
    fn pick_lead_picks_first_member_with_provider_and_model() {
        let fallback = ProviderWithModel {
            provider_id: String::new(),
            model: String::new(),
            use_model: None,
        };
        // members[0] lacks a model; members[1] is fully specified → pick [1].
        let members = vec![
            member_with(Some("prov_a"), None),
            member_with(Some("prov_b"), Some("model_b")),
        ];
        let lead = pick_lead(&members, &fallback);
        assert_eq!(lead.provider_id, "prov_b");
        assert_eq!(lead.model, "model_b");
        assert_eq!(lead.use_model.as_deref(), Some("model_b"));
    }

    #[test]
    fn pick_lead_skips_empty_string_provider() {
        let fallback = ProviderWithModel {
            provider_id: String::new(),
            model: String::new(),
            use_model: None,
        };
        // members[0] has an EMPTY provider_id → skipped; members[1] qualifies.
        let members = vec![
            member_with(Some(""), Some("model_x")),
            member_with(Some("prov_real"), Some("model_real")),
        ];
        let lead = pick_lead(&members, &fallback);
        assert_eq!(lead.provider_id, "prov_real");
        assert_eq!(lead.model, "model_real");
        assert_eq!(lead.use_model.as_deref(), Some("model_real"));
    }

    #[test]
    fn pick_lead_falls_back_when_no_member_qualifies() {
        let fallback = ProviderWithModel {
            provider_id: "fallback_prov".to_string(),
            model: "fallback_model".to_string(),
            use_model: Some("fallback_use".to_string()),
        };
        // No member carries both provider+model → return the fallback as-is.
        let members = vec![member_with(None, Some("m")), member_with(Some("p"), None), member_with(Some(""), Some(""))];
        let lead = pick_lead(&members, &fallback);
        assert_eq!(lead.provider_id, "fallback_prov");
        assert_eq!(lead.model, "fallback_model");
        assert_eq!(lead.use_model.as_deref(), Some("fallback_use"));
    }

    /// Build a minimal `Provider` row carrying the given `model_descriptions`
    /// JSON (the only field `build_description_map` reads, besides `id`).
    fn provider_with_descriptions(id: &str, model_descriptions: Option<&str>) -> Provider {
        Provider {
            id: id.to_string(),
            platform: "openai".to_string(),
            name: "p".to_string(),
            base_url: String::new(),
            api_key_encrypted: String::new(),
            models: "[]".to_string(),
            enabled: true,
            capabilities: "[]".to_string(),
            context_limit: None,
            model_context_limits: None,
            model_protocols: None,
            model_descriptions: model_descriptions.map(str::to_string),
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    // build_description_map decodes each provider's model_descriptions JSON and
    // keys the result by (provider_id, model) for the members that reference it.
    #[test]
    fn build_description_map_keys_by_provider_and_model() {
        let providers = vec![provider_with_descriptions(
            "prov_a",
            Some(r#"{"model-a":"擅长前端","model-b":"擅长后端"}"#),
        )];
        let members = vec![
            member_with(Some("prov_a"), Some("model-a")),
            member_with(Some("prov_a"), Some("model-b")),
        ];
        let map = build_description_map(&providers, &members);
        assert_eq!(
            map.get(&("prov_a".to_string(), "model-a".to_string())).map(String::as_str),
            Some("擅长前端")
        );
        assert_eq!(
            map.get(&("prov_a".to_string(), "model-b".to_string())).map(String::as_str),
            Some("擅长后端")
        );
    }

    // An unset model_descriptions (Task 1 stores the default as `Some("{}")`) and
    // an absent model entry both yield "no description" (no map entry) — not an error.
    #[test]
    fn build_description_map_treats_empty_object_as_no_description() {
        let providers = vec![
            provider_with_descriptions("prov_empty", Some("{}")),
            provider_with_descriptions("prov_partial", Some(r#"{"other-model":"x"}"#)),
        ];
        let members = vec![
            member_with(Some("prov_empty"), Some("model-a")),
            member_with(Some("prov_partial"), Some("model-a")),
        ];
        let map = build_description_map(&providers, &members);
        assert!(map.is_empty(), "no member matched a description entry: {map:?}");
    }

    // A blank description string is dropped (treated as "no description"), and a
    // malformed model_descriptions JSON is fail-soft (no entries, no panic/error).
    #[test]
    fn build_description_map_is_fail_soft_on_bad_json_and_blank() {
        let providers = vec![
            provider_with_descriptions("prov_bad", Some("not json at all")),
            provider_with_descriptions("prov_blank", Some(r#"{"model-a":"   "}"#)),
            provider_with_descriptions("prov_none", None),
        ];
        let members = vec![
            member_with(Some("prov_bad"), Some("model-a")),
            member_with(Some("prov_blank"), Some("model-a")),
            member_with(Some("prov_none"), Some("model-a")),
        ];
        let map = build_description_map(&providers, &members);
        assert!(map.is_empty(), "bad/blank/absent descriptions yield no entries: {map:?}");
    }

    // ── UC-3a: adjust schema / prompt / fail-soft parse ──────────────────────

    /// Build a minimal `RunTask` for the adjust-prompt tests.
    fn run_task(id: &str, title: &str, status: &str, output: Option<&str>) -> RunTask {
        RunTask {
            id: id.to_string(),
            run_id: "run_x".to_string(),
            last_error: None,
            title: title.to_string(),
            spec: format!("spec-{title}"),
            task_profile: None,
            status: status.to_string(),
            conversation_id: None,
            output_summary: output.map(str::to_string),
            output_files: vec![],
            attempt: 0,
            tokens: None,
            graph_x: None,
            graph_y: None,
            role: Some("研究".to_string()),
            kind: "agent".to_string(),
            pattern_config: None,
            override_provider_id: None,
            override_model: None,
            preset_prompt: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    // parse_adjusted_plan accepts a mixed keep+new plan: a kept existing id, a new
    // task whose depends_on references the kept id (string) AND an earlier new
    // index (int). Both ref kinds round-trip.
    #[test]
    fn parse_adjusted_plan_accepts_keep_and_new_with_mixed_deps() {
        let raw = r#"{"tasks":[
            {"keep":"rtask_abc"},
            {"title":"扩写","spec":"基于上游产出扩写","role":"写作","depends_on":["rtask_abc"]},
            {"title":"汇总","spec":"合并","kind":"synthesis","depends_on":["rtask_abc",1]}
        ]}"#;
        let plan = parse_adjusted_plan(raw).expect("valid adjusted plan parses");
        assert_eq!(plan.tasks.len(), 3);
        // Node 0 keeps an existing task by id.
        match &plan.tasks[0] {
            AdjustedNode::Keep { keep } => assert_eq!(keep, "rtask_abc"),
            other => panic!("expected Keep, got {other:?}"),
        }
        // Node 1 is a new task depending on the kept id (a string ref).
        match &plan.tasks[1] {
            AdjustedNode::New(t) => {
                assert_eq!(t.title, "扩写");
                assert_eq!(t.kind, "agent", "kind defaults to agent");
                assert_eq!(t.depends_on, vec![AdjustedDepRef::Kept("rtask_abc".to_string())]);
            }
            other => panic!("expected New, got {other:?}"),
        }
        // Node 2 mixes a kept-id string ref + a new-index int ref (→ node 1).
        match &plan.tasks[2] {
            AdjustedNode::New(t) => {
                assert_eq!(t.kind, "synthesis");
                assert_eq!(
                    t.depends_on,
                    vec![AdjustedDepRef::Kept("rtask_abc".to_string()), AdjustedDepRef::NewIndex(1)]
                );
            }
            other => panic!("expected New, got {other:?}"),
        }
    }

    // parse_adjusted_plan strips ```json fences + surrounding prose (reuses
    // extract_json_object).
    #[test]
    fn parse_adjusted_plan_strips_fences_and_prose() {
        let raw = "好的，这是调整后的计划：\n```json\n{\"tasks\":[{\"keep\":\"rtask_1\"}]}\n```\n如需修改请告诉我。";
        let plan = parse_adjusted_plan(raw).expect("parses through fences + prose");
        assert_eq!(plan.tasks.len(), 1);
    }

    // parse_adjusted_plan is fail-soft TO AN ERROR (not a fallback): garbage,
    // malformed JSON, and an empty tasks array each return a BadRequest so the
    // caller leaves the run UNTOUCHED (no guessing at a mangled plan).
    #[test]
    fn parse_adjusted_plan_errors_on_garbage_malformed_and_empty() {
        for raw in [
            "对不起，我无法处理这个请求。",
            r#"{"tasks":[{"keep":"rtask_1" "#, // unterminated
            r#"{"tasks":[]}"#,                 // empty → would wipe the run
        ] {
            let err = parse_adjusted_plan(raw).expect_err("must error, not fall back");
            assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?} for {raw}");
        }
    }

    // build_adjust_user_prompt surfaces the intent + one line per task (id, title,
    // status, depends_on ids), and truncates a long output_summary.
    #[test]
    fn build_adjust_user_prompt_includes_intent_state_and_truncates_output() {
        let long_output = "段".repeat(400); // 400 CJK chars > 300 cap
        let tasks = vec![
            run_task("rtask_a", "研究", "done", Some(&long_output)),
            run_task("rtask_b", "写作", "pending", None),
        ];
        let deps = vec![RunTaskDep {
            blocker_task_id: "rtask_a".to_string(),
            blocked_task_id: "rtask_b".to_string(),
        }];
        let prompt = build_adjust_user_prompt("把报告改成中文", &tasks, &deps);
        assert!(prompt.contains("把报告改成中文"), "intent present: {prompt}");
        assert!(prompt.contains("rtask_a"), "task id present");
        assert!(prompt.contains("status=done"), "status present");
        // rtask_b's depends_on lists rtask_a.
        assert!(prompt.contains("depends_on=[\"rtask_a\"]"), "deps listed: {prompt}");
        // The 400-char output was truncated with an ellipsis.
        assert!(prompt.contains('…'), "long output truncated: {prompt}");
        assert!(
            !prompt.contains(&"段".repeat(350)),
            "output must not carry the full 400-char summary"
        );
    }

    // The adjust system prompt must TEACH the keep-vs-new schema + the dep-ref
    // convention + empower judging, otherwise the lead never emits a valid plan.
    #[test]
    fn adjust_system_teaches_keep_new_and_dep_refs() {
        assert!(ADJUST_SYSTEM.contains("\"keep\""), "must teach keep node: {ADJUST_SYSTEM}");
        assert!(ADJUST_SYSTEM.contains("depends_on"), "must teach deps: {ADJUST_SYSTEM}");
        assert!(ADJUST_SYSTEM.contains("INTENT"), "must mention the intent: {ADJUST_SYSTEM}");
        // Empowered to judge keep vs re-decompose (not a fixed policy).
        assert!(ADJUST_SYSTEM.contains("KEEP"), "must empower keeping: {ADJUST_SYSTEM}");
        assert!(
            ADJUST_SYSTEM.contains("RE-DECOMPOSE") || ADJUST_SYSTEM.contains("REPLACE"),
            "must empower re-decomposition: {ADJUST_SYSTEM}"
        );
        assert!(ADJUST_SYSTEM.contains("DROPPED"), "must allow dropping: {ADJUST_SYSTEM}");
    }
}
