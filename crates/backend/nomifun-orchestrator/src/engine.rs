//! [`RunEngine`]: the **bounded-parallel** execution loop that drives an
//! orchestration run's task DAG to completion.
//!
//! The engine skeleton — the per-run handle registry, the `start` =
//! stop-then-spawn dance, the generation-guarded [`HandleGuard`] that removes
//! only its own entry on task exit, and [`RunEngine::resume_persisted_runs`] —
//! is a faithful reduction of `nomifun_requirement::Orchestrator` (see
//! `crates/backend/nomifun-requirement/src/orchestrator.rs`). The differences
//! are deliberate: a run is keyed by a single `String` run id (no dual-domain
//! `(kind, id)`), and the dispatch loop is **concurrent** (P2) — it runs up to
//! `cap` ready tasks at a time on overlapping worker conversations (P1a was
//! serial; P2 lifts the one-at-a-time restriction while keeping dependencies
//! strict).
//!
//! ## Concurrency model (no busy-spin, dependencies strict)
//!
//! In-flight workers are held in a [`futures::stream::FuturesUnordered`]; each
//! future resolves to `(task_id, Result<WorkerOutcome, AppError>)`. The loop:
//!
//! 1. **Cancel check** — cooperative flag set → stop scheduling, break.
//! 2. **Fill** — while `inflight.len() < cap`, re-query
//!    [`list_ready_tasks`](nomifun_db::IRunRepository::list_ready_tasks)
//!    (skipping tasks already in-flight), take up to the free slots, mark each
//!    `running` + emit, resolve member/workspace/brief, and push a worker future.
//!    Re-querying every fill means a downstream task is only ever dispatched
//!    after its blockers have actually reached `done` (which only happens when a
//!    worker completes and `update_task(done)` runs) — **dependency strictness**.
//! 3. **Decide / await** —
//!    - `inflight.is_empty()` (nothing ready AND nothing running) → the task
//!      statuses are conclusive: all `done`/`skipped` → run `completed` (+
//!      aggregated summary), any `failed` → run `failed`, otherwise a "stuck"
//!      graph → break. **break, never spin.**
//!    - otherwise `await inflight.next()` — the loop parks on the next worker to
//!      finish; it never re-loops on an unchanged empty-ready state while work is
//!      in flight. Processing the completion (`done`/`failed` + emit) may unblock
//!      downstream tasks, so the loop re-fills.
//!
//! Because the loop only re-enters its body after either dispatching a task or
//! awaiting an in-flight completion, it cannot busy-spin.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use nomifun_api_types::FleetMember;
use nomifun_common::{AppError, now_ms};
use nomifun_db::models::{OrchRunRow, OrchRunTaskRow};
use nomifun_db::{IOrchWorkspaceRepository, IRunRepository};
use nomifun_db::{UpdateRunParams, UpdateTaskParams};
use tracing::{info, warn};

use crate::events::OrchestratorRunEventEmitter;
use crate::plan::PlanProducer;
use crate::worker::{WorkerOutcome, WorkerRunner};

/// Cancels an in-flight worker conversation so its turn ends as
/// `Finish(Cancelled)`. The app injects an implementation that wraps
/// [`ConversationService::cancel`](nomifun_conversation::ConversationService::cancel)
/// (stamps `user_cancel` + calls `agent.cancel`; idempotent — a no-op when no
/// live agent exists). Defined as a trait (not a bare `Fn`) so the impl can be
/// `async` and the orchestrator crate stays free of a `nomifun-conversation`
/// dependency (the wiring lives in `build_orchestrator_state`).
#[async_trait]
pub trait ConversationCanceller: Send + Sync {
    /// Cancel the conversation identified by `conversation_id`. Best-effort and
    /// idempotent: a missing/already-finished conversation is a silent no-op.
    async fn cancel(&self, conversation_id: i64);
}

/// A [`ConversationCanceller`] that does nothing — the default for harnesses /
/// tests that drive the engine without a live conversation layer. Lets
/// [`RunEngineDeps::new`] stay infallible and keeps the all-mock engine tests
/// (which never cancel) from having to construct a canceller.
pub struct NoopConversationCanceller;

#[async_trait]
impl ConversationCanceller for NoopConversationCanceller {
    async fn cancel(&self, _conversation_id: i64) {}
}

/// Steers (mid-turn injects) a message into an in-flight worker conversation so
/// the supervisor can nudge a running task without restarting it. The app injects
/// an implementation wrapping
/// [`ConversationService::steer_message`](nomifun_conversation::ConversationService::steer_message)
/// (Nomi-only mid-turn injection; falls back to a fresh send when no live turn
/// exists). Defined as a trait (not a bare `Fn`) so the impl can be `async` and
/// the orchestrator crate stays free of a `nomifun-conversation` dependency (the
/// wiring lives in `build_orchestrator_state`, exactly like
/// [`ConversationCanceller`]).
#[async_trait]
pub trait ConversationSteerer: Send + Sync {
    /// Inject `text` into the conversation identified by `conversation_id`.
    /// Returns an error when the injection cannot be performed (e.g. a non-Nomi
    /// engine that does not support steering); the engine maps that to a 400.
    async fn steer(&self, conversation_id: i64, text: &str) -> Result<(), AppError>;
}

/// A [`ConversationSteerer`] that always errors — the default for harnesses /
/// tests that drive the engine without a live conversation layer. Keeps
/// [`RunEngineDeps::new`] infallible; the app overrides it with a real steerer.
pub struct NoopConversationSteerer;

#[async_trait]
impl ConversationSteerer for NoopConversationSteerer {
    async fn steer(&self, _conversation_id: i64, _text: &str) -> Result<(), AppError> {
        Err(AppError::BadRequest(
            "steering is not wired in this engine".to_owned(),
        ))
    }
}

/// The terminal / notable outcome of a run, reported back to the LEAD conversation
/// that launched it so the master agent is re-engaged exactly once per notable
/// event to summarize / re-strategize / relay — instead of LLM-driven busy-polling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// The run finished successfully.
    Completed,
    /// The run finished with a hard failure (a required task failed after its
    /// retry budget, or a non-recoverable error).
    Failed,
    /// The run made no progress past the stall deadline and was force-finalized by
    /// the watchdog (planner-hang / soft-strand safety net) — never a silent black hole.
    Stalled,
    /// An interactive run planned and parked awaiting the user's approval.
    AwaitingApproval,
    /// A single node PERMANENTLY failed mid-run (retry budget exhausted / non-
    /// retryable). Reported best-effort to the lead BEFORE the run's terminal state
    /// so the master agent can repair (rerun w/ another model, adjust the graph,
    /// adopt, or abandon) instead of waiting for the whole run to end. Distinct from
    /// [`RunOutcome::Failed`], which is the run's terminal receipt.
    NodeFailed,
    /// A THROTTLED mid-run heartbeat: after a batch of nodes finished `done` the
    /// engine re-engages the lead so the master agent can OBSERVE progress and, if
    /// warranted, append more work (`nomi_run_add_tasks`) to the SAME run — without
    /// waiting for the terminal receipt. Rate-limited by node-count + time (see
    /// [`BATCH_REPORT_MIN_NODES`] / [`BATCH_REPORT_INTERVAL`]) so a fast fan-out
    /// cannot spam the lead. Non-terminal; the run keeps running.
    BatchProgress,
    /// A node raised a DECISION QUESTION and parked at `needs_review`
    /// (审批模式，迁移 030): the worker called `nomi_task_question` mid-run, so the
    /// lead must tell the USER to dive into that node (画布/进度条的提问图标 →
    /// 投影视图), answer in the worker conversation, then 「采用为该节点产出」to
    /// resume. Non-terminal; the run keeps running (other branches keep driving).
    NodeQuestion,
}

impl RunOutcome {
    /// Stable lowercase tag (logs / the injected receipt discriminator).
    pub fn as_str(self) -> &'static str {
        match self {
            RunOutcome::Completed => "completed",
            RunOutcome::Failed => "failed",
            RunOutcome::Stalled => "stalled",
            RunOutcome::AwaitingApproval => "awaiting_approval",
            RunOutcome::NodeFailed => "node_failed",
            RunOutcome::BatchProgress => "batch_progress",
            RunOutcome::NodeQuestion => "node_question",
        }
    }
}

/// Reports a run's terminal / notable outcome back to the LEAD conversation that
/// launched it (`lead_conv_id`), re-engaging the master agent exactly once per
/// notable event to summarize / re-strategize / relay to the user — replacing the
/// old LLM busy-poll loop. The app injects an implementation wrapping
/// [`ConversationService::steer_message`](nomifun_conversation::ConversationService::steer_message)
/// (starts a fresh lead turn when the conversation is idle, injects into a live one
/// otherwise), resolving the lead's REAL owner. A trait (not a bare `Fn`) so the
/// impl can be `async` and the orchestrator crate stays free of a
/// `nomifun-conversation` dependency — wired in `build_orchestrator_state`, exactly
/// like [`ConversationSteerer`].
#[async_trait]
pub trait LeadReporter: Send + Sync {
    /// Deliver `brief` about `run_id`'s `outcome` to conversation `lead_conv_id`.
    /// Best-effort: callers only `warn!` on error — a report failure must NEVER
    /// fail, block, or delay the run.
    async fn report(
        &self,
        lead_conv_id: i64,
        run_id: &str,
        outcome: RunOutcome,
        brief: &str,
    ) -> Result<(), AppError>;
}

/// A [`LeadReporter`] that does nothing (returns `Ok`) — the default for harnesses
/// / CLI / tests with no conversation layer. Unlike [`NoopConversationSteerer`] it
/// succeeds silently (a report is best-effort; a no-op must not spam warnings).
/// `build_orchestrator_state` overrides it with the real reporter.
pub struct NoopLeadReporter;

#[async_trait]
impl LeadReporter for NoopLeadReporter {
    async fn report(
        &self,
        _lead_conv_id: i64,
        _run_id: &str,
        _outcome: RunOutcome,
        _brief: &str,
    ) -> Result<(), AppError> {
        Ok(())
    }
}

/// A [`PlanProducer`] used ONLY as the engine's default run-summarizer (B2): it
/// supports neither `produce` nor `adjust` (those stay on the real planner held
/// by `RunService`) and its `summarize` errors via the trait default. This is the
/// default for [`RunEngineDeps::new`] so the all-mock engine tests need not wire a
/// summarizer — the engine's fail-soft path simply falls back to the mechanical
/// `aggregate_summary` whenever `summarize` errors. The app overrides this field
/// with the same `LlmPlanProducer` it gives `RunService` (a real one-shot summary).
pub struct NoopSummaryProducer;

#[async_trait]
impl PlanProducer for NoopSummaryProducer {
    async fn produce(
        &self,
        _goal: &str,
        _members: &[FleetMember],
        _sink: Option<&crate::plan::LeadThinkingSink>,
    ) -> Result<nomifun_api_types::PlannedDag, AppError> {
        // The engine never calls `produce` on the summarizer (planning lives in
        // RunService); error rather than fabricate a plan.
        Err(AppError::BadRequest(
            "NoopSummaryProducer is a summarize-only stub".to_string(),
        ))
    }
    // `summarize` intentionally uses the trait default (errors) → the engine
    // falls back to `aggregate_summary`.
}

/// Hard ceiling on a single worker task's turn.
pub const DEFAULT_WORKER_TIMEOUT: Duration = Duration::from_secs(1800);

/// Fallback concurrency cap when neither the run nor the fleet snapshot pins one.
pub const DEFAULT_MAX_PARALLEL: usize = 4;

/// Max AUTO-retries for a worker task that fails on a RETRYABLE provider error
/// (rate limit / transient gateway / timeout — see [`WorkerRunner::last_error_retryable`]).
/// The first dispatch is attempt 0; each retryable failure bumps `attempt` and
/// re-`pending`s the node with a backoff gate, up to this many retries — so a
/// transient blip (e.g. a per-minute rate limit) self-heals without the node ever
/// permanently failing the run. After the cap the node fails as before. A
/// non-retryable error (auth / billing / bad request) never retries. Overridable
/// per [`RunEngineDeps`] (tests set it small).
pub const DEFAULT_MAX_WORKER_RETRIES: usize = 3;

/// Mid-run batch-progress throttle (Phase 3a Task 3): the settle branch fires a
/// best-effort [`RunOutcome::BatchProgress`] lead report only after AT LEAST this
/// many nodes have completed `done` since the last report. Coupled with
/// [`BATCH_REPORT_INTERVAL`] (BOTH must clear) so a fast fan-out cannot spam the
/// lead conversation. Non-terminal — the run keeps driving.
///
/// 需求4（agent 集群实时反馈）：由 3 降到 1——中途每有节点交付，最迟一个
/// [`BATCH_REPORT_INTERVAL`] 内主 agent 就收到一次批量回执并向用户转述各节点
/// 产出（含 `build_summary_digest` 摘要）。防刷屏由时间间隔单独兜底：20s 内多个
/// 节点完成仍只并成一次回执。
const BATCH_REPORT_MIN_NODES: usize = 1;

/// Minimum wall-clock between two consecutive mid-run batch-progress lead reports
/// (see [`BATCH_REPORT_MIN_NODES`]). The FIRST eligible batch fires immediately
/// (no prior report); subsequent ones wait out this interval.
const BATCH_REPORT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

/// Independent, smaller budget for NO-MARKER failures (timeout / empty reply):
/// a worker that returns `ok:false` with no persisted error marker. Bounded
/// separately from provider-marker retries so a genuinely stuck node cannot loop.
pub const DEFAULT_MAX_TIMEOUT_RETRIES: usize = 2;

/// Base backoff before the FIRST auto-retry; doubles each subsequent retry
/// (`base · 2^attempt`). 5s comfortably clears a per-minute RPM cap within the
/// retry budget while staying snappy for a one-off blip. Overridable per
/// [`RunEngineDeps`] (tests set [`Duration::ZERO`] for instant retries).
pub const DEFAULT_RETRY_BACKOFF_BASE: Duration = Duration::from_secs(5);

/// Cap on a single auto-retry backoff (`retry_backoff_ms`), so a large retry
/// budget with exponential growth can't schedule an absurd multi-hour wait. 60s
/// comfortably spans a per-minute RPM window while staying recoverable.
pub const MAX_RETRY_BACKOFF_MS: i64 = 60_000;

/// Run-level stall watchdog: a `running` run whose execution loop is DEAD
/// (`!is_running`) for at least this long is a soft-strand — the loop exited
/// without writing a terminal status (e.g. `TerminalDecision::Break` / a panicked
/// loop). Force-finalize it. Generous so a boot-resume loop restart or a transient
/// sweep race never trips it; a HEALTHY `running` run always has a live loop and is
/// excluded regardless of age.
pub const STRAND_GRACE_MS: i64 = 5 * 60 * 1000;

/// A `planning` run older than this has a hung/errored planner (the planner LLM is
/// itself bounded by `PLAN_TIMEOUT_SECS`, so anything past that + margin is stuck /
/// left re-plannable and abandoned) → force-finalize as stalled.
pub const PLANNING_MAX_MS: i64 = 10 * 60 * 1000;

// ── Phase 3b: autonomous runaway backstop (deterministic hard budget) ──────
//
// An AUTONOMOUS run drives an EMERGENT loop: a terminal worker's report re-engages
// the lead as a fresh turn, which may call `nomi_run_add_tasks` to append more work
// → re-arms the run → loops. Phase 3b Task 2's autonomous prompt asks the lead to
// stop once the goal is met, but a prompt is COOPERATIVE — the LLM can ignore it.
// These caps are the DETERMINISTIC backstop it CANNOT bypass: the gateway's
// appendability gate folds them in, so an autonomous run at/over EITHER cap becomes
// NOT appendable — the next append is refused WITHOUT re-arming and the run stays
// terminal, halting the loop cleanly. Only `autonomy == "autonomous"` runs are
// bounded here; a manual `nomi_spawn` / `nomi_run_add_tasks` on a supervised run is
// human-driven and unbounded.

/// Cumulative token budget for an AUTONOMOUS run — the sum of its tasks' real usage
/// (`Run.total_tokens`, written by `finish_run` via `sum_task_tokens`). At/over this
/// the run is no longer appendable, so the emergent loop terminates deterministically.
pub const AUTONOMOUS_TOKEN_BUDGET: i64 = 2_000_000;

/// Hard cap on the number of tasks an AUTONOMOUS run may accumulate. At/over this the
/// run is no longer appendable — bounds an append-only runaway even when each round
/// is cheap in tokens (a token cap alone would not stop many tiny rounds).
pub const AUTONOMOUS_MAX_TASKS: usize = 60;

/// Max consecutive autonomous re-engagements that append work WITHOUT completing any
/// new task (no forward progress) before the loop is judged stuck. Consumed by the
/// autonomous re-engagement guard (Phase 3b Task 2); defined here now alongside the
/// other autonomous caps so Task 1 lands the full tunable set.
pub const NO_PROGRESS_LIMIT: u32 = 2;

/// Per-run async lock registry serializing the run-loop's **terminal-check +
/// finish** against the rerun path's **reset + re-activation** (UC-2a 评审
/// Critical). Both critical sections take the SAME run's lock, so the race window
/// — where a rerun resets a task to `pending` while the loop concludes the run is
/// terminal and writes `completed`/`failed` (stranding the run with a pending task
/// and no live loop) — is closed:
///
/// - Under the lock, the loop re-reads the task statuses and ONLY calls
///   `finish_run` if they are still all-terminal; a concurrently-reset `pending`
///   task makes it re-loop (re-pick the task) instead of finishing.
/// - Under the SAME lock, the rerun re-reads the run status (no stale snapshot)
///   and decides re-activation atomically with the reset.
///
/// The map lives on [`RunEngineDeps`] so it is reachable from BOTH the free-
/// function [`run_loop`] (via `deps.run_locks`) and the [`RunEngine::rerun_task`]
/// path (via `self.deps.run_locks`) — a single shared registry, no second source
/// of truth. Locks are created on first access and kept thereafter (the set of run
/// ids in a process is bounded; a stale entry is a cheap idle `Mutex`).
///
/// **No deadlock:** the loop holds a run lock ONLY around the terminal check +
/// `finish_run` (it NEVER awaits a worker future while holding it); the rerun path
/// holds it ONLY around the reset + re-activation DB writes (it NEVER calls
/// `engine.start` while holding it). The two holders never nest, and `start` takes
/// no lock — so they cannot wait on each other in a cycle.
#[derive(Default)]
pub struct RunLocks {
    locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl RunLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (creating on miss) the per-run lock. The returned `Arc<Mutex<()>>` is
    /// cloned out of the map so the caller can `.lock().await` without holding a
    /// `DashMap` shard guard across the await (which would risk a cross-shard
    /// deadlock and pin a shard).
    pub fn for_run(&self, run_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        if let Some(existing) = self.locks.get(run_id) {
            return existing.clone();
        }
        self.locks
            .entry(run_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

/// How long the run loop idles (between paused-status re-checks) when the run is
/// `paused` and has no in-flight workers. A bounded sleep — NOT a busy-spin: the
/// loop yields the runtime each tick, then re-reads the status so a `resume` is
/// observed within ~one interval.
const PAUSE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long the run loop idles when its ONLY outstanding work is a task whose
/// transient-error retry backoff has not yet elapsed (no ready work, no in-flight
/// worker, run not terminal). A bounded sleep — NOT a busy-spin: the gated task is
/// excluded from `list_ready_tasks` until its `next_retry_at`, so without this idle
/// the loop would otherwise mis-read the graph as "stuck" and exit. Re-loops within
/// ~one interval of the backoff elapsing, then `list_ready_tasks` returns the task.
const RETRY_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Shared dependencies for all run loops. The `fleet_snapshot` is read off the
/// run row via `run_repo` (no separate fleet handle is needed at runtime — the
/// snapshot is the single source of truth once a run is created).
pub struct RunEngineDeps {
    pub run_repo: Arc<dyn IRunRepository>,
    pub worker: Arc<dyn WorkerRunner>,
    pub emitter: OrchestratorRunEventEmitter,
    /// Max wall-clock budget for one worker task turn.
    pub worker_timeout: Duration,
    /// Global fallback concurrency cap, used when a run carries no `max_parallel`
    /// of its own (which itself captures the fleet's cap at create time).
    pub default_max_parallel: usize,
    /// Resolves a run's workspace → its `workspace_dir`, injected into the worker
    /// conversation `extra` (fixes the P1a `None` stub).
    pub ws_repo: Arc<dyn IOrchWorkspaceRepository>,
    /// Cancels an in-flight worker conversation on `stop` so its turn ends as
    /// `Finish(Cancelled)` (Task 3). Defaults to [`NoopConversationCanceller`]
    /// (set [`cancel_conversation`](Self::cancel_conversation) afterward to wire a
    /// real one — `build_orchestrator_state` injects the `ConversationService`
    /// wrapper).
    pub cancel_conversation: Arc<dyn ConversationCanceller>,
    /// Steers (mid-turn injects) a message into an in-flight worker conversation
    /// (P3b). Defaults to [`NoopConversationSteerer`] (which errors); the app sets
    /// a real one wrapping `ConversationService::steer_message`.
    pub steer_conversation: Arc<dyn ConversationSteerer>,
    /// Reports a run's terminal / notable outcome back to the LEAD conversation
    /// (`run.lead_conv_id`), re-engaging the master agent once per notable event
    /// instead of busy-polling. Defaults to [`NoopLeadReporter`] (no-op); the app
    /// sets a real one wrapping `ConversationService::steer_message` in
    /// `build_orchestrator_state`. FAIL-SOFT: a report error never fails the run.
    pub lead_reporter: Arc<dyn LeadReporter>,
    /// Per-run lock registry serializing the loop's terminal-check+finish with the
    /// rerun reset+re-activation (UC-2a 评审 Critical — see [`RunLocks`]). Reachable
    /// from `run_loop` (here, via `deps`) and `RunEngine::rerun_task` (via its
    /// `deps`), so BOTH critical sections take the same run's lock.
    pub run_locks: Arc<RunLocks>,
    /// B2: the lead-model run-summarizer. When a run COMPLETES, the loop asks this
    /// producer for a one-shot synthesized run summary (`PlanProducer::summarize`)
    /// instead of writing the mechanical `aggregate_summary` concat. Defaults to
    /// [`NoopSummaryProducer`] (whose `summarize` errors) so the engine FALLS BACK
    /// to `aggregate_summary` — zero regression. `build_orchestrator_state` injects
    /// the same `LlmPlanProducer` it gives `RunService` (a real one-shot summary).
    /// FAIL-SOFT: any error here NEVER fails the run — it only loses the synthesis.
    pub summarizer: Arc<dyn PlanProducer>,
    /// Max AUTO-retries for a worker task that fails on a RETRYABLE provider error
    /// (see [`DEFAULT_MAX_WORKER_RETRIES`]). 0 disables auto-retry (every failure is
    /// terminal — the pre-retry behaviour).
    pub max_worker_retries: usize,
    /// Max auto-retries for a NO-MARKER failure (timeout / empty reply). See
    /// [`DEFAULT_MAX_TIMEOUT_RETRIES`]. 0 disables timeout auto-retry.
    pub max_timeout_retries: usize,
    /// Base backoff before the first auto-retry, doubled per retry
    /// (see [`DEFAULT_RETRY_BACKOFF_BASE`]). [`Duration::ZERO`] retries instantly.
    pub retry_backoff_base: Duration,
    /// Base data dir under which a workspace-less (ad-hoc / conversation-native)
    /// run gets an auto-allocated SHARED working directory
    /// (`<data_dir>/orchestrator/runs/{run_id}`), so all its nodes share one cwd
    /// (files + a shared workpath KB binding). See `run_loop`'s workspace_dir
    /// resolution. Defaults to `.` (tests override; the app injects the real
    /// data dir via `services.work_dir`).
    pub data_dir: std::path::PathBuf,
}

impl RunEngineDeps {
    /// Construct with the global default concurrency cap
    /// ([`DEFAULT_MAX_PARALLEL`]); set `default_max_parallel` afterward to
    /// override. `ws_repo` is required (workspace_dir resolution has no sane
    /// fallback). `cancel_conversation` defaults to a no-op; set it afterward to
    /// propagate cancellation to in-flight worker conversations.
    pub fn new(
        run_repo: Arc<dyn IRunRepository>,
        worker: Arc<dyn WorkerRunner>,
        emitter: OrchestratorRunEventEmitter,
        ws_repo: Arc<dyn IOrchWorkspaceRepository>,
    ) -> Self {
        Self {
            run_repo,
            worker,
            emitter,
            worker_timeout: DEFAULT_WORKER_TIMEOUT,
            default_max_parallel: DEFAULT_MAX_PARALLEL,
            ws_repo,
            cancel_conversation: Arc::new(NoopConversationCanceller),
            steer_conversation: Arc::new(NoopConversationSteerer),
            lead_reporter: Arc::new(NoopLeadReporter),
            run_locks: Arc::new(RunLocks::new()),
            summarizer: Arc::new(NoopSummaryProducer),
            max_worker_retries: DEFAULT_MAX_WORKER_RETRIES,
            max_timeout_retries: DEFAULT_MAX_TIMEOUT_RETRIES,
            retry_backoff_base: DEFAULT_RETRY_BACKOFF_BASE,
            data_dir: std::path::PathBuf::from("."),
        }
    }
}

/// One running loop's handle. The `generation` lets a naturally-exiting loop
/// remove only its own entry (not a fresh one a concurrent `start` inserted).
struct RunHandle {
    cancelled: Arc<AtomicBool>,
    /// The spawned loop task; `stop` aborts it (covers a long in-flight worker).
    join: tokio::task::JoinHandle<()>,
    generation: u64,
}

/// Removes a loop's handle from the registry on task exit — normal OR panic
/// (Drop runs during unwind). The generation guard prevents clobbering a fresh
/// handle a concurrent `start` may have inserted.
struct HandleGuard {
    handles: Arc<DashMap<String, RunHandle>>,
    run_id: String,
    generation: u64,
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        self.handles
            .remove_if(&self.run_id, |_, h| h.generation == self.generation);
    }
}

/// Drives per-run bounded-parallel execution loops.
#[derive(Clone)]
pub struct RunEngine {
    deps: Arc<RunEngineDeps>,
    handles: Arc<DashMap<String, RunHandle>>,
    next_generation: Arc<AtomicU64>,
}

impl RunEngine {
    pub fn new(deps: Arc<RunEngineDeps>) -> Self {
        Self {
            deps,
            handles: Arc::new(DashMap::new()),
            next_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Is a loop currently registered for this run?
    pub fn is_running(&self, run_id: &str) -> bool {
        self.handles.contains_key(run_id)
    }

    /// Start (or restart) the execution loop for a run. Stops any existing loop
    /// for the same run first (cooperative cancel + abort), then spawns a fresh
    /// one. Idempotent in the sense that a second `start` simply replaces the
    /// first; combined with `is_running`, callers can guard re-entry.
    pub fn start(&self, run_id: String) {
        self.stop(&run_id);

        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_task = cancelled.clone();
        let deps = self.deps.clone();
        let handles = self.handles.clone();
        let loop_run_id = run_id.clone();
        let guard_run_id = run_id.clone();

        let join = tokio::spawn(async move {
            // Drop runs on normal exit AND panic-unwind → handle always removed.
            let _guard = HandleGuard {
                handles: handles.clone(),
                run_id: guard_run_id,
                generation,
            };
            info!(run_id = %loop_run_id, "Run engine loop started");
            run_loop(deps, &loop_run_id, cancelled_for_task, handles, generation).await;
            info!(run_id = %loop_run_id, "Run engine loop exited");
        });

        self.handles.insert(
            run_id,
            RunHandle {
                cancelled,
                join,
                generation,
            },
        );
    }

    fn stop_registered_loop(&self, run_id: &str) {
        if let Some((_, handle)) = self.handles.remove(run_id) {
            handle.cancelled.store(true, Ordering::SeqCst);
            handle.join.abort();
        }
    }

    /// Stop a run's loop: set the cooperative cancel flag, abort the task, and
    /// cancel any in-flight worker conversations so their turns end as
    /// `Finish(Cancelled)`.
    ///
    /// The loop checks the flag between tasks; the abort covers a long in-flight
    /// worker await. **Cancel propagation (Task 3):** aborting the loop task drops
    /// the in-flight worker futures but does NOT stop the underlying agent turns —
    /// those run on independent runtime tasks. So we additionally find the run's
    /// `running` tasks (their `conversation_id` was stamped live via `on_started`)
    /// and cancel each conversation, making the worker's `await_turn` see
    /// `is_processing` clear and return `ok = false`.
    ///
    /// Done on a detached task because `stop` is synchronous (called from the
    /// cancel route before the persisted [`RunService::cancel`]); the DB query +
    /// per-conversation cancel are async. We query by THIS run's tasks, so only
    /// this run's conversations are cancelled. Persisting `cancelled` is the
    /// service's job ([`RunService::cancel`](crate::run_service::RunService::cancel)).
    pub fn stop(&self, run_id: &str) {
        self.stop_registered_loop(run_id);
        // Cancel in-flight worker conversations for this run (detached + best
        // effort). Idempotent: if no task is running / no conversation is stamped
        // / no live agent exists, the canceller no-ops. Safe to run even when the
        // loop was not registered (a stale `running` row with no live loop).
        let deps = self.deps.clone();
        let run_id = run_id.to_string();
        tokio::spawn(async move {
            cancel_in_flight_conversations(&deps, &run_id).await;
        });
    }

    /// Stop a run's loop and wait until in-flight worker cancellation has been
    /// attempted. Use this before immediately rewriting `running` task rows (for
    /// example, pause -> reset to `pending`); otherwise the detached `stop()` query
    /// can race with the row rewrite and miss the worker conversation.
    pub async fn stop_and_cancel_in_flight(&self, run_id: &str) {
        self.stop_registered_loop(run_id);
        cancel_in_flight_conversations(&self.deps, run_id).await;
    }

    /// Run-level stall watchdog (safety net): finalize runs stuck non-terminal with
    /// NO live execution loop (soft-strand) or a long-hung `planning` (planner black
    /// hole) as `failed(stalled)`, so EVERY run eventually reaches a terminal state
    /// and is reported to its lead — no silent black holes where the master agent
    /// waits forever. Conservative: a healthy `running` run always has a live loop
    /// (`is_running`), so it is NEVER touched regardless of age; only a `running`
    /// run whose loop is dead past [`STRAND_GRACE_MS`], or a `planning` run older
    /// than [`PLANNING_MAX_MS`], is a candidate. Idempotent + per-run-lock guarded.
    /// The app spawns a periodic caller (e.g. every 60s).
    pub async fn reap_stalled_runs(&self) {
        let now = now_ms();
        let mut candidates: Vec<OrchRunRow> = Vec::new();
        // `running` with a DEAD loop past the grace = soft-strand (loop exited
        // without a terminal write). A live loop → healthy → skipped here.
        for row in self
            .deps
            .run_repo
            .list_runs_by_status("running")
            .await
            .unwrap_or_default()
        {
            if !self.is_running(&row.id) && now.saturating_sub(row.updated_at) >= STRAND_GRACE_MS {
                candidates.push(row);
            }
        }
        // `planning` older than the ceiling = hung/errored planner.
        for row in self
            .deps
            .run_repo
            .list_runs_by_status("planning")
            .await
            .unwrap_or_default()
        {
            if now.saturating_sub(row.updated_at) >= PLANNING_MAX_MS {
                candidates.push(row);
            }
        }
        for row in candidates {
            self.reap_one_stalled(row).await;
        }
    }

    /// Finalize ONE stalled run as `failed(stalled)` under its per-run lock (re-
    /// verifying it is still non-terminal — and, for a `running` run, still loop-
    /// dead — so a concurrent finish/restart WINS), stop any residual handle +
    /// cancel in-flight workers, then report `Stalled` to the lead (best-effort).
    async fn reap_one_stalled(&self, row: OrchRunRow) {
        let run_id = row.id.clone();
        let lock = self.deps.run_locks.for_run(&run_id);
        let finalized = {
            let _guard = lock.lock().await;
            match self.deps.run_repo.get_run(&run_id).await {
                Ok(Some(r)) if matches!(r.status.as_str(), "running" | "planning") => {
                    // A `running` run whose loop came back alive under the lock is
                    // healthy after all — do not reap.
                    if r.status == "running" && self.is_running(&run_id) {
                        false
                    } else if r.status == "running"
                        && self
                            .deps
                            .run_repo
                            .list_tasks(&run_id)
                            .await
                            .map(|ts| ts.iter().any(|t| t.status == "needs_review"))
                            .unwrap_or(false)
                    {
                        // 审批模式（迁移 030）：节点挂在 needs_review 等人工作答——
                        // 合法停车（可等数小时），绝不判 stalled。用户 adopt/rerun
                        // 后路由重启循环；此处直接放行。
                        false
                    } else {
                        let summary = format!(
                            "编排卡死：状态「{}」超过阈值无进展，已由看门狗终止。",
                            r.status
                        );
                        finish_run(&self.deps, &run_id, "failed", Some(summary), None).await;
                        true
                    }
                }
                // Already terminal / gone → nothing to do.
                _ => false,
            }
        };
        if !finalized {
            return;
        }
        warn!(
            run_id,
            "Run watchdog: reaped stalled run as failed(stalled)"
        );
        // Clean up any residual (dead/wedged) loop handle + in-flight worker convs.
        self.stop(&run_id);
        // Report to the lead so the master agent surfaces the stall (lock-free).
        if let Some(lead_conv_id) = row.lead_conv_id {
            let brief = match self.deps.run_repo.list_tasks(&run_id).await {
                Ok(tasks) if !tasks.is_empty() => build_summary_digest(&tasks),
                _ => "（run 长时间无进展，无已知节点产出）".to_string(),
            };
            if let Err(e) = self
                .deps
                .lead_reporter
                .report(lead_conv_id, &run_id, RunOutcome::Stalled, &brief)
                .await
            {
                warn!(run_id, error = %e, "stalled-run lead report failed");
            }
        }
    }

    /// Resume every persisted `running` run at boot. The running set (`handles`)
    /// is in-memory, but run status is persisted — on a process restart nothing
    /// would drive a `running` run until... never. This makes the backend the
    /// single source of truth: a `running` run resumes from boot. Idempotent via
    /// `is_running`. Detached + best-effort (mirrors `resume_persisted_bindings`).
    pub fn resume_persisted_runs(&self, run_repo: Arc<dyn IRunRepository>) {
        let this = self.clone();
        tokio::spawn(async move {
            // RECONCILE FIRST: a fresh process has NO live workers, so EVERY
            // persisted `running` task is an orphan (the worker/loop that owned it
            // died with the previous process). Settle them back to `pending` BEFORE
            // restarting any loop, restoring the invariant `running ⟺ live worker`.
            // Without this, an orphaned `running` task shows「执行中」forever and the
            // rerun guard rejects it ("任务运行中…") — permanently stuck. Resettable
            // runs then re-dispatch these pending tasks; terminal runs leave them
            // re-runnable. Kind-aware (verify/judge/loop keep their policy config).
            match run_repo.reset_orphaned_running_tasks(None).await {
                Ok(n) if n > 0 => {
                    info!(
                        reset = n,
                        "Run engine boot: reset orphaned running tasks → pending"
                    )
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "Run engine boot: reset orphaned running tasks failed"),
            }

            let runs = match run_repo.list_runs_by_status("running").await {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "Run engine resume: list_runs_by_status failed");
                    return;
                }
            };
            let mut resumed = 0usize;
            for run in runs {
                if this.is_running(&run.id) {
                    continue;
                }
                this.start(run.id);
                resumed += 1;
            }
            if resumed > 0 {
                info!(resumed, "Run engine resumed persisted running runs on boot");
            }
        });
    }

    /// Steer (mid-turn inject) a message into the worker conversation of a task
    /// (P3b). The task must belong to `run_id` and carry a stamped
    /// `conversation_id` (i.e. its worker is — or was — live); we then delegate to
    /// the injected [`ConversationSteerer`]. Steering does NOT change the run's
    /// status (it nudges a running worker, it does not pause/resume/cancel).
    ///
    /// Lives on the engine (not [`RunService`](crate::run_service::RunService))
    /// because the conversation layer is reachable here via `steer_conversation`
    /// — exactly the seam [`ConversationCanceller`] uses for cancel. Guards:
    /// - run / task not found → `NotFound` (404);
    /// - task not in `run_id` → `NotFound` (404);
    /// - task has no `conversation_id` (never dispatched) → `BadRequest` (400);
    /// - a non-Nomi engine that cannot steer → the steerer's `BadRequest` (400).
    pub async fn steer_task(
        &self,
        run_id: &str,
        task_id: &str,
        text: &str,
    ) -> Result<(), AppError> {
        if text.trim().is_empty() {
            return Err(AppError::BadRequest(
                "steer text must not be empty".to_owned(),
            ));
        }
        // Confirm the run exists (clean 404 vs. a confusing task-only error).
        if self
            .deps
            .run_repo
            .get_run(run_id)
            .await
            .map_err(|e| AppError::Internal(format!("orchestrator database error: {e}")))?
            .is_none()
        {
            return Err(AppError::NotFound(format!("run {run_id}")));
        }
        // The task must exist and belong to this run.
        let task = self
            .deps
            .run_repo
            .get_task(task_id)
            .await
            .map_err(|e| AppError::Internal(format!("orchestrator database error: {e}")))?
            .ok_or_else(|| AppError::NotFound(format!("task {task_id}")))?;
        if task.run_id != run_id {
            return Err(AppError::NotFound(format!(
                "task {task_id} in run {run_id}"
            )));
        }
        // A stamped conversation_id means the task's worker is (or was) live; with
        // no conversation there is nothing to steer.
        let Some(conv_id) = task.conversation_id else {
            return Err(AppError::BadRequest(format!(
                "task {task_id} has no worker conversation to steer (not dispatched yet)"
            )));
        };
        self.deps.steer_conversation.steer(conv_id, text).await
    }

    /// Re-execute a single node (UC-2a) with the loop-vs-rerun race CLOSED. This is
    /// the engine-side entry the route calls instead of `RunService::rerun_task`
    /// directly: it acquires the run's lock (the SAME [`RunLocks`] registry the
    /// `run_loop` terminal check holds — see [`RunLocks`]) and performs the reset +
    /// cascade + re-activation UNDER it, so the loop cannot conclude the run is
    /// terminal (and write `completed`/`failed`) in the gap between our reset and
    /// re-activation.
    ///
    /// Returns the (possibly re-activated) run DTO. The CALLER (route) then makes
    /// the engine-lifecycle decision — `if run.status == "running" && !is_running →
    /// engine.start` — OUTSIDE this lock. We deliberately do NOT call `start` here:
    /// `start` first `stop`s (aborting the prior loop task, which may be parked
    /// holding this very lock at its terminal check); calling `start` while WE hold
    /// the lock is technically safe (the aborted task's guard drops on unwind), but
    /// keeping `start` strictly outside the lock keeps the no-deadlock invariant
    /// trivially obvious — the lock is only ever held around pure DB mutations on
    /// both sides, never across a `start`/`stop`/worker await.
    pub async fn rerun_task(
        &self,
        run_service: &crate::run_service::RunService,
        user_id: &str,
        run_id: &str,
        task_id: &str,
    ) -> Result<nomifun_api_types::Run, AppError> {
        let lock = self.deps.run_locks.for_run(run_id);
        let _rerun_guard = lock.lock().await;
        // LIVENESS RECOVERY: `RunService::rerun_task` rejects a `running` target
        // (its live worker would settle `done` over the reset). But if THIS run has
        // no live loop (`!is_running`), a `running` task is a phantom ORPHAN — its
        // worker died (e.g. a mid-session loop panic that deregistered the handle,
        // or a stop before boot re-armed) — so the guard would wrongly wedge the
        // node forever. Normalize the run's orphaned `running` → `pending` first (we
        // hold the run lock + there is no loop, so nothing else races the reset),
        // then rerun proceeds. A live run (`is_running`) is left untouched — its
        // `running` really is live and the guard correctly asks the user to
        // pause/stop first.
        if !self.is_running(run_id) {
            run_service.reset_orphaned_running(run_id).await?;
        }
        run_service.rerun_task(user_id, run_id, task_id).await
    }

    /// UC-2c — **"采用为该节点产出" (adopt task result)**. The engine-side entry the
    /// route calls instead of [`RunService::adopt_task_result`] directly, so the
    /// service's node-write + terminal-run re-activation run UNDER the per-run lock
    /// (the SAME lock the run-loop's terminal-check-and-finish holds) — closing the
    /// re-activation race exactly as [`rerun_task`](Self::rerun_task) does. The lock
    /// is released before the route's engine-lifecycle decision (`start` only when
    /// the loop is not already running). The worker handle the service needs to read
    /// the conversation's final output is the engine's own `deps.worker` — the same
    /// runner that produced the node — so the read goes through the production
    /// `ConversationWorkerRunner` (the mock's default returns `None`).
    pub async fn adopt_task_result(
        &self,
        run_service: &crate::run_service::RunService,
        user_id: &str,
        run_id: &str,
        task_id: &str,
    ) -> Result<nomifun_api_types::Run, AppError> {
        let lock = self.deps.run_locks.for_run(run_id);
        let _adopt_guard = lock.lock().await;
        run_service
            .adopt_task_result(&self.deps.worker, user_id, run_id, task_id)
            .await
    }

    /// 审批模式（迁移 030）— **节点提问 (`nomi_task_question`)**。worker 会话在自己的
    /// 回合中途遇关键决策时调用：把问题原文写到 `pending_question`、任务转入
    /// `needs_review`（挂起，下游保持阻塞）、emit `task.statusChanged` 点亮画布/进度条
    /// 的提问徽标，并（锁外、best-effort）向 lead 回执 [`RunOutcome::NodeQuestion`]，
    /// 由主 agent 提醒用户进入该节点作答。恢复路径：用户在 worker 会话内回答 →
    /// 「采用为该节点产出」（adopt 清问题、置 done、重激活）或重跑。
    ///
    /// 守卫：run 必须归属 `user_id`（不泄露存在性——非 owner 一律 NotFound）、
    /// `approval_mode == "manual"`（全授权 run 拒绝——worker 应自行决策）、任务属于该
    /// run 且处于 `running`（本轮在飞）或 `needs_review`（幂等更新问题文本）。
    /// DB 写在 per-run 锁内，与循环的 settle/终态判定串行；lead 回执在锁外。
    pub async fn raise_task_question(
        &self,
        user_id: &str,
        run_id: &str,
        task_id: &str,
        question: &str,
    ) -> Result<(), AppError> {
        let question = question.trim();
        if question.is_empty() {
            return Err(AppError::BadRequest("question must not be empty".into()));
        }
        let lock = self.deps.run_locks.for_run(run_id);
        let (title, lead_conv_id) = {
            let _guard = lock.lock().await;
            let run = self
                .deps
                .run_repo
                .get_run(run_id)
                .await
                .map_err(|e| AppError::Internal(e.to_string()))?
                .ok_or_else(|| AppError::NotFound(format!("run {run_id}")))?;
            if run.user_id != user_id {
                return Err(AppError::NotFound(format!("run {run_id}")));
            }
            if run.approval_mode.as_deref() != Some("manual") {
                return Err(AppError::BadRequest(
                    "本编排未开启「审批模式」：不要提问，请自行选择最合理的方案继续执行，并在产出中说明你的选择理由。".into(),
                ));
            }
            let task = self
                .deps
                .run_repo
                .get_task(task_id)
                .await
                .map_err(|e| AppError::Internal(e.to_string()))?
                .ok_or_else(|| AppError::NotFound(format!("task {task_id}")))?;
            if task.run_id != run_id {
                return Err(AppError::NotFound(format!("task {task_id} in run {run_id}")));
            }
            if !matches!(task.status.as_str(), "running" | "needs_review") {
                return Err(AppError::BadRequest(
                    "该节点当前不在执行中，无法提交决策问题。".into(),
                ));
            }
            self.deps
                .run_repo
                .set_task_question(task_id, Some(question))
                .await
                .map_err(|e| AppError::Internal(e.to_string()))?;
            update_task_status(&self.deps, task_id, "needs_review").await;
            self.deps
                .emitter
                .emit_task_status(run_id, task_id, "needs_review");
            (task.title, run.lead_conv_id)
        };
        // Lead 回执（锁外、best-effort）：主 agent 提醒用户进入该节点作答。失败只
        // warn——问题已落库、画布已点亮，用户仍能从 UI 发现并处理。
        if let Some(lead_conv_id) = lead_conv_id {
            let brief = format!("节点『{title}』提问：{question}");
            if let Err(e) = self
                .deps
                .lead_reporter
                .report(lead_conv_id, run_id, RunOutcome::NodeQuestion, &brief)
                .await
            {
                warn!(run_id, task_id, error = %e, "node-question lead report failed");
            }
        }
        Ok(())
    }

    /// UC-3a — **conversation-driven intelligent re-adjust** with the loop-vs-
    /// reconcile race CLOSED **and the lead LLM call moved OUT of the per-run lock
    /// (B4)**. The engine-side entry the route calls instead of
    /// [`RunService::adjust`](crate::run_service::RunService::adjust) directly.
    ///
    /// **Lock invariant (B4 — the per-run lock MUST NOT span an LLM await):** the
    /// adjust is split into two halves, mirroring the B2 summarize lock-split:
    ///   1. [`compute_adjusted_plan`](crate::run_service::RunService::compute_adjusted_plan)
    ///      runs LOCK-FREE — it snapshots the run, validates fast (404/403/empty
    ///      intent/no-running), and awaits the lead LLM (streamed over a phase
    ///      `"adjust"` throttle so the planning thought fans out live over WS). A
    ///      multi-second / hanging lead call therefore NEVER stalls a concurrent
    ///      rerun/loop on this run — the lock is simply not held across it.
    ///   2. [`apply_adjusted_plan`](crate::run_service::RunService::apply_adjusted_plan)
    ///      runs UNDER the per-run lock — it RE-READS the run state FRESH (NOT the
    ///      compute snapshot), RE-VALIDATES (no-running / kept-exists / acyclic), then
    ///      reconciles in one transaction + re-activates a terminal run. Zero LLM
    ///      await under the lock. This serializes with the run loop's
    ///      terminal-check-and-finish so the loop cannot conclude the run terminal
    ///      (writing `completed`/`failed`) in the gap between our mutation and
    ///      re-activation, stranding the run with freshly-added pending tasks.
    ///
    /// **TOCTOU:** the snapshot taken lock-free in step 1 is the LEAD's input only.
    /// Step 2 re-reads + re-validates under the lock, so a rerun/loop that lands
    /// during the LLM await is caught there (a vanished kept id / a newly-`running`
    /// task → BadRequest, run UNCHANGED) — never trusted blindly.
    ///
    /// Returns the (possibly re-activated) run DTO; the CALLER (route) makes the
    /// engine-lifecycle decision (`run.status == "running" && !is_running →
    /// engine.start`) OUTSIDE this lock, exactly like [`rerun_task`](Self::rerun_task)
    /// — `start` (which `stop`s first) is kept strictly off the locked path so the
    /// no-deadlock invariant stays trivially obvious.
    pub async fn adjust(
        &self,
        run_service: &crate::run_service::RunService,
        user_id: &str,
        run_id: &str,
        intent: &str,
    ) -> Result<nomifun_api_types::Run, AppError> {
        // ① LOCK-FREE: compute the adjusted plan (snapshot + lead LLM), streaming the
        // lead's adjust-phase thought over WS via a phase="adjust" throttle (same
        // wiring as `run_service.plan()` and the engine's `compute_completed_summary`).
        // The throttle coalesces deltas to avoid WS flooding; `flush()` after the call
        // emits the residue (nothing dropped). NO per-run lock is held across this.
        let throttle =
            crate::plan::LeadThinkingThrottle::new(self.deps.emitter.clone(), run_id, "adjust");
        let sink = throttle.sink();
        let computed = run_service
            .compute_adjusted_plan(user_id, run_id, intent, Some(&sink))
            .await;
        throttle.flush();
        let adjusted = computed?;

        // ② UNDER THE LOCK: apply the precomputed plan (pure DB — re-read FRESH +
        // re-validate + reconcile + re-activate). Zero LLM await inside the guard.
        let lock = self.deps.run_locks.for_run(run_id);
        let guard = lock.lock().await;
        let run = run_service
            .apply_adjusted_plan(user_id, run_id, adjusted)
            .await;
        drop(guard);
        run
    }

    /// **Phase 3a — runtime task APPEND under the per-run lock.** The engine-side
    /// entry the master agent (route/gateway) calls instead of
    /// [`RunService::add_tasks`](crate::run_service::RunService::add_tasks)
    /// directly, so the service's insert + terminal-run re-arm run UNDER the per-run
    /// lock — the SAME [`RunLocks`] registry the [`run_loop`]'s terminal-check-and-
    /// finish holds. This closes the append-vs-finish race: without the lock the loop
    /// could read `all_settled == true`, this append could insert a `pending` task in
    /// the gap, then the loop `finish_run`s `completed` AND deregisters its handle →
    /// a terminal run with an un-run pending task and no live driver (boot-resume only
    /// re-lists `running`, so it would never recover). Holding the lock makes the
    /// insert + re-arm atomic w.r.t. the loop's terminal decision.
    ///
    /// Unlike [`adjust`](Self::adjust) there is NO LLM await here — the whole call is
    /// pure DB — so the entire method runs under the lock (mirrors
    /// [`rerun_task`](Self::rerun_task) / [`adopt_task_result`](Self::adopt_task_result)).
    /// Returns the (possibly re-armed) run DTO; the CALLER (route) makes the
    /// engine-lifecycle decision (`run.status == "running" && !is_running →
    /// engine.start`) OUTSIDE this lock, keeping `start` (which `stop`s first) off the
    /// locked path so the no-deadlock invariant stays trivially obvious.
    pub async fn add_tasks(
        &self,
        run_service: &crate::run_service::RunService,
        run_id: &str,
        tasks: Vec<nomifun_api_types::PlannedTask>,
    ) -> Result<nomifun_api_types::Run, AppError> {
        let lock = self.deps.run_locks.for_run(run_id);
        let _guard = lock.lock().await;
        run_service.add_tasks(run_id, tasks).await
        // Lock drops here — the insert + re-arm were atomic w.r.t. the run_loop's
        // terminal-check-and-finish (no strand, no orphaned terminal run).
    }
}

/// The bounded-parallel run loop: dispatch up to `cap` ready tasks concurrently,
/// awaiting in-flight workers, until the run reaches a terminal state, then
/// settle the run row + emit and exit.
///
/// `handles` + `generation` let the loop DEREGISTER its own handle UNDER the run
/// lock at the moment it `finish_run`s (closing the UC-2a 评审 Critical variant-A
/// window: a rerun that lands between the status write and the [`HandleGuard`]
/// drop would otherwise see `is_running == true` and skip the restart). Removing
/// the handle inside the same lock the terminal check holds makes
/// `is_running()` flip false ATOMICALLY with the terminal status write, so the
/// rerun (serialized after on the lock) observes a stopped loop and the route
/// `engine.start`s a fresh one. The `HandleGuard` still covers the panic / early-
/// return paths (its remove is idempotent + generation-guarded).
async fn run_loop(
    deps: Arc<RunEngineDeps>,
    run_id: &str,
    cancelled: Arc<AtomicBool>,
    handles: Arc<DashMap<String, RunHandle>>,
    generation: u64,
) {
    // Resolve the run once for cap + workspace; if the run row is unreadable we
    // cannot drive anything — bail (the handle guard still deregisters).
    let run = match deps.run_repo.get_run(run_id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            warn!(run_id, "Run loop: run not found — exiting");
            return;
        }
        Err(e) => {
            warn!(run_id, error = %e, "Run loop: get_run failed — exiting");
            return;
        }
    };

    // cap = run.max_parallel (which already captured the fleet's cap at create
    // time, or is None) → else the global default. Clamp to >= 1 so the loop
    // always makes progress. The fleet_snapshot layer is intentionally dropped:
    // run.max_parallel is the run's own materialized copy of it.
    let cap = run
        .max_parallel
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(deps.default_max_parallel)
        .max(1);

    // Resolve the run's workspace_dir once — it is stable for the run's lifetime
    // (the workspace row's dir does not change mid-run in this design). An ad-hoc
    // (workspace-less) run carries its own `work_dir`, which takes precedence;
    // otherwise fall back to the owning workspace's dir. A run with neither has no
    // cwd (workers run in their default location).
    let workspace_dir: Option<String> = if let Some(wd) = run
        .work_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(wd.to_string())
    } else if let Some(ws_id) = run.workspace_id.as_deref() {
        deps.ws_repo
            .get(ws_id)
            .await
            .ok()
            .flatten()
            .and_then(|w| w.workspace_dir)
    } else {
        None
    };

    // Ad-hoc/conversation-native run with no dir: auto-allocate a per-run SHARED
    // directory so all nodes share one cwd (files + a shared workpath KB binding).
    // Deterministic path (`<data_dir>/orchestrator/runs/{run_id}`, stable across
    // restarts); persisted to `run.work_dir` so browse + boot-resume resolve it
    // (and the resolution above reads it back on a later loop). Best-effort — a
    // create/persist failure logs and falls back to per-node temp dirs (never
    // fails the run). Runs once per loop, before the fill loop.
    let workspace_dir: Option<String> = match workspace_dir {
        Some(d) => Some(d),
        // Only a TRULY ad-hoc run (no workspace binding) gets an auto-allocated
        // shared dir. A workspace-bound run whose workspace dir couldn't be
        // resolved (e.g. a transient ws read error) is left as-is — never rebind
        // it to an ad-hoc dir (the persist would stick and permanently rebind it).
        None if run.workspace_id.is_none() => {
            let dir = deps
                .data_dir
                .join("orchestrator")
                .join("runs")
                .join(run_id);
            match std::fs::create_dir_all(&dir) {
                Ok(()) => {
                    let dir_str = dir.to_string_lossy().to_string();
                    if let Err(e) = deps
                        .run_repo
                        .update_run(
                            run_id,
                            UpdateRunParams {
                                work_dir: Some(Some(dir_str.clone())),
                                ..Default::default()
                            },
                        )
                        .await
                    {
                        warn!(run_id, error = %e, "failed to persist auto shared work_dir (continuing)");
                    }
                    Some(dir_str)
                }
                Err(e) => {
                    warn!(run_id, error = %e, "failed to create shared run dir (nodes use temp dirs)");
                    None
                }
            }
        }
        None => None,
    };

    // Seed a human-readable RUN_NOTES.md in the shared dir (best-effort, idempotent
    // — only if absent, to preserve node-appended content across restart/rerun). The
    // plan is already persisted at this point (the choreography plans BEFORE
    // `engine.start`), so `list_tasks` here returns the planned nodes' titles. A
    // workspace-bound run also benefits (its shared dir gets the notes seed).
    if let Some(ref dir) = workspace_dir {
        seed_run_notes(&deps, run_id, dir).await;
    }

    // In-flight worker futures, each resolving to (task_id, outcome). The set's
    // length is the live concurrency; we never exceed `cap`.
    let mut inflight: FuturesUnordered<WorkerFuture> = FuturesUnordered::new();
    // Tasks currently in-flight — so a re-query of the ready set does not
    // re-dispatch a task whose worker is still running (list_ready_tasks keys off
    // persisted status, and a task is marked `running` before its future is
    // pushed, so this is belt-and-suspenders against a status read race).
    let mut in_progress: HashSet<String> = HashSet::new();

    // Terminal outcome to report back to the LEAD conversation AFTER the loop exits
    // (lock-free, exactly-once). Set only on completed/failed; `Break` (stuck/read
    // error) leaves it None → the run-level watchdog reports those.
    let mut terminal_report: Option<(RunOutcome, String)> = None;

    // Mid-run batch-progress throttle state (Phase 3a Task 3). `done_since_report`
    // counts nodes that finished `done` since the last batch report; `last_batch_report`
    // gates the wall-clock interval (None = never reported → first eligible batch
    // fires immediately). See BATCH_REPORT_MIN_NODES / BATCH_REPORT_INTERVAL.
    let mut last_batch_report: Option<std::time::Instant> = None;
    let mut done_since_report: usize = 0;

    loop {
        // (a) Cancelled → stop scheduling. Task 3 adds in-flight cancel
        // propagation; here we simply stop dispatching and let the loop unwind
        // (the spawned loop task is also aborted by `stop`).
        if cancelled.load(Ordering::SeqCst) {
            info!(run_id, "Run loop cancelled — exiting");
            break;
        }

        // (a') Paused gate (P3b): re-read the persisted run status each iteration.
        // The product pause route stops the loop and cancels live workers before
        // resetting interrupted nodes. This gate remains defensive for a loop that
        // observes `paused` through a direct state transition: it dispatches no new
        // workers and idles when empty until `resume` flips the status to `running`.
        // A read error is treated as not-paused (fail-open: better to keep driving
        // than to wedge on a transient error).
        let status = match deps.run_repo.get_run(run_id).await {
            Ok(Some(r)) => Some(r.status),
            _ => None,
        };
        let paused = matches!(status.as_deref(), Some("paused"));

        // (a'') Awaiting-approval gate (P6 Task 1): an `interactive` run parked at
        // `awaiting_plan_approval` must NOT dispatch any worker — the human-in-the-
        // loop sits at the PLAN gate, approved via `approve_plan` (which then
        // `engine.start`s the loop afresh). The conversation-native choreography
        // already SKIPS `engine.start` for an awaiting run, so this is
        // defense-in-depth: a stray start (or a future boot-resume that mis-listed
        // an awaiting run) must run nothing. With no in-flight work on a fresh start
        // there is nothing to drain, so we exit cleanly — approval will restart us.
        if matches!(status.as_deref(), Some("awaiting_plan_approval")) && inflight.is_empty() {
            info!(
                run_id,
                "Run loop: run awaits plan approval — not dispatching (exiting until approved)"
            );
            break;
        }

        // (b) Fill: dispatch ready tasks up to the free slots — SKIPPED while
        // paused (no new workers dispatch). Re-query every fill so completion-
        // driven unblocking is observed. A list error is not fatal mid-flight
        // (workers may still be running) — log and proceed to the await branch;
        // the next fill retries.
        // (b) Fill: dispatch ready tasks up to the free slots — SKIPPED while
        // paused (no new workers dispatch). Re-query every fill so completion-
        // driven unblocking is observed. A list error is not fatal mid-flight
        // (workers may still be running) — log and proceed to the await branch;
        // the next fill retries.
        //
        // `settled_sync` records whether this fill pass settled a SYNCHRONOUS
        // aggregator (a `verify` or `judge` task) NO-LLM. A settle is genuine
        // forward progress (a task went pending→done and may have unblocked
        // downstream), so when it happens we re-loop to re-fill BEFORE the
        // terminal decision — otherwise an aggregator that settles with no worker
        // in flight would be misread as a "stuck" graph (its downstream is freshly
        // ready but not yet dispatched). This is NOT a busy-spin: it only re-loops
        // because a task actually transitioned.
        let mut settled_sync = false;
        if !paused && inflight.len() < cap {
            match deps.run_repo.list_ready_tasks(run_id).await {
                Ok(ready) => {
                    let free = cap - inflight.len();
                    // Collect the eligible slice first so the `in_progress` filter
                    // borrow ends before the dispatch loop mutates `in_progress`.
                    let to_dispatch: Vec<OrchRunTaskRow> = ready
                        .into_iter()
                        .filter(|t| !in_progress.contains(&t.id))
                        .take(free)
                        .collect();
                    for task in to_dispatch {
                        // verify 模式 (UC-1b): a `verify` aggregator is settled
                        // SYNCHRONOUSLY here — NOT dispatched to a worker. It reads
                        // its skeptic deps' outputs, tallies a verdict, writes it,
                        // marks itself `done`, and gates downstream on FAIL. It never
                        // enters the in-flight set (no worker, no spin); because it is
                        // only reached when already in the ready set (all skeptics
                        // `done`), it settles in this single fill pass. We then
                        // re-loop (settled_sync) to re-fill, observing the verdict
                        // (downstream proceeds on PASS, is `skipped` on FAIL).
                        if task.kind == "verify" {
                            settle_verify_task(&deps, run_id, &task).await;
                            settled_sync = true;
                            continue;
                        }
                        // judge 模式 (UC-1c): a `judge` aggregator is settled
                        // SYNCHRONOUSLY here too — NOT dispatched to a worker. It reads
                        // its N judge deps' ballots (per-candidate scores), aggregates
                        // them (mean / borda) to pick a winner candidate, writes the
                        // WINNER marker + per-candidate aggregates to its
                        // `output_summary`, and marks itself `done`. No downstream gate
                        // (a judge picks a winner; it does not fail the run) — so it
                        // never skips dependents. Like verify it never enters the
                        // in-flight set and settles in this single fill pass; we
                        // re-loop (settled_sync) so any consumer of the winner is
                        // re-filled.
                        if task.kind == "judge" {
                            settle_judge_task(&deps, run_id, &task).await;
                            settled_sync = true;
                            continue;
                        }
                        // loop 模式 (UC-1d): a `loop` controller is settled
                        // SYNCHRONOUSLY here too — NOT dispatched to a worker. When it
                        // is in the ready set its body dep is `done`, so it evaluates
                        // the stop decision over the body's output + iteration count
                        // (`settle_loop_task`): STOP → mark itself `done` with the
                        // final result; CONTINUE → RESET the body to re-run in place
                        // (pending, clear output, attempt+1) and stay `pending`. A
                        // CONTINUE un-`done`s its only blocker, so it leaves the ready
                        // set until the body re-completes — bounded by the HARD
                        // max_iter cap (no spin). Like verify/judge it never enters the
                        // in-flight set; we re-loop (settled_sync) so the reset body /
                        // a freshly-`done` loop's downstream is re-filled.
                        if task.kind == "loop" {
                            settle_loop_task(&deps, run_id, &task).await;
                            settled_sync = true;
                            continue;
                        }
                        let fut = dispatch_task(&deps, run_id, task, workspace_dir.clone()).await;
                        if let Some((task_id, fut)) = fut {
                            in_progress.insert(task_id);
                            inflight.push(fut);
                        }
                        // dispatch_task returning None means the task was already
                        // failed (e.g. member unresolved) — it is not in-flight,
                        // and a re-query will not return it (status no longer
                        // pending), so the loop converges.
                    }
                }
                Err(e) => {
                    warn!(run_id, error = %e, "Run loop: list_ready_tasks failed — will retry after next completion");
                }
            }
        }

        // loop 模式 (UC-1d) failed-body branch: a `loop` controller whose body dep
        // FAILED never appears in `list_ready_tasks` (its only blocker is not
        // `done`), so it would otherwise hang `pending` while the run wedges. Scan
        // for such controllers and settle them (`settle_loop_task` STOPs the loop
        // `failed` + gates downstream — a failing body never iterates). Skipped
        // while paused (a paused run does not iterate / settle). Bounded: each loop
        // settles at most once (it becomes terminal `failed`, never re-matched), so
        // this cannot spin. Only when this pass dispatched/settled nothing else do
        // we even need it (a freshly-failed body is observed on the NEXT fill pass).
        if !paused && !settled_sync {
            if let Ok(all) = deps.run_repo.list_tasks(run_id).await {
                let dep_edges = deps.run_repo.list_deps(run_id).await.unwrap_or_default();
                // Stalled loop controllers: still `pending`, with a `failed` body
                // blocker. (A `done` body is handled by the ready-set branch above.)
                let stalled: Vec<OrchRunTaskRow> = all
                    .iter()
                    .filter(|t| t.kind == "loop" && t.status == "pending")
                    .filter(|t| {
                        dep_edges
                            .iter()
                            .filter(|d| d.blocked_task_id == t.id)
                            .any(|d| {
                                all.iter()
                                    .any(|b| b.id == d.blocker_task_id && b.status == "failed")
                            })
                    })
                    .cloned()
                    .collect();
                for ctrl in stalled {
                    settle_loop_task(&deps, run_id, &ctrl).await;
                    settled_sync = true;
                }
            }
        }

        // A synchronous aggregator (verify/judge) settled this pass → re-loop to
        // re-fill on the newly-unblocked (or freshly-skipped) downstream before any
        // terminal decision. Bounded: each aggregator settles exactly once (it is
        // `done` afterward, never returned by `list_ready_tasks` again), so this
        // cannot loop forever. A `loop` controller CONTINUE is also bounded: it
        // resets the body (un-`done`s its blocker) so the controller is not re-ready
        // until a REAL body worker round completes, and the body's `attempt` is hard-
        // capped by `max_iter` — there is no path past the cap (no spin).
        if settled_sync {
            continue;
        }

        // (c) No in-flight worker → either idle on the paused gate OR make the
        // conclusive terminal decision.
        if inflight.is_empty() {
            if paused {
                // Paused with nothing in flight: idle-wait (NOT a busy-spin — the
                // sleep yields the runtime) then re-loop to re-read the status. We
                // must NOT declare the run complete/stuck here: a paused run with
                // pending tasks is intentionally idle, not terminal. Cancel is
                // re-checked at the top of the loop, so a `stop` still breaks out.
                tokio::time::sleep(PAUSE_POLL_INTERVAL).await;
                continue;
            }
            // Not paused: with zero workers in flight the task statuses are
            // conclusive — EXCEPT for one concurrent mutator: a `rerun_task` may
            // reset a settled task back to `pending` (and re-activate the run). To
            // make the "all terminal? → finish" decision atomic with that reset, we
            // take the run's lock and RE-READ under it (the reads above the lock can
            // be stale w.r.t. a reset that lands in the gap). The rerun path holds
            // the SAME lock across reset + re-activation, so under the lock exactly
            // one of two states is observable:
            //   - a reset already committed and produced RUNNABLE work → re-loop so
            //     the fill pass dispatches it (the live loop keeps driving), OR
            //   - no runnable work remains → the statuses are genuinely terminal →
            //     we finish_run under the lock (the rerun, serialized after, then
            //     re-reads the now-terminal run status and re-activates + restarts).
            // Either way a run is never left non-running/terminal with a runnable
            // pending task.
            //
            // LOCK INVARIANT: the per-run lock guards ONLY pure DB mutations and the
            // handle deregister — it is NEVER held across an await of a slow/external
            // op (a worker await, or the B2 LLM `summarize`). The `completed` path
            // therefore SPLITS into two lock scopes around the LLM summary, which is
            // the only slow await on this path:
            //   1. (first scope) take the lock, re-check `list_ready_tasks` (re-loop
            //      if ready) + `list_tasks`; if `completed`, capture `total_tokens`
            //      and DROP the lock, then await `compute_completed_summary` with NO
            //      lock held (a multi-second / hanging LLM call must not stall a
            //      concurrent rerun/adjust on this run);
            //   2. (second scope) RE-ACQUIRE the lock and RE-VERIFY the run is still
            //      completable — re-check `list_ready_tasks` AND `list_tasks`
            //      all-terminal. A rerun/adjust that landed DURING the summarize
            //      resets a task → ready work / not-all-terminal → drop + re-loop
            //      (the reset wins, the loop re-drives, the computed summary is
            //      discarded). Otherwise finish_run(`completed`) + deregister under
            //      the re-acquired lock — the terminal write + deregister stay atomic.
            // The `failed` path has NO LLM call, so it finishes + deregisters under
            // the single first-scope lock hold (unchanged). Holding the lock only
            // around DB checks + finish — never across an await — means it cannot
            // deadlock the rerun path, and the re-acquire is a FRESH `lock().await`
            // after the first guard is dropped (NOT nested), so no self-deadlock.
            //
            // The re-loop signal is the READY set (not "any pending task"): a
            // legitimately-FAILED run keeps its downstream tasks `pending` forever
            // (their blocker is `failed`, never `done`, so they are never ready) —
            // those must NOT block the `failed` finish. A rerun, by contrast, resets
            // a settled task whose blockers are done (or resets the whole subtree),
            // so at least one reset task becomes READY. Keying off readiness finishes
            // the failed run correctly while still re-driving a genuine rerun reset.
            let lock = deps.run_locks.for_run(run_id);

            // The terminal decision is made under the lock (first scope), but the
            // `completed` branch only CAPTURES the inputs for the LLM summary here —
            // it does NOT finish under this guard. We yield a decision out of the
            // scope so the slow `summarize` await happens AFTER the guard is dropped.
            enum TerminalDecision {
                /// Concurrent rerun produced runnable work under the lock, or a read
                /// failed conclusively in a way that warrants re-driving → re-loop.
                ReLoop,
                /// All terminal, completable → summarize (no lock) then re-verify.
                /// Carries the tasks (for the digest) + token total. `with_failures`
                /// = any node soft-failed (`on_fail=skip_and_continue`), so the run
                /// settles `completed_with_failures` instead of `completed`.
                Completed {
                    tasks: Vec<OrchRunTaskRow>,
                    total_tokens: Option<i64>,
                    with_failures: bool,
                },
                /// A task failed → finished `failed` under this guard already;
                /// carries the tasks so the after-loop lead report can build a
                /// failure brief lock-free.
                FinishedFailed { tasks: Vec<OrchRunTaskRow> },
                /// No ready/in-flight work, but a task is `pending` waiting out its
                /// transient-error retry backoff (`next_retry_at` in the future) →
                /// idle-wait (NOT terminal, NOT stuck) and re-loop.
                IdleRetry,
                /// Stuck / list_tasks error → already logged; just break.
                Break,
            }

            let decision = {
                let _terminal_guard = lock.lock().await;
                // A concurrent rerun reset a task to a now-RUNNABLE pending state
                // under the lock just before us → there is real work to do; do not
                // finish. Release the lock (drop the guard) and re-loop so the fill
                // pass dispatches it. This is the atomic check-then-act that closes
                // the strand race. A `list_ready_tasks` error here is treated as "no
                // ready work" (fail toward the terminal decision below, which logs)
                // — the statuses read still drive the conclusive branch.
                let has_ready_work = deps
                    .run_repo
                    .list_ready_tasks(run_id)
                    .await
                    .map(|ready| !ready.is_empty())
                    .unwrap_or(false);
                if has_ready_work {
                    TerminalDecision::ReLoop
                } else {
                    match deps.run_repo.list_tasks(run_id).await {
                        Ok(tasks) => {
                            // 迁移 029 终态分流。软失败 = 失败但策略 skip_and_continue
                            // (其传递性下游已在 settle 时 skip);硬失败 = 失败且策略
                            // fail_run(默认)。已 settled = 每个任务处于终态
                            // (done/skipped)或软失败。硬失败优先并支配任何 done/软失败
                            // 兄弟节点 → 整 run failed(现状,零回归)。
                            let hard_failed = tasks
                                .iter()
                                .any(|t| t.status == "failed" && !is_soft_fail(t));
                            let all_settled = tasks.iter().all(|t| {
                                t.status == "done" || t.status == "skipped" || is_soft_fail(t)
                            });
                            let any_soft_fail = tasks.iter().any(is_soft_fail);
                            if hard_failed {
                                // No LLM on the hard-failed path → finish + deregister
                                // under THIS held guard (atomic terminal-write +
                                // deregister, as before).
                                let total_tokens = sum_task_tokens(&tasks);
                                finish_run(&deps, run_id, "failed", None, total_tokens).await;
                                handles.remove_if(run_id, |_, h| h.generation == generation);
                                TerminalDecision::FinishedFailed { tasks }
                            } else if !tasks.is_empty() && all_settled {
                                // Completable: capture inputs and DROP the guard at
                                // the end of this scope — the LLM summarize runs
                                // OUTSIDE the lock, then we re-acquire + re-verify.
                                // `any_soft_fail` selects completed vs
                                // completed_with_failures at the finish site (③).
                                let total_tokens = sum_task_tokens(&tasks);
                                TerminalDecision::Completed {
                                    tasks,
                                    total_tokens,
                                    with_failures: any_soft_fail,
                                }
                            } else {
                                // Not terminal and nothing ready. Distinguish a
                                // transient-error retry-in-backoff (a `pending` task
                                // gated by a future `next_retry_at` — `list_ready_tasks`
                                // holds it out until due) from a genuine stuck graph.
                                // The former must NOT finish/break: idle-wait and
                                // re-loop so the node re-dispatches once its backoff
                                // elapses. Bounded — `attempt` is capped by
                                // `max_worker_retries`, after which the task fails (no
                                // spin). The latter exits to avoid a busy-spin.
                                let now = now_ms();
                                let awaiting_retry = tasks.iter().any(|t| {
                                    t.status == "pending"
                                        && t.next_retry_at.is_some_and(|at| at > now)
                                });
                                // 审批模式（迁移 030）：有节点挂在 needs_review 等人工
                                // 作答——合法停车，不是卡死。干净退出（run 保持
                                // running；看门狗对此豁免），用户 adopt/rerun 后由路由
                                // 重启循环再驱动（与 awaiting_plan_approval 的退出
                                // 契约一致）。
                                let awaiting_review =
                                    tasks.iter().any(|t| t.status == "needs_review");
                                if awaiting_retry {
                                    TerminalDecision::IdleRetry
                                } else if awaiting_review {
                                    info!(
                                        run_id,
                                        "Run loop: node(s) parked at needs_review (节点提问) — exiting until人工作答 re-drives"
                                    );
                                    TerminalDecision::Break
                                } else {
                                    // Stuck (no ready, no in-flight, not terminal) —
                                    // break, never spin.
                                    warn!(
                                        run_id,
                                        task_count = tasks.len(),
                                        "Run loop: no ready tasks and run not terminal — exiting to avoid spin"
                                    );
                                    TerminalDecision::Break
                                }
                            }
                        }
                        Err(e) => {
                            warn!(run_id, error = %e, "Run loop: list_tasks failed — exiting");
                            TerminalDecision::Break
                        }
                    }
                }
                // `_terminal_guard` drops HERE — the lock is released before any LLM
                // await below.
            };

            match decision {
                TerminalDecision::ReLoop => continue,
                TerminalDecision::IdleRetry => {
                    // A task is waiting out its transient-error backoff. Idle-wait
                    // (the guard was dropped at the end of the decision scope) then
                    // re-loop — `list_ready_tasks` returns the node once `next_retry_at`
                    // elapses, and the fill pass re-dispatches it.
                    tokio::time::sleep(RETRY_POLL_INTERVAL).await;
                    continue;
                }
                TerminalDecision::Break => break,
                TerminalDecision::FinishedFailed { tasks } => {
                    // 失败终态：run 已在 guard 内落 `failed` 且注销句柄。这里仅捕获给
                    // lead 的失败简报（含各节点状态/部分产出），循环外锁free统一回执。
                    terminal_report = Some((RunOutcome::Failed, build_summary_digest(&tasks)));
                    break;
                }
                TerminalDecision::Completed {
                    tasks,
                    total_tokens,
                    with_failures,
                } => {
                    // B2: synthesize a coherent run summary with a one-shot lead
                    // call — WITH NO LOCK HELD (the guard above was dropped). This is
                    // the only slow/external await on the terminal path; holding the
                    // per-run lock across it would stall a concurrent rerun/adjust
                    // for the whole summarization (unbounded if the LLM hangs).
                    // FAIL-SOFT — `compute_completed_summary` returns the mechanical
                    // `aggregate_summary` concat on ANY problem (summarizer error, no
                    // derivable lead, blank result), so a summary failure NEVER
                    // blocks/fails the completed run.
                    let summary = compute_completed_summary(&deps, run_id, &run, &tasks).await;

                    // RE-ACQUIRE the lock (a FRESH `lock().await`, the first guard is
                    // already dropped — not nested, so no self-deadlock) and
                    // RE-VERIFY the run is STILL completable: a rerun/adjust may have
                    // reset a task DURING the summarize above. Re-check ready work AND
                    // all-terminal under the re-acquired lock — if the reset landed,
                    // the reset WINS: drop + re-loop (discard the computed summary,
                    // the loop re-drives). Only when the run is still genuinely
                    // terminal do we commit finish + deregister, keeping the
                    // strand-race closure intact (finish only commits under the lock
                    // after the re-verify).
                    let _terminal_guard = lock.lock().await;
                    let ready_now = deps
                        .run_repo
                        .list_ready_tasks(run_id)
                        .await
                        .map(|ready| !ready.is_empty())
                        .unwrap_or(false);
                    if ready_now {
                        drop(_terminal_guard);
                        continue;
                    }
                    let still_terminal = match deps.run_repo.list_tasks(run_id).await {
                        Ok(now) => {
                            !now.is_empty()
                                && now.iter().all(|t| {
                                    t.status == "done"
                                        || t.status == "skipped"
                                        || is_soft_fail(t)
                                })
                        }
                        // A re-read error → do NOT finish on stale data; re-loop and
                        // re-decide on the next pass (fail toward not-committing).
                        Err(e) => {
                            warn!(run_id, error = %e, "Run loop: list_tasks re-verify failed — re-looping");
                            false
                        }
                    };
                    if !still_terminal {
                        // A concurrent rerun/adjust reset a task during summarize →
                        // no longer all-terminal → the reset wins. Re-loop.
                        drop(_terminal_guard);
                        continue;
                    }
                    // Still terminal, no ready work → commit the completion.
                    // 迁移 029:有软失败节点(skip_and_continue)时写
                    // completed_with_failures,否则 completed。
                    let terminal_status = if with_failures {
                        "completed_with_failures"
                    } else {
                        "completed"
                    };
                    finish_run(
                        &deps,
                        run_id,
                        terminal_status,
                        Some(summary.clone()),
                        total_tokens,
                    )
                    .await;
                    // Deregister our handle UNDER the (re-acquired) lock so
                    // `is_running` flips false atomically with the terminal status
                    // write — a rerun serialized after us on this lock then observes
                    // a stopped loop and the route restarts it (no variant-A strand).
                    handles.remove_if(run_id, |_, h| h.generation == generation);
                    // 完成终态：捕获 LLM 综合 summary 作 lead 回执素材。此处仍持有
                    // 重取的 guard（break 时释放），仅做赋值(无 await)；真正回执在循环外锁free。
                    terminal_report = Some((RunOutcome::Completed, summary));
                    break;
                }
            }
        }

        // (d) Park on the next worker to finish (NOT a poll — this awaits). The
        // completion may unblock downstream tasks, so the loop re-fills.
        if let Some((task_id, outcome)) = inflight.next().await {
            in_progress.remove(&task_id);
            // Whether this settle actually landed the node as `done` — returned by
            // settle itself, NOT inferred from `Ok(o) if o.ok`（评审确认缺陷：审批
            // 模式的 needs_review park 路径 worker 同样以 ok:true 结束回合，但
            // settle 顶部守卫提前 return、节点并未完成——按 outcome 推断会紧跟
            // NodeQuestion 回执再发一条自相矛盾的虚假 BatchProgress）。Retries /
            // failures / park 都不计入。
            let did_complete = settle_task_outcome(&deps, run_id, &task_id, outcome).await;

            // Mid-run batch-progress heartbeat (Phase 3a Task 3). This branch is
            // reached ONLY while `inflight` is non-empty, so it is structurally
            // DISJOINT from the `inflight.is_empty()` terminal decision above — we
            // never hold a RunLocks terminal guard here. Best-effort: a report
            // failure only warns and never affects the run. Throttled by BOTH a
            // node-count floor and a wall-clock interval so a fast fan-out cannot
            // spam the lead. A run with no lead conversation short-circuits (None).
            if did_complete {
                done_since_report += 1;
            }
            if let Some(lead_conv_id) = run.lead_conv_id {
                let interval_due = last_batch_report
                    .map(|t| t.elapsed() >= BATCH_REPORT_INTERVAL)
                    .unwrap_or(true);
                // Gate the batch report on `did_complete` too: a settle that is a
                // permanent FAILURE fires `NodeFailed` inside `settle_task_outcome`
                // above, so firing the batch report on the SAME settle would emit
                // `node_failed` then `batch_progress` back-to-back. Only consider the
                // batch report when THIS settle was a completion (the count floor + the
                // interval gate still apply).
                if did_complete && done_since_report >= BATCH_REPORT_MIN_NODES && interval_due {
                    let digest = match deps.run_repo.list_tasks(run_id).await {
                        Ok(tasks) if !tasks.is_empty() => build_summary_digest(&tasks),
                        _ => "（run 进行中，暂无可汇总的节点产出）".to_string(),
                    };
                    if let Err(e) = deps
                        .lead_reporter
                        .report(lead_conv_id, run_id, RunOutcome::BatchProgress, &digest)
                        .await
                    {
                        warn!(run_id, error = %e, "batch-progress lead report failed");
                    }
                    last_batch_report = Some(std::time::Instant::now());
                    done_since_report = 0;
                }
            }
        }
        // Loop again — re-evaluate the ready set (newly unblocked) and the
        // terminal condition.
    }

    // Lead 回执（锁外、exactly-once）：run 到终态（completed/failed）后，把结果回执给
    // 发起它的 lead 会话，触发主 agent 一次汇总/重策略——取代旧的 LLM busy-poll。
    // best-effort：报告失败只 warn，绝不影响已落库的终态；`Break`(卡死/读错) 不回执，
    // 由 run 级看门狗兜底；纯 MCP/无 lead 会话（lead_conv_id=None）的 run 不回执。
    if let (Some((outcome, brief)), Some(lead_conv_id)) = (terminal_report, run.lead_conv_id) {
        if let Err(e) = deps
            .lead_reporter
            .report(lead_conv_id, run_id, outcome, &brief)
            .await
        {
            warn!(run_id, error = %e, "lead report failed (run already finalized)");
        }
    }
}

/// Cancel every in-flight worker conversation belonging to `run_id`: query the
/// run's tasks, take those still `running` with a stamped `conversation_id`, and
/// call [`ConversationCanceller::cancel`] on each. Best-effort: a `list_tasks`
/// error is logged, not propagated (the run is being torn down regardless).
///
/// The DB-query approach (vs. an in-memory in-flight map plumbed through the
/// `RunHandle`) keeps `stop` decoupled from the loop's internal state: the loop
/// already stamps `task.conversation_id` live on dispatch (via `on_started`) and
/// marks the task `running` before pushing its worker future, so a `running` row
/// with a non-null `conversation_id` is exactly an in-flight worker. Filtering by
/// `list_tasks(run_id)` guarantees we only touch THIS run's conversations.
async fn cancel_in_flight_conversations(deps: &Arc<RunEngineDeps>, run_id: &str) {
    let tasks = match deps.run_repo.list_tasks(run_id).await {
        Ok(t) => t,
        Err(e) => {
            warn!(run_id, error = %e, "Run stop: list_tasks failed — cannot cancel in-flight conversations");
            return;
        }
    };
    let mut cancelled = 0usize;
    for task in tasks {
        if task.status != "running" {
            continue;
        }
        let Some(conv_id) = task.conversation_id else {
            // Running but conversation_id not yet stamped (the on_started detached
            // stamp lags the `running` mark by a hair). The cooperative cancel flag
            // + loop abort still stop scheduling; this conversation either never
            // got created or will be orphaned — acceptable for cancel.
            continue;
        };
        deps.cancel_conversation.cancel(conv_id).await;
        cancelled += 1;
    }
    if cancelled > 0 {
        info!(
            run_id,
            cancelled, "Run stop: cancelled in-flight worker conversations"
        );
    }
}

/// The future a single in-flight worker resolves to: its task id paired with the
/// worker outcome. Boxed because each closure type differs by captured task id.
type WorkerFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = (String, Result<WorkerOutcome, AppError>)> + Send>,
>;

/// Seed a human-readable shared notes file (`RUN_NOTES.md`) in the run's shared
/// dir. Best-effort and idempotent — writes ONLY if the file is absent, so a
/// restart/rerun never clobbers content nodes appended (their appends survive).
/// Carries the run goal + plan (task titles) + a「共享笔记」append region nodes
/// add findings to via built-in file tools (their cwd is this shared dir). A
/// read/write failure only `warn!`s and NEVER fails the run.
async fn seed_run_notes(deps: &Arc<RunEngineDeps>, run_id: &str, dir: &str) {
    let path = std::path::Path::new(dir).join("RUN_NOTES.md");
    // Idempotent: preserve any node-appended content across restart/rerun.
    if path.exists() {
        return;
    }
    let goal = deps
        .run_repo
        .get_run(run_id)
        .await
        .ok()
        .flatten()
        .map(|r| r.goal)
        .unwrap_or_default();
    let tasks = deps.run_repo.list_tasks(run_id).await.unwrap_or_default();
    let mut body = String::from("# RUN_NOTES\n\n## 目标 (GOAL)\n");
    body.push_str(&goal);
    body.push_str("\n\n## 计划 (PLAN)\n");
    for t in &tasks {
        body.push_str(&format!("- {}\n", t.title));
    }
    body.push_str("\n## 共享笔记（各节点可追加发现/结论，供其它节点参考）\n");
    if let Err(e) = std::fs::write(&path, body) {
        warn!(run_id, error = %e, "failed to seed RUN_NOTES.md (continuing)");
    }
}

/// Prepare a task for dispatch: resolve its member from the run's fleet snapshot,
/// mark it `running` + emit, compose the brief, and build the worker future
/// (which fires `on_started` to stamp `task.conversation_id` live). Returns
/// `(task_id, future)` to push into the in-flight set, or `None` if the member
/// could not be resolved (the task is marked `failed` in that case).
async fn dispatch_task(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    task: OrchRunTaskRow,
    workspace_dir: Option<String>,
) -> Option<(String, WorkerFuture)> {
    // Resolve the assignment → member from the run's fleet snapshot, defaulting
    // to member[0] when no assignment exists (mirrors P1a's tolerance for an
    // unassigned task in a single-member fleet).
    let member = match resolve_task_member(deps, run_id, &task.id).await {
        Ok(m) => m,
        Err(reason) => {
            warn!(run_id, task_id = %task.id, reason, "Run loop: cannot resolve member — failing task");
            mark_task_failed(deps, run_id, &task.id, None, None).await;
            return None;
        }
    };

    // Mark running + emit BEFORE pushing the future, so a concurrent re-query of
    // list_ready_tasks (keyed on persisted status) never re-dispatches it.
    update_task_status(deps, &task.id, "running").await;
    deps.emitter.emit_task_status(run_id, &task.id, "running");

    // Compose the brief: role hint + the task + completed upstream outputs, plus
    // cross-node context (run goal + plan shape + transitive-ancestor digest +
    // shared-notes pointer) so a node deep in a chain retains earlier context.
    let upstream = collect_upstream_outputs(deps, run_id, &task.id).await;
    let ctx = build_brief_context(deps, run_id, &task.id, workspace_dir.as_deref()).await;
    let brief = compose_brief(member.role_hint.as_deref(), &task, &upstream, &ctx);

    // Clones captured by the worker future + the on_started closure. on_started is
    // a sync FnOnce(i64); the async task.conversation_id stamp is done in a
    // detached tokio::spawn (acceptable + simplest — it stamps the id live for the
    // frontend without blocking the worker turn).
    let worker = deps.worker.clone();
    let run_repo_for_started = deps.run_repo.clone();
    let emitter_for_started = deps.emitter.clone();
    let run_id_for_started = run_id.to_string();
    let run_id_for_run = run_id.to_string();
    let task_id = task.id.clone();
    let task_id_for_started = task.id.clone();
    let task_id_for_fut = task.id.clone();
    let spec = task.spec.clone();
    // 任务角色随 dispatch 下发：受限角色（searcher/reviewer/verifier）在 worker
    // 侧收缩为 per-node 工具白名单 + 网关收缩（run_restricted）。
    let task_role = task.role.clone();
    // 受控嵌套深度（Phase 3b W7d）：从 task.pattern_config 读出 delegation_depth，随
    // dispatch 烙进 worker 会话 extra（build_worker_extra）。pattern_config 里同时可能
    // 存 fan-out 的 {"group":...} 等键——只读 delegation_depth，缺失/不可解析默认 0。
    // root lead 的任务无此键 → 0；网关追加口给追加任务盖 caller_depth+1。
    let delegation_depth = task
        .pattern_config
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v.get("delegation_depth").and_then(serde_json::Value::as_u64))
        .unwrap_or(0) as u32;
    let timeout = deps.worker_timeout;

    let fut: WorkerFuture = Box::pin(async move {
        let on_started: Box<dyn FnOnce(i64) + Send> = Box::new(move |conv_id| {
            // Stamp task.conversation_id live (detached). Best-effort: the worker
            // turn proceeds regardless. Also emit so the frontend can attach to
            // the live transcript as soon as the conversation exists.
            tokio::spawn(async move {
                let _ = run_repo_for_started
                    .update_task(
                        &task_id_for_started,
                        UpdateTaskParams {
                            last_error: None,
                            status: None,
                            spec: None,
                            conversation_id: Some(Some(conv_id)),
                            output_summary: None,
                            output_files: None,
                            attempt: None,
                            tokens: None,
                            graph_x: None,
                            graph_y: None,
                            pattern_config: None,
                            next_retry_at: None,
                        },
                    )
                    .await;
                emitter_for_started.emit_task_status(
                    &run_id_for_started,
                    &task_id_for_started,
                    "running",
                );
            });
        });
        let outcome = worker
            .run_restricted(
                task_role.as_deref(),
                delegation_depth,
                &member,
                workspace_dir.as_deref(),
                &run_id_for_run,
                &task_id_for_fut,
                &brief,
                &spec,
                timeout,
                on_started,
            )
            .await;
        (task_id_for_fut, outcome)
    });

    Some((task_id, fut))
}

/// Settle a finished worker's outcome: `ok` → mark the task `done` with its
/// conversation id + output summary + emit; otherwise (timeout / no reply /
/// error) → AUTO-RETRY if the failure was a transient/retryable provider error and
/// the retry budget remains (see [`settle_failed_or_retry`]), else mark `failed` +
/// emit. Completion is what unblocks downstream tasks.
///
/// Returns `true` ONLY when this settle actually landed the node as `done` —
/// the run_loop's batch-progress heartbeat keys off this instead of inferring
/// from the raw outcome（needs_review park 也以 ok:true 回场，但并未完成）。
async fn settle_task_outcome(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    task_id: &str,
    outcome: Result<WorkerOutcome, AppError>,
) -> bool {
    // 审批模式（迁移 030）：worker 在本轮中途经 nomi_task_question 把任务置为
    // `needs_review`（挂起等人工），随后它的回合正常结束并流到这里。此时绝不能把
    // 状态覆盖为 done/failed：只补写 conversation_id / 产出文本 / tokens（供投影
    // 视图与后续「采用为该节点产出」使用），保持 needs_review + pending_question
    // 原样——下游保持阻塞，由用户进节点作答后 adopt/rerun 恢复。读失败按未挂起
    // 处理（fail-open 到既有 settle 路径，绝不因一次读错吞掉正常结算）。
    let parked_for_review = matches!(
        deps.run_repo.get_task(task_id).await,
        Ok(Some(t)) if t.status == "needs_review"
    );
    if parked_for_review {
        if let Ok(o) = &outcome {
            let _ = deps
                .run_repo
                .update_task(
                    task_id,
                    UpdateTaskParams {
                        conversation_id: Some(Some(o.conversation_id)),
                        output_summary: o.text.clone().map(Some),
                        tokens: o.tokens.map(Some),
                        next_retry_at: Some(None),
                        ..Default::default()
                    },
                )
                .await;
        }
        info!(run_id, task_id, "Run loop: task parked at needs_review (节点提问) — settle skipped");
        return false;
    }
    match outcome {
        Ok(o) if o.ok => {
            let _ = deps
                .run_repo
                .update_task(
                    task_id,
                    UpdateTaskParams {
                        last_error: None,
                        status: Some("done".to_string()),
                        spec: None,
                        conversation_id: Some(Some(o.conversation_id)),
                        output_summary: Some(o.text),
                        output_files: None,
                        attempt: None,
                        // Per-task token usage (`input + output`, summed over the
                        // worker conversation's turns), surfaced via the conversation
                        // runtime-state accumulator the stream relay fills from each
                        // `TurnCompleted` metrics event. `None` when no usage was
                        // observed (non-nomi member / turn never completed / mock
                        // runner) — task.tokens stays None, the prior behaviour. The
                        // `orch_run_tasks.tokens` column + RunTask DTO + UI are already
                        // plumbed; this is the real source. Never fabricated.
                        tokens: o.tokens.map(Some),
                        graph_x: None,
                        graph_y: None,
                        pattern_config: None,
                        // A successful settle clears any leftover retry gate (a prior
                        // attempt may have set one) so the `done` row is clean.
                        next_retry_at: Some(None),
                    },
                )
                .await;
            deps.emitter.emit_task_status(run_id, task_id, "done");
            true
        }
        Ok(o) => {
            // Worker returned but produced no final text (rate-limit / transient
            // error / timeout / empty). Auto-retry transient errors; else fail.
            settle_failed_or_retry(deps, run_id, task_id, Some(o.conversation_id), o.tokens).await;
            false
        }
        Err(e) => {
            warn!(run_id, task_id, error = %e, "Run loop: worker errored — failing or retrying task");
            settle_failed_or_retry(deps, run_id, task_id, None, None).await;
            false
        }
    }
}

/// Backoff (ms) before the `attempt`-th auto-retry: `base · 2^attempt`, saturating
/// and exponent-clamped so a huge attempt count can never overflow. `attempt` is
/// the PRE-bump retry count (0 = first retry → one `base`).
fn retry_backoff_ms(base: Duration, attempt: i64) -> i64 {
    let base_ms = base.as_millis() as i64;
    let factor = 1i64
        .checked_shl(attempt.clamp(0, 16) as u32)
        .unwrap_or(i64::MAX);
    // Cap the exponential growth so a high retry budget can't schedule an absurd
    // multi-hour backoff (`base · 2^attempt` explodes fast). Past the cap the delay
    // is constant — still spaced out, but bounded and snappy to recover.
    base_ms.saturating_mul(factor).min(MAX_RETRY_BACKOFF_MS)
}

/// Decide a failed worker turn's fate via a THREE-WAY classification against the
/// worker conversation, honouring TWO independent per-node budgets plus a run-level
/// total-retry budget:
///
///   - **Retryable**  — a provider marker with `retryable=true` (e.g. a rate limit):
///     retried under [`RunEngineDeps::max_worker_retries`], the same as before.
///   - **NoMarker**   — `ok:false` with NO error marker (a timeout / empty reply):
///     retried under the SEPARATE, smaller [`RunEngineDeps::max_timeout_retries`]
///     budget so a genuinely stuck node self-heals on a transient blip yet cannot
///     loop forever. This is the Task 1 addition (previously NoMarker → fail fast).
///   - **NonRetryable** — a provider marker with `retryable=false` (auth / billing /
///     bad request), or an `Err` outcome with no conversation to classify: fail
///     immediately, exactly as before.
///
/// On a retry we keep the run `running` and put the node back to `pending` with
/// `attempt+1` and a `next_retry_at = now + backoff` gate; `list_ready_tasks` holds
/// it out until the backoff elapses, then the loop re-dispatches it (a fresh worker
/// turn). The **run-level total-retry budget** additionally bounds a wide fan-out:
/// stateless — the sum of persisted `attempt`s across the run vs
/// `tasks.len() * max_worker_retries` — so many independently backing-off nodes
/// cannot storm the provider. When neither budget permits a retry we fall back to
/// [`mark_task_failed`] exactly as before.
async fn settle_failed_or_retry(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    task_id: &str,
    conv_from_outcome: Option<i64>,
    tokens: Option<i64>,
) {
    let task = deps.run_repo.get_task(task_id).await.ok().flatten();
    let attempt = task.as_ref().map(|t| t.attempt).unwrap_or(0);
    let conv = conv_from_outcome.or_else(|| task.as_ref().and_then(|t| t.conversation_id));

    // Three-way classification against the worker conversation:
    //   Retryable  = provider marker with retryable=true (e.g. rate limit)
    //   NoMarker   = ok:false with no error marker (timeout / empty reply)
    //   Otherwise  = NonRetryable (auth/billing/bad-request marker, or Err w/ no conv)
    let (retryable_marker, has_marker) = match conv {
        Some(c) => {
            let s = c.to_string();
            (deps.worker.last_error_retryable(&s).await, deps.worker.last_error_present(&s).await)
        }
        None => (false, true), // Err outcome (no conversation) → treat as NonRetryable
    };
    let (should_retry, budget) = if retryable_marker {
        (true, deps.max_worker_retries)
    } else if !has_marker && conv.is_some() {
        (true, deps.max_timeout_retries) // NoMarker: timeout / empty reply
    } else {
        (false, 0) // NonRetryable marker
    };

    // Run-level total-retry budget: bound wide fan-outs so many independently
    // backing-off nodes cannot storm the provider. Stateless: sum of persisted
    // attempts across the run vs tasks.len() * max_worker_retries.
    //
    // NOTE on coupling: the cap is expressed in `max_worker_retries`, and each
    // node's own budget already caps its attempts, so this run gate only binds
    // AFTER nodes have exhausted their per-node budgets (inert under defaults
    // 3/2). Two consequences to keep in mind: (a) `max_worker_retries == 0`
    // makes `cap == 0` → this gate blocks ALL retries incl. NoMarker timeouts,
    // regardless of `max_timeout_retries`; (b) a caller setting
    // `max_timeout_retries > max_worker_retries` would have timeout retries
    // throttled by this run cap. Both are as-designed (per spec) but non-obvious.
    let within_run_budget = match deps.run_repo.list_tasks(run_id).await {
        Ok(tasks) => {
            let used: i64 = tasks.iter().map(|t| t.attempt).sum();
            let cap = (tasks.len().max(1) as i64) * (deps.max_worker_retries as i64);
            used < cap
        }
        Err(_) => true, // fail-open on a transient read error; per-node budget still bounds it
    };

    if should_retry && (attempt as usize) < budget && within_run_budget {
        let next = now_ms() + retry_backoff_ms(deps.retry_backoff_base, attempt);
        let _ = deps
            .run_repo
            .update_task(
                task_id,
                UpdateTaskParams {
                    status: Some("pending".to_string()),
                    attempt: Some(attempt + 1),
                    next_retry_at: Some(Some(next)),
                    tokens: tokens.map(Some),
                    ..Default::default()
                },
            )
            .await;
        deps.emitter.emit_task_status(run_id, task_id, "pending");
        info!(
            run_id,
            task_id,
            attempt = attempt + 1,
            no_marker = !has_marker,
            "Run loop: scheduling bounded auto-retry"
        );
    } else {
        // Non-retryable, retry budget exhausted, or run-level budget hit → permanent
        // failure (the run loop's terminal check then fails the run, as before).
        mark_task_failed(deps, run_id, task_id, conv_from_outcome, tokens).await;
        // 迁移 029 部分交付:若该节点策略是 skip_and_continue,跳过它的传递性下游
        // (标 skipped),让独立分支跑完、run 终态走 completed_with_failures。默认
        // (None/fail_run) 不进这里 → 整 run 仍判 failed(零回归)。mark_task_failed
        // 保持单一职责,策略分流只在此处。
        if let Ok(Some(t)) = deps.run_repo.get_task(task_id).await {
            if t.on_fail.as_deref() == Some("skip_and_continue") {
                if let Ok(edges) = deps.run_repo.list_deps(run_id).await {
                    skip_downstream(deps, run_id, task_id, &edges).await;
                }
            }
        }

        // 中途上报 lead:某节点永久失败时(必需节点将拖垮 run,软失败也值得知会),
        // best-effort 唤起 master 会话让其决策(改图/换模型重跑/adopt/放弃),不等终态。
        // 绝不在终态 `RunLocks` 锁内(本函数在 run_loop 分支(d),不持终态锁);失败仅
        // warn。每永久失败节点触发一次(天然有界)。`last_error` 已由上面的
        // `mark_task_failed` 落库,此处读到的是真实原因。
        if let Ok(Some(run)) = deps.run_repo.get_run(run_id).await {
            if let Some(lead_conv_id) = run.lead_conv_id {
                let brief = match deps.run_repo.get_task(task_id).await {
                    Ok(Some(t)) => format!(
                        "节点「{}」永久失败：{}",
                        t.title,
                        t.last_error.as_deref().unwrap_or("未知原因")
                    ),
                    _ => format!("run {run_id} 的一个节点永久失败"),
                };
                if let Err(e) = deps
                    .lead_reporter
                    .report(lead_conv_id, run_id, RunOutcome::NodeFailed, &brief)
                    .await
                {
                    warn!(run_id, task_id, error = %e, "mid-run node-failed lead report failed");
                }
            }
        }
    }
}

/// Resolve the member assigned to `task_id` from the run's `fleet_snapshot`.
/// When no assignment exists, default to the snapshot's first member (mirrors
/// P1a's tolerance for an unassigned task in a single-member fleet). Returns a
/// short static reason string on failure (for the warn log).
async fn resolve_task_member(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    task_id: &str,
) -> Result<FleetMember, &'static str> {
    let assignment = deps
        .run_repo
        .get_assignment_for_task(task_id)
        .await
        .map_err(|_| "assignment query failed")?;
    let run = deps
        .run_repo
        .get_run(run_id)
        .await
        .map_err(|_| "run query failed")?
        .ok_or("run not found")?;
    let members: Vec<FleetMember> =
        serde_json::from_str(&run.fleet_snapshot).map_err(|_| "fleet snapshot unparseable")?;
    let mut member = match assignment {
        Some(a) => members
            .into_iter()
            .find(|m| m.id == a.member_id)
            .ok_or("assigned member not in snapshot")?,
        // No assignment → default to member[0] (single-member fleet path).
        None => members.into_iter().next().ok_or("fleet snapshot empty")?,
    };
    // 迁移 026 — per-task 模型覆盖(启动前配置台):当该节点同时设了 provider + model
    // 覆盖时,用它们覆写解析出的成员的 provider/model(可选任意可用模型,不受 run 冻结
    // 的 fleet 池限制),保留成员原有角色/persona/skills。门控于「两者都非空」——没有
    // 覆盖的节点与 025 前逐字节一致(pending 调度 / rerun / loop 全走这里,一处即全覆盖)。
    if let Ok(Some(task)) = deps.run_repo.get_task(task_id).await {
        let ov_provider = task
            .override_provider_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let ov_model = task
            .override_model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let (Some(p), Some(m)) = (ov_provider, ov_model) {
            member.provider_id = Some(p.to_string());
            member.model = Some(m.to_string());
        }
    }
    Ok(member)
}

/// The completed upstream tasks' output summaries, in task order. Used to inject
/// prior results into the worker brief so a downstream task has context.
async fn collect_upstream_outputs(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    task_id: &str,
) -> Vec<(String, String)> {
    let deps_edges = deps.run_repo.list_deps(run_id).await.unwrap_or_default();
    let blocker_ids: Vec<String> = deps_edges
        .into_iter()
        .filter(|d| d.blocked_task_id == task_id)
        .map(|d| d.blocker_task_id)
        .collect();
    if blocker_ids.is_empty() {
        return vec![];
    }
    let tasks = deps.run_repo.list_tasks(run_id).await.unwrap_or_default();
    tasks
        .into_iter()
        .filter(|t| blocker_ids.contains(&t.id))
        .filter_map(|t| t.output_summary.map(|s| (t.title, s)))
        .collect()
}

/// Build the cross-node [`BriefContext`] for a task at dispatch time (Phase 1b
/// Task 3): the run goal, a short plan summary (task titles in plan order), a
/// transitive-ancestor digest, and a pointer to the shared `RUN_NOTES.md`.
///
/// **Transitive-ancestor digest.** A node's DIRECT upstream is injected separately
/// (as UPSTREAM RESULTS via [`collect_upstream_outputs`]); this walks the dep graph
/// UP FROM those direct blockers to gather the blockers-of-blockers (and beyond) —
/// the earlier context a long chain would otherwise lose. It mirrors
/// [`skip_downstream`]'s bounded, `seen`-guarded BFS but in the REVERSE direction
/// (following `blocked → blocker` edges upward). Direct upstream and the task
/// itself are pre-seeded into `seen` so they are EXCLUDED (never re-injected). Only
/// ancestors that have produced an `output_summary` land in the digest, each passed
/// through [`truncate_summary_output`] so a verbose upstream cannot blow the budget.
async fn build_brief_context(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    task_id: &str,
    workspace_dir: Option<&str>,
) -> BriefContext {
    let run_row = deps.run_repo.get_run(run_id).await.ok().flatten();
    let goal = run_row.as_ref().map(|r| r.goal.clone()).unwrap_or_default();
    // 审批模式（迁移 030）：只认显式 "manual"；读失败/缺省 = 全授权（不渲染决策段）。
    let manual_approval = run_row
        .as_ref()
        .and_then(|r| r.approval_mode.as_deref())
        .is_some_and(|m| m == "manual");

    let tasks = deps.run_repo.list_tasks(run_id).await.unwrap_or_default();
    // Short plan summary: the task titles in plan order, so a node sees the overall
    // shape and its own position. Kept compact (titles only) to bound the prompt.
    let plan_summary = tasks
        .iter()
        .map(|t| t.title.as_str())
        .collect::<Vec<_>>()
        .join(" → ");

    let dep_edges = deps.run_repo.list_deps(run_id).await.unwrap_or_default();
    // DIRECT upstream (blockers of this task): injected separately as UPSTREAM
    // RESULTS, so it is the SEED of the upward walk but is EXCLUDED from the digest.
    let direct: Vec<String> = dep_edges
        .iter()
        .filter(|d| d.blocked_task_id == task_id)
        .map(|d| d.blocker_task_id.clone())
        .collect();

    // Index tasks by id for O(1) title/output lookups during the walk.
    let by_id: std::collections::HashMap<&str, &OrchRunTaskRow> =
        tasks.iter().map(|t| (t.id.as_str(), t)).collect();

    // Reverse BFS (blocked → blocker) from the direct upstream. `seen` is pre-seeded
    // with the task itself + its direct upstream so neither is ever added to the
    // digest; bounded because each id is visited once.
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(task_id.to_string());
    let mut frontier: Vec<String> = Vec::new();
    for d in &direct {
        // Direct upstream: excluded from the digest but the roots of the walk.
        seen.insert(d.clone());
        frontier.push(d.clone());
    }
    let mut ancestor_digest: Vec<(String, String)> = Vec::new();
    while let Some(current) = frontier.pop() {
        // Enqueue `current`'s own blockers (one level further up the chain).
        for d in dep_edges.iter().filter(|d| d.blocked_task_id == current) {
            let ancestor = d.blocker_task_id.clone();
            if !seen.insert(ancestor.clone()) {
                continue;
            }
            // A transitive ancestor beyond the direct upstream: include only when it
            // has an output_summary, truncated to bound the prompt.
            if let Some(t) = by_id.get(ancestor.as_str())
                && let Some(summary) = t.output_summary.as_deref()
            {
                ancestor_digest.push((t.title.clone(), truncate_summary_output(summary)));
            }
            frontier.push(ancestor);
        }
    }

    // Shared-notes pointer: the run's RUN_NOTES.md (None for a dir-less run). Built
    // with Path::join so the separator is platform-correct.
    let notes_hint = workspace_dir.map(|d| {
        std::path::Path::new(d)
            .join("RUN_NOTES.md")
            .to_string_lossy()
            .into_owned()
    });

    BriefContext {
        goal,
        plan_summary,
        ancestor_digest,
        notes_hint,
        manual_approval,
    }
}

/// The `pattern_config` JSON field a `loop` body carries to its NEXT re-run with
/// the PRIOR round's output text (written by [`settle_loop_task`] on CONTINUE).
/// Its presence is what gates the "上一轮产出" brief section — a task without it
/// (any normal task, and the loop body's first iteration) is unaffected.
const LOOP_PRIOR_OUTPUT_KEY: &str = "loop_prior_output";

/// The `pattern_config` JSON field a `loop` body carries with its NEXT (1-based)
/// iteration number, alongside [`LOOP_PRIOR_OUTPUT_KEY`]. Informational (the
/// brief does not require it); kept so a consumer/UI can read the round.
const LOOP_ITERATION_KEY: &str = "loop_iteration";

/// Extract the carried prior-round output from a body task's `pattern_config`
/// (the [`LOOP_PRIOR_OUTPUT_KEY`] string). Returns `None` (no carry → fresh
/// brief) when the config is absent / blank / not a JSON object / lacks the key /
/// the value is blank. The brief section is gated SOLELY on `Some(_)` here, so a
/// task without this field gets the exact pre-existing brief (zero regression).
fn loop_prior_output(pattern_config: Option<&str>) -> Option<String> {
    let raw = pattern_config.map(str::trim).filter(|s| !s.is_empty())?;
    let value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    let prior = value
        .get(LOOP_PRIOR_OUTPUT_KEY)
        .and_then(serde_json::Value::as_str)?;
    let prior = prior.trim();
    if prior.is_empty() {
        return None;
    }
    Some(prior.to_string())
}

/// Cross-node context injected into every worker brief (Phase 1b Task 3) so a
/// node deep in a chain sees the whole run's goal, the plan shape, relevant
/// transitive-ancestor outputs (not just its DIRECT deps, which long chains would
/// otherwise lose), and where the shared notes live. Built by
/// [`build_brief_context`] at dispatch time. Every field is optional-by-emptiness:
/// each brief section renders ONLY when its input is non-empty, so a default/empty
/// `BriefContext` produces the exact legacy brief (zero regression).
pub(crate) struct BriefContext {
    /// The run goal (`orch_runs.goal`); empty when unavailable.
    pub goal: String,
    /// A short plan summary — the task titles in plan order — so a node knows its
    /// position in the overall shape. Empty when there are no tasks.
    pub plan_summary: String,
    /// `(title, truncated output_summary)` for transitive ancestors BEYOND the
    /// direct upstream (which is injected separately as UPSTREAM RESULTS). Only
    /// ancestors that have produced an `output_summary` are included; each is
    /// passed through [`truncate_summary_output`].
    pub ancestor_digest: Vec<(String, String)>,
    /// Absolute path to the run's shared `RUN_NOTES.md` (None when the run has no
    /// shared dir). A node can read it and append its findings for other nodes.
    pub notes_hint: Option<String>,
    /// 审批模式（迁移 030）：run 的 `approval_mode == "manual"` 时为 true——brief
    /// 追加「决策策略」段，指示 worker 遇关键决策调用 `nomi_task_question` 挂起等
    /// 人工。false（全授权/缺省/读失败）= 不渲染该段，brief 与既有逐字节一致
    /// （零回归）。
    pub manual_approval: bool,
}

/// Compose the worker's brief: role hint + task title/spec + completed upstream
/// outputs (injected as context). Sent as the conversation `system_prompt`.
///
/// **Kind-aware (迁移 023).** For `kind == "synthesis"` the brief is framed as an
/// explicit synthesis instruction — the upstream dependency outputs are the
/// PRIMARY material to merge, not just background context — while the `agent`
/// kind (the default, and anything unknown) keeps the exact previous framing
/// (zero regression). The upstream gathering is identical for both kinds; only
/// the framing differs.
///
/// **Cross-node context (Phase 1b Task 3).** Every brief also carries a
/// [`BriefContext`] — the run goal, the plan shape, a transitive-ancestor digest
/// (blockers-of-blockers beyond the DIRECT upstream), and a pointer to the shared
/// notes file. Each new section renders ONLY when its input is non-empty, so an
/// EMPTY `BriefContext` yields the exact legacy brief (zero regression).
fn compose_brief(
    role_hint: Option<&str>,
    task: &OrchRunTaskRow,
    upstream: &[(String, String)],
    ctx: &BriefContext,
) -> String {
    let mut out = if task.kind == "synthesis" {
        compose_synthesis_brief(role_hint, task, upstream, ctx)
    } else {
        compose_agent_brief(role_hint, task, upstream, ctx)
    };
    // 迁移 026 — 用户预置要求(启动前配置台):作为独立一段追加到 worker brief,与
    // 规划器写的 spec 分离(不覆盖它),读作明确的用户要求。门控于字段非空 —— 没有
    // 预置要求的任务与 025 前逐字节一致。对 agent / synthesis 均生效(统一在此追加)。
    if let Some(preset) = task
        .preset_prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push_str("\n用户预置要求(请优先遵守):\n");
        out.push_str(preset);
        out.push('\n');
    }
    // 审批模式（迁移 030）：仅 manual 追加决策策略段——遇关键决策经 nomi_task_question
    // 挂起等人工作答；auto（全授权，缺省）不渲染任何新内容，brief 零回归。
    if ctx.manual_approval {
        out.push_str(
            "\n决策策略(审批模式):\n本编排处于「审批模式」。当你遇到会显著影响方向、取舍或对外效果的关键决策问题\
             (例如互斥方案二选一、需求歧义、可能造成破坏性影响的操作)时,不要自行猜测:调用 `nomi_task_question` 工具\
             提交问题(简明列出候选选项与你的倾向建议),然后立即结束本轮回复,等待用户进入本会话作答后再继续。\
             琐碎的实现细节不必提问,自行合理处理。\n",
        );
    }
    out
}

/// The unchanged `agent`-kind brief: role hint + task title/spec + completed
/// upstream outputs as build-on context. This is byte-for-byte the pre-023
/// `compose_brief` body — the agent path must not regress.
///
/// **loop 迭代回看 (UC-1d, 评审 Important).** A `loop` body's re-run carries the
/// PRIOR round's output forward via its `pattern_config` (`loop_prior_output`,
/// written by [`settle_loop_task`] on CONTINUE — the loop controller is
/// downstream of the body so it is NOT in `upstream`). When that field is
/// present, a clear "上一轮产出" section is APPENDED so the body refines the prior
/// round (a real iterative refinement loop, not a fresh start each round). The
/// section is gated SOLELY on the field's presence: a task without
/// `loop_prior_output` (every normal agent/synthesis/verify/judge task, AND the
/// loop body's FIRST iteration which has no prior) is byte-for-byte unchanged.
fn compose_agent_brief(
    role_hint: Option<&str>,
    task: &OrchRunTaskRow,
    upstream: &[(String, String)],
    ctx: &BriefContext,
) -> String {
    let mut out = String::new();
    if let Some(role) = role_hint.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("ROLE: ");
        out.push_str(role);
        out.push_str("\n\n");
    }
    out.push_str("TASK: ");
    out.push_str(&task.title);
    out.push('\n');
    if !task.spec.trim().is_empty() {
        out.push_str("SPEC:\n");
        out.push_str(&task.spec);
        out.push('\n');
    }
    // Phase 1b Task 3: the run goal + plan shape, so a node deep in a chain knows
    // WHAT the whole orchestration is for and where it sits. Each gated on non-empty
    // so an empty BriefContext yields the exact legacy brief (byte-for-byte).
    if !ctx.goal.trim().is_empty() {
        out.push_str("\nRUN GOAL (整个编排的目标，你的产出要服务于它):\n");
        out.push_str(ctx.goal.trim());
        out.push('\n');
    }
    if !ctx.plan_summary.trim().is_empty() {
        out.push_str("\nPLAN (全局计划概要):\n");
        out.push_str(ctx.plan_summary.trim());
        out.push('\n');
    }
    if !upstream.is_empty() {
        out.push_str("\nUPSTREAM RESULTS (completed dependencies you can build on):\n");
        for (title, summary) in upstream {
            out.push_str("- ");
            out.push_str(title);
            out.push_str(": ");
            out.push_str(&truncate_summary_output(summary));
            out.push('\n');
        }
    }
    // Phase 1b Task 3: transitive-ancestor digest (blockers-of-blockers beyond the
    // DIRECT upstream) + a pointer to the shared notes file. Both gated on presence
    // (already truncated in build_brief_context). Empty ctx → neither renders.
    if !ctx.ancestor_digest.is_empty() {
        out.push_str("\nEARLIER CONTEXT (更早的相关产出摘要):\n");
        for (title, summary) in &ctx.ancestor_digest {
            out.push_str("- ");
            out.push_str(title);
            out.push_str(": ");
            out.push_str(summary);
            out.push('\n');
        }
    }
    if let Some(notes) = &ctx.notes_hint {
        out.push_str(&format!(
            "\nSHARED NOTES: 你可读取并追加 {notes}（记录发现/结论供其它节点参考）。\n"
        ));
    }
    // loop 迭代回看: APPEND the prior round's output when this body re-run carries
    // it (gated on the field — zero effect on any task without it).
    if let Some(prior) = loop_prior_output(task.pattern_config.as_deref()) {
        out.push_str("\n上一轮产出(请在此基础上改进/迭代):\n");
        out.push_str(&prior);
        out.push('\n');
    }
    out
}

/// The `synthesis`-kind brief: an explicit instruction to MERGE the dependency
/// outputs into one coherent final result. The upstream outputs lead (they are
/// the material being synthesized), and the task spec states what the merged
/// result should be. Replaces `aggregate_summary`'s mechanical concatenation for
/// a synthesis task — here a real worker reasons over the upstream outputs.
fn compose_synthesis_brief(
    role_hint: Option<&str>,
    task: &OrchRunTaskRow,
    upstream: &[(String, String)],
    ctx: &BriefContext,
) -> String {
    let mut out = String::new();
    if let Some(role) = role_hint.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("ROLE: ");
        out.push_str(role);
        out.push_str("\n\n");
    }
    out.push_str("SYNTHESIS TASK: ");
    out.push_str(&task.title);
    out.push('\n');
    out.push_str(
        "综合/合并以下上游产出，按任务要求产出最终结果（不要简单拼接，要消解冲突、去重并形成连贯整体）。\n",
    );
    if !task.spec.trim().is_empty() {
        out.push_str("SPEC:\n");
        out.push_str(&task.spec);
        out.push('\n');
    }
    // Phase 1b Task 3: the run goal + plan shape (transitive ancestors carry less
    // signal for a synthesis node — its material IS the upstream — so they are
    // omitted here). Gated on non-empty → empty ctx = legacy synthesis brief.
    if !ctx.goal.trim().is_empty() {
        out.push_str("\nRUN GOAL (整个编排的目标，你的产出要服务于它):\n");
        out.push_str(ctx.goal.trim());
        out.push('\n');
    }
    if !ctx.plan_summary.trim().is_empty() {
        out.push_str("\nPLAN (全局计划概要):\n");
        out.push_str(ctx.plan_summary.trim());
        out.push('\n');
    }
    if upstream.is_empty() {
        // Defensive: a synthesis task with no resolved upstream still runs (it just
        // has nothing to merge) — note it so the worker does not hallucinate inputs.
        out.push_str("\n(注意：没有可合并的上游产出。)\n");
    } else {
        out.push_str("\nUPSTREAM OUTPUTS TO SYNTHESIZE (合并对象):\n");
        for (title, summary) in upstream {
            out.push_str("- ");
            out.push_str(title);
            out.push_str(": ");
            // A synthesis node's PURPOSE is merging these upstream outputs — they
            // are its source material, not mere context — so inject them VERBATIM
            // (do NOT truncate to SUMMARY_TASK_OUTPUT_LEN as agent-node context is).
            // Truncating/flattening here would silently degrade the merge.
            out.push_str(summary);
            out.push('\n');
        }
    }
    // Phase 1b Task 3: pointer to the shared notes file (gated on presence).
    if let Some(notes) = &ctx.notes_hint {
        out.push_str(&format!(
            "\nSHARED NOTES: 你可读取并追加 {notes}（记录发现/结论供其它节点参考）。\n"
        ));
    }
    out
}

/// Aggregate the run summary from completed task outputs (P1: concatenation;
/// TODO: a lead-model summarization pass). Always non-empty when there is at
/// least one task (falls back to a count line).
fn aggregate_summary(tasks: &[OrchRunTaskRow]) -> String {
    let mut out = String::new();
    let done = tasks.iter().filter(|t| t.status == "done").count();
    out.push_str(&format!(
        "Run complete: {done}/{} tasks done.\n",
        tasks.len()
    ));
    for t in tasks {
        if let Some(summary) = t
            .output_summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            out.push_str("\n## ");
            out.push_str(&t.title);
            out.push('\n');
            out.push_str(summary);
            out.push('\n');
        }
    }
    out
}

/// How many chars of each task's `output_summary` to surface in the B2 summary
/// digest. Keeps a long deliverable from blowing the lead's prompt budget (CJK-
/// safe truncation); the lead synthesizes from these short renderings.
const SUMMARY_TASK_OUTPUT_LEN: usize = 600;

/// B2: compute the run summary for a COMPLETED run — a one-shot lead-model
/// synthesis with a **fail-soft** fallback to the mechanical [`aggregate_summary`]
/// concat (the historical behavior). The summarization is best-effort
/// observability and MUST NEVER fail or block the run, so EVERY failure mode
/// degrades to the concat:
/// - the run's `fleet_snapshot` is unparseable / empty (no lead derivable),
/// - the [`PlanProducer::summarize`] call errors (default Noop, no provider, LLM
///   error), or
/// - the synthesized text comes back blank.
///
/// On success it returns the synthesized prose. Called with NO lock held — the
/// run loop DROPS the per-run terminal guard before awaiting this (a slow / hanging
/// LLM call must not stall a concurrent rerun/adjust on the run), then RE-ACQUIRES
/// the lock and RE-VERIFIES the run is still all-terminal with no ready work before
/// `finish_run` writes the `completed` status + this summary. If a rerun/adjust
/// reset a task during this call, the re-verify catches it and the loop re-drives,
/// discarding the returned summary.
async fn compute_completed_summary(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    run: &OrchRunRow,
    tasks: &[OrchRunTaskRow],
) -> String {
    // The lead model is derived (inside `summarize`) from the run's frozen fleet
    // snapshot. If it does not parse / is empty, there is no lead to summarize with
    // → fall back immediately (no point making the call).
    let members: Vec<FleetMember> = match serde_json::from_str(&run.fleet_snapshot) {
        Ok(m) => m,
        Err(e) => {
            warn!(run_id, error = %e, "B2 summary: fleet snapshot unparseable — using mechanical summary");
            return aggregate_summary(tasks);
        }
    };

    let digest = build_summary_digest(tasks);
    // B2: stream the lead's summarization thought over WS. This runs with NO lock
    // held (the loop drops the per-run terminal guard before awaiting this — see
    // the doc comment above), so streaming is safe. Throttle coalesces deltas;
    // flush() after the call emits the residue. Fail-soft is unchanged below.
    let throttle =
        crate::plan::LeadThinkingThrottle::new(deps.emitter.clone(), run_id, "summarize");
    let sink = throttle.sink();
    let summarized = deps
        .summarizer
        .summarize(&run.goal, &digest, &members, Some(&sink))
        .await;
    throttle.flush();
    match summarized {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => {
            // Producer returned blank → no synthesis; fall back (no regression).
            warn!(
                run_id,
                "B2 summary: lead returned a blank summary — using mechanical summary"
            );
            aggregate_summary(tasks)
        }
        Err(e) => {
            // Producer error (Noop default, no provider, LLM failure) → fall back.
            // FAIL-SOFT: this NEVER fails the run; we just lose the synthesis.
            warn!(run_id, error = %e, "B2 summary: lead summarize failed — using mechanical summary");
            aggregate_summary(tasks)
        }
    }
}

/// Build the compact per-task digest the B2 summarizer reasons over: one line per
/// task — `title | role | status | output` — with each task's `output_summary`
/// collapsed to one line and CJK-safe truncated to [`SUMMARY_TASK_OUTPUT_LEN`].
/// A task with no output renders `output=-`. Empty when there are no tasks.
fn build_summary_digest(tasks: &[OrchRunTaskRow]) -> String {
    let mut out = String::new();
    for t in tasks {
        let role = t
            .role
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("-");
        let output = t
            .output_summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(truncate_summary_output)
            .unwrap_or_else(|| "-".to_string());
        out.push_str(&format!(
            "{} | role={role} | status={} | output={output}\n",
            t.title, t.status
        ));
    }
    out
}

/// Collapse a task's `output_summary` to one line and CJK-safe truncate it to
/// [`SUMMARY_TASK_OUTPUT_LEN`] chars (mirrors `plan::truncate_summary`).
fn truncate_summary_output(summary: &str) -> String {
    let one_line: String = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= SUMMARY_TASK_OUTPUT_LEN {
        return one_line;
    }
    let truncated: String = one_line.chars().take(SUMMARY_TASK_OUTPUT_LEN).collect();
    format!("{truncated}…")
}

// ──────────────────────────────────────────────────────────────────────────
// verify 模式 (UC-1b): N skeptic agent tasks → a no-LLM `verify` aggregator
// that tallies their pass/fail verdicts by a vote policy and gates downstream.
//
// The aggregator is NOT a worker dispatch: it is settled SYNCHRONOUSLY in the
// run loop's fill step (see `settle_verify_task`), reading its dependency
// tasks' `output_summary` and computing a verdict. It never enters the in-flight
// worker set, never spins (it settles in one pass once its deps are done), and
// its `conversation_id` stays None (there is no worker conversation).
// ──────────────────────────────────────────────────────────────────────────

/// The vote policy for a `verify` aggregator, read from its `pattern_config`
/// JSON (`{"vote": ...}`). Defaults to [`VotePolicy::Majority`] when the config
/// is absent, blank, malformed, or carries an unknown `vote` value — fail-soft,
/// matching `parse_plan`'s tolerance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VotePolicy {
    /// Pass iff strictly more than half the skeptics voted pass (> N/2).
    Majority,
    /// Pass iff EVERY skeptic voted pass (and there is at least one skeptic).
    Unanimous,
    /// Pass iff at least `n` skeptics voted pass.
    Threshold(usize),
}

impl VotePolicy {
    /// Parse the policy from a `verify` task's `pattern_config` raw JSON string.
    /// Fail-soft: any problem (None / blank / not JSON / unknown shape) yields
    /// [`VotePolicy::Majority`], the safe default. Accepted `vote` shapes:
    /// - `"majority"` → Majority
    /// - `"unanimous"` → Unanimous
    /// - `{"threshold": N}` → Threshold(N)
    fn parse(pattern_config: Option<&str>) -> Self {
        let Some(raw) = pattern_config.map(str::trim).filter(|s| !s.is_empty()) else {
            return VotePolicy::Majority;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            return VotePolicy::Majority;
        };
        let Some(vote) = value.get("vote") else {
            return VotePolicy::Majority;
        };
        // String form: "majority" | "unanimous".
        if let Some(s) = vote.as_str() {
            return match s.trim().to_ascii_lowercase().as_str() {
                "unanimous" => VotePolicy::Unanimous,
                // "majority" and anything unknown both fall to the safe default.
                _ => VotePolicy::Majority,
            };
        }
        // Object form: {"threshold": N}.
        if let Some(n) = vote.get("threshold").and_then(serde_json::Value::as_u64) {
            return VotePolicy::Threshold(n as usize);
        }
        VotePolicy::Majority
    }

    /// Does `pass_count` of `total` skeptic verdicts satisfy this policy?
    fn passes(self, pass_count: usize, total: usize) -> bool {
        match self {
            // Strict majority: more than half. With 0 skeptics, 0 > 0 is false →
            // a verify with no skeptic deps fails (fail-safe).
            VotePolicy::Majority => pass_count * 2 > total,
            // Every skeptic passed AND there was at least one skeptic.
            VotePolicy::Unanimous => total > 0 && pass_count == total,
            // At least `n` passes. A Threshold(0) trivially passes (the planner
            // is responsible for a sensible threshold; we do not second-guess it).
            VotePolicy::Threshold(n) => pass_count >= n,
        }
    }
}

/// Parse a single skeptic's `output_summary` into a pass/fail verdict.
///
/// **Fail-safe: an unparseable / missing output counts as FAIL.** Unvalidated
/// or unreadable skeptic output must never be treated as approval.
///
/// Order of preference:
/// 1. A JSON object anywhere in the text carrying a boolean `"pass"` field
///    (e.g. `{"pass": true, "critique": "..."}`) — the skeptic prompt asks for
///    exactly this shape. Quote/escape-aware extraction via [`first_json_object`].
/// 2. Fallback to text scanning: an explicit `FAIL` wins over `PASS` (a skeptic
///    that says both is treated conservatively as a fail). Matched
///    case-insensitively as a whole-ish token.
/// 3. Neither → FAIL.
fn parse_verdict_pass(output_summary: Option<&str>) -> bool {
    let Some(text) = output_summary.map(str::trim).filter(|s| !s.is_empty()) else {
        return false; // missing/blank output → fail-safe
    };

    // 1) Prefer a JSON `{"pass": bool}` anywhere in the text.
    if let Some(json) = first_json_object(text) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) {
            if let Some(pass) = value.get("pass").and_then(serde_json::Value::as_bool) {
                return pass;
            }
        }
    }

    // 2) Text fallback: FAIL beats PASS (conservative), else look for PASS.
    let upper = text.to_ascii_uppercase();
    if upper.contains("FAIL") {
        return false;
    }
    if upper.contains("PASS") {
        return true;
    }

    // 3) Unrecognizable → fail-safe.
    false
}

/// Extract the first balanced top-level `{...}` substring from `text`,
/// quote/escape-aware (so braces inside string values don't confuse the
/// counter). Mirrors `plan::extract_json_object` but kept local to the engine
/// (the verify aggregator must not depend on the planner module). Returns `None`
/// when no balanced object is present.
fn first_json_object(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
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
                    return Some(text[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// The computed result of a `verify` aggregator over its skeptic dependencies.
struct VerifyVerdict {
    pass: bool,
    pass_count: usize,
    total: usize,
    /// `(skeptic title, pass, output_summary)` per dependency, in dep order —
    /// for the human-readable summary written to the verify task.
    critiques: Vec<(String, bool, String)>,
}

/// Tally a `verify` aggregator's verdict from its skeptic dependency tasks.
///
/// `deps_in_order` are the verify task's blocker tasks (the skeptics), each with
/// its `output_summary`. Each is parsed via [`parse_verdict_pass`] (fail-safe);
/// the pass count is then judged against `policy`. Pure (no I/O) so it is unit-
/// testable in isolation.
fn tally_verify(deps_in_order: &[OrchRunTaskRow], policy: VotePolicy) -> VerifyVerdict {
    let mut pass_count = 0usize;
    let mut critiques: Vec<(String, bool, String)> = Vec::new();
    for t in deps_in_order {
        let pass = parse_verdict_pass(t.output_summary.as_deref());
        if pass {
            pass_count += 1;
        }
        critiques.push((
            t.title.clone(),
            pass,
            t.output_summary.clone().unwrap_or_default(),
        ));
    }
    let total = deps_in_order.len();
    VerifyVerdict {
        pass: policy.passes(pass_count, total),
        pass_count,
        total,
        critiques,
    }
}

/// Render a `verify` verdict into the aggregator task's `output_summary` — a
/// machine-leading line (`VERDICT: PASS|FAIL (k/n, policy=...)`) followed by the
/// per-skeptic critiques, so both the engine/downstream and the UI can read it.
fn render_verify_summary(verdict: &VerifyVerdict, policy: VotePolicy) -> String {
    let policy_label = match policy {
        VotePolicy::Majority => "majority".to_string(),
        VotePolicy::Unanimous => "unanimous".to_string(),
        VotePolicy::Threshold(n) => format!("threshold:{n}"),
    };
    let mut out = format!(
        "VERDICT: {} ({}/{} skeptics passed, policy={})\n",
        if verdict.pass { "PASS" } else { "FAIL" },
        verdict.pass_count,
        verdict.total,
        policy_label,
    );
    if verdict.critiques.is_empty() {
        out.push_str("(no skeptic verdicts to aggregate)\n");
    } else {
        out.push_str("\nSKEPTIC VERDICTS:\n");
        for (title, pass, critique) in &verdict.critiques {
            out.push_str(&format!(
                "- {} → {}: {}\n",
                title,
                if *pass { "PASS" } else { "FAIL" },
                critique.trim(),
            ));
        }
    }
    out
}

/// Settle a ready `verify` aggregator task SYNCHRONOUSLY (no worker dispatch):
/// read its skeptic dependency outputs, tally a verdict by the task's vote
/// policy, write the verdict to the task's `output_summary`, mark it `done`, and
/// — on a FAIL verdict — GATE downstream by marking the verify task's transitive
/// dependents `skipped` so unvalidated work never proceeds.
///
/// **Gate design (skip dependents, NOT mark-verify-failed):** the verify task
/// itself is marked `done` (it successfully computed a verdict — that is its
/// job; a fail verdict is a valid outcome, not a task failure). Its downstream
/// dependents are marked `skipped`. This is the cleaner option because:
/// - the run can still reach `completed` (all tasks `done`/`skipped`) — the
///   verification ran correctly and gated correctly; the RUN did not fail;
/// - the verify node stays `done` with the FAIL verdict in its `output_summary`
///   (high observability — the user sees WHY downstream was skipped);
/// - it does not rely on `list_ready_tasks`' `status != 'done'` blocker gating
///   leaving dependents stuck `pending` forever (which would make the loop
///   declare the graph "stuck" and break, an ambiguous terminal state).
///
/// Bounded + no spin: the task is read once, tallied once, and transitioned once
/// (the skip walk is a finite BFS over the dep edges). It is invoked only when
/// the task is already in the ready set (all skeptics `done`), so it settles in a
/// single fill pass.
async fn settle_verify_task(deps: &Arc<RunEngineDeps>, run_id: &str, task: &OrchRunTaskRow) {
    // Resolve the skeptic dependencies (this task's blockers), in task order, so
    // the verdict tally + summary are deterministic.
    let dep_edges = deps.run_repo.list_deps(run_id).await.unwrap_or_default();
    let blocker_ids: HashSet<String> = dep_edges
        .iter()
        .filter(|d| d.blocked_task_id == task.id)
        .map(|d| d.blocker_task_id.clone())
        .collect();
    let all_tasks = deps.run_repo.list_tasks(run_id).await.unwrap_or_default();
    let skeptics: Vec<OrchRunTaskRow> = all_tasks
        .iter()
        .filter(|t| blocker_ids.contains(&t.id))
        .cloned()
        .collect();

    let policy = VotePolicy::parse(task.pattern_config.as_deref());
    let verdict = tally_verify(&skeptics, policy);
    let summary = render_verify_summary(&verdict, policy);

    // Persist the verdict + mark the aggregator `done` (conversation_id stays
    // None — there is no worker conversation for a verify task).
    let _ = deps
        .run_repo
        .update_task(
            &task.id,
            UpdateTaskParams {
                last_error: None,
                status: Some("done".to_string()),
                spec: None,
                conversation_id: None,
                output_summary: Some(Some(summary)),
                output_files: None,
                attempt: None,
                tokens: None,
                graph_x: None,
                graph_y: None,
                pattern_config: None,
                next_retry_at: None,
            },
        )
        .await;
    deps.emitter.emit_task_status(run_id, &task.id, "done");
    info!(
        run_id,
        task_id = %task.id,
        pass = verdict.pass,
        pass_count = verdict.pass_count,
        total = verdict.total,
        "verify aggregator settled"
    );

    // FAIL → gate downstream: mark the verify task's transitive dependents
    // `skipped`. PASS → do nothing (downstream proceeds normally).
    if !verdict.pass {
        skip_downstream(deps, run_id, &task.id, &dep_edges).await;
    }
}

// ──────────────────────────────────────────────────────────────────────────
// judge 模式 (UC-1c): the `judge` aggregator. The lead plans M candidate `agent`
// tasks (usually a fan-out group producing alternatives) + N judge `agent` tasks
// (each depends on ALL M candidates and OUTPUTs a JSON ballot scoring every
// candidate) + ONE `judge` aggregator task depending on all N judges. The engine
// settles the aggregator NO-LLM in the fill step (`settle_judge_task`): it parses
// each judge's ballot into M scores, aggregates across judges per candidate by
// policy (mean | borda), picks the winner (argmax, ties → lowest index), and
// writes a parseable `WINNER:` marker to its `output_summary`. There is NO
// downstream gate (a judge picks a winner, it does not fail the run) — the winner
// is just REPORTED for a downstream consumer/synthesis to use.
//
// Like the verify aggregator it is NOT a worker dispatch: it is settled
// SYNCHRONOUSLY in the run loop's fill step, never enters the in-flight worker
// set, never spins (settles in one pass once its deps are done), and its
// `conversation_id` stays None (no worker conversation).
// ──────────────────────────────────────────────────────────────────────────

/// The aggregation policy for a `judge` aggregator, read from its
/// `pattern_config` JSON (`{"aggregate": ...}`). Defaults to [`JudgePolicy::Mean`]
/// when the config is absent, blank, malformed, or carries an unknown
/// `aggregate` value — fail-soft, matching `VotePolicy::parse`'s tolerance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JudgePolicy {
    /// Average each candidate's scores across judges; winner = highest mean.
    Mean,
    /// Each judge RANKS the M candidates by its scores; award Borda points
    /// (M-1, M-2, …, 0) summed across judges; winner = highest total.
    Borda,
}

impl JudgePolicy {
    /// Parse the policy from a `judge` task's `pattern_config` raw JSON string.
    /// Fail-soft: any problem (None / blank / not JSON / unknown shape) yields
    /// [`JudgePolicy::Mean`], the safe default. Accepted `aggregate` shapes:
    /// - `"mean"` → Mean
    /// - `"borda"` → Borda
    fn parse(pattern_config: Option<&str>) -> Self {
        let Some(raw) = pattern_config.map(str::trim).filter(|s| !s.is_empty()) else {
            return JudgePolicy::Mean;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            return JudgePolicy::Mean;
        };
        let Some(agg) = value.get("aggregate").and_then(serde_json::Value::as_str) else {
            return JudgePolicy::Mean;
        };
        match agg.trim().to_ascii_lowercase().as_str() {
            "borda" => JudgePolicy::Borda,
            // "mean" and anything unknown both fall to the safe default.
            _ => JudgePolicy::Mean,
        }
    }

    fn label(self) -> &'static str {
        match self {
            JudgePolicy::Mean => "mean",
            JudgePolicy::Borda => "borda",
        }
    }
}

/// Parse a single judge's `output_summary` into a ballot of per-candidate scores.
///
/// A ballot is `M` numeric scores, one per candidate, keyed by candidate index.
/// Two JSON shapes are accepted (the judge prompt asks for either):
/// - **array**: `{"scores":[0.8,0.3,0.6]}` → index i = candidate i's score.
/// - **object**: `{"scores":{"0":0.8,"2":0.6}}` → key = candidate index (a
///   sparse ballot is fine; missing candidates are left as `None`).
///
/// Returns a `Vec<Option<f64>>` of length `candidates` (so the matrix is
/// rectangular across judges): a candidate the judge did not score is `None`.
///
/// **Fail-safe: a missing / blank / unparseable ballot, or one with no usable
/// `scores`, returns `None` — the caller DROPS that judge (it contributes no
/// scores), never panics.** Out-of-range indices and non-numeric entries are
/// silently ignored.
fn parse_judge_ballot(output_summary: Option<&str>, candidates: usize) -> Option<Vec<Option<f64>>> {
    let text = output_summary.map(str::trim).filter(|s| !s.is_empty())?;
    // Prefer a JSON object anywhere in the text carrying a `scores` field.
    let json = first_json_object(text)?;
    let value = serde_json::from_str::<serde_json::Value>(&json).ok()?;
    let scores = value.get("scores")?;

    let mut ballot: Vec<Option<f64>> = vec![None; candidates];
    let mut any = false;
    match scores {
        // Array form: positional by candidate index.
        serde_json::Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                if i >= candidates {
                    break; // ignore extra entries beyond the candidate count
                }
                if let Some(n) = v.as_f64() {
                    ballot[i] = Some(n);
                    any = true;
                }
            }
        }
        // Object form: keyed by candidate index ("0", "1", …).
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let Ok(idx) = k.trim().parse::<usize>() else {
                    continue; // non-index key → ignore
                };
                if idx >= candidates {
                    continue; // out-of-range index → ignore
                }
                if let Some(n) = v.as_f64() {
                    ballot[idx] = Some(n);
                    any = true;
                }
            }
        }
        _ => return None, // scores is neither array nor object → drop
    }

    // A ballot with no usable scores at all contributes nothing → drop it.
    if any { Some(ballot) } else { None }
}

/// Determine the candidate count `M` for a judge aggregator.
///
/// Preference order (fail-soft):
/// 1. An explicit `{"candidates":M}` in the judge task's `pattern_config`.
/// 2. The max ballot length observed across the judges' parsed score arrays /
///    the highest object key + 1 (so we size the matrix to what the judges
///    actually scored).
///
/// Returns the resolved `M` (0 when neither source yields a positive count — the
/// caller then produces an empty result, no winner).
fn resolve_candidate_count(pattern_config: Option<&str>, judge_outputs: &[Option<&str>]) -> usize {
    // 1) Explicit pin in pattern_config.
    if let Some(raw) = pattern_config.map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
            if let Some(m) = value.get("candidates").and_then(serde_json::Value::as_u64) {
                return m as usize;
            }
        }
    }
    // 2) Infer from the judges' ballots: the widest array / highest object key.
    let mut max_m = 0usize;
    for out in judge_outputs {
        let Some(text) = out.map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some(json) = first_json_object(text) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) else {
            continue;
        };
        let Some(scores) = value.get("scores") else {
            continue;
        };
        match scores {
            serde_json::Value::Array(arr) => max_m = max_m.max(arr.len()),
            serde_json::Value::Object(map) => {
                for k in map.keys() {
                    if let Ok(idx) = k.trim().parse::<usize>() {
                        max_m = max_m.max(idx + 1);
                    }
                }
            }
            _ => {}
        }
    }
    max_m
}

/// The computed result of a `judge` aggregator over its judge dependencies.
struct JudgeResult {
    /// Winning candidate index (argmax of `aggregate`, ties → lowest index), or
    /// `None` when there is nothing to pick (no candidates or no usable ballots).
    winner: Option<usize>,
    /// Per-candidate aggregate score (mean of scores, or summed Borda points),
    /// indexed by candidate. Length `M`.
    aggregate: Vec<f64>,
    /// How many judges contributed a usable ballot (dropped judges excluded).
    judges_counted: usize,
    /// Total judge dependencies seen (including dropped ones).
    judges_total: usize,
}

/// Aggregate `N` judge ballots (`ballots[judge][candidate]`, each a sparse
/// `Vec<Option<f64>>` of length `M`) into a per-candidate aggregate + winner,
/// under `policy`. Pure (no I/O) so it is unit-testable in isolation.
///
/// - **Mean**: per candidate, average the scores from the judges that scored it
///   (a candidate no judge scored gets `0.0`, so it can never win over a scored
///   one). Winner = argmax.
/// - **Borda**: per judge, rank the candidates it scored by descending score and
///   award `(count-1, count-2, …, 0)` points (ties within a judge share the
///   average of their contested point block, kept deterministic by stable
///   ordering on candidate index); sum across judges. Winner = argmax.
///
/// **Determinism**: ties in the final aggregate are broken by LOWEST candidate
/// index (the first argmax wins). Within a single judge's Borda ranking, equal
/// scores are ordered by candidate index so the same ballots always yield the
/// same points.
fn aggregate_judge(
    ballots: &[Vec<Option<f64>>],
    candidates: usize,
    policy: JudgePolicy,
) -> JudgeResult {
    let judges_total = ballots.len();
    // (ballots passed in are already the usable ones; `judges_counted` == len)
    let judges_counted = ballots.len();

    if candidates == 0 {
        return JudgeResult {
            winner: None,
            aggregate: Vec::new(),
            judges_counted,
            judges_total,
        };
    }

    let mut aggregate = vec![0.0f64; candidates];

    match policy {
        JudgePolicy::Mean => {
            // Sum + count per candidate across the judges that scored it.
            let mut sums = vec![0.0f64; candidates];
            let mut counts = vec![0usize; candidates];
            for ballot in ballots {
                for (c, score) in ballot.iter().enumerate().take(candidates) {
                    if let Some(s) = score {
                        sums[c] += *s;
                        counts[c] += 1;
                    }
                }
            }
            for c in 0..candidates {
                aggregate[c] = if counts[c] > 0 {
                    sums[c] / counts[c] as f64
                } else {
                    0.0
                };
            }
        }
        JudgePolicy::Borda => {
            // Each judge ranks the candidates it scored; award M'-1 … 0 points
            // where M' is how many candidates that judge scored. Ties within a
            // judge share the average of the contested points (deterministic via
            // stable ordering on candidate index).
            for ballot in ballots {
                // (candidate_index, score) for candidates this judge scored.
                let mut scored: Vec<(usize, f64)> = ballot
                    .iter()
                    .enumerate()
                    .take(candidates)
                    .filter_map(|(c, s)| s.map(|v| (c, v)))
                    .collect();
                let m = scored.len();
                if m == 0 {
                    continue;
                }
                // Sort by descending score; ties broken by ASCENDING candidate
                // index so the ordering is deterministic.
                scored.sort_by(|a, b| {
                    b.1.partial_cmp(&a.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.0.cmp(&b.0))
                });
                // Award points, splitting ties evenly across the contested block
                // so two candidates with the same score get the same points.
                let mut i = 0usize;
                while i < m {
                    let mut j = i + 1;
                    while j < m && (scored[j].1 - scored[i].1).abs() < f64::EPSILON {
                        j += 1;
                    }
                    // Candidates scored[i..j] are tied. Points for ranks i..j are
                    // (m-1-i), (m-1-(i+1)), …; share their average.
                    let block = j - i;
                    let mut block_points = 0.0f64;
                    for rank in i..j {
                        block_points += (m - 1 - rank) as f64;
                    }
                    let per = block_points / block as f64;
                    for (c, _) in &scored[i..j] {
                        aggregate[*c] += per;
                    }
                    i = j;
                }
            }
        }
    }

    // Winner = argmax with ties → lowest candidate index. Only meaningful when at
    // least one judge contributed (otherwise every aggregate is 0 and we report
    // no winner so a downstream consumer can tell nothing was judged).
    let winner = if judges_counted == 0 {
        None
    } else {
        let mut best_idx = 0usize;
        let mut best_val = aggregate[0];
        for (c, v) in aggregate.iter().enumerate().skip(1) {
            if *v > best_val {
                best_val = *v;
                best_idx = c;
            }
        }
        Some(best_idx)
    };

    JudgeResult {
        winner,
        aggregate,
        judges_counted,
        judges_total,
    }
}

/// Render a `judge` result into the aggregator task's `output_summary` — a
/// machine-leading line (`WINNER: candidate K (aggregate=mean|borda, scores=[…],
/// judges=c/n)`) so both a downstream consumer and the UI can parse the winner.
/// When there is no winner (no candidates / no usable ballots) it leads with
/// `WINNER: none`.
fn render_judge_summary(result: &JudgeResult, policy: JudgePolicy) -> String {
    let scores_csv = result
        .aggregate
        .iter()
        .map(|v| format!("{v:.3}"))
        .collect::<Vec<_>>()
        .join(", ");
    match result.winner {
        Some(k) => format!(
            "WINNER: candidate {k} (aggregate={}, scores=[{}], judges={}/{})",
            policy.label(),
            scores_csv,
            result.judges_counted,
            result.judges_total,
        ),
        None => format!(
            "WINNER: none (aggregate={}, scores=[{}], judges={}/{})",
            policy.label(),
            scores_csv,
            result.judges_counted,
            result.judges_total,
        ),
    }
}

/// Settle a ready `judge` aggregator task SYNCHRONOUSLY (no worker dispatch):
/// read its N judge dependency outputs, parse each as a ballot of M per-candidate
/// scores (fail-safe — an unparseable judge is DROPPED), aggregate across judges
/// by the task's policy (mean / borda), pick the winning candidate index (argmax,
/// ties → lowest index), write the `WINNER:` marker + per-candidate aggregates to
/// the task's `output_summary`, and mark it `done`.
///
/// **No downstream gate:** unlike `verify`, a judge does NOT skip its dependents —
/// it picks a winner, it does not fail the run. The winner is REPORTED in the
/// `output_summary` for a downstream synthesis/consumer to use.
///
/// Bounded + no spin: the deps are read once, aggregated once, and the task is
/// transitioned once. It is invoked only when the task is already in the ready
/// set (all judges `done`), so it settles in a single fill pass.
async fn settle_judge_task(deps: &Arc<RunEngineDeps>, run_id: &str, task: &OrchRunTaskRow) {
    // Resolve the judge dependencies (this task's blockers), in task order, so the
    // aggregate + summary are deterministic.
    let dep_edges = deps.run_repo.list_deps(run_id).await.unwrap_or_default();
    let blocker_ids: HashSet<String> = dep_edges
        .iter()
        .filter(|d| d.blocked_task_id == task.id)
        .map(|d| d.blocker_task_id.clone())
        .collect();
    let all_tasks = deps.run_repo.list_tasks(run_id).await.unwrap_or_default();
    let judges: Vec<OrchRunTaskRow> = all_tasks
        .iter()
        .filter(|t| blocker_ids.contains(&t.id))
        .cloned()
        .collect();

    let policy = JudgePolicy::parse(task.pattern_config.as_deref());
    let judge_outputs: Vec<Option<&str>> =
        judges.iter().map(|j| j.output_summary.as_deref()).collect();
    let candidates = resolve_candidate_count(task.pattern_config.as_deref(), &judge_outputs);

    // Parse each judge's ballot; DROP the unparseable ones (fail-safe).
    let judges_total = judges.len();
    let ballots: Vec<Vec<Option<f64>>> = judge_outputs
        .iter()
        .filter_map(|out| parse_judge_ballot(*out, candidates))
        .collect();

    let mut result = aggregate_judge(&ballots, candidates, policy);
    // `aggregate_judge` was handed only the usable ballots, so it reports
    // `judges_total == usable`. Surface the TRUE total (including dropped) so the
    // summary's `judges=c/n` reflects how many were dropped.
    result.judges_total = judges_total;
    let summary = render_judge_summary(&result, policy);

    // Persist the result + mark the aggregator `done` (conversation_id stays None
    // — there is no worker conversation for a judge task).
    let _ = deps
        .run_repo
        .update_task(
            &task.id,
            UpdateTaskParams {
                last_error: None,
                status: Some("done".to_string()),
                spec: None,
                conversation_id: None,
                output_summary: Some(Some(summary)),
                output_files: None,
                attempt: None,
                tokens: None,
                graph_x: None,
                graph_y: None,
                pattern_config: None,
                next_retry_at: None,
            },
        )
        .await;
    deps.emitter.emit_task_status(run_id, &task.id, "done");
    info!(
        run_id,
        task_id = %task.id,
        winner = ?result.winner,
        candidates,
        judges_counted = result.judges_counted,
        judges_total = result.judges_total,
        "judge aggregator settled"
    );
    // NOTE: no downstream gate — a judge reports a winner, it does not skip work.
}

// ──────────────────────────────────────────────────────────────────────────
// loop 模式 (UC-1d): the `loop` controller. The lead plans ONE BODY `agent` task
// + ONE `loop` controller task that `depends_on` the body. The controller is
// settled NO-LLM in the fill step (`settle_loop_task`) every time the body
// reaches `done`: it evaluates a stop criterion over the body's `output_summary`
// + iteration count and either
//   - STOPs (criterion met OR the HARD `max_iter` cap reached): marks itself
//     `done`, writing the final body output + iteration count to its summary; or
//   - CONTINUEs (criterion not met AND under the cap): RESETS the body in place
//     (status→`pending`, clear output_summary/conversation_id, attempt+1) and
//     leaves itself `pending`. The body re-enters the normal ready→dispatch→
//     worker→done path; when it completes the controller fires again.
//
// NO-SPIN (the critical invariant): the loop ALWAYS terminates. The HARD
// `max_iter` cap is the backstop — `iterations_done + 1 >= max_iter` forces STOP
// even if the criterion never fires. Each CONTINUE requires a REAL body worker
// run (a `done`→`pending`→worker→`done` cycle = one unit of monotonic progress,
// counted by the body's `attempt`), and `attempt` is strictly bounded by
// `max_iter`. The controller settle is a one-shot per body-completion: after a
// CONTINUE the body is `pending` (un-`done`), so the controller's only blocker is
// no longer `done` and `list_ready_tasks` does NOT return the controller again
// until the body re-completes — there is no path where the controller re-settles
// without the body having re-run, and no path past the cap.
//
// A body that FAILS mid-loop never reaches `done`, so the controller never
// becomes ready by the normal path. `settle_loop_task` is therefore also invoked
// when the body is `failed` (see the fill step's loop branch): it STOPs the loop
// as `failed` and gates (skips) downstream, so a failing body never iterates and
// the run still reaches a clean terminal state.
//
// Like verify/judge the controller is NOT a worker dispatch: it is settled
// SYNCHRONOUSLY in the fill step, never enters the in-flight worker set, and its
// `conversation_id` stays None (there is no worker conversation).
// ──────────────────────────────────────────────────────────────────────────

/// Default hard iteration cap when a `loop` task's `pattern_config` omits (or
/// fail-soft-loses) `max_iter`. Small by design — the cap is the no-spin backstop.
const DEFAULT_LOOP_MAX_ITER: u64 = 5;

/// A `loop` controller's stop criterion, parsed from its `pattern_config`
/// (`{"max_iter":N,"stop":{...}}`). The HARD `max_iter` cap is held separately on
/// [`LoopConfig`]; this enum is only the EARLY-stop test. Fail-soft: an unknown /
/// missing `stop` degrades to [`StopCriteria::MaxIter`] (cap-only — the loop runs
/// to the cap, never unbounded).
#[derive(Debug, Clone, PartialEq, Eq)]
enum StopCriteria {
    /// Stop only when the hard `max_iter` cap is reached (no early stop).
    MaxIter,
    /// Stop early once the body output contains `done_marker` (or strict JSON
    /// `{"done":true}`).
    Predicate { done_marker: String },
    /// Stop early once `quiet_rounds` consecutive rounds produced the SAME body
    /// output (no further change). `quiet_rounds` is clamped to >= 1.
    Dry { quiet_rounds: usize },
    /// Stop early once the body's output PASSES a verdict check — a
    /// verdict-gated loop: "iterate until the result is approved". The body is
    /// expected to SELF-ASSESS each round and emit a verdict the engine parses via
    /// [`parse_verdict_pass`] (JSON `{"pass":true}` preferred, a `PASS` text marker
    /// fallback). Fail-safe: an unparseable / not-yet-approved output is treated as
    /// NOT approved → CONTINUE (still hard-capped by `max_iter`, so it always
    /// terminates — there is NO unbounded path).
    Approved,
}

/// A parsed `loop` controller config: the HARD cap + the early-stop criterion.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LoopConfig {
    /// HARD upper bound on iterations (>= 1). The no-spin backstop — the loop
    /// ALWAYS stops once this many body rounds have completed, criterion or not.
    max_iter: u64,
    stop: StopCriteria,
}

impl LoopConfig {
    /// Parse from a `loop` task's `pattern_config` raw JSON string. Fail-soft:
    /// any problem (None / blank / not JSON / missing fields / unknown stop kind)
    /// degrades to a bounded cap-only loop (`max_iter` default
    /// [`DEFAULT_LOOP_MAX_ITER`], `stop` = [`StopCriteria::MaxIter`]) — there is
    /// NEVER an unbounded path.
    fn parse(pattern_config: Option<&str>) -> Self {
        let default = LoopConfig {
            max_iter: DEFAULT_LOOP_MAX_ITER,
            stop: StopCriteria::MaxIter,
        };
        let Some(raw) = pattern_config.map(str::trim).filter(|s| !s.is_empty()) else {
            return default;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            return default;
        };

        // max_iter: REQUIRED hard cap — default when absent/invalid, clamped >= 1.
        let max_iter = value
            .get("max_iter")
            .and_then(serde_json::Value::as_u64)
            .filter(|n| *n >= 1)
            .unwrap_or(DEFAULT_LOOP_MAX_ITER);

        // stop: fail-soft to MaxIter (cap-only) on anything unrecognized.
        let stop = match value.get("stop") {
            Some(stop_val) => Self::parse_stop(stop_val),
            None => StopCriteria::MaxIter,
        };

        LoopConfig { max_iter, stop }
    }

    /// Parse the `stop` object fail-soft. Unknown `kind` (or a non-object) →
    /// [`StopCriteria::MaxIter`].
    fn parse_stop(stop: &serde_json::Value) -> StopCriteria {
        let kind = stop
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        match kind.trim().to_ascii_lowercase().as_str() {
            "predicate" => {
                // The marker is required for a useful predicate; an absent/blank
                // marker degrades to cap-only (it could never fire).
                let marker = stop
                    .get("done_marker")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                match marker {
                    Some(m) => StopCriteria::Predicate {
                        done_marker: m.to_string(),
                    },
                    None => StopCriteria::MaxIter,
                }
            }
            "dry" => {
                // quiet_rounds clamped to >= 1 (1 = "stop as soon as one round
                // repeats the previous one").
                let k = stop
                    .get("quiet_rounds")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1)
                    .max(1) as usize;
                StopCriteria::Dry { quiet_rounds: k }
            }
            // Verdict-gated: stop once the body's self-assessed output PASSES the
            // verdict check (reusing `parse_verdict_pass`). Needs no extra config —
            // the body emits the verdict, the engine reads it. Aliases accepted so
            // a slightly-different planner phrasing still lands here (still bounded
            // by max_iter either way — an unrecognized kind only degrades to
            // cap-only, never unbounded).
            "approved" | "verdict" | "verify" => StopCriteria::Approved,
            // "max_iter" and anything unknown both fall to the safe cap-only stop.
            _ => StopCriteria::MaxIter,
        }
    }
}

/// Does the body's latest output satisfy a `predicate` stop? True when the text
/// contains the `done_marker` (case-sensitive substring) OR a strict JSON object
/// anywhere in it carries `"done": true`.
fn predicate_done(body_output: Option<&str>, done_marker: &str) -> bool {
    let Some(text) = body_output.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    if !done_marker.is_empty() && text.contains(done_marker) {
        return true;
    }
    if let Some(json) = first_json_object(text) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) {
            if value.get("done").and_then(serde_json::Value::as_bool) == Some(true) {
                return true;
            }
        }
    }
    false
}

/// A stable, collision-resistant-enough content key for a body output round,
/// used by the `dry` criterion to detect unchanged rounds. Trims surrounding
/// whitespace (so trivial reformatting is not the signal) then hashes. An absent
/// output hashes the empty string (distinct rounds with no output are "equal").
fn round_hash(body_output: Option<&str>) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let normalized = body_output.map(str::trim).unwrap_or("");
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    hasher.finish()
}

/// The decision `settle_loop_task` makes after one body completion.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoopDecision {
    /// Stop the loop `done` — criterion met or the hard cap reached. Carries the
    /// human-readable stop reason for the summary.
    Stop { reason: &'static str },
    /// Re-run the body for another round (criterion not met AND under the cap).
    Continue,
}

/// Decide STOP vs CONTINUE for a `loop` controller after the body finished a
/// round, GIVEN the parsed config, the iteration count just completed
/// (`iterations_done` = body `attempt` + 1), the body's current output, and the
/// recorded hashes of every PRIOR round's output (oldest→newest, NOT including
/// this round). Pure (no I/O) so the no-spin termination is unit-testable.
///
/// **HARD cap wins:** the very first check is `iterations_done >= max_iter` → STOP
/// (reason `max_iter`). This is the backstop: regardless of the criterion, the
/// loop can never run more than `max_iter` body rounds. Only when UNDER the cap is
/// the early-stop criterion consulted.
fn decide_loop(
    config: &LoopConfig,
    iterations_done: u64,
    body_output: Option<&str>,
    prior_hashes: &[u64],
) -> LoopDecision {
    // (1) HARD cap — ALWAYS wins. Once this many rounds have completed, stop no
    // matter what. iterations_done is body.attempt + 1, so attempt+1 >= max_iter.
    if iterations_done >= config.max_iter {
        return LoopDecision::Stop { reason: "max_iter" };
    }

    // (2) Under the cap → consult the early-stop criterion.
    match &config.stop {
        StopCriteria::MaxIter => LoopDecision::Continue, // cap-only: keep going
        StopCriteria::Predicate { done_marker } => {
            if predicate_done(body_output, done_marker) {
                LoopDecision::Stop {
                    reason: "predicate",
                }
            } else {
                LoopDecision::Continue
            }
        }
        StopCriteria::Dry { quiet_rounds } => {
            // The last `quiet_rounds` rounds (this round + the prior ones) must
            // all share the same hash. With this round's hash appended, we need
            // `quiet_rounds` consecutive equal hashes at the tail.
            let this_hash = round_hash(body_output);
            // Need at least `quiet_rounds` rounds total to have that many equal.
            if *quiet_rounds <= 1 {
                // quiet_rounds==1 means "stop the moment a round equals the prior
                // one" → need at least one prior hash equal to this one.
                if prior_hashes.last() == Some(&this_hash) {
                    return LoopDecision::Stop { reason: "dry" };
                }
                return LoopDecision::Continue;
            }
            // We need the last (quiet_rounds-1) PRIOR hashes plus this one to all
            // be equal to this_hash.
            let need_prior = quiet_rounds - 1;
            if prior_hashes.len() >= need_prior
                && prior_hashes[prior_hashes.len() - need_prior..]
                    .iter()
                    .all(|h| *h == this_hash)
            {
                LoopDecision::Stop { reason: "dry" }
            } else {
                LoopDecision::Continue
            }
        }
        StopCriteria::Approved => {
            // Verdict-gated: REUSE `parse_verdict_pass` over the body's latest
            // output. PASS → the body's work is approved → STOP early (reason
            // `approved`). Otherwise (not-yet-approved, or fail-safe on an
            // unparseable output) → CONTINUE — still bounded by the HARD `max_iter`
            // cap checked FIRST above, so the loop ALWAYS terminates.
            if parse_verdict_pass(body_output) {
                LoopDecision::Stop { reason: "approved" }
            } else {
                LoopDecision::Continue
            }
        }
    }
}

/// Machine-readable prefix the controller writes to its OWN `output_summary`
/// while iterating, so the next settle can recover the round hashes (the body's
/// own output is cleared on reset, so the controller is the only place to keep
/// the `dry` history). Format (single line): `LOOP-STATE: hashes=h1,h2,...`.
const LOOP_STATE_PREFIX: &str = "LOOP-STATE: hashes=";

/// Recover the recorded prior-round hashes from the controller's persisted
/// `output_summary` (the `LOOP-STATE:` line). Returns an empty vec when absent or
/// unparseable (fail-soft — a lost history just makes `dry` conservative, never
/// unbounded: the hard cap still terminates).
fn parse_loop_state_hashes(controller_summary: Option<&str>) -> Vec<u64> {
    let Some(text) = controller_summary else {
        return vec![];
    };
    let Some(line) = text
        .lines()
        .find(|l| l.trim_start().starts_with(LOOP_STATE_PREFIX))
    else {
        return vec![];
    };
    let after = line
        .trim_start()
        .trim_start_matches(LOOP_STATE_PREFIX)
        .trim();
    if after.is_empty() {
        return vec![];
    }
    after
        .split(',')
        .filter_map(|s| s.trim().parse::<u64>().ok())
        .collect()
}

/// Render the controller's iterating-state `output_summary`: just the machine
/// `LOOP-STATE:` line carrying the running round-hash history. Overwritten each
/// CONTINUE settle; replaced wholesale by [`render_loop_final`] on STOP.
fn render_loop_state(hashes: &[u64]) -> String {
    let csv = hashes
        .iter()
        .map(|h| h.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("{LOOP_STATE_PREFIX}{csv}")
}

/// Build the body's NEXT-round `pattern_config` on CONTINUE: MERGE
/// [`LOOP_PRIOR_OUTPUT_KEY`] = the round-just-finished output + [`LOOP_ITERATION_KEY`]
/// = the next (1-based) iteration into the body's EXISTING pattern_config object
/// (preserving any prior keys, e.g. a fan-out `group` tag), so the body's next
/// brief refines the prior round (see [`compose_agent_brief`]). When the prior
/// output is blank there is nothing useful to carry → returns `None` (the body
/// re-runs with a fresh brief, as the FIRST iteration does). The existing config
/// is parsed fail-soft: a non-object / unparseable config is replaced by a fresh
/// object carrying only the two loop fields (never errors).
fn build_body_loop_carry(
    existing_pattern_config: Option<&str>,
    prior_output: Option<&str>,
    next_iteration: u64,
) -> Option<String> {
    let prior = prior_output.map(str::trim).filter(|s| !s.is_empty())?;
    // Start from the existing config object (preserve its keys) or a fresh object.
    let mut obj = existing_pattern_config
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    obj.insert(
        LOOP_PRIOR_OUTPUT_KEY.to_string(),
        serde_json::Value::String(prior.to_string()),
    );
    obj.insert(
        LOOP_ITERATION_KEY.to_string(),
        serde_json::Value::Number(next_iteration.into()),
    );
    // serde_json::to_string on a Map never fails for these value kinds; fall back
    // to None on the impossible error (no unwrap in prod).
    serde_json::to_string(&serde_json::Value::Object(obj)).ok()
}

/// Render the controller's FINAL `output_summary` on STOP — a machine-leading
/// marker (`LOOP: STOPPED (reason=..., iterations=N, max_iter=M)`) followed by the
/// body's last output (the loop's result), so both a downstream consumer and the
/// UI can parse the outcome + read the final result. `outcome` is `done` or
/// `failed` (a failing body stops the loop as failed).
fn render_loop_final(
    outcome: &str,
    reason: &str,
    iterations: u64,
    max_iter: u64,
    body_output: Option<&str>,
) -> String {
    let mut out = format!(
        "LOOP: {} (reason={reason}, iterations={iterations}, max_iter={max_iter})\n",
        outcome.to_ascii_uppercase()
    );
    if let Some(body) = body_output.map(str::trim).filter(|s| !s.is_empty()) {
        out.push('\n');
        out.push_str(body);
        out.push('\n');
    }
    out
}

/// Settle a `loop` controller task SYNCHRONOUSLY (no worker dispatch). Invoked in
/// the fill step when EITHER the controller is ready (its body dep is `done`) OR
/// its body dep is `failed` (the dedicated failed-body branch). Resolves the
/// body, then:
///   - body `failed` → STOP the loop `failed`, write the failed marker, GATE
///     (skip) downstream so a failing body never iterates;
///   - body `done` → [`decide_loop`]:
///     - STOP → mark the controller `done`, write the final marker + the body's
///       last output;
///     - CONTINUE → RESET the body (`pending`, clear output_summary +
///       conversation_id, `attempt`+1) and persist the updated round-hash history
///       to the controller's `output_summary` (controller stays `pending`).
///
/// Bounded + no spin: a CONTINUE un-`done`s the body, so the controller leaves
/// the ready set until the body re-completes (a real worker round); the body's
/// `attempt` is strictly bounded by `max_iter` (the hard cap is checked FIRST in
/// `decide_loop`). The controller transitions at most once per body completion.
async fn settle_loop_task(deps: &Arc<RunEngineDeps>, run_id: &str, task: &OrchRunTaskRow) {
    // Resolve the loop's body dependency. A well-formed loop has exactly ONE
    // blocker (the body); if the planner emitted more, we use the first blocker
    // by task order (deterministic) and ignore the rest — fail-soft.
    let dep_edges = deps.run_repo.list_deps(run_id).await.unwrap_or_default();
    let blocker_ids: HashSet<String> = dep_edges
        .iter()
        .filter(|d| d.blocked_task_id == task.id)
        .map(|d| d.blocker_task_id.clone())
        .collect();
    let all_tasks = deps.run_repo.list_tasks(run_id).await.unwrap_or_default();
    let body = all_tasks
        .iter()
        .filter(|t| blocker_ids.contains(&t.id))
        .min_by_key(|t| t.created_at)
        .cloned();

    let config = LoopConfig::parse(task.pattern_config.as_deref());

    let Some(body) = body else {
        // No body dependency at all → nothing to iterate. Settle `done` with a
        // degenerate marker (never spin / never wait forever).
        let summary = render_loop_final("done", "no_body", 0, config.max_iter, None);
        finish_loop_controller(deps, run_id, &task.id, "done", summary).await;
        return;
    };

    // FAILED body → stop the loop failed + gate downstream (never iterate a
    // failing body). Reached via the fill step's failed-body branch.
    if body.status == "failed" {
        let iterations = body.attempt.max(0) as u64 + 1; // rounds attempted
        let summary = render_loop_final(
            "failed",
            "body_failed",
            iterations,
            config.max_iter,
            body.output_summary.as_deref(),
        );
        finish_loop_controller(deps, run_id, &task.id, "failed", summary).await;
        // Gate downstream: a failing loop must not let its consumers run.
        skip_downstream(deps, run_id, &task.id, &dep_edges).await;
        info!(run_id, task_id = %task.id, "loop controller stopped: body failed");
        return;
    }

    // DONE body → evaluate the stop decision over this completed round.
    // body.attempt is the 0-based round index just completed → iterations_done is
    // attempt + 1.
    let iterations_done = body.attempt.max(0) as u64 + 1;
    let prior_hashes = parse_loop_state_hashes(task.output_summary.as_deref());
    let decision = decide_loop(
        &config,
        iterations_done,
        body.output_summary.as_deref(),
        &prior_hashes,
    );

    match decision {
        LoopDecision::Stop { reason } => {
            let summary = render_loop_final(
                "done",
                reason,
                iterations_done,
                config.max_iter,
                body.output_summary.as_deref(),
            );
            finish_loop_controller(deps, run_id, &task.id, "done", summary).await;
            info!(
                run_id,
                task_id = %task.id,
                reason,
                iterations = iterations_done,
                max_iter = config.max_iter,
                "loop controller stopped"
            );
        }
        LoopDecision::Continue => {
            // Record this round's hash in the controller's running history so the
            // next settle can evaluate `dry`. Then RESET the body in place.
            let mut hashes = prior_hashes;
            hashes.push(round_hash(body.output_summary.as_deref()));
            let state_summary = render_loop_state(&hashes);
            // Persist the controller's history (controller STAYS pending — no
            // status change; its blocker is about to become un-done).
            let _ = deps
                .run_repo
                .update_task(
                    &task.id,
                    UpdateTaskParams {
                        last_error: None,
                        status: None,
                        spec: None,
                        conversation_id: None,
                        output_summary: Some(Some(state_summary)),
                        output_files: None,
                        attempt: None,
                        tokens: None,
                        graph_x: None,
                        graph_y: None,
                        pattern_config: None,
                        next_retry_at: None,
                    },
                )
                .await;
            // loop 迭代回看 (评审 Important): carry the round-just-finished output
            // forward into the body's `pattern_config` so its NEXT brief refines it
            // (the loop controller is DOWNSTREAM of the body, so the body never
            // sees it via `upstream` — this is the only channel). The next 1-based
            // iteration is `body.attempt + 2` (the round about to run). A blank
            // prior output → no carry (the body re-runs fresh, like iteration 0).
            // `Some(None)` clears the body's prior carry when there is nothing to
            // forward, so a stale carry never leaks into a fresh round.
            let body_carry = build_body_loop_carry(
                body.pattern_config.as_deref(),
                body.output_summary.as_deref(),
                (body.attempt.max(0) as u64) + 2,
            );
            // RESET the body: pending + clear output_summary/conversation_id +
            // attempt+1, and set the prior-output carry on its pattern_config. This
            // un-`done`s the controller's only blocker, so the controller leaves the
            // ready set until the body re-completes (a real worker round) — the
            // monotonic progress that bounds the loop.
            let _ = deps
                .run_repo
                .update_task(
                    &body.id,
                    UpdateTaskParams {
                        last_error: None,
                        status: Some("pending".to_string()),
                        spec: None,
                        conversation_id: Some(None), // clear the prior round's conv
                        output_summary: Some(None),  // clear the prior round's output
                        output_files: Some(None),
                        attempt: Some(body.attempt + 1),
                        tokens: None,
                        graph_x: None,
                        graph_y: None,
                        // Carry the prior round's output forward (or clear a stale
                        // carry when there is nothing to forward).
                        pattern_config: Some(body_carry),
                        next_retry_at: Some(None),
                    },
                )
                .await;
            deps.emitter.emit_task_status(run_id, &body.id, "pending");
            info!(
                run_id,
                task_id = %task.id,
                body_id = %body.id,
                next_attempt = body.attempt + 1,
                iterations_done,
                max_iter = config.max_iter,
                "loop controller continues: body reset for another round"
            );
        }
    }
}

/// Mark a `loop` controller terminal (`done`/`failed`) with the given final
/// `output_summary`. The controller has NO worker conversation (conversation_id
/// stays None). Shared by every STOP path in [`settle_loop_task`].
async fn finish_loop_controller(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    task_id: &str,
    status: &str,
    summary: String,
) {
    let _ = deps
        .run_repo
        .update_task(
            task_id,
            UpdateTaskParams {
                last_error: None,
                status: Some(status.to_string()),
                spec: None,
                conversation_id: None,
                output_summary: Some(Some(summary)),
                output_files: None,
                attempt: None,
                tokens: None,
                graph_x: None,
                graph_y: None,
                pattern_config: None,
                next_retry_at: None,
            },
        )
        .await;
    deps.emitter.emit_task_status(run_id, task_id, status);
}

/// Mark every task transitively downstream of `from_task_id` (its dependents,
/// their dependents, …) as `skipped`. Used by the verify gate on a FAIL verdict
/// so unvalidated work does not run.
///
/// Bounded: a finite BFS over the (acyclic) dep edges, visiting each task at most
/// once (`seen` guard). Only `pending` tasks are skipped (a `running`/`done`
/// task is left alone — downstream of a verify cannot be `running`/`done` yet,
/// since the verify only just settled, but the guard keeps this defensive).
async fn skip_downstream(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    from_task_id: &str,
    dep_edges: &[nomifun_db::models::OrchRunTaskDepRow],
) {
    // Adjacency: blocker → [blocked] (downstream successors).
    let mut frontier: Vec<String> = dep_edges
        .iter()
        .filter(|d| d.blocker_task_id == from_task_id)
        .map(|d| d.blocked_task_id.clone())
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(tid) = frontier.pop() {
        if !seen.insert(tid.clone()) {
            continue;
        }
        // Skip only a still-pending task (do not clobber a terminal/running one).
        if let Ok(Some(t)) = deps.run_repo.get_task(&tid).await {
            if t.status == "pending" {
                update_task_status(deps, &tid, "skipped").await;
                deps.emitter.emit_task_status(run_id, &tid, "skipped");
            }
        }
        // Enqueue this task's own dependents (transitive gate).
        for d in dep_edges.iter().filter(|d| d.blocker_task_id == tid) {
            frontier.push(d.blocked_task_id.clone());
        }
    }
}

async fn update_task_status(deps: &Arc<RunEngineDeps>, task_id: &str, status: &str) {
    let _ = deps
        .run_repo
        .update_task(
            task_id,
            UpdateTaskParams {
                last_error: None,
                status: Some(status.to_string()),
                spec: None,
                conversation_id: None,
                output_summary: None,
                output_files: None,
                attempt: None,
                tokens: None,
                graph_x: None,
                graph_y: None,
                pattern_config: None,
                next_retry_at: None,
            },
        )
        .await;
}

async fn mark_task_failed(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    task_id: &str,
    conversation_id: Option<i64>,
    tokens: Option<i64>,
) {
    // Lift a failure reason from the worker conversation's latest error marker
    // (best-effort) and PERSIST it, so the lead-report / escalation / diagnostic
    // tools can show WHY a node failed without re-reading the conversation. When no
    // diagnostic marker exists (e.g. a plain timeout / empty reply), record a
    // generic reason so escalation is never left with an empty "why".
    let reason = match conversation_id {
        Some(c) => deps.worker.last_error_summary(&c.to_string()).await,
        None => None,
    }
    .unwrap_or_else(|| "worker 未产出结果（超时或无回复），无诊断错误标记".to_string());
    let _ = deps
        .run_repo
        .update_task(
            task_id,
            UpdateTaskParams {
                status: Some("failed".to_string()),
                spec: None,
                conversation_id: conversation_id.map(Some),
                output_summary: None,
                output_files: None,
                attempt: None,
                // Record any token usage observed before the failure (a timed-out
                // multi-turn run may have completed earlier turns). `None` keeps
                // the prior behaviour when nothing was observed. Never fabricated.
                tokens: tokens.map(Some),
                graph_x: None,
                graph_y: None,
                pattern_config: None,
                next_retry_at: None,
                last_error: Some(Some(reason)),
            },
        )
        .await;
    deps.emitter.emit_task_status(run_id, task_id, "failed");
}

/// A *soft* failure: the task permanently `failed` but its per-node `on_fail`
/// policy is `skip_and_continue` (迁移 029), so its transitive downstream was
/// `skipped` and the RUN still settles — its terminal status becomes
/// `completed_with_failures` rather than `failed`. `None`/`"fail_run"` is a HARD
/// failure (default, current behaviour) → the whole run fails.
fn is_soft_fail(t: &OrchRunTaskRow) -> bool {
    t.status == "failed" && t.on_fail.as_deref() == Some("skip_and_continue")
}

/// Settle the run row to a terminal status (with an optional summary) and emit
/// `run.completed`. Best-effort: a persistence error is logged, not propagated
/// (the loop is exiting regardless).
///
/// `total_tokens` is the run's aggregate token usage (sum of the tasks' real
/// per-task `tokens`); `None` leaves the run total untouched (no observed
/// usage). The DAG/inspector run header reads `Run.total_tokens` — already
/// plumbed; this writes the real sum.
async fn finish_run(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    status: &str,
    summary: Option<String>,
    total_tokens: Option<i64>,
) {
    if let Err(e) = deps
        .run_repo
        .update_run(
            run_id,
            UpdateRunParams {
                status: Some(status.to_string()),
                summary: summary.map(Some),
                lead_conv_id: None,
                total_tokens: total_tokens.map(Some),
                goal: None,
                autonomy: None,
                fleet_snapshot: None,
                work_dir: None,
            },
        )
        .await
    {
        warn!(run_id, status, error = %e, "Run loop: failed to persist terminal run status");
    }
    deps.emitter.emit_run_status(run_id, status);
    deps.emitter.emit_run_completed(run_id, status);
    info!(run_id, status, "Run finished");
}

/// Sum the real per-task `tokens` across a run's tasks for the run-level total.
/// Returns `None` when NO task recorded any usage (so the run total stays
/// untouched rather than being written as a fabricated `0`); otherwise the
/// saturating sum of every present per-task count. Aggregator tasks
/// (verify/judge/loop) carry no `tokens`, so they contribute nothing.
fn sum_task_tokens(tasks: &[OrchRunTaskRow]) -> Option<i64> {
    let mut total: i64 = 0;
    let mut any = false;
    for t in tasks {
        if let Some(n) = t.tokens {
            total = total.saturating_add(n);
            any = true;
        }
    }
    any.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::OrchestratorRunEventEmitter;
    use crate::plan::PlanProducer;
    use crate::run_service::RunService;
    use crate::worker::MockWorkerRunner;

    use async_trait::async_trait;
    use nomifun_api_types::{
        CapabilityProfile, CreateAdhocRunRequest, CreateFleetRequest, CreateRunRequest,
        CreateWorkspaceRequest, FleetMember, FleetMemberInput, ModelRange, ModelRef, PlannedDag,
        PlannedTask,
    };
    use nomifun_common::AppError;
    use nomifun_db::{
        SqliteFleetRepository, SqliteOrchWorkspaceRepository, SqliteRunRepository,
        init_database_memory,
    };
    use nomifun_realtime::EventBroadcaster;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Capturing broadcaster so engine tests can assert the realtime event trail.
    struct RecordingBroadcaster {
        events: Mutex<Vec<nomifun_api_types::WebSocketMessage<serde_json::Value>>>,
    }
    impl RecordingBroadcaster {
        fn new() -> Self {
            Self {
                events: Mutex::new(vec![]),
            }
        }
        fn names(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|e| e.name.clone())
                .collect()
        }
    }
    impl EventBroadcaster for RecordingBroadcaster {
        fn broadcast(&self, event: nomifun_api_types::WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(event);
        }
    }

    /// A→B→C chain DAG: task0 (no dep), task1 (depends on 0), task2 (depends on 1).
    /// Each task pre-assigned to member 0 so a single-member fleet suffices.
    struct ChainPlanProducer;
    #[async_trait]
    impl PlanProducer for ChainPlanProducer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<PlannedDag, AppError> {
            Ok(PlannedDag {
                tasks: vec![
                    PlannedTask {
                        title: "A".to_string(),
                        spec: "do A".to_string(),
                        task_profile: None,
                        depends_on: vec![],
                        member_index: Some(0),
                        rationale: Some("first".to_string()),
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    },
                    PlannedTask {
                        title: "B".to_string(),
                        spec: "do B".to_string(),
                        task_profile: None,
                        depends_on: vec![0],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    },
                    PlannedTask {
                        title: "C".to_string(),
                        spec: "do C".to_string(),
                        task_profile: None,
                        depends_on: vec![1],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    },
                ],
            })
        }
    }

    struct Harness {
        run_service: RunService,
        engine: RunEngine,
        #[allow(dead_code)]
        run_repo: Arc<SqliteRunRepository>,
        fleet_repo: Arc<SqliteFleetRepository>,
        ws_repo: Arc<SqliteOrchWorkspaceRepository>,
        broadcaster: Arc<RecordingBroadcaster>,
    }

    /// Build the full mock stack over a shared in-memory DB: run/fleet/workspace
    /// repos, a chain PlanProducer, and a fixed-text MockWorkerRunner. Returns the
    /// wired RunService + RunEngine, the run repo (for direct assertions), and the
    /// recording broadcaster.
    async fn harness() -> Harness {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let emitter = OrchestratorRunEventEmitter::new(broadcaster.clone());
        let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);

        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );

        let worker: Arc<dyn WorkerRunner> =
            Arc::new(MockWorkerRunner::with_text(777, "task output"));
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(5);
        let engine = RunEngine::new(Arc::new(engine_deps));

        Harness {
            run_service,
            engine,
            run_repo,
            fleet_repo,
            ws_repo,
            broadcaster,
        }
    }

    fn sample_member(agent_id: &str) -> FleetMemberInput {
        FleetMemberInput {
            agent_id: agent_id.to_string(),
            provider_id: Some("prov_x".to_string()),
            model: Some("claude-opus-4-8".to_string()),
            role_hint: Some("researcher".to_string()),
            capability_profile: Some(CapabilityProfile {
                strengths: vec!["coding".to_string()],
                modalities: vec!["text".to_string()],
                tools: true,
                reasoning: "high".to_string(),
                cost_tier: "premium".to_string(),
                speed_tier: "medium".to_string(),
            }),
            constraints: None,
            sort_order: None,
        }
    }

    /// Create a workspace + a single-member fleet, then a run against them.
    /// Returns the run id.
    async fn seed_run(h: &Harness) -> String {
        // Need the fleet + workspace persisted via their repos. The RunService
        // create() snapshots the fleet, so create the fleet first.
        let fleet = crate::service::FleetService::new(h.fleet_repo.clone())
            .create(
                "u1",
                CreateFleetRequest {
                    name: "chain fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet create");
        let ws = crate::service::WorkspaceService::new(h.ws_repo.clone())
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "chain ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws create");
        let run = h
            .run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "build the chain".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: None,
                },
            )
            .await
            .expect("run create");
        run.id
    }

    #[tokio::test]
    async fn full_run_executes_chain_in_dependency_order_to_completion() {
        let h = harness().await;
        let run_id = seed_run(&h).await;

        // After create: status planning.
        let detail = h.run_service.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.run.status, "planning", "fresh run is planning");
        assert!(detail.tasks.is_empty(), "no tasks before plan");

        // Plan: 3 tasks, 2 deps, 3 assignments, status running.
        h.run_service.plan(&run_id).await.expect("plan");
        let detail = h.run_service.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.run.status, "running", "planned run is running");
        assert_eq!(detail.tasks.len(), 3, "3 tasks persisted");
        assert_eq!(detail.deps.len(), 2, "2 dep edges persisted (A→B, B→C)");
        assert_eq!(detail.assignments.len(), 3, "3 assignments persisted");
        for a in &detail.assignments {
            assert_eq!(a.source, "auto");
            assert!(!a.locked);
        }
        // The dep edges connect the tasks in chain order.
        let title_of = |id: &str| {
            detail
                .tasks
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.title.clone())
                .unwrap_or_default()
        };
        for d in &detail.deps {
            let (b, k) = (title_of(&d.blocker_task_id), title_of(&d.blocked_task_id));
            assert!(
                (b == "A" && k == "B") || (b == "B" && k == "C"),
                "edge must be A→B or B→C, got {b}→{k}"
            );
        }

        // Run the engine; poll get_detail until completed (bounded ~50×50ms).
        h.engine.start(run_id.clone());
        let mut completed = false;
        for _ in 0..50 {
            let d = h.run_service.get_detail(&run_id).await.expect("detail");
            if d.run.status == "completed" {
                completed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            completed,
            "run must reach completed within the bounded poll"
        );

        let detail = h.run_service.get_detail(&run_id).await.expect("detail");
        // All tasks done, each with the worker's output summary.
        for t in &detail.tasks {
            assert_eq!(t.status, "done", "task {} should be done", t.title);
            assert_eq!(
                t.output_summary.as_deref(),
                Some("task output"),
                "task {} output_summary should be set",
                t.title
            );
            assert_eq!(
                t.conversation_id,
                Some(777),
                "worker conversation id recorded"
            );
        }
        // Run summary non-empty.
        let summary = detail.run.summary.expect("run summary set on completion");
        assert!(!summary.trim().is_empty(), "run summary must be non-empty");
        assert!(
            summary.contains("3/3"),
            "summary reflects 3/3 done, got: {summary}"
        );

        // The realtime trail includes a run.completed event.
        let names = h.broadcaster.names();
        assert!(
            names.iter().any(|n| n == "orchestrator.run.completed"),
            "run.completed must be emitted; got {names:?}"
        );
        assert!(
            names
                .iter()
                .filter(|n| *n == "orchestrator.task.statusChanged")
                .count()
                >= 6,
            "each task emits running+done (≥6 task status events); got {names:?}"
        );

        // The loop must have exited (not still registered). The guard drop that
        // deregisters the handle runs just after the loop returns, which can lag
        // the persisted `completed` status the poll observed — give it a bounded
        // moment to unwind rather than asserting on a race.
        let mut deregistered = false;
        for _ in 0..20 {
            if !h.engine.is_running(&run_id) {
                deregistered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            deregistered,
            "engine loop should deregister after the run completes"
        );
    }

    // ── Lead 回执（LeadReporter）：run 到终态时 exactly-once 回执给发起会话 ────────

    /// Records every lead report so a test can assert the terminal report fires
    /// EXACTLY once with the right (lead_conv_id, run_id, outcome).
    #[derive(Default)]
    struct RecordingLeadReporter {
        reports: Arc<Mutex<Vec<(i64, String, String)>>>,
    }
    #[async_trait]
    impl LeadReporter for RecordingLeadReporter {
        async fn report(
            &self,
            lead_conv_id: i64,
            run_id: &str,
            outcome: RunOutcome,
            _brief: &str,
        ) -> Result<(), AppError> {
            self.reports.lock().unwrap().push((
                lead_conv_id,
                run_id.to_string(),
                outcome.as_str().to_string(),
            ));
            Ok(())
        }
    }

    /// A worker that always fails (ok:false, no error marker) — with
    /// `max_worker_retries = 0` the task fails immediately → the run fails.
    struct AlwaysFailWorker;
    #[async_trait]
    impl WorkerRunner for AlwaysFailWorker {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            _task_id: &str,
            _brief: &str,
            _task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            on_started(778);
            Ok(WorkerOutcome {
                conversation_id: 778,
                text: None,
                ok: false,
                tokens: None,
            })
        }
        // Represents a DEFINITE failure (a marked provider error): a marker IS
        // present but `last_error_retryable` defaults false → NonRetryable, so the
        // node fails immediately (attempt stays 0) — NOT retried under the NoMarker
        // timeout budget. Preserves the pre-Task-1 dispatch counts / assertions for
        // `failed_run_reports_once_to_lead` and `failed_task_persists_last_error`.
        async fn last_error_present(&self, _conversation_id: &str) -> bool {
            true
        }
    }

    /// Build an engine over a shared in-memory DB wired with a RecordingLeadReporter
    /// and the given worker + retry budget; returns (run_service, engine, reporter).
    async fn reporter_stack(
        worker: Arc<dyn WorkerRunner>,
        max_retries: usize,
    ) -> (RunService, RunEngine, Arc<RecordingLeadReporter>) {
        let (svc, engine, reporter, _repo, _pool) = reporter_stack_full(worker, max_retries).await;
        (svc, engine, reporter)
    }

    /// [`reporter_stack`] 的全量版：额外返回 run_repo + pool，供需要直接操作行
    /// （模拟 nomi_task_question 写入 / 老化 updated_at）的审批模式测试使用。
    async fn reporter_stack_full(
        worker: Arc<dyn WorkerRunner>,
        max_retries: usize,
    ) -> (
        RunService,
        RunEngine,
        Arc<RecordingLeadReporter>,
        Arc<SqliteRunRepository>,
        sqlx::SqlitePool,
    ) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo,
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let reporter = Arc::new(RecordingLeadReporter::default());
        let mut engine_deps = RunEngineDeps::new(run_repo.clone(), worker, emitter, ws_repo);
        engine_deps.worker_timeout = Duration::from_secs(5);
        engine_deps.max_worker_retries = max_retries;
        engine_deps.retry_backoff_base = Duration::ZERO;
        engine_deps.lead_reporter = reporter.clone();
        let engine = RunEngine::new(Arc::new(engine_deps));
        (run_service, engine, reporter, run_repo, pool)
    }

    /// Create an ad-hoc run BOUND to a lead conversation (so the terminal report has
    /// a target), plan it, and return the run id. `autonomous` → runs without a gate.
    async fn adhoc_lead_run(run_service: &RunService, lead_conv_id: i64) -> String {
        let run = run_service
            .create_adhoc(
                "u1",
                CreateAdhocRunRequest {
                    approval_mode: None,
                    goal: "chain".to_string(),
                    work_dir: Some("/tmp/lead-report".to_string()),
                    model_range: ModelRange::Single {
                        model: ModelRef {
                            provider_id: "prov_x".to_string(),
                            model: "claude-opus-4-8".to_string(),
                        },
                    },
                    pinned_roles: vec![],
                    role_members: vec![],
                    autonomy: Some("autonomous".to_string()),
                    max_parallel: None,
                    lead_conv_id: Some(lead_conv_id),
                    lead_model: None,
                },
            )
            .await
            .expect("create_adhoc");
        run_service.plan(&run.id).await.expect("plan");
        run.id
    }

    async fn wait_run_status(run_service: &RunService, run_id: &str, want: &str) -> bool {
        for _ in 0..100 {
            if let Ok(d) = run_service.get_detail(run_id).await {
                if d.run.status == want {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    async fn await_first_report(reporter: &RecordingLeadReporter) {
        for _ in 0..40 {
            if !reporter.reports.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// A COMPLETED run reports its TERMINAL `Completed` receipt exactly once to its
    /// lead conv. (Task 3 adds a mid-run `batch_progress` receipt BEFORE the terminal
    /// one when ≥3 nodes complete — the 3-node chain does — so this asserts on the
    /// count of `completed` receipts, not the total report count.)
    #[tokio::test]
    async fn completed_run_reports_once_to_lead() {
        let worker: Arc<dyn WorkerRunner> =
            Arc::new(MockWorkerRunner::with_text(777, "task output"));
        let (run_service, engine, reporter) = reporter_stack(worker, 3).await;
        let run_id = adhoc_lead_run(&run_service, 909).await;
        engine.start(run_id.clone());
        assert!(
            wait_run_status(&run_service, &run_id, "completed").await,
            "run must complete"
        );
        // The terminal `completed` receipt fires AFTER the loop exits, so it can lag
        // the persisted status the poll above observed (and any mid-run
        // `batch_progress` receipt) — wait for it specifically rather than for the
        // first report.
        let mut saw_completed = false;
        for _ in 0..40 {
            if reporter
                .reports
                .lock()
                .unwrap()
                .iter()
                .any(|(_, _, o)| o == "completed")
            {
                saw_completed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let reports = reporter.reports.lock().unwrap().clone();
        assert!(
            saw_completed,
            "terminal completed receipt must fire, got {reports:?}"
        );
        // Exactly ONE terminal `completed` receipt (a mid-run `batch_progress` is
        // correct additional behaviour, not a duplicate terminal report).
        let completed: Vec<_> = reports.iter().filter(|(_, _, o)| o == "completed").collect();
        assert_eq!(
            completed.len(),
            1,
            "exactly one terminal `completed` lead report, got {reports:?}"
        );
        assert_eq!(completed[0].0, 909, "reported to the lead conversation");
    }

    /// A FAILED run reports its TERMINAL `Failed` receipt exactly once to its lead
    /// conv. (Task 3 adds a mid-run `node_failed` receipt BEFORE the terminal one, so
    /// this asserts on the count of `failed` receipts, not the total report count.)
    #[tokio::test]
    async fn failed_run_reports_once_to_lead() {
        let worker: Arc<dyn WorkerRunner> = Arc::new(AlwaysFailWorker);
        // max_retries = 0 → the first failure is terminal (no retry-backoff wait).
        let (run_service, engine, reporter) = reporter_stack(worker, 0).await;
        let run_id = adhoc_lead_run(&run_service, 42).await;
        engine.start(run_id.clone());
        assert!(
            wait_run_status(&run_service, &run_id, "failed").await,
            "run must fail"
        );
        // The terminal `failed` receipt fires AFTER the loop exits, so it can lag the
        // persisted status the poll above observed (and the mid-run `node_failed`
        // receipt) — wait for it specifically rather than for the first report.
        let mut saw_failed = false;
        for _ in 0..40 {
            if reporter
                .reports
                .lock()
                .unwrap()
                .iter()
                .any(|(_, _, o)| o == "failed")
            {
                saw_failed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let reports = reporter.reports.lock().unwrap().clone();
        assert!(saw_failed, "terminal failed receipt must fire, got {reports:?}");
        // Exactly ONE terminal `failed` receipt (the mid-run `node_failed` is correct
        // additional behaviour, not a duplicate terminal report).
        let failed: Vec<_> = reports.iter().filter(|(_, _, o)| o == "failed").collect();
        assert_eq!(
            failed.len(),
            1,
            "exactly one terminal `failed` lead report, got {reports:?}"
        );
        assert_eq!(failed[0].0, 42, "reported to the lead conversation");
    }

    /// A node that PERMANENTLY fails mid-run re-engages the lead conversation with a
    /// best-effort `node_failed` receipt BEFORE the run reaches its terminal state,
    /// so the master agent can repair (rerun w/ another model, adjust the graph,
    /// adopt, or abandon) instead of waiting for the whole run to end. `AlwaysFail`
    /// worker + `max_worker_retries = 0` → the first attempt is terminal for the
    /// node; the mid-run report fires from `settle_failed_or_retry`'s permanent-fail
    /// branch, after `mark_task_failed` persisted the reason.
    #[tokio::test]
    async fn permanent_node_failure_reports_to_lead_midrun() {
        let worker: Arc<dyn WorkerRunner> = Arc::new(AlwaysFailWorker);
        // max_retries = 0 → the first failure is terminal for the node (no retry wait).
        let (run_service, engine, reporter) = reporter_stack(worker, 0).await;
        let run_id = adhoc_lead_run(&run_service, 55).await;
        engine.start(run_id.clone());
        assert!(
            wait_run_status(&run_service, &run_id, "failed").await,
            "run must fail"
        );
        // The mid-run receipt is pushed during settle (before the terminal status
        // write the poll above observed), so it is already present here — but poll a
        // bounded moment to stay robust against scheduling.
        let mut saw_node_failed = false;
        for _ in 0..40 {
            if reporter
                .reports
                .lock()
                .unwrap()
                .iter()
                .any(|(_, _, o)| o == "node_failed")
            {
                saw_node_failed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let reports = reporter.reports.lock().unwrap().clone();
        assert!(
            saw_node_failed,
            "a permanent node failure must produce a mid-run node_failed lead report, got {reports:?}"
        );
        // The mid-run receipt targets the lead conversation.
        let node_failed = reports
            .iter()
            .find(|(_, _, o)| o == "node_failed")
            .expect("node_failed report present");
        assert_eq!(
            node_failed.0, 55,
            "mid-run node_failed reported to the lead conversation"
        );
    }

    /// A run that completes ≥3 nodes re-engages the lead with a THROTTLED, best-effort
    /// `batch_progress` receipt BEFORE the terminal receipt, so the master agent can
    /// OBSERVE progress and append more work (`nomi_run_add_tasks`) to the SAME run.
    /// 需求4：BATCH_REPORT_MIN_NODES=1 —— 第 1 个节点完成、且首个 interval 门开放
    /// （无前次回执）时即触发一次 `batch_progress`，先于终态 `completed` 回执。
    #[tokio::test]
    async fn batch_progress_reports_to_lead_midrun() {
        let worker: Arc<dyn WorkerRunner> =
            Arc::new(MockWorkerRunner::with_text(777, "task output"));
        let (run_service, engine, reporter) = reporter_stack(worker, 3).await;
        let run_id = adhoc_lead_run(&run_service, 321).await;
        engine.start(run_id.clone());
        assert!(
            wait_run_status(&run_service, &run_id, "completed").await,
            "run must complete"
        );
        // The mid-run batch_progress receipt is pushed during settle (before the
        // terminal completed report), but poll a bounded moment to stay robust.
        await_first_report(&reporter).await;
        let mut saw_batch = false;
        for _ in 0..40 {
            if reporter
                .reports
                .lock()
                .unwrap()
                .iter()
                .any(|(_, _, o)| o == "batch_progress")
            {
                saw_batch = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let reports = reporter.reports.lock().unwrap().clone();
        assert!(
            saw_batch,
            "≥3 node completions must produce a mid-run batch_progress lead report, got {reports:?}"
        );
        // The mid-run batch_progress targets the lead conversation and PRECEDES the
        // terminal `completed` receipt.
        let batch_pos = reports
            .iter()
            .position(|(_, _, o)| o == "batch_progress")
            .expect("batch_progress report present");
        assert_eq!(
            reports[batch_pos].0, 321,
            "mid-run batch_progress reported to the lead conversation"
        );
        if let Some(done_pos) = reports.iter().position(|(_, _, o)| o == "completed") {
            assert!(
                batch_pos < done_pos,
                "batch_progress must precede the terminal completed receipt, got {reports:?}"
            );
        }
    }

    // ── 审批模式（迁移 030）：节点提问 park / 看门狗豁免 / adopt 恢复 ────────────

    /// 模拟审批模式 worker：第一个被派发的任务在自己的回合中途「调用
    /// nomi_task_question」（直写 repo：pending_question + needs_review，与网关工具
    /// 的锁内写等效），然后正常结束回合（ok:true）；后续任务正常产出。
    /// `read_final_output` 返回定稿文本，供「采用为该节点产出」读取。
    struct QuestionWorker {
        repo: Arc<SqliteRunRepository>,
        asked: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl WorkerRunner for QuestionWorker {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            task_id: &str,
            _brief: &str,
            _task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            on_started(881);
            let first = {
                let mut a = self.asked.lock().unwrap();
                if *a {
                    false
                } else {
                    *a = true;
                    true
                }
            };
            if first {
                self.repo
                    .set_task_question(task_id, Some("定价方案选 A（订阅）还是 B（买断）？"))
                    .await
                    .expect("set question");
                self.repo
                    .update_task(
                        task_id,
                        UpdateTaskParams {
                            status: Some("needs_review".to_string()),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("park needs_review");
                return Ok(WorkerOutcome {
                    conversation_id: 881,
                    text: Some("我已提交决策问题，等待用户选择。".to_string()),
                    ok: true,
                    tokens: None,
                });
            }
            Ok(WorkerOutcome {
                conversation_id: 882,
                text: Some("后续节点产出".to_string()),
                ok: true,
                tokens: None,
            })
        }
        async fn read_final_output(&self, _conversation_id: &str) -> Option<String> {
            Some("用户拍板选 A：按订阅制完成定价方案定稿。".to_string())
        }
    }

    /// 审批模式全链路：节点提问 park（settle 不覆盖 needs_review、补写产出文本、
    /// run 保持 running、循环干净退出）→ 看门狗豁免（老化后 reap 不判 stalled）→
    /// 「采用为该节点产出」恢复（问题清空、节点 done、重启循环后整 run completed）。
    #[tokio::test]
    async fn needs_review_parks_survives_watchdog_and_resumes_via_adopt() {
        let asked = Arc::new(Mutex::new(false));
        let (run_service, engine, reporter, repo, pool) = {
            // 两段构造：QuestionWorker 需要 repo，而 repo 由 stack 构造——先建 stack
            // 拿 repo，再热插 worker 不可行（deps 不可变），故手工复刻 stack 并共享 repo。
            let db = init_database_memory().await.expect("db init");
            let pool = db.pool().clone();
            let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
            let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
            let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
            let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
            let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);
            let run_service = RunService::new(
                run_repo.clone(),
                fleet_repo,
                ws_repo.clone(),
                planner,
                emitter.clone(),
            );
            let worker: Arc<dyn WorkerRunner> = Arc::new(QuestionWorker {
                repo: run_repo.clone(),
                asked: asked.clone(),
            });
            let reporter = Arc::new(RecordingLeadReporter::default());
            let mut engine_deps =
                RunEngineDeps::new(run_repo.clone(), worker, emitter, ws_repo);
            engine_deps.worker_timeout = Duration::from_secs(5);
            engine_deps.max_worker_retries = 0;
            engine_deps.retry_backoff_base = Duration::ZERO;
            engine_deps.lead_reporter = reporter.clone();
            let engine = RunEngine::new(Arc::new(engine_deps));
            (run_service, engine, reporter, run_repo, pool)
        };
        let run_id = adhoc_lead_run(&run_service, 606).await;
        engine.start(run_id.clone());

        // ① park：循环退出（is_running=false）、run 仍 running、首节点 needs_review
        //    且 settle 只补写了产出文本（不覆盖状态）。
        for _ in 0..100 {
            if !engine.is_running(&run_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!engine.is_running(&run_id), "循环必须干净退出（park）");
        let detail = run_service.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.run.status, "running", "park 时 run 保持 running");
        let parked = detail
            .tasks
            .iter()
            .find(|t| t.status == "needs_review")
            .expect("有节点挂在 needs_review");
        assert_eq!(
            parked.pending_question.as_deref(),
            Some("定价方案选 A（订阅）还是 B（买断）？"),
            "问题原文在场"
        );
        assert_eq!(
            parked.output_summary.as_deref(),
            Some("我已提交决策问题，等待用户选择。"),
            "settle 守卫补写产出文本但不覆盖状态"
        );
        assert!(
            !reporter.reports.lock().unwrap().iter().any(|(_, _, o)| o == "completed" || o == "failed"),
            "park 不产生终态回执"
        );
        // 评审确认缺陷的回归锁：park 的 settle 以 ok:true 回场但节点并未完成，
        // 绝不能紧跟 NodeQuestion 再发一条自相矛盾的虚假 batch_progress。
        assert!(
            !reporter.reports.lock().unwrap().iter().any(|(_, _, o)| o == "batch_progress"),
            "park 不是完成——不得触发虚假 batch_progress 回执"
        );

        // ② 看门狗豁免：把 run 行老化到 STRAND_GRACE_MS 之外，reap 不得判 stalled。
        sqlx::query("UPDATE orch_runs SET updated_at = ? WHERE id = ?")
            .bind(now_ms() - STRAND_GRACE_MS - 60_000)
            .bind(&run_id)
            .execute(&pool)
            .await
            .expect("age run row");
        engine.reap_stalled_runs().await;
        let after_reap = run_service.get_detail(&run_id).await.expect("detail");
        assert_eq!(
            after_reap.run.status, "running",
            "含 needs_review 节点的 run 豁免看门狗"
        );
        assert!(
            !reporter.reports.lock().unwrap().iter().any(|(_, _, o)| o == "stalled"),
            "不得发出 stalled 回执"
        );

        // ③ adopt 恢复：run 仍 running（例外放行 needs_review），采用产出 → done +
        //    问题清空 → 手动重启循环（镜像路由的 engine-lifecycle 决策）→ 整链完成。
        let task_id = parked.id.clone();
        let run_dto = engine
            .adopt_task_result(&run_service, "u1", &run_id, &task_id)
            .await
            .expect("adopt 必须放行 needs_review 节点");
        assert_eq!(run_dto.status, "running");
        let adopted = repo.get_task(&task_id).await.unwrap().unwrap();
        assert_eq!(adopted.status, "done", "采用后节点 done");
        assert!(adopted.pending_question.is_none(), "采用后问题清空");
        assert_eq!(
            adopted.output_summary.as_deref(),
            Some("用户拍板选 A：按订阅制完成定价方案定稿。"),
            "采用写入 worker 定稿文本"
        );
        engine.start(run_id.clone());
        assert!(
            wait_run_status(&run_service, &run_id, "completed").await,
            "作答采用后整 run 必须完成"
        );
    }

    /// `raise_task_question`（nomi_task_question 的引擎入口）守卫矩阵：
    /// 空问题 / 非审批模式 run / 非 owner / 非在飞节点一律拒绝；合法调用写入
    /// pending_question + needs_review 并向 lead 回执 node_question。
    #[tokio::test]
    async fn raise_task_question_guards_and_reports() {
        let worker: Arc<dyn WorkerRunner> =
            Arc::new(MockWorkerRunner::with_text(777, "task output"));
        let (run_service, engine, reporter, repo, _pool) =
            reporter_stack_full(worker, 0).await;

        // manual run（不启动引擎——手动把首节点置 running 模拟在飞）。
        let run = run_service
            .create_adhoc(
                "u1",
                CreateAdhocRunRequest {
                    approval_mode: Some("manual".to_string()),
                    goal: "chain".to_string(),
                    work_dir: None,
                    model_range: ModelRange::Single {
                        model: ModelRef {
                            provider_id: "prov_x".to_string(),
                            model: "claude-opus-4-8".to_string(),
                        },
                    },
                    pinned_roles: vec![],
                    role_members: vec![],
                    autonomy: Some("supervised".to_string()),
                    max_parallel: None,
                    lead_conv_id: Some(707),
                    lead_model: None,
                },
            )
            .await
            .expect("create manual run");
        run_service.plan(&run.id).await.expect("plan");
        let task = run_service.get_detail(&run.id).await.unwrap().tasks[0].clone();
        repo.update_task(
            &task.id,
            UpdateTaskParams {
                status: Some("running".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // 守卫：空问题。
        assert!(matches!(
            engine.raise_task_question("u1", &run.id, &task.id, "  ").await,
            Err(AppError::BadRequest(_))
        ));
        // 守卫：非 owner 一律 NotFound（不泄露存在性）。
        assert!(matches!(
            engine.raise_task_question("intruder", &run.id, &task.id, "问题?").await,
            Err(AppError::NotFound(_))
        ));

        // 合法调用：写入 + 回执。
        engine
            .raise_task_question("u1", &run.id, &task.id, "选 A 还是 B？")
            .await
            .expect("raise ok");
        let parked = repo.get_task(&task.id).await.unwrap().unwrap();
        assert_eq!(parked.status, "needs_review");
        assert_eq!(parked.pending_question.as_deref(), Some("选 A 还是 B？"));
        let reports = reporter.reports.lock().unwrap().clone();
        assert!(
            reports.iter().any(|(conv, rid, o)| *conv == 707 && rid == &run.id && o == "node_question"),
            "须向 lead 回执 node_question，got {reports:?}"
        );

        // 守卫：非审批模式 run 拒绝提问（指示自行决策）。
        let auto_run = adhoc_lead_run(&run_service, 707).await;
        let auto_task = run_service.get_detail(&auto_run).await.unwrap().tasks[0].clone();
        repo.update_task(
            &auto_task.id,
            UpdateTaskParams {
                status: Some("running".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let err = engine
            .raise_task_question("u1", &auto_run, &auto_task.id, "选 A 还是 B？")
            .await
            .expect_err("auto run must refuse");
        assert!(
            err.to_string().contains("审批模式"),
            "拒绝文案须指示自行决策: {err}"
        );
    }

    /// compose_brief 决策策略段：manual_approval=true 追加审批模式指令（点名
    /// nomi_task_question）；false 与既有 brief 逐字节一致（零回归由既有测试锁定）。
    #[test]
    fn compose_brief_manual_approval_appends_decision_policy() {
        let task = task_row_with_kind("agent", "任务A", "做出取舍");
        let mut ctx = empty_brief_ctx();
        ctx.manual_approval = true;
        let brief = compose_brief(Some("writer"), &task, &[], &ctx);
        assert!(brief.contains("决策策略"), "决策策略段在场");
        assert!(brief.contains("nomi_task_question"), "点名提问工具");
        ctx.manual_approval = false;
        let plain = compose_brief(Some("writer"), &task, &[], &ctx);
        assert!(!plain.contains("决策策略"), "全授权不渲染决策段");
    }

    /// A permanently-failed task PERSISTS a `last_error` reason, surfaced on the DTO
    /// for the lead's diagnosis. The mock worker has no error marker → the generic
    /// timeout/no-reply fallback is recorded (never an empty "why").
    #[tokio::test]
    async fn failed_task_persists_last_error() {
        let worker: Arc<dyn WorkerRunner> = Arc::new(AlwaysFailWorker);
        let (run_service, engine, _reporter) = reporter_stack(worker, 0).await;
        let run_id = adhoc_lead_run(&run_service, 7).await;
        engine.start(run_id.clone());
        assert!(
            wait_run_status(&run_service, &run_id, "failed").await,
            "run must fail"
        );
        let detail = run_service.get_detail(&run_id).await.expect("detail");
        let failed = detail
            .tasks
            .iter()
            .find(|t| t.status == "failed")
            .expect("a failed task");
        let err = failed.last_error.as_deref().unwrap_or("");
        assert!(
            !err.is_empty(),
            "failed task must persist a last_error reason"
        );
        assert!(
            err.contains("worker") || err.contains("超时") || err.contains("无回复"),
            "generic fallback reason expected, got: {err}"
        );
    }

    // ── UC-2b: per-task token observability ───────────────────────────────────

    /// A worker that reports a FIXED token count on every task it runs, so a test
    /// can prove `settle_task_outcome` writes the real `WorkerOutcome.tokens` into
    /// `orch_run_tasks.tokens` (and that the run total sums them). The production
    /// `ConversationWorkerRunner` derives this same value from the conversation
    /// runtime-state token accumulator (filled by the stream relay's
    /// `TurnCompleted` handler); here we inject it directly to isolate the engine
    /// settle/aggregate wiring from the live conversation layer.
    struct TokenReportingWorker {
        tokens: i64,
    }
    #[async_trait]
    impl WorkerRunner for TokenReportingWorker {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            task_id: &str,
            _brief: &str,
            _task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            on_started(777);
            Ok(WorkerOutcome {
                conversation_id: 777,
                text: Some(format!("output of {task_id}")),
                ok: true,
                tokens: Some(self.tokens),
            })
        }
    }

    /// A run whose worker reports N tokens per task → every done task's `tokens`
    /// is written N (the REAL count, not fabricated), and the run's `total_tokens`
    /// is the sum across tasks. Exercises the full settle/aggregate path on the
    /// 3-task chain.
    #[tokio::test]
    async fn per_task_tokens_are_written_and_summed_into_run_total() {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let emitter = OrchestratorRunEventEmitter::new(broadcaster.clone());
        let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let worker: Arc<dyn WorkerRunner> = Arc::new(TokenReportingWorker { tokens: 125 });
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(5);
        let engine = RunEngine::new(Arc::new(engine_deps));

        let h = Harness {
            run_service,
            engine,
            run_repo,
            fleet_repo,
            ws_repo,
            broadcaster,
        };
        let run_id = seed_run(&h).await;
        h.run_service.plan(&run_id).await.expect("plan");
        h.engine.start(run_id.clone());

        let mut completed = false;
        for _ in 0..50 {
            let d = h.run_service.get_detail(&run_id).await.expect("detail");
            if d.run.status == "completed" {
                completed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            completed,
            "run must reach completed within the bounded poll"
        );

        let detail = h.run_service.get_detail(&run_id).await.expect("detail");
        for t in &detail.tasks {
            assert_eq!(t.status, "done", "task {} should be done", t.title);
            assert_eq!(
                t.tokens,
                Some(125),
                "task {} must record the worker's real token count",
                t.title
            );
        }
        // Run total = 3 tasks × 125 = 375.
        assert_eq!(
            detail.run.total_tokens,
            Some(375),
            "run total_tokens must sum the per-task token counts (3 × 125)"
        );
    }

    /// Zero-regression: a worker with NO token source (the fixed MockWorkerRunner,
    /// which mirrors the production zero-source path) leaves every task's `tokens`
    /// as `None` and the run's `total_tokens` as `None` — exactly the prior
    /// behaviour. No fabricated zeros.
    #[tokio::test]
    async fn no_token_source_leaves_tokens_none() {
        let h = harness().await; // MockWorkerRunner::with_text → tokens: None
        let run_id = seed_run(&h).await;
        h.run_service.plan(&run_id).await.expect("plan");
        h.engine.start(run_id.clone());

        let mut completed = false;
        for _ in 0..50 {
            let d = h.run_service.get_detail(&run_id).await.expect("detail");
            if d.run.status == "completed" {
                completed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            completed,
            "run must reach completed within the bounded poll"
        );

        let detail = h.run_service.get_detail(&run_id).await.expect("detail");
        for t in &detail.tasks {
            assert_eq!(t.status, "done");
            assert_eq!(
                t.tokens, None,
                "task {} tokens must stay None (no source)",
                t.title
            );
        }
        assert_eq!(
            detail.run.total_tokens, None,
            "run total_tokens must stay None when no task reported usage"
        );
    }

    // The conversation-native lead path creates an AD-HOC (workspace_id NULL) run
    // with autonomy `interactive`. After plan() the run must park at
    // `awaiting_plan_approval` and the engine, when started for it, must dispatch
    // ZERO workers (the human-in-the-loop gate sits at the PLAN, not per-worker) —
    // tasks stay `pending`. After `approve_plan` flips it to `running`, starting
    // the engine again drives the chain to completion. This is the whole point of
    // the interactive default: no auto-start, workers wait for approval.
    //
    // Crucially this exercises the AD-HOC path (no workspace entity): approve_plan
    // + engine.start must work off the run's own `work_dir`, with no workspace
    // lookup — the engine resolves work_dir from `run.work_dir` first (it is set
    // here), and approve_plan only reads the run row.
    #[tokio::test]
    async fn interactive_adhoc_run_waits_for_approval_then_completes() {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let emitter = OrchestratorRunEventEmitter::new(broadcaster);
        // The A→B→C chain planner so we get a real multi-task DAG to (not) run.
        let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo,
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        // A worker that records every dispatch via its start_order — so we can
        // assert the engine dispatched NOTHING while the run awaits approval.
        let worker = Arc::new(ConcurrencyMockWorkerRunner::new(Duration::ZERO));
        let worker_dyn: Arc<dyn WorkerRunner> = worker.clone();
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker_dyn, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(10);
        let engine = RunEngine::new(Arc::new(engine_deps));

        // Ad-hoc (workspace-less) interactive run with its own work_dir — exactly
        // what the conversation-native lead path builds (autonomy "interactive").
        let run = run_service
            .create_adhoc(
                "u1",
                CreateAdhocRunRequest {
                    approval_mode: None,
                    goal: "build the chain".to_string(),
                    work_dir: Some("/tmp/adhoc-proj".to_string()),
                    model_range: ModelRange::Single {
                        model: ModelRef {
                            provider_id: "prov_x".to_string(),
                            model: "claude-opus-4-8".to_string(),
                        },
                    },
                    pinned_roles: vec![],
                    role_members: vec![],
                    autonomy: Some("interactive".to_string()),
                    max_parallel: None,
                    lead_conv_id: Some(909),
                    lead_model: None,
                },
            )
            .await
            .expect("create_adhoc interactive");
        assert!(run.workspace_id.is_none(), "ad-hoc run has no workspace");

        // Plan: the autonomy gate parks an interactive run at awaiting_plan_approval.
        run_service.plan(&run.id).await.expect("plan");
        let detail = run_service.get_detail(&run.id).await.expect("detail");
        assert_eq!(
            detail.run.status, "awaiting_plan_approval",
            "interactive ad-hoc run parks at awaiting_plan_approval after plan"
        );
        assert_eq!(detail.tasks.len(), 3, "the 3-task chain was planned");

        // The conversation-native choreography must NOT start the engine for an
        // awaiting run. Start it anyway here to PROVE the engine itself dispatches
        // nothing for a non-`running` run (defense-in-depth: even a stray start
        // before approval must not run a worker).
        engine.start(run.id.clone());
        // Give the loop a moment; it should read the non-running status and exit
        // without dispatching any worker.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            worker.start_order.lock().unwrap().len(),
            0,
            "no worker may dispatch while the run awaits plan approval"
        );
        // Tasks remain pending (nothing was marked running/done).
        let detail = run_service.get_detail(&run.id).await.expect("detail");
        assert!(
            detail.tasks.iter().all(|t| t.status == "pending"),
            "all tasks stay pending until approval; got {:?}",
            detail
                .tasks
                .iter()
                .map(|t| (&t.title, &t.status))
                .collect::<Vec<_>>()
        );

        // Approve → running, then start the engine (the approve route's exact
        // choreography). The chain now runs to completion off the run's work_dir.
        run_service.approve_plan(&run.id).await.expect("approve");
        assert_eq!(
            run_service.get_detail(&run.id).await.unwrap().run.status,
            "running",
            "approve flips the ad-hoc run to running"
        );
        engine.start(run.id.clone());

        let mut completed = false;
        for _ in 0..80 {
            let d = run_service.get_detail(&run.id).await.expect("detail");
            if d.run.status == "completed" {
                completed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(completed, "approved ad-hoc run must reach completed");
        // The worker ran exactly the 3 chain tasks (each off the ad-hoc work_dir).
        let dispatched = worker.start_order.lock().unwrap().clone();
        assert_eq!(dispatched.len(), 3, "all 3 tasks dispatched after approval");
        let dirs = worker.seen_workspace_dir.lock().unwrap().clone();
        assert!(
            dirs.iter().all(|d| d.as_deref() == Some("/tmp/adhoc-proj")),
            "workers run off the ad-hoc run's own work_dir (no workspace); got {dirs:?}"
        );
    }

    #[tokio::test]
    async fn cancel_stops_a_running_engine_and_persists_cancelled() {
        let h = harness().await;
        let run_id = seed_run(&h).await;
        h.run_service.plan(&run_id).await.expect("plan");

        // Start then immediately stop + persist cancel.
        h.engine.start(run_id.clone());
        h.engine.stop(&run_id);
        h.run_service.cancel(&run_id).await.expect("cancel");

        assert!(
            !h.engine.is_running(&run_id),
            "stopped loop is no longer registered"
        );
        let detail = h.run_service.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.run.status, "cancelled", "run persisted as cancelled");
    }

    #[test]
    fn compose_brief_includes_role_task_and_upstream() {
        let task = OrchRunTaskRow {
            pending_question: None,
            last_error: None,
            id: "rtask_1".to_string(),
            run_id: "run_1".to_string(),
            title: "Synthesize".to_string(),
            spec: "write the report".to_string(),
            task_profile: None,
            status: "pending".to_string(),
            conversation_id: None,
            output_summary: None,
            output_files: None,
            attempt: 0,
            tokens: None,
            graph_x: None,
            graph_y: None,
            role: None,
            kind: "agent".to_string(),
            pattern_config: None,
            next_retry_at: None,
            override_provider_id: None,
            override_model: None,
            preset_prompt: None,
            on_fail: None,
            created_at: 0,
            updated_at: 0,
        };
        let upstream = vec![("Gather".to_string(), "found 12 sources".to_string())];
        let brief = compose_brief(Some("writer"), &task, &upstream, &empty_brief_ctx());
        assert!(brief.contains("ROLE: writer"));
        assert!(brief.contains("TASK: Synthesize"));
        assert!(brief.contains("write the report"));
        assert!(brief.contains("Gather: found 12 sources"));
    }

    /// An empty [`BriefContext`] — no goal, no plan, no ancestors, no notes. With
    /// it, every Phase 1b Task 3 section is gated off, so a brief is byte-for-byte
    /// the legacy brief. Every pre-existing compose_brief test passes this.
    fn empty_brief_ctx() -> BriefContext {
        BriefContext {
            manual_approval: false,
            goal: String::new(),
            plan_summary: String::new(),
            ancestor_digest: vec![],
            notes_hint: None,
        }
    }

    // Phase 1b Task 3: a NON-empty BriefContext injects the run goal, the plan
    // summary, a transitive-ancestor digest, and a shared-notes pointer — all
    // ALONGSIDE the (still present) direct upstream.
    #[test]
    fn compose_brief_includes_goal_plan_ancestors_and_notes_pointer() {
        let task = task_row_with_kind("agent", "Write", "写最终报告");
        let upstream = vec![("B".to_string(), "b out".to_string())];
        let ctx = BriefContext {
            manual_approval: false,
            goal: "构建报告".into(),
            plan_summary: "A → B → Write".into(),
            ancestor_digest: vec![("A".to_string(), "a findings".to_string())],
            notes_hint: Some("/ws/run1/RUN_NOTES.md".into()),
        };
        let brief = compose_brief(Some("writer"), &task, &upstream, &ctx);
        assert!(brief.contains("构建报告"), "run goal injected: {brief}");
        assert!(
            brief.contains("A → B → Write"),
            "plan summary injected: {brief}"
        );
        assert!(
            brief.contains("a findings"),
            "transitive ancestor digest injected: {brief}"
        );
        assert!(brief.contains("b out"), "direct upstream still present: {brief}");
        assert!(
            brief.contains("RUN_NOTES.md"),
            "shared-notes pointer injected: {brief}"
        );
    }

    // Phase 1b Task 3: a long direct-upstream output is truncated (CJK-safe) so a
    // verbose upstream cannot blow the worker's prompt budget — the full text is
    // NOT carried, and the truncation ellipsis marks where it was cut.
    #[test]
    fn compose_brief_truncates_long_upstream() {
        let long = "x".repeat(SUMMARY_TASK_OUTPUT_LEN + 500);
        let upstream = vec![("B".to_string(), long.clone())];
        let ctx = BriefContext {
            manual_approval: false,
            goal: "g".into(),
            plan_summary: "p".into(),
            ancestor_digest: vec![],
            notes_hint: None,
        };
        let brief = compose_brief(
            Some("w"),
            &task_row_with_kind("agent", "T", "s"),
            &upstream,
            &ctx,
        );
        assert!(!brief.contains(&long), "long upstream truncated: {brief}");
        assert!(brief.contains('…'), "truncation marker present: {brief}");
    }

    /// Build an `OrchRunTaskRow` with the given `kind` (other fields fixed) — used
    /// by the kind-aware compose_brief tests.
    fn task_row_with_kind(kind: &str, title: &str, spec: &str) -> OrchRunTaskRow {
        OrchRunTaskRow {
            pending_question: None,
            last_error: None,
            id: "rtask_k".to_string(),
            run_id: "run_1".to_string(),
            title: title.to_string(),
            spec: spec.to_string(),
            task_profile: None,
            status: "pending".to_string(),
            conversation_id: None,
            output_summary: None,
            output_files: None,
            attempt: 0,
            tokens: None,
            graph_x: None,
            graph_y: None,
            role: None,
            kind: kind.to_string(),
            pattern_config: None,
            next_retry_at: None,
            override_provider_id: None,
            override_model: None,
            preset_prompt: None,
            on_fail: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    // 迁移 023: a `synthesis`-kind task's brief is framed as an explicit merge
    // instruction (NOT the agent "build on" framing) AND it still carries the
    // dependency outputs (the material to synthesize). The framing must DIFFER
    // from the agent brief for the same task/upstream.
    #[test]
    fn compose_brief_synthesis_framing_differs_and_merges_deps() {
        let upstream = vec![
            ("Draft A".to_string(), "草稿A的要点".to_string()),
            ("Draft B".to_string(), "草稿B的要点".to_string()),
        ];

        let synth_task = task_row_with_kind("synthesis", "合并草稿", "写出最终稿");
        let synth_brief = compose_brief(Some("写手"), &synth_task, &upstream, &empty_brief_ctx());

        // Synthesis-specific framing present.
        assert!(
            synth_brief.contains("综合") && synth_brief.contains("合并"),
            "synthesis brief must instruct to 综合/合并: {synth_brief}"
        );
        assert!(
            synth_brief.contains("SYNTHESIS TASK"),
            "synthesis brief uses the SYNTHESIS framing: {synth_brief}"
        );
        // The dependency outputs are merged into the brief (the material to combine).
        assert!(
            synth_brief.contains("Draft A: 草稿A的要点"),
            "dep A output present: {synth_brief}"
        );
        assert!(
            synth_brief.contains("Draft B: 草稿B的要点"),
            "dep B output present: {synth_brief}"
        );
        // The role + spec are still surfaced.
        assert!(synth_brief.contains("ROLE: 写手"));
        assert!(synth_brief.contains("写出最终稿"));

        // The SAME task as an `agent` kind produces the OLD framing (no synthesis
        // instruction, the plain TASK/UPSTREAM RESULTS labels) — proving the
        // branches diverge and agent is unchanged.
        let agent_task = task_row_with_kind("agent", "合并草稿", "写出最终稿");
        let agent_brief = compose_brief(Some("写手"), &agent_task, &upstream, &empty_brief_ctx());
        assert_ne!(
            synth_brief, agent_brief,
            "synthesis framing must differ from agent"
        );
        assert!(
            agent_brief.contains("TASK: 合并草稿"),
            "agent keeps TASK framing: {agent_brief}"
        );
        assert!(
            agent_brief.contains("UPSTREAM RESULTS"),
            "agent keeps the build-on framing: {agent_brief}"
        );
        assert!(
            !agent_brief.contains("SYNTHESIS TASK"),
            "agent brief must NOT carry synthesis framing: {agent_brief}"
        );
    }

    // ZERO-REGRESSION: the agent-kind brief is byte-for-byte the legacy framing —
    // assert it matches the explicit expected string for a known task/upstream, so
    // any drift in the agent path is caught.
    #[test]
    fn compose_brief_agent_kind_is_unchanged_legacy_framing() {
        let task = task_row_with_kind("agent", "Synthesize", "write the report");
        let upstream = vec![("Gather".to_string(), "found 12 sources".to_string())];
        let brief = compose_brief(Some("writer"), &task, &upstream, &empty_brief_ctx());
        let expected = "ROLE: writer\n\nTASK: Synthesize\nSPEC:\nwrite the report\n\nUPSTREAM RESULTS (completed dependencies you can build on):\n- Gather: found 12 sources\n";
        assert_eq!(
            brief, expected,
            "agent-kind brief must match the pre-023 framing exactly"
        );
    }

    // 迁移 026: a non-empty `preset_prompt` (启动前配置台) is APPENDED as its own
    // section for BOTH agent and synthesis kinds; a blank/absent one changes
    // nothing (zero-regression, gated on presence).
    #[test]
    fn compose_brief_appends_preset_requirement_gated_on_presence() {
        // agent: preset appended AFTER the unchanged base brief.
        let mut a = task_row_with_kind("agent", "Do X", "the spec");
        let a_base = compose_brief(Some("writer"), &a, &[], &empty_brief_ctx());
        assert!(!a_base.contains("用户预置要求"), "no preset → no section");
        a.preset_prompt = Some("务必用中文，并附引用".to_string());
        let a_with = compose_brief(Some("writer"), &a, &[], &empty_brief_ctx());
        assert!(
            a_with.starts_with(&a_base),
            "preset is appended, base brief unchanged"
        );
        assert!(
            a_with.contains("用户预置要求"),
            "agent preset section present: {a_with}"
        );
        assert!(a_with.contains("务必用中文，并附引用"));

        // synthesis: preset also appended (uniform in compose_brief).
        let mut s = task_row_with_kind("synthesis", "Merge", "synthesize");
        let up = vec![("A".to_string(), "a".to_string())];
        let s_base = compose_brief(None, &s, &up, &empty_brief_ctx());
        s.preset_prompt = Some("只输出要点".to_string());
        let s_with = compose_brief(None, &s, &up, &empty_brief_ctx());
        assert!(
            s_with.starts_with(&s_base),
            "synthesis preset appended after base"
        );
        assert!(s_with.contains("用户预置要求") && s_with.contains("只输出要点"));

        // blank preset (whitespace only) → byte-for-byte unchanged.
        let mut b = task_row_with_kind("agent", "Y", "s");
        let b_base = compose_brief(None, &b, &[], &empty_brief_ctx());
        b.preset_prompt = Some("   \n  ".to_string());
        assert_eq!(
            compose_brief(None, &b, &[], &empty_brief_ctx()),
            b_base,
            "blank preset appends nothing"
        );
    }

    #[test]
    fn aggregate_summary_is_non_empty_and_counts_done() {
        let mk = |title: &str, status: &str, summary: Option<&str>| OrchRunTaskRow {
            pending_question: None,
            last_error: None,
            id: format!("rtask_{title}"),
            run_id: "run_1".to_string(),
            title: title.to_string(),
            spec: String::new(),
            task_profile: None,
            status: status.to_string(),
            conversation_id: None,
            output_summary: summary.map(str::to_string),
            output_files: None,
            attempt: 0,
            tokens: None,
            graph_x: None,
            graph_y: None,
            role: None,
            kind: "agent".to_string(),
            pattern_config: None,
            next_retry_at: None,
            override_provider_id: None,
            override_model: None,
            preset_prompt: None,
            on_fail: None,
            created_at: 0,
            updated_at: 0,
        };
        let tasks = vec![
            mk("A", "done", Some("did A")),
            mk("B", "done", Some("did B")),
        ];
        let summary = aggregate_summary(&tasks);
        assert!(summary.contains("2/2"));
        assert!(summary.contains("did A"));
        assert!(summary.contains("did B"));
    }

    // ── verify 模式 (UC-1b): vote-policy parse, verdict parse (fail-safe), tally ──

    /// Build a skeptic task row carrying the given `output_summary` (the verdict
    /// material the verify aggregator reads).
    fn skeptic_with(title: &str, output: Option<&str>) -> OrchRunTaskRow {
        let mut t = task_row_with_kind("agent", title, "critically evaluate");
        t.id = format!("rtask_{title}");
        t.status = "done".to_string();
        t.output_summary = output.map(str::to_string);
        t
    }

    #[test]
    fn vote_policy_parse_is_fail_soft_to_majority() {
        // Explicit shapes.
        assert_eq!(
            VotePolicy::parse(Some(r#"{"vote":"majority"}"#)),
            VotePolicy::Majority
        );
        assert_eq!(
            VotePolicy::parse(Some(r#"{"vote":"unanimous"}"#)),
            VotePolicy::Unanimous
        );
        assert_eq!(
            VotePolicy::parse(Some(r#"{"vote":"UNANIMOUS"}"#)),
            VotePolicy::Unanimous
        );
        assert_eq!(
            VotePolicy::parse(Some(r#"{"vote":{"threshold":2}}"#)),
            VotePolicy::Threshold(2)
        );
        // Fail-soft: absent / blank / not-JSON / no `vote` / unknown string → Majority.
        assert_eq!(VotePolicy::parse(None), VotePolicy::Majority);
        assert_eq!(VotePolicy::parse(Some("   ")), VotePolicy::Majority);
        assert_eq!(VotePolicy::parse(Some("not json")), VotePolicy::Majority);
        assert_eq!(
            VotePolicy::parse(Some(r#"{"group":"x"}"#)),
            VotePolicy::Majority
        );
        assert_eq!(
            VotePolicy::parse(Some(r#"{"vote":"weird"}"#)),
            VotePolicy::Majority
        );
    }

    #[test]
    fn vote_policy_passes_thresholds() {
        // Majority = strictly more than half.
        assert!(VotePolicy::Majority.passes(2, 3), "2/3 is a majority");
        assert!(!VotePolicy::Majority.passes(1, 3), "1/3 is not");
        assert!(
            !VotePolicy::Majority.passes(1, 2),
            "1/2 is a TIE, not a majority"
        );
        assert!(VotePolicy::Majority.passes(2, 2), "2/2 is a majority");
        assert!(
            !VotePolicy::Majority.passes(0, 0),
            "0 skeptics never passes (fail-safe)"
        );
        // Unanimous = all, with at least one skeptic.
        assert!(VotePolicy::Unanimous.passes(3, 3));
        assert!(!VotePolicy::Unanimous.passes(2, 3));
        assert!(
            !VotePolicy::Unanimous.passes(0, 0),
            "unanimous with 0 skeptics fails"
        );
        // Threshold(n) = at least n.
        assert!(VotePolicy::Threshold(2).passes(2, 5));
        assert!(VotePolicy::Threshold(2).passes(3, 5));
        assert!(!VotePolicy::Threshold(2).passes(1, 5));
    }

    #[test]
    fn parse_verdict_prefers_json_pass_field() {
        assert!(parse_verdict_pass(Some(
            r#"{"pass": true, "critique": "looks good"}"#
        )));
        assert!(!parse_verdict_pass(Some(
            r#"{"pass": false, "critique": "broken auth"}"#
        )));
        // JSON embedded in prose still found.
        assert!(parse_verdict_pass(Some(
            "After review: {\"pass\": true, \"critique\": \"ok\"} done."
        )));
    }

    #[test]
    fn parse_verdict_text_fallback_fail_beats_pass() {
        // No JSON pass field → text scan. FAIL is conservative and wins.
        assert!(!parse_verdict_pass(Some(
            "Verdict: FAIL — the logic is wrong"
        )));
        assert!(parse_verdict_pass(Some("Verdict: PASS — all good")));
        // Both present → FAIL wins (conservative).
        assert!(!parse_verdict_pass(Some(
            "It could PASS but I must FAIL it"
        )));
    }

    #[test]
    fn parse_verdict_unparseable_defaults_to_fail() {
        // The fail-safe invariant: missing / blank / unrecognizable → FAIL.
        assert!(!parse_verdict_pass(None), "missing output → fail-safe");
        assert!(!parse_verdict_pass(Some("   ")), "blank output → fail-safe");
        assert!(
            !parse_verdict_pass(Some("hmm, not sure")),
            "no verdict token → fail-safe"
        );
        // Malformed JSON with no PASS/FAIL token → fail-safe.
        assert!(
            !parse_verdict_pass(Some(r#"{"pas": tru"#)),
            "broken JSON → fail-safe"
        );
        // JSON without a `pass` field, no text token → fail-safe.
        assert!(!parse_verdict_pass(Some(r#"{"critique":"meh"}"#)));
    }

    #[test]
    fn tally_verify_majority_pass_and_fail() {
        // 3 skeptics, 2 pass → majority PASS.
        let pass_case = vec![
            skeptic_with("S1", Some(r#"{"pass":true}"#)),
            skeptic_with("S2", Some(r#"{"pass":true}"#)),
            skeptic_with("S3", Some(r#"{"pass":false,"critique":"nit"}"#)),
        ];
        let v = tally_verify(&pass_case, VotePolicy::Majority);
        assert!(v.pass, "2/3 majority passes");
        assert_eq!(v.pass_count, 2);
        assert_eq!(v.total, 3);

        // 3 skeptics, 1 pass → majority FAIL.
        let fail_case = vec![
            skeptic_with("S1", Some(r#"{"pass":true}"#)),
            skeptic_with("S2", Some(r#"{"pass":false}"#)),
            skeptic_with("S3", Some(r#"{"pass":false}"#)),
        ];
        let v = tally_verify(&fail_case, VotePolicy::Majority);
        assert!(!v.pass, "1/3 fails majority");
        assert_eq!(v.pass_count, 1);
    }

    #[test]
    fn tally_verify_unanimous_and_threshold() {
        let all_pass = vec![
            skeptic_with("S1", Some(r#"{"pass":true}"#)),
            skeptic_with("S2", Some("PASS")),
        ];
        assert!(
            tally_verify(&all_pass, VotePolicy::Unanimous).pass,
            "all pass → unanimous"
        );

        let one_fail = vec![
            skeptic_with("S1", Some(r#"{"pass":true}"#)),
            skeptic_with("S2", Some("FAIL")),
        ];
        assert!(
            !tally_verify(&one_fail, VotePolicy::Unanimous).pass,
            "one fail breaks unanimous"
        );

        // Threshold(1): a single pass among 3 satisfies it.
        let one_pass = vec![
            skeptic_with("S1", Some(r#"{"pass":true}"#)),
            skeptic_with("S2", Some("FAIL")),
            skeptic_with("S3", Some(r#"{"pass":false}"#)),
        ];
        assert!(
            tally_verify(&one_pass, VotePolicy::Threshold(1)).pass,
            "1 pass ≥ threshold 1"
        );
        assert!(
            !tally_verify(&one_pass, VotePolicy::Threshold(2)).pass,
            "1 pass < threshold 2"
        );
    }

    #[test]
    fn tally_verify_unparseable_skeptic_counts_as_fail() {
        // One skeptic produced garbage (fail-safe FAIL), the other a clean pass.
        // Under majority, 1/2 is a tie → FAIL (the garbage vote correctly drags it).
        let mixed = vec![
            skeptic_with("S1", Some(r#"{"pass":true}"#)),
            skeptic_with("S2", Some("the worker timed out, no verdict")),
        ];
        let v = tally_verify(&mixed, VotePolicy::Majority);
        assert_eq!(v.pass_count, 1, "the unparseable skeptic counts as fail");
        assert!(
            !v.pass,
            "1/2 (tie) fails majority — unvalidated output never approves"
        );
    }

    #[test]
    fn render_verify_summary_leads_with_machine_verdict() {
        let v = tally_verify(
            &[
                skeptic_with("S1", Some(r#"{"pass":true,"critique":"ok"}"#)),
                skeptic_with("S2", Some(r#"{"pass":false,"critique":"边界未处理"}"#)),
            ],
            VotePolicy::Majority,
        );
        let s = render_verify_summary(&v, VotePolicy::Majority);
        assert!(
            s.starts_with("VERDICT: FAIL"),
            "summary leads with the verdict: {s}"
        );
        assert!(s.contains("1/2"), "tally surfaced: {s}");
        assert!(s.contains("policy=majority"), "policy surfaced: {s}");
        // Per-skeptic critiques collected.
        assert!(s.contains("S1 → PASS"), "skeptic verdict surfaced: {s}");
        assert!(s.contains("S2 → FAIL"), "skeptic verdict surfaced: {s}");
        assert!(s.contains("边界未处理"), "critique text collected: {s}");
    }

    // ── judge 模式 (UC-1c): policy parse, ballot parse (fail-safe), mean/borda ──

    #[test]
    fn judge_policy_parse_is_fail_soft_to_mean() {
        // Explicit shapes.
        assert_eq!(
            JudgePolicy::parse(Some(r#"{"aggregate":"mean"}"#)),
            JudgePolicy::Mean
        );
        assert_eq!(
            JudgePolicy::parse(Some(r#"{"aggregate":"borda"}"#)),
            JudgePolicy::Borda
        );
        assert_eq!(
            JudgePolicy::parse(Some(r#"{"aggregate":"BORDA"}"#)),
            JudgePolicy::Borda
        );
        // candidates pin alongside the policy still resolves the policy.
        assert_eq!(
            JudgePolicy::parse(Some(r#"{"aggregate":"borda","candidates":3}"#)),
            JudgePolicy::Borda
        );
        // Fail-soft: absent / blank / not-JSON / no `aggregate` / unknown → Mean.
        assert_eq!(JudgePolicy::parse(None), JudgePolicy::Mean);
        assert_eq!(JudgePolicy::parse(Some("   ")), JudgePolicy::Mean);
        assert_eq!(JudgePolicy::parse(Some("not json")), JudgePolicy::Mean);
        assert_eq!(
            JudgePolicy::parse(Some(r#"{"group":"x"}"#)),
            JudgePolicy::Mean
        );
        assert_eq!(
            JudgePolicy::parse(Some(r#"{"aggregate":"weird"}"#)),
            JudgePolicy::Mean
        );
    }

    #[test]
    fn parse_judge_ballot_array_and_object_forms() {
        // Array form: positional by candidate index.
        let arr = parse_judge_ballot(Some(r#"{"scores":[0.8,0.3,0.6]}"#), 3).expect("array ballot");
        assert_eq!(arr, vec![Some(0.8), Some(0.3), Some(0.6)]);

        // Object form: keyed by candidate index (sparse OK — missing → None).
        let obj =
            parse_judge_ballot(Some(r#"{"scores":{"0":0.8,"2":0.6}}"#), 3).expect("object ballot");
        assert_eq!(obj, vec![Some(0.8), None, Some(0.6)]);

        // Embedded in prose still found.
        let prose = parse_judge_ballot(Some("After review: {\"scores\":[0.1,0.9]} done."), 2)
            .expect("prose-embedded ballot");
        assert_eq!(prose, vec![Some(0.1), Some(0.9)]);

        // Extra array entries beyond M are ignored; the ballot is sized to M.
        let extra = parse_judge_ballot(Some(r#"{"scores":[0.1,0.2,0.3,0.4]}"#), 2).expect("extra");
        assert_eq!(extra, vec![Some(0.1), Some(0.2)]);

        // Out-of-range / non-index object keys are ignored.
        let oor =
            parse_judge_ballot(Some(r#"{"scores":{"0":0.5,"9":0.9,"x":0.1}}"#), 2).expect("oor");
        assert_eq!(oor, vec![Some(0.5), None]);
    }

    #[test]
    fn parse_judge_ballot_unparseable_is_dropped_no_panic() {
        // The fail-safe invariant: missing / blank / unparseable / no usable
        // scores → None (the judge is DROPPED), never a panic.
        assert!(
            parse_judge_ballot(None, 3).is_none(),
            "missing output → dropped"
        );
        assert!(
            parse_judge_ballot(Some("   "), 3).is_none(),
            "blank output → dropped"
        );
        assert!(
            parse_judge_ballot(Some("no json here"), 3).is_none(),
            "no JSON → dropped"
        );
        assert!(
            parse_judge_ballot(Some(r#"{"scor: ["#), 3).is_none(),
            "broken JSON → dropped"
        );
        assert!(
            parse_judge_ballot(Some(r#"{"verdict":"good"}"#), 3).is_none(),
            "no scores field → dropped"
        );
        assert!(
            parse_judge_ballot(Some(r#"{"scores":"not-a-list"}"#), 3).is_none(),
            "scores not array/object → dropped"
        );
        // An object whose only keys are out-of-range → no usable scores → dropped.
        assert!(
            parse_judge_ballot(Some(r#"{"scores":{"9":0.9}}"#), 2).is_none(),
            "all out-of-range → dropped"
        );
        // An empty scores array → no usable scores → dropped (not a panic).
        assert!(
            parse_judge_ballot(Some(r#"{"scores":[]}"#), 3).is_none(),
            "empty array → dropped"
        );
    }

    #[test]
    fn aggregate_judge_mean_picks_highest_average() {
        // 3 candidates, 2 judges. Means: c0=(0.9+0.7)/2=0.8, c1=(0.2+0.4)/2=0.3,
        // c2=(0.5+0.6)/2=0.55 → winner c0.
        let ballots = vec![
            vec![Some(0.9), Some(0.2), Some(0.5)],
            vec![Some(0.7), Some(0.4), Some(0.6)],
        ];
        let r = aggregate_judge(&ballots, 3, JudgePolicy::Mean);
        assert_eq!(r.winner, Some(0), "c0 has the highest mean");
        assert_eq!(r.judges_counted, 2);
        assert!(
            (r.aggregate[0] - 0.8).abs() < 1e-9,
            "mean c0: {:?}",
            r.aggregate
        );
        assert!(
            (r.aggregate[2] - 0.55).abs() < 1e-9,
            "mean c2: {:?}",
            r.aggregate
        );
    }

    #[test]
    fn aggregate_judge_mean_order_independent() {
        // Permuting the judges' order must not change the mean aggregate / winner.
        let a = vec![
            vec![Some(0.1), Some(0.9), Some(0.4)],
            vec![Some(0.2), Some(0.8), Some(0.3)],
            vec![Some(0.3), Some(0.7), Some(0.5)],
        ];
        let b = vec![
            vec![Some(0.3), Some(0.7), Some(0.5)],
            vec![Some(0.1), Some(0.9), Some(0.4)],
            vec![Some(0.2), Some(0.8), Some(0.3)],
        ];
        let ra = aggregate_judge(&a, 3, JudgePolicy::Mean);
        let rb = aggregate_judge(&b, 3, JudgePolicy::Mean);
        assert_eq!(ra.winner, Some(1), "c1 wins by mean");
        assert_eq!(ra.winner, rb.winner, "permutation-independent winner");
        for c in 0..3 {
            assert!(
                (ra.aggregate[c] - rb.aggregate[c]).abs() < 1e-9,
                "candidate {c} aggregate stable"
            );
        }
    }

    #[test]
    fn aggregate_judge_borda_picks_highest_rank_sum() {
        // 3 candidates, 2 judges (M=3 → points 2,1,0 per judge).
        // Judge0 ranks: c0(0.9) > c2(0.6) > c1(0.2) → c0=2,c2=1,c1=0.
        // Judge1 ranks: c2(0.8) > c0(0.7) > c1(0.1) → c2=2,c0=1,c1=0.
        // Totals: c0=3, c1=0, c2=3 → TIE c0/c2 → lowest index c0 wins.
        let ballots = vec![
            vec![Some(0.9), Some(0.2), Some(0.6)],
            vec![Some(0.7), Some(0.1), Some(0.8)],
        ];
        let r = aggregate_judge(&ballots, 3, JudgePolicy::Borda);
        assert!(
            (r.aggregate[0] - 3.0).abs() < 1e-9,
            "c0 borda=3: {:?}",
            r.aggregate
        );
        assert!(
            (r.aggregate[1] - 0.0).abs() < 1e-9,
            "c1 borda=0: {:?}",
            r.aggregate
        );
        assert!(
            (r.aggregate[2] - 3.0).abs() < 1e-9,
            "c2 borda=3: {:?}",
            r.aggregate
        );
        assert_eq!(r.winner, Some(0), "tie c0/c2 broken to lowest index c0");
    }

    #[test]
    fn aggregate_judge_borda_clear_winner_and_order_independent() {
        // Judge0: c1(0.9)>c0(0.5)>c2(0.1) → c1=2,c0=1,c2=0.
        // Judge1: c1(0.8)>c2(0.6)>c0(0.2) → c1=2,c2=1,c0=0.
        // Totals: c0=1, c1=4, c2=1 → c1 clear winner.
        let a = vec![
            vec![Some(0.5), Some(0.9), Some(0.1)],
            vec![Some(0.2), Some(0.8), Some(0.6)],
        ];
        let r = aggregate_judge(&a, 3, JudgePolicy::Borda);
        assert_eq!(r.winner, Some(1), "c1 is the clear borda winner");
        assert!(
            (r.aggregate[1] - 4.0).abs() < 1e-9,
            "c1 borda=4: {:?}",
            r.aggregate
        );

        // Order independence: reverse the ballots → same winner + same totals.
        let b = vec![a[1].clone(), a[0].clone()];
        let rb = aggregate_judge(&b, 3, JudgePolicy::Borda);
        assert_eq!(rb.winner, Some(1));
        for c in 0..3 {
            assert!(
                (r.aggregate[c] - rb.aggregate[c]).abs() < 1e-9,
                "candidate {c} borda stable"
            );
        }
    }

    #[test]
    fn aggregate_judge_borda_ties_within_a_judge_share_points() {
        // One judge, 3 candidates, c0 and c1 TIED at 0.5, c2 at 0.1.
        // Ranks for points 2,1,0: the tied block {c0,c1} occupies ranks 0,1 →
        // share (2+1)/2 = 1.5 each; c2 gets 0. Deterministic (no index drift).
        let ballots = vec![vec![Some(0.5), Some(0.5), Some(0.1)]];
        let r = aggregate_judge(&ballots, 3, JudgePolicy::Borda);
        assert!(
            (r.aggregate[0] - 1.5).abs() < 1e-9,
            "c0 shares tie: {:?}",
            r.aggregate
        );
        assert!(
            (r.aggregate[1] - 1.5).abs() < 1e-9,
            "c1 shares tie: {:?}",
            r.aggregate
        );
        assert!(
            (r.aggregate[2] - 0.0).abs() < 1e-9,
            "c2 last: {:?}",
            r.aggregate
        );
        // Final tie c0/c1 → lowest index wins.
        assert_eq!(r.winner, Some(0));
    }

    #[test]
    fn settle_judge_drops_missing_ballot_fail_safe() {
        // Two usable judges + one dropped (unparseable). The drop must not crash;
        // the winner is computed from the two usable ballots only.
        let candidates = 2;
        let raw = vec![
            Some(r#"{"scores":[0.9,0.1]}"#),     // c0 strong
            Some("worker timed out, no ballot"), // unparseable → dropped
            Some(r#"{"scores":[0.8,0.2]}"#),     // c0 strong
        ];
        let ballots: Vec<Vec<Option<f64>>> = raw
            .iter()
            .filter_map(|o| parse_judge_ballot(*o, candidates))
            .collect();
        assert_eq!(ballots.len(), 2, "the unparseable judge is dropped");
        let mut r = aggregate_judge(&ballots, candidates, JudgePolicy::Mean);
        r.judges_total = raw.len();
        assert_eq!(r.winner, Some(0), "c0 wins from the two usable ballots");
        assert_eq!(r.judges_counted, 2);
        assert_eq!(r.judges_total, 3, "total reflects the dropped judge");
    }

    #[test]
    fn aggregate_judge_no_usable_ballots_reports_no_winner() {
        // Every judge was dropped → no winner (downstream can tell nothing judged).
        let r = aggregate_judge(&[], 3, JudgePolicy::Mean);
        assert_eq!(r.winner, None, "no ballots → no winner");
        assert_eq!(r.judges_counted, 0);
        // Zero candidates → no winner regardless of ballots.
        let r0 = aggregate_judge(&[vec![]], 0, JudgePolicy::Borda);
        assert_eq!(r0.winner, None, "no candidates → no winner");
    }

    #[test]
    fn resolve_candidate_count_pin_then_infer() {
        // Explicit pin wins.
        assert_eq!(
            resolve_candidate_count(Some(r#"{"aggregate":"mean","candidates":4}"#), &[]),
            4
        );
        // No pin → infer the widest ballot across judges (array len / max key+1).
        let outs = vec![
            Some(r#"{"scores":[0.1,0.2]}"#),
            Some(r#"{"scores":{"3":0.9}}"#),
        ];
        assert_eq!(
            resolve_candidate_count(Some(r#"{"aggregate":"borda"}"#), &outs),
            4,
            "max(2, key3+1=4) = 4"
        );
        // Nothing to infer from → 0.
        assert_eq!(resolve_candidate_count(None, &[Some("garbage")]), 0);
    }

    #[test]
    fn render_judge_summary_leads_with_parseable_winner_marker() {
        let r = aggregate_judge(
            &[
                vec![Some(0.9), Some(0.2), Some(0.5)],
                vec![Some(0.7), Some(0.4), Some(0.6)],
            ],
            3,
            JudgePolicy::Mean,
        );
        let s = render_judge_summary(&r, JudgePolicy::Mean);
        assert!(
            s.starts_with("WINNER: candidate 0"),
            "summary leads with the winner: {s}"
        );
        assert!(s.contains("aggregate=mean"), "policy surfaced: {s}");
        assert!(
            s.contains("scores=["),
            "per-candidate aggregates surfaced: {s}"
        );
        assert!(s.contains("judges=2/2"), "judge count surfaced: {s}");

        // No winner → leads with `WINNER: none`.
        let none = render_judge_summary(
            &aggregate_judge(&[], 3, JudgePolicy::Mean),
            JudgePolicy::Mean,
        );
        assert!(none.starts_with("WINNER: none"), "no-winner marker: {none}");
    }

    // -------------------------------------------------------------------------
    // P2: bounded-parallel scheduling (concurrency, dependency strictness,
    // workspace_dir injection). All-mock: a delay-and-counting WorkerRunner +
    // a diamond DAG (A,B independent → C depends on both).
    // -------------------------------------------------------------------------

    use std::sync::atomic::AtomicUsize;

    /// A→C, B→C diamond: task0 (A, no dep), task1 (B, no dep), task2 (C, depends
    /// on BOTH A and B). With cap≥2, A and B are concurrently dispatchable; C is
    /// only ready after both finish. Each task pre-assigned to member 0.
    struct DiamondPlanProducer;
    #[async_trait]
    impl PlanProducer for DiamondPlanProducer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<PlannedDag, AppError> {
            Ok(PlannedDag {
                tasks: vec![
                    PlannedTask {
                        title: "A".to_string(),
                        spec: "do A".to_string(),
                        task_profile: None,
                        depends_on: vec![],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    },
                    PlannedTask {
                        title: "B".to_string(),
                        spec: "do B".to_string(),
                        task_profile: None,
                        depends_on: vec![],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    },
                    PlannedTask {
                        title: "C".to_string(),
                        spec: "do C".to_string(),
                        task_profile: None,
                        depends_on: vec![0, 1],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    },
                ],
            })
        }
    }

    /// WorkerRunner that records peak concurrency (a live counter incremented on
    /// entry / decremented on exit, tracking the max seen), the per-task start
    /// order, and the `workspace_dir` it was handed. Each call sleeps `delay`
    /// (after firing `on_started`) to create overlap windows.
    struct ConcurrencyMockWorkerRunner {
        delay: Duration,
        live: AtomicUsize,
        max_concurrent: AtomicUsize,
        start_order: Mutex<Vec<String>>,
        seen_workspace_dir: Mutex<Vec<Option<String>>>,
    }
    impl ConcurrencyMockWorkerRunner {
        fn new(delay: Duration) -> Self {
            Self {
                delay,
                live: AtomicUsize::new(0),
                max_concurrent: AtomicUsize::new(0),
                start_order: Mutex::new(vec![]),
                seen_workspace_dir: Mutex::new(vec![]),
            }
        }
    }
    #[async_trait]
    impl WorkerRunner for ConcurrencyMockWorkerRunner {
        async fn run(
            &self,
            _member: &FleetMember,
            workspace_dir: Option<&str>,
            _run_id: &str,
            task_id: &str,
            _brief: &str,
            _task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            // Record the workspace_dir + start order under the live count bump so
            // the peak reflects真实 overlap.
            self.seen_workspace_dir
                .lock()
                .unwrap()
                .push(workspace_dir.map(str::to_string));
            self.start_order.lock().unwrap().push(task_id.to_string());
            let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            // Track the max concurrency seen.
            self.max_concurrent.fetch_max(now, Ordering::SeqCst);

            on_started(900); // arbitrary fixed conv id
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.live.fetch_sub(1, Ordering::SeqCst);
            Ok(WorkerOutcome {
                conversation_id: 900,
                text: Some(format!("output of {task_id}")),
                ok: true,
                tokens: None,
            })
        }
    }

    /// Build a harness whose worker is a shared `ConcurrencyMockWorkerRunner`
    /// (returned alongside) and whose planner is the diamond DAG. `cap` is the
    /// run's `max_parallel`; `workspace_dir` is seeded onto the workspace row so
    /// the engine resolves + injects it. Returns (RunService, RunEngine, the mock
    /// worker for assertions, the seeded run id).
    async fn diamond_harness(
        cap: i64,
        workspace_dir: Option<&str>,
        delay: Duration,
    ) -> (
        RunService,
        RunEngine,
        Arc<ConcurrencyMockWorkerRunner>,
        String,
    ) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let emitter = OrchestratorRunEventEmitter::new(broadcaster);
        let planner: Arc<dyn PlanProducer> = Arc::new(DiamondPlanProducer);

        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );

        let worker = Arc::new(ConcurrencyMockWorkerRunner::new(delay));
        let worker_dyn: Arc<dyn WorkerRunner> = worker.clone();
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker_dyn, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(10);
        let engine = RunEngine::new(Arc::new(engine_deps));

        // Seed: fleet (one member) → workspace (with workspace_dir) → run (cap).
        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "diamond fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet create");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "diamond ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: workspace_dir.map(str::to_string),
                },
            )
            .await
            .expect("ws create");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "build the diamond".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: Some(cap),
                },
            )
            .await
            .expect("run create");
        run_service.plan(&run.id).await.expect("plan");
        (run_service, engine, worker, run.id)
    }

    /// Poll get_detail until the run reaches `completed` (bounded). Returns the
    /// final RunDetail.
    async fn drive_to_completion(
        run_service: &RunService,
        run_id: &str,
    ) -> nomifun_api_types::RunDetail {
        for _ in 0..100 {
            let d = run_service.get_detail(run_id).await.expect("detail");
            if d.run.status == "completed" || d.run.status == "failed" {
                return d;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("run did not reach a terminal status within the bounded poll");
    }

    // -------------------------------------------------------------------------
    // Phase 1b Task 1: an ad-hoc/conversation-native run with NO work_dir and NO
    // workspace gets a per-run SHARED working directory auto-allocated under
    // `<data_dir>/orchestrator/runs/{run_id}`, persisted to `run.work_dir`, and
    // handed to EVERY node (so files + a shared workpath KB are shared). All-mock:
    // the diamond planner + a temp `data_dir` on `RunEngineDeps`.
    // -------------------------------------------------------------------------

    /// Process-unique suffix so parallel/repeat runs never collide on the temp
    /// `data_dir` base (the run subdir is already run-id-unique, this belts it).
    static ADHOC_TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Build an ad-hoc (workspace-less) run with NO work_dir + the diamond planner,
    /// wired to a fresh temp `data_dir` on `RunEngineDeps` so the engine can
    /// auto-allocate a per-run shared workspace directory. Autonomous (no approval
    /// gate) so `engine.start` drives it straight to completion. Returns
    /// `(svc, engine, run_id, data_dir)`.
    async fn adhoc_no_dir_harness(
        worker: Arc<ConcurrencyMockWorkerRunner>,
    ) -> (RunService, RunEngine, String, std::path::PathBuf) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let emitter = OrchestratorRunEventEmitter::new(broadcaster);
        let planner: Arc<dyn PlanProducer> = Arc::new(DiamondPlanProducer);

        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo,
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );

        // Unique temp data_dir so the auto-allocated run dir is real + isolated.
        let data_dir = std::env::temp_dir().join(format!(
            "nomifun-test-adhoc-{}-{}",
            now_ms(),
            ADHOC_TEST_DIR_SEQ.fetch_add(1, Ordering::SeqCst)
        ));

        let worker_dyn: Arc<dyn WorkerRunner> = worker;
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker_dyn, emitter, ws_repo);
        engine_deps.worker_timeout = Duration::from_secs(10);
        engine_deps.data_dir = data_dir.clone();
        let engine = RunEngine::new(Arc::new(engine_deps));

        // Ad-hoc run: workspace-less, NO work_dir, autonomous (no approval gate).
        // `create_adhoc` synthesizes the fleet from the model range → member 0
        // exists for the diamond's `member_index: Some(0)` tasks.
        let run = run_service
            .create_adhoc(
                "u1",
                CreateAdhocRunRequest {
                    approval_mode: None,
                    goal: "share one dir".to_string(),
                    work_dir: None,
                    model_range: ModelRange::Single {
                        model: ModelRef {
                            provider_id: "prov_x".to_string(),
                            model: "claude-opus-4-8".to_string(),
                        },
                    },
                    pinned_roles: vec![],
                    role_members: vec![],
                    autonomy: Some("autonomous".to_string()),
                    max_parallel: Some(2),
                    lead_conv_id: None,
                    lead_model: None,
                },
            )
            .await
            .expect("create_adhoc");
        assert!(run.workspace_id.is_none(), "ad-hoc run has no workspace");
        assert!(run.work_dir.is_none(), "ad-hoc run starts with no work_dir");
        run_service.plan(&run.id).await.expect("plan");
        (run_service, engine, run.id, data_dir)
    }

    #[tokio::test]
    async fn adhoc_run_auto_allocates_shared_workspace_dir() {
        // 一个无 work_dir / 无 workspace 的 ad-hoc run:引擎应自动分配一个
        // <data_dir>/orchestrator/runs/{run_id} 共享目录,持久化到 run.work_dir,
        // 并把它作为 workspace_dir 传给每个节点(此前每节点各自临时目录、无法共享)。
        let worker = Arc::new(ConcurrencyMockWorkerRunner::new(Duration::from_millis(10)));
        let (svc, engine, run_id, data_dir) = adhoc_no_dir_harness(worker.clone()).await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        // 持久化了 work_dir
        let expected = data_dir.join("orchestrator").join("runs").join(&run_id);
        assert_eq!(
            detail.run.work_dir.as_deref(),
            Some(expected.to_str().unwrap()),
            "auto-allocated shared dir must be persisted to run.work_dir"
        );
        assert!(expected.is_dir(), "shared dir must be created on disk");
        // 每个节点都拿到同一个共享目录
        let dirs = worker.seen_workspace_dir.lock().unwrap().clone();
        assert!(!dirs.is_empty());
        for d in &dirs {
            assert_eq!(
                d.as_deref(),
                expected.to_str(),
                "every node shares the one run dir"
            );
        }
    }

    #[tokio::test]
    async fn run_start_seeds_run_notes_md() {
        // run 启动时在共享目录 seed 一个人类可读的 RUN_NOTES.md:含 run 目标 +
        // 计划(各节点 title)+ 「共享笔记」追加区,供节点经 cwd 内置文件工具读/追加。
        let worker = Arc::new(ConcurrencyMockWorkerRunner::new(Duration::from_millis(10)));
        let (svc, engine, run_id, data_dir) = adhoc_no_dir_harness(worker.clone()).await;
        engine.start(run_id.clone());
        let _ = drive_to_completion(&svc, &run_id).await;
        let notes = data_dir
            .join("orchestrator")
            .join("runs")
            .join(&run_id)
            .join("RUN_NOTES.md");
        assert!(notes.is_file(), "RUN_NOTES.md seeded in the shared dir");
        let body = std::fs::read_to_string(&notes).unwrap();
        assert!(
            body.contains("目标") || body.contains("GOAL"),
            "notes carry the run goal"
        );
        // 计划摘要含节点标题(harness 的 plan 至少含一个已知 title)
        assert!(
            body.contains("共享笔记"),
            "has an append region for node findings"
        );
    }

    // -------------------------------------------------------------------------
    // 迁移 024: transient-error AUTO-RETRY. A single-task plan + a worker mock
    // that fails a configurable number of dispatches (classified retryable or
    // not) before succeeding. Proves a retryable failure (rate limit / timeout)
    // auto-retries to completion (self-heals) without ever permanently failing
    // the run, that the retry budget is bounded, and that a non-retryable error
    // still fails fast — the pre-retry behaviour.
    // -------------------------------------------------------------------------

    /// One-task plan (no deps), pre-assigned to member 0 — the minimal graph for
    /// the per-node retry decision without dependency noise.
    struct SingleTaskPlanProducer;
    #[async_trait]
    impl PlanProducer for SingleTaskPlanProducer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<PlannedDag, AppError> {
            Ok(PlannedDag {
                tasks: vec![PlannedTask {
                    title: "solo".to_string(),
                    spec: "do it".to_string(),
                    task_profile: None,
                    depends_on: vec![],
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                    kind: "agent".to_string(),
                    pattern_config: None,
                }],
            })
        }
    }

    /// Worker mock that FAILS its first `fail_times` dispatches — returning
    /// `ok:false` with no final text, the same shape a turn that ended on a
    /// provider error produces — then SUCCEEDS. `retryable` is what
    /// `last_error_retryable` reports for those failures (the signal the engine
    /// reads to decide auto-retry). `dispatches` counts total `run` calls.
    struct RetryMockWorkerRunner {
        fail_times: usize,
        retryable: bool,
        dispatches: AtomicUsize,
    }
    #[async_trait]
    impl WorkerRunner for RetryMockWorkerRunner {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            task_id: &str,
            _brief: &str,
            _task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            let n = self.dispatches.fetch_add(1, Ordering::SeqCst); // 0-based dispatch index
            on_started(900); // arbitrary fixed conv id (stamps task.conversation_id)
            if n < self.fail_times {
                // Finished turn with NO final text → ok:false (the rate-limit shape).
                Ok(WorkerOutcome {
                    conversation_id: 900,
                    text: None,
                    ok: false,
                    tokens: None,
                })
            } else {
                Ok(WorkerOutcome {
                    conversation_id: 900,
                    text: Some(format!("output of {task_id}")),
                    ok: true,
                    tokens: None,
                })
            }
        }
        async fn last_error_retryable(&self, _conversation_id: &str) -> bool {
            self.retryable
        }
        // Simulates a MARKED provider error (rate-limit / auth / etc.): a marker IS
        // present, so the three-way classification uses `retryable` above —
        // `retryable=true` → Retryable path, `retryable=false` → NonRetryable
        // (immediate fail). This keeps the marker-retry semantics distinct from the
        // NoMarker timeout budget (which `TimeoutMockWorkerRunner` covers).
        async fn last_error_present(&self, _conversation_id: &str) -> bool {
            true
        }
    }

    /// Single-task harness with the retry mock + ZERO backoff (instant retries in
    /// tests) + the given retry budget. Returns (RunService, RunEngine, mock, run id).
    async fn retry_harness(
        fail_times: usize,
        retryable: bool,
        max_retries: usize,
    ) -> (RunService, RunEngine, Arc<RetryMockWorkerRunner>, String) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let emitter = OrchestratorRunEventEmitter::new(broadcaster);
        let planner: Arc<dyn PlanProducer> = Arc::new(SingleTaskPlanProducer);
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );

        let worker = Arc::new(RetryMockWorkerRunner {
            fail_times,
            retryable,
            dispatches: AtomicUsize::new(0),
        });
        let worker_dyn: Arc<dyn WorkerRunner> = worker.clone();
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker_dyn, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(10);
        engine_deps.max_worker_retries = max_retries;
        engine_deps.retry_backoff_base = Duration::ZERO; // instant retries — no wall-clock wait in tests
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "retry fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet create");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "retry ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws create");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "retry test".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: Some(1),
                },
            )
            .await
            .expect("run create");
        run_service.plan(&run.id).await.expect("plan");
        (run_service, engine, worker, run.id)
    }

    #[tokio::test]
    async fn retryable_failure_auto_retries_then_succeeds() {
        // Fails once (retryable), succeeds on the retry → the run self-heals with
        // no manual intervention (the core fix: a transient rate-limit no longer
        // permanently fails the run).
        let (svc, engine, worker, run_id) = retry_harness(1, true, 3).await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            detail.run.status, "completed",
            "a retryable failure must auto-retry to completion"
        );
        assert_eq!(detail.tasks.len(), 1);
        assert_eq!(detail.tasks[0].status, "done", "node done after the retry");
        assert_eq!(detail.tasks[0].attempt, 1, "one retry bumps attempt 0 → 1");
        assert_eq!(
            worker.dispatches.load(Ordering::SeqCst),
            2,
            "dispatched twice: initial + 1 retry"
        );
    }

    #[tokio::test]
    async fn retryable_failure_exhausts_bounded_retries_then_fails() {
        // Always fails (retryable); budget = 2 retries → fails after 1 initial + 2
        // retries (bounded — no infinite retry loop).
        let (svc, engine, worker, run_id) = retry_harness(99, true, 2).await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            detail.run.status, "failed",
            "exhausting the retry budget fails the run"
        );
        assert_eq!(detail.tasks[0].status, "failed");
        assert_eq!(
            detail.tasks[0].attempt, 2,
            "attempt reaches the retry cap (2)"
        );
        assert_eq!(
            worker.dispatches.load(Ordering::SeqCst),
            3,
            "1 initial + 2 retries"
        );
    }

    #[tokio::test]
    async fn non_retryable_failure_fails_immediately_without_retry() {
        // Fails, NOT retryable (auth / billing / bad request) → no retry, immediate
        // failure (the pre-retry behaviour is preserved for permanent errors).
        let (svc, engine, worker, run_id) = retry_harness(99, false, 3).await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            detail.run.status, "failed",
            "a non-retryable failure fails the run at once"
        );
        assert_eq!(detail.tasks[0].status, "failed");
        assert_eq!(detail.tasks[0].attempt, 0, "no retry → attempt stays 0");
        assert_eq!(
            worker.dispatches.load(Ordering::SeqCst),
            1,
            "dispatched exactly once"
        );
    }

    // -------------------------------------------------------------------------
    // 迁移 029 (P1a Task 2): per-node `on_fail` policy + `completed_with_failures`
    // terminal status. A two-INDEPENDENT-node DAG (A + B, no edge) seeded DIRECTLY
    // via the `CreateTaskParams.on_fail` create path (the planner does not emit
    // `on_fail` yet) proves: A's `skip_and_continue` soft-failure lets independent
    // node B deliver and settles the run `completed_with_failures`; A's default
    // `fail_run` policy fails the whole run (zero regression).
    // -------------------------------------------------------------------------

    /// A worker that FAILS any task whose spec contains `"FAIL"` (a MARKED,
    /// non-retryable provider error → immediate permanent failure, attempt stays 0)
    /// and SUCCEEDS every other task. A single instance drives a two-node DAG where
    /// node A fails and independent node B still delivers.
    struct SelectiveFailWorker;
    #[async_trait]
    impl WorkerRunner for SelectiveFailWorker {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            task_id: &str,
            _brief: &str,
            task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            on_started(778);
            if task_spec.contains("FAIL") {
                Ok(WorkerOutcome { conversation_id: 778, text: None, ok: false, tokens: None })
            } else {
                Ok(WorkerOutcome {
                    conversation_id: 778,
                    text: Some(format!("output of {task_id}")),
                    ok: true,
                    tokens: None,
                })
            }
        }
        // A MARKED provider error → the three-way classifier reads
        // `last_error_retryable` (default false) → NonRetryable → the failing node
        // fails immediately with no retry (attempt stays 0).
        async fn last_error_present(&self, _conversation_id: &str) -> bool {
            true
        }
    }

    /// Two INDEPENDENT nodes A + B (no dep edge). Seeded DIRECTLY through the
    /// `CreateTaskParams.on_fail` create path so node A carries an arbitrary
    /// `on_fail` policy (the planner does not emit `on_fail` — 迁移 029). A's spec
    /// contains `"FAIL"` so [`SelectiveFailWorker`] fails it permanently; B
    /// succeeds. No assignments are created — `resolve_task_member` falls back to
    /// member[0]. The run is flipped straight to `running` (the manual seed skips
    /// `persist_dag_and_activate`). Returns (svc, engine, run id).
    async fn two_independent_nodes_harness(a_on_fail: &str) -> (RunService, RunEngine, String) {
        use nomifun_db::CreateTaskParams;
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let emitter = OrchestratorRunEventEmitter::new(broadcaster);
        // Planner is required by RunService::new but never invoked (tasks are seeded
        // manually, `plan()` is not called).
        let planner: Arc<dyn PlanProducer> = Arc::new(SingleTaskPlanProducer);
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );

        let worker: Arc<dyn WorkerRunner> = Arc::new(SelectiveFailWorker);
        let mut engine_deps = RunEngineDeps::new(run_repo.clone(), worker, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(10);
        engine_deps.retry_backoff_base = Duration::ZERO; // instant — no wall-clock wait
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "on_fail fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet create");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "on_fail ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws create");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "on_fail policy test".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: Some(2),
                },
            )
            .await
            .expect("run create");

        // Seed the two independent nodes DIRECTLY (bypassing the planner) so A
        // carries the `on_fail` policy under test; B is a plain default node.
        run_repo
            .create_task(CreateTaskParams {
                run_id: run.id.clone(),
                title: "A".to_string(),
                spec: "FAIL A".to_string(),
                task_profile: None,
                status: "pending".to_string(),
                graph_x: None,
                graph_y: None,
                role: None,
                kind: "agent".to_string(),
                pattern_config: None,
                on_fail: Some(a_on_fail.to_string()),
            })
            .await
            .expect("create task A");
        run_repo
            .create_task(CreateTaskParams {
                run_id: run.id.clone(),
                title: "B".to_string(),
                spec: "do B".to_string(),
                task_profile: None,
                status: "pending".to_string(),
                graph_x: None,
                graph_y: None,
                role: None,
                kind: "agent".to_string(),
                pattern_config: None,
                on_fail: None,
            })
            .await
            .expect("create task B");

        // Activate: flip the run to `running` so the engine picks it up.
        run_repo
            .update_run(
                &run.id,
                UpdateRunParams {
                    status: Some("running".to_string()),
                    summary: None,
                    lead_conv_id: None,
                    total_tokens: None,
                    goal: None,
                    autonomy: None,
                    fleet_snapshot: None,
                    work_dir: None,
                },
            )
            .await
            .expect("activate run");

        (run_service, engine, run.id)
    }

    /// Poll until the run reaches ANY terminal status (completed /
    /// completed_with_failures / failed / cancelled).
    async fn drive_to_terminal(
        run_service: &RunService,
        run_id: &str,
    ) -> nomifun_api_types::RunDetail {
        for _ in 0..100 {
            let d = run_service.get_detail(run_id).await.expect("detail");
            if matches!(
                d.run.status.as_str(),
                "completed" | "completed_with_failures" | "failed" | "cancelled"
            ) {
                return d;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("run did not reach a terminal status within the bounded poll");
    }

    #[tokio::test]
    async fn skip_and_continue_failed_node_completes_with_failures() {
        // Two independent nodes: A permanently fails (on_fail=skip_and_continue),
        // B succeeds. A has no downstream to skip → the run is all-settled with a
        // soft failure → completed_with_failures, and B's product still delivers.
        let (svc, engine, run_id) = two_independent_nodes_harness("skip_and_continue").await;
        engine.start(run_id.clone());
        let detail = drive_to_terminal(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed_with_failures");
        let a = detail.tasks.iter().find(|t| t.title == "A").expect("task A");
        let b = detail.tasks.iter().find(|t| t.title == "B").expect("task B");
        assert_eq!(a.status, "failed");
        assert_eq!(b.status, "done", "independent branch still delivers");
    }

    #[tokio::test]
    async fn fail_run_policy_fails_whole_run() {
        // Default policy: A fails (on_fail="fail_run") → the whole run fails (the
        // current hard-fail behaviour is preserved — zero regression).
        let (svc, engine, run_id) = two_independent_nodes_harness("fail_run").await;
        engine.start(run_id.clone());
        let detail = drive_to_terminal(&svc, &run_id).await;
        assert_eq!(detail.run.status, "failed");
    }

    // -------------------------------------------------------------------------
    // Task 1 (P1a): NO-MARKER (timeout / empty reply) BOUNDED auto-retry. A worker
    // that returns `ok:false, text:None` with NO error marker is now retried under
    // the SEPARATE `max_timeout_retries` budget (distinct from the provider-marker
    // `max_worker_retries` path), so a transient timeout self-heals but a genuinely
    // stuck node still fails after a bounded number of retries.
    // -------------------------------------------------------------------------

    /// Single-task harness parameterized by an arbitrary worker + ZERO backoff
    /// (instant retries in tests). Mirrors [`retry_harness`]'s construction
    /// (`SingleTaskPlanProducer`, one-member fleet) but leaves the retry/timeout
    /// budgets at their [`RunEngineDeps::new`] defaults. Returns (svc, engine, run id).
    async fn single_task_harness(worker: Arc<dyn WorkerRunner>) -> (RunService, RunEngine, String) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let emitter = OrchestratorRunEventEmitter::new(broadcaster);
        let planner: Arc<dyn PlanProducer> = Arc::new(SingleTaskPlanProducer);
        let run_service =
            RunService::new(run_repo.clone(), fleet_repo.clone(), ws_repo.clone(), planner, emitter.clone());

        let mut engine_deps = RunEngineDeps::new(run_repo.clone(), worker, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(10);
        engine_deps.retry_backoff_base = Duration::ZERO; // instant retries — no wall-clock wait in tests
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "timeout fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet create");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "timeout ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws create");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "timeout test".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: Some(1),
                },
            )
            .await
            .expect("run create");
        run_service.plan(&run.id).await.expect("plan");
        (run_service, engine, run.id)
    }

    /// Simulates a worker that TIMES OUT / returns no final text with NO error
    /// marker for the first `fail_times` dispatches, then succeeds. Mirrors the
    /// production timeout/empty shape: `ok:false, text:None`, and (by NOT
    /// overriding `last_error_present`) reports no error marker → NoMarker class.
    struct TimeoutMockWorkerRunner {
        fail_times: usize,
        dispatches: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl WorkerRunner for TimeoutMockWorkerRunner {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            _task_id: &str,
            _brief: &str,
            _task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            on_started(900);
            let n = self.dispatches.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                Ok(WorkerOutcome { conversation_id: 900, text: None, ok: false, tokens: None })
            } else {
                Ok(WorkerOutcome { conversation_id: 900, text: Some("ok".into()), ok: true, tokens: None })
            }
        }
        // last_error_present defaults false, last_error_retryable defaults false → NoMarker.
    }

    #[tokio::test]
    async fn timeout_without_marker_retries_bounded_then_succeeds() {
        // Times out once (no error marker) then succeeds → self-heals via the
        // NoMarker timeout budget (NOT the marker-retryable path).
        let worker: Arc<dyn WorkerRunner> =
            Arc::new(TimeoutMockWorkerRunner { fail_times: 1, dispatches: Default::default() });
        let (svc, engine, run_id) = single_task_harness(worker.clone()).await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed", "a no-marker timeout must retry to completion");
        assert_eq!(detail.tasks[0].status, "done");
        assert_eq!(detail.tasks[0].attempt, 1, "one timeout retry bumps attempt 0 → 1");
    }

    #[tokio::test]
    async fn timeout_without_marker_exhausts_timeout_budget_then_fails() {
        // Times out more than DEFAULT_MAX_TIMEOUT_RETRIES → permanent failure.
        let worker: Arc<dyn WorkerRunner> =
            Arc::new(TimeoutMockWorkerRunner { fail_times: 99, dispatches: Default::default() });
        let (svc, engine, run_id) = single_task_harness(worker.clone()).await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "failed", "timeout budget exhausted → run fails");
        assert_eq!(detail.tasks[0].status, "failed");
        assert!(
            detail.tasks[0].attempt as usize >= super::DEFAULT_MAX_TIMEOUT_RETRIES,
            "attempt should reach the timeout budget"
        );
    }

    #[tokio::test]
    async fn cap_2_runs_independent_tasks_concurrently_then_dependent_last() {
        // delay=100ms gives a wide overlap window: with cap=2, A and B must run
        // at the same time (peak concurrency 2); C runs only after both finish.
        let (svc, engine, worker, run_id) =
            diamond_harness(2, Some("/tmp/diamond-ws"), Duration::from_millis(100)).await;

        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;

        // Run completed, all three tasks done with their worker output.
        assert_eq!(detail.run.status, "completed", "diamond run must complete");
        assert_eq!(detail.tasks.len(), 3);
        for t in &detail.tasks {
            assert_eq!(t.status, "done", "task {} must be done", t.title);
            assert_eq!(
                t.output_summary.as_deref(),
                Some(format!("output of {}", t.id).as_str()),
                "task {} output_summary should be the worker text",
                t.title
            );
        }
        let summary = detail.run.summary.expect("summary set");
        assert!(
            summary.contains("3/3"),
            "summary reflects 3/3 done: {summary}"
        );

        // CONCURRENCY PROOF: peak concurrency reached 2 (A and B overlapped).
        let peak = worker.max_concurrent.load(Ordering::SeqCst);
        assert_eq!(
            peak, 2,
            "with cap=2, A and B must run concurrently (peak=2), got {peak}"
        );

        // DEPENDENCY STRICTNESS: C started only after both A and B. The first two
        // starts are A,B (in some order); the third start is C.
        let order = worker.start_order.lock().unwrap().clone();
        assert_eq!(order.len(), 3, "all three tasks ran exactly once");
        let title_of = |id: &str| {
            detail
                .tasks
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.title.clone())
                .unwrap_or_default()
        };
        let titles: Vec<String> = order.iter().map(|id| title_of(id)).collect();
        assert_eq!(
            titles[2], "C",
            "C must be the LAST task to start (after A+B done), got {titles:?}"
        );
        assert!(
            (titles[0] == "A" && titles[1] == "B") || (titles[0] == "B" && titles[1] == "A"),
            "A and B must start before C, got {titles:?}"
        );

        // WORKSPACE_DIR INJECTION: every worker received the run's workspace_dir.
        let dirs = worker.seen_workspace_dir.lock().unwrap().clone();
        assert_eq!(dirs.len(), 3);
        for d in &dirs {
            assert_eq!(
                d.as_deref(),
                Some("/tmp/diamond-ws"),
                "worker must receive the run's workspace_dir"
            );
        }
    }

    #[tokio::test]
    async fn cap_2_total_elapsed_reflects_overlap_not_serial_sum() {
        // Independent A/B plus dependent C. With cap=2, peak worker concurrency
        // is the stable proof that A and B overlapped; wall-clock elapsed is too
        // noisy on a loaded Windows test host.
        let (svc, engine, worker, run_id) =
            diamond_harness(2, None, Duration::from_millis(100)).await;

        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;

        assert_eq!(detail.run.status, "completed");
        let peak = worker.max_concurrent.load(Ordering::SeqCst);
        assert_eq!(
            peak, 2,
            "A/B overlap should reach peak concurrency 2, got {peak}"
        );
    }

    #[tokio::test]
    async fn cap_1_serializes_tasks_peak_concurrency_one() {
        // cap=1 degrades to serial: peak concurrency 1, start order A,B,C
        // (A and B both ready but only one slot, A first; C last after both).
        let (svc, engine, worker, run_id) =
            diamond_harness(1, Some("/tmp/serial-ws"), Duration::from_millis(40)).await;

        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;

        assert_eq!(detail.run.status, "completed", "serial run must complete");
        for t in &detail.tasks {
            assert_eq!(t.status, "done");
        }
        let peak = worker.max_concurrent.load(Ordering::SeqCst);
        assert_eq!(
            peak, 1,
            "with cap=1, no two workers may overlap (peak=1), got {peak}"
        );

        // C is still strictly last; A/B order between them is not constrained.
        let order = worker.start_order.lock().unwrap().clone();
        let title_of = |id: &str| {
            detail
                .tasks
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.title.clone())
                .unwrap_or_default()
        };
        let titles: Vec<String> = order.iter().map(|id| title_of(id)).collect();
        assert_eq!(titles.len(), 3);
        assert_eq!(
            titles[2], "C",
            "C must be last even serially, got {titles:?}"
        );
    }

    #[tokio::test]
    async fn cap_defaults_when_run_max_parallel_absent() {
        // max_parallel=None on the run → engine falls back to default_max_parallel.
        // With a default of 2 and two independent tasks, A and B overlap (peak 2).
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let planner: Arc<dyn PlanProducer> = Arc::new(DiamondPlanProducer);
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let worker = Arc::new(ConcurrencyMockWorkerRunner::new(Duration::from_millis(80)));
        let worker_dyn: Arc<dyn WorkerRunner> = worker.clone();
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker_dyn, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(10);
        engine_deps.default_max_parallel = 2; // explicit default for the assertion
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "f".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "w".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "g".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: None, // <- forces the default fallback
                },
            )
            .await
            .expect("run");
        run_service.plan(&run.id).await.expect("plan");

        engine.start(run.id.clone());
        let detail = drive_to_completion(&run_service, &run.id).await;
        assert_eq!(detail.run.status, "completed");
        let peak = worker.max_concurrent.load(Ordering::SeqCst);
        assert_eq!(
            peak, 2,
            "absent run cap → default_max_parallel=2 governs (peak=2), got {peak}"
        );
    }

    // -------------------------------------------------------------------------
    // P2 Task 3: cancellation propagates to in-flight worker conversations.
    // -------------------------------------------------------------------------

    /// A canceller that records every conversation id it was asked to cancel, so
    /// the test can assert the engine propagated `stop` to the in-flight workers.
    struct RecordingCanceller {
        cancelled: Arc<Mutex<Vec<i64>>>,
    }
    impl RecordingCanceller {
        fn new() -> Self {
            Self {
                cancelled: Arc::new(Mutex::new(vec![])),
            }
        }
        fn handle(&self) -> Arc<Mutex<Vec<i64>>> {
            self.cancelled.clone()
        }
    }
    #[async_trait]
    impl ConversationCanceller for RecordingCanceller {
        async fn cancel(&self, conversation_id: i64) {
            self.cancelled.lock().unwrap().push(conversation_id);
        }
    }

    /// A worker that reports a distinct conversation id per task via `on_started`
    /// (so the in-flight conv id is observable on the running task row) and then
    /// blocks for a long delay — long enough for the test to observe the task
    /// `running` and `stop` the engine while the worker is still in flight.
    struct LongDelayWorkerRunner {
        delay: Duration,
        next_conv_id: AtomicUsize,
    }
    impl LongDelayWorkerRunner {
        fn new(delay: Duration) -> Self {
            Self {
                delay,
                // Start conv ids at a recognizable base so assertions are clear.
                next_conv_id: AtomicUsize::new(5000),
            }
        }
    }
    #[async_trait]
    impl WorkerRunner for LongDelayWorkerRunner {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            task_id: &str,
            _brief: &str,
            _task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            let conv_id = self.next_conv_id.fetch_add(1, Ordering::SeqCst) as i64;
            on_started(conv_id);
            // Block for a long time; the test cancels mid-flight.
            tokio::time::sleep(self.delay).await;
            Ok(WorkerOutcome {
                conversation_id: conv_id,
                text: Some(format!("output of {task_id}")),
                ok: true,
                tokens: None,
            })
        }
    }

    #[tokio::test]
    async fn stop_cancels_in_flight_worker_conversations() {
        // Diamond DAG, cap=2 → A and B run concurrently. A long worker delay keeps
        // both in flight while we cancel. The engine must, on `stop`, cancel the
        // in-flight conversations (the conv ids the running tasks carry).
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let planner: Arc<dyn PlanProducer> = Arc::new(DiamondPlanProducer);
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );

        let worker = Arc::new(LongDelayWorkerRunner::new(Duration::from_secs(30)));
        let worker_dyn: Arc<dyn WorkerRunner> = worker.clone();
        let canceller = Arc::new(RecordingCanceller::new());
        let recorded = canceller.handle();

        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker_dyn, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(60);
        engine_deps.default_max_parallel = 2;
        engine_deps.cancel_conversation = canceller;
        let engine = RunEngine::new(Arc::new(engine_deps));

        // Seed: fleet (one member) → workspace → run (cap=2) → plan.
        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "cancel fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "cancel ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "to be cancelled mid-flight".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: Some(2),
                },
            )
            .await
            .expect("run");
        run_service.plan(&run.id).await.expect("plan");

        engine.start(run.id.clone());

        // Wait until at least one task is `running` with its conversation_id
        // stamped (the on_started detached stamp). Bounded ~200×10ms.
        let mut in_flight_convs: Vec<i64> = vec![];
        for _ in 0..200 {
            let detail = run_service.get_detail(&run.id).await.expect("detail");
            in_flight_convs = detail
                .tasks
                .iter()
                .filter(|t| t.status == "running")
                .filter_map(|t| t.conversation_id)
                .collect();
            if !in_flight_convs.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !in_flight_convs.is_empty(),
            "at least one task must be running with a stamped conversation_id before stop"
        );

        // Stop the engine → it must cancel the in-flight conversations.
        engine.stop(&run.id);
        run_service.cancel(&run.id).await.expect("cancel");

        // The canceller must have received the in-flight conv id(s). `stop`
        // schedules the cancellation on a detached task (it queries running tasks
        // then cancels each), so poll for the records to land. Bounded ~200×10ms.
        let mut got: Vec<i64> = vec![];
        for _ in 0..200 {
            got = recorded.lock().unwrap().clone();
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !got.is_empty(),
            "stop must cancel the in-flight worker conversation(s); none recorded"
        );
        // Every recorded cancel must be one of the in-flight conv ids (this run's).
        for c in &got {
            assert!(
                in_flight_convs.contains(c),
                "cancelled conv {c} must be an in-flight conv of THIS run, in-flight={in_flight_convs:?}"
            );
        }

        // Run persisted as cancelled.
        let detail = run_service.get_detail(&run.id).await.expect("detail");
        assert_eq!(detail.run.status, "cancelled", "run persisted as cancelled");
    }

    // -------------------------------------------------------------------------
    // P3b: pause freezes new dispatch (in-flight finishes), resume completes.
    // -------------------------------------------------------------------------

    /// Records the conversation ids it was asked to steer (P3b steer test).
    struct RecordingSteerer {
        steered: Arc<Mutex<Vec<(i64, String)>>>,
        /// When true, `steer` errors (simulates a non-Nomi engine that cannot steer).
        fail: bool,
    }
    impl RecordingSteerer {
        fn new() -> Self {
            Self {
                steered: Arc::new(Mutex::new(vec![])),
                fail: false,
            }
        }
        fn handle(&self) -> Arc<Mutex<Vec<(i64, String)>>> {
            self.steered.clone()
        }
    }
    #[async_trait]
    impl ConversationSteerer for RecordingSteerer {
        async fn steer(&self, conversation_id: i64, text: &str) -> Result<(), AppError> {
            if self.fail {
                return Err(AppError::BadRequest("steer_unsupported".to_owned()));
            }
            self.steered
                .lock()
                .unwrap()
                .push((conversation_id, text.to_owned()));
            Ok(())
        }
    }

    /// A worker that records its per-task start count + the live concurrency, then
    /// sleeps `delay`. Used by the pause test to observe that no NEW worker starts
    /// while the run is paused (the start count freezes).
    struct CountingWorkerRunner {
        delay: Duration,
        started: AtomicUsize,
        live: AtomicUsize,
        max_concurrent: AtomicUsize,
    }
    impl CountingWorkerRunner {
        fn new(delay: Duration) -> Self {
            Self {
                delay,
                started: AtomicUsize::new(0),
                live: AtomicUsize::new(0),
                max_concurrent: AtomicUsize::new(0),
            }
        }
    }
    #[async_trait]
    impl WorkerRunner for CountingWorkerRunner {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            task_id: &str,
            _brief: &str,
            _task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            self.started.fetch_add(1, Ordering::SeqCst);
            let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_concurrent.fetch_max(now, Ordering::SeqCst);
            on_started(900);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.live.fetch_sub(1, Ordering::SeqCst);
            Ok(WorkerOutcome {
                conversation_id: 900,
                text: Some(format!("output of {task_id}")),
                ok: true,
                tokens: None,
            })
        }
    }

    #[tokio::test]
    async fn pause_freezes_new_dispatch_then_resume_completes() {
        // Diamond DAG, cap=1 → tasks run one at a time. After the FIRST task
        // starts, pause the run: the engine must NOT dispatch the next independent
        // task (start count frozen at 1) while the in-flight one finishes. After
        // resume, the remaining tasks dispatch and the run completes.
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let planner: Arc<dyn PlanProducer> = Arc::new(DiamondPlanProducer);
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );

        // 60ms delay per task gives a window to pause after task 1 starts.
        let worker = Arc::new(CountingWorkerRunner::new(Duration::from_millis(60)));
        let worker_dyn: Arc<dyn WorkerRunner> = worker.clone();
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker_dyn, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(10);
        engine_deps.default_max_parallel = 1;
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "pause fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "pause ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "pause me".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None, // supervised → running after plan
                    max_parallel: Some(1),
                },
            )
            .await
            .expect("run");
        run_service.plan(&run.id).await.expect("plan");

        engine.start(run.id.clone());

        // Wait until the FIRST worker has started, then pause immediately.
        for _ in 0..200 {
            if worker.started.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            worker.started.load(Ordering::SeqCst) >= 1,
            "first worker must start"
        );
        run_service.pause(&run.id).await.expect("pause");

        // While paused, the in-flight worker finishes but NO new worker dispatches.
        // Wait well past the in-flight worker's delay (+ a couple pause-poll ticks)
        // and assert the start count did not grow past 1.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let started_while_paused = worker.started.load(Ordering::SeqCst);
        assert_eq!(
            started_while_paused, 1,
            "paused run must not dispatch a new worker (started={started_while_paused})"
        );
        // The run must still be paused (NOT completed/failed — the engine did not
        // declare a terminal state while paused with pending tasks).
        assert_eq!(
            run_service.get_detail(&run.id).await.unwrap().run.status,
            "paused",
            "run stays paused (not terminal) while idle with pending tasks"
        );
        // Peak concurrency never exceeded the cap.
        assert_eq!(worker.max_concurrent.load(Ordering::SeqCst), 1);

        // Resume → the loop resumes filling and the run completes (all 3 tasks).
        run_service.resume(&run.id).await.expect("resume");
        engine.start(run.id.clone()); // idempotent restart (route does this on resume)
        let detail = drive_to_completion(&run_service, &run.id).await;
        assert_eq!(detail.run.status, "completed", "resumed run completes");
        assert_eq!(
            worker.started.load(Ordering::SeqCst),
            3,
            "all 3 tasks eventually run"
        );
        for t in &detail.tasks {
            assert_eq!(t.status, "done", "task {} done after resume", t.title);
        }
    }

    // -------------------------------------------------------------------------
    // P3b: steer_task injects into the running task's conversation; guards.
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn steer_task_injects_into_running_worker_conversation() {
        // A long-delay worker keeps a task running with a stamped conversation_id;
        // steer_task must call the steerer with THAT conv id + the text.
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );

        let worker = Arc::new(LongDelayWorkerRunner::new(Duration::from_secs(30)));
        let worker_dyn: Arc<dyn WorkerRunner> = worker.clone();
        let steerer = Arc::new(RecordingSteerer::new());
        let recorded = steerer.handle();
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker_dyn, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(60);
        engine_deps.default_max_parallel = 1;
        engine_deps.steer_conversation = steerer;
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "steer fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "steer ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "steer me".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: Some(1),
                },
            )
            .await
            .expect("run");
        run_service.plan(&run.id).await.expect("plan");
        engine.start(run.id.clone());

        // Wait for a task to be running with a stamped conversation_id.
        let mut running_task: Option<(String, i64)> = None;
        for _ in 0..200 {
            let detail = run_service.get_detail(&run.id).await.expect("detail");
            running_task = detail
                .tasks
                .iter()
                .find(|t| t.status == "running")
                .and_then(|t| t.conversation_id.map(|c| (t.id.clone(), c)));
            if running_task.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (task_id, conv_id) = running_task.expect("a running task with conv_id");

        // Steer the running task → the steerer records (conv_id, text).
        engine
            .steer_task(&run.id, &task_id, "focus on the auth module")
            .await
            .expect("steer ok");
        let got = recorded.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "steer must call the steerer exactly once");
        assert_eq!(got[0].0, conv_id, "steered the running task's conversation");
        assert_eq!(got[0].1, "focus on the auth module");

        // Steering did NOT change the run status (still running).
        assert_eq!(
            run_service.get_detail(&run.id).await.unwrap().run.status,
            "running",
            "steer does not change run status"
        );

        engine.stop(&run.id);
    }

    #[tokio::test]
    async fn steer_task_guards_no_conversation_and_unknown_ids() {
        // A run whose only task is `pending` (engine never started) → the task has
        // no conversation_id → steer is a BadRequest. Unknown run / task → NotFound.
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(1, "x"));
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker, emitter, ws_repo.clone());
        engine_deps.steer_conversation = Arc::new(RecordingSteerer::new());
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "guard fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "guard ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "guard me".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: Some(1),
                },
            )
            .await
            .expect("run");
        run_service.plan(&run.id).await.expect("plan");
        // Do NOT start the engine: tasks stay `pending` with no conversation_id.
        let detail = run_service.get_detail(&run.id).await.expect("detail");
        let pending_task = detail.tasks[0].id.clone();

        // No conversation → BadRequest.
        let err = engine
            .steer_task(&run.id, &pending_task, "hello")
            .await
            .expect_err("no-conv steer must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");

        // Empty text → BadRequest.
        let err = engine
            .steer_task(&run.id, &pending_task, "   ")
            .await
            .expect_err("empty text must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");

        // Unknown run → NotFound.
        let err = engine
            .steer_task("run_missing", &pending_task, "hi")
            .await
            .expect_err("unknown run must 404");
        assert!(matches!(err, AppError::NotFound(_)), "got: {err:?}");

        // Unknown task (valid run) → NotFound.
        let err = engine
            .steer_task(&run.id, "rtask_missing", "hi")
            .await
            .expect_err("unknown task must 404");
        assert!(matches!(err, AppError::NotFound(_)), "got: {err:?}");
    }

    // -------------------------------------------------------------------------
    // verify 模式 (UC-1b): end-to-end engine drive. A verify aggregator settles
    // in the FILL step (no worker dispatch, no in-flight slot), tallies its
    // skeptic deps' verdicts, and gates downstream on FAIL (skip dependents) /
    // lets downstream proceed on PASS.
    // -------------------------------------------------------------------------

    /// A worker whose output text is keyed by the task SPEC so a test can make a
    /// skeptic "pass" or "fail" deterministically. Convention:
    /// - spec contains "EMIT:PASS" → output `{"pass":true,"critique":"ok"}`;
    /// - spec contains "EMIT:FAIL" → output `{"pass":false,"critique":"nope"}`;
    /// - otherwise → a plain "did <spec>" output (a normal agent worker).
    ///
    /// Also records its per-task start order, so the test can assert the verify
    /// aggregator NEVER reached a worker (no spin, no dispatch).
    struct VerdictWorkerRunner {
        start_order: Mutex<Vec<String>>,
        seen_specs: Mutex<Vec<String>>,
    }
    impl VerdictWorkerRunner {
        fn new() -> Self {
            Self {
                start_order: Mutex::new(vec![]),
                seen_specs: Mutex::new(vec![]),
            }
        }
    }
    #[async_trait]
    impl WorkerRunner for VerdictWorkerRunner {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            task_id: &str,
            _brief: &str,
            task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            self.start_order.lock().unwrap().push(task_id.to_string());
            self.seen_specs.lock().unwrap().push(task_spec.to_string());
            on_started(900);
            let text = if task_spec.contains("EMIT:PASS") {
                r#"{"pass":true,"critique":"ok"}"#.to_string()
            } else if task_spec.contains("EMIT:FAIL") {
                r#"{"pass":false,"critique":"nope"}"#.to_string()
            } else {
                format!("did {task_spec}")
            };
            Ok(WorkerOutcome {
                conversation_id: 900,
                text: Some(text),
                ok: true,
                tokens: None,
            })
        }
    }

    /// Plan: Build(0) → 3 skeptics(1,2,3 dep on 0) → Gate(4 verify, dep on 1,2,3)
    /// → Deploy(5 dep on 4). Each skeptic's spec is driven by `skeptic_verdicts`
    /// (true → "EMIT:PASS", false → "EMIT:FAIL"); the verify task's vote policy is
    /// `vote_config` (raw pattern_config JSON, or None for the default majority).
    struct VerifyPlanProducer {
        skeptic_verdicts: Vec<bool>,
        vote_config: Option<String>,
    }
    #[async_trait]
    impl PlanProducer for VerifyPlanProducer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<PlannedDag, AppError> {
            let mut tasks = vec![PlannedTask {
                title: "Build".to_string(),
                spec: "build the feature".to_string(),
                task_profile: None,
                depends_on: vec![],
                member_index: Some(0),
                rationale: None,
                role: None,
                kind: "agent".to_string(),
                pattern_config: None,
            }];
            // Skeptic tasks 1..=N, each depending on Build (index 0).
            let mut skeptic_indices = vec![];
            for (i, pass) in self.skeptic_verdicts.iter().enumerate() {
                let idx = tasks.len();
                skeptic_indices.push(idx);
                tasks.push(PlannedTask {
                    title: format!("Skeptic {}", i + 1),
                    spec: format!(
                        "critically evaluate Build; output JSON verdict. EMIT:{}",
                        if *pass { "PASS" } else { "FAIL" }
                    ),
                    task_profile: None,
                    depends_on: vec![0],
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                    kind: "agent".to_string(),
                    pattern_config: None,
                });
            }
            // Verify aggregator depending on every skeptic.
            let verify_idx = tasks.len();
            tasks.push(PlannedTask {
                title: "Gate".to_string(),
                spec: "aggregate skeptic verdicts".to_string(),
                task_profile: None,
                depends_on: skeptic_indices,
                member_index: Some(0),
                rationale: None,
                role: None,
                kind: "verify".to_string(),
                pattern_config: self.vote_config.clone(),
            });
            // Downstream task gated on the verify verdict.
            tasks.push(PlannedTask {
                title: "Deploy".to_string(),
                spec: "deploy the validated feature".to_string(),
                task_profile: None,
                depends_on: vec![verify_idx],
                member_index: Some(0),
                rationale: None,
                role: None,
                kind: "agent".to_string(),
                pattern_config: None,
            });
            Ok(PlannedDag { tasks })
        }
    }

    /// Seed + plan a verify run over a single-member fleet. Returns
    /// (RunService, RunEngine, the verdict worker, run id).
    async fn verify_harness(
        skeptic_verdicts: Vec<bool>,
        vote_config: Option<&str>,
    ) -> (RunService, RunEngine, Arc<VerdictWorkerRunner>, String) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let planner: Arc<dyn PlanProducer> = Arc::new(VerifyPlanProducer {
            skeptic_verdicts,
            vote_config: vote_config.map(str::to_string),
        });
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let worker = Arc::new(VerdictWorkerRunner::new());
        let worker_dyn: Arc<dyn WorkerRunner> = worker.clone();
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker_dyn, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(10);
        engine_deps.default_max_parallel = 4;
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "verify fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "verify ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "build, verify, deploy".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None, // supervised → running after plan
                    max_parallel: Some(4),
                },
            )
            .await
            .expect("run");
        run_service.plan(&run.id).await.expect("plan");
        (run_service, engine, worker, run.id)
    }

    #[tokio::test]
    async fn verify_pass_lets_downstream_proceed() {
        // 3 skeptics, all PASS → majority PASS → Deploy runs; run completes.
        let (svc, engine, worker, run_id) = verify_harness(vec![true, true, true], None).await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            detail.run.status, "completed",
            "PASS verdict → run completes"
        );

        let by_title = |t: &str| detail.tasks.iter().find(|x| x.title == t).cloned().unwrap();
        let gate = by_title("Gate");
        // The verify aggregator is `done` with a PASS verdict and NO worker conv.
        assert_eq!(gate.kind, "verify");
        assert_eq!(gate.status, "done", "verify settled done");
        assert_eq!(
            gate.conversation_id, None,
            "verify has no worker conversation"
        );
        let gate_summary = gate.output_summary.as_deref().unwrap_or("");
        assert!(
            gate_summary.starts_with("VERDICT: PASS"),
            "PASS verdict: {gate_summary}"
        );
        assert!(gate_summary.contains("3/3"), "3/3 tally: {gate_summary}");

        // Downstream Deploy actually ran (it is `done`, not skipped).
        let deploy = by_title("Deploy");
        assert_eq!(deploy.status, "done", "downstream proceeds on PASS");

        // The verify task NEVER reached the worker (no dispatch / no spin): the
        // worker saw exactly Build + 3 skeptics + Deploy = 5 tasks, never "Gate".
        let started = worker.start_order.lock().unwrap().clone();
        assert_eq!(
            started.len(),
            5,
            "worker ran 5 tasks (verify excluded): {started:?}"
        );
        let specs = worker.seen_specs.lock().unwrap().clone();
        assert!(
            !specs
                .iter()
                .any(|s| s.contains("aggregate skeptic verdicts")),
            "the verify task's spec must never reach a worker: {specs:?}"
        );
    }

    #[tokio::test]
    async fn verify_fail_gates_downstream_via_skip() {
        // 3 skeptics, 1 PASS / 2 FAIL → majority FAIL → Deploy skipped; the run
        // still COMPLETES (all tasks done/skipped — verification ran + gated
        // correctly, the run did not fail).
        let (svc, engine, worker, run_id) = verify_harness(vec![true, false, false], None).await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            detail.run.status, "completed",
            "FAIL verdict gates downstream but the run still completes (done/skipped)"
        );

        let by_title = |t: &str| detail.tasks.iter().find(|x| x.title == t).cloned().unwrap();
        let gate = by_title("Gate");
        assert_eq!(
            gate.status, "done",
            "verify itself is done (it computed a verdict)"
        );
        assert_eq!(
            gate.conversation_id, None,
            "verify has no worker conversation"
        );
        let gate_summary = gate.output_summary.as_deref().unwrap_or("");
        assert!(
            gate_summary.starts_with("VERDICT: FAIL"),
            "FAIL verdict: {gate_summary}"
        );
        assert!(gate_summary.contains("1/3"), "1/3 tally: {gate_summary}");

        // Downstream Deploy was GATED → skipped, and never reached the worker.
        let deploy = by_title("Deploy");
        assert_eq!(
            deploy.status, "skipped",
            "downstream gated (skipped) on FAIL"
        );
        assert_eq!(
            deploy.conversation_id, None,
            "skipped downstream never dispatched"
        );

        let specs = worker.seen_specs.lock().unwrap().clone();
        assert!(
            !specs
                .iter()
                .any(|s| s.contains("deploy the validated feature")),
            "gated downstream must never reach a worker: {specs:?}"
        );
        // Build + 3 skeptics ran (4 tasks); verify + Deploy did not.
        assert_eq!(
            worker.start_order.lock().unwrap().len(),
            4,
            "Build + 3 skeptics only"
        );
    }

    #[tokio::test]
    async fn verify_unanimous_one_skeptic_fail_gates() {
        // Unanimous policy: 2 PASS, 1 unparseable-skeptic-as-FAIL would gate, but
        // here we use 3 PASS to prove unanimous PASS proceeds, then a 2/3 case for
        // the fail. First: unanimous PASS → Deploy runs.
        let (svc, engine, _w, run_id) =
            verify_harness(vec![true, true, true], Some(r#"{"vote":"unanimous"}"#)).await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed");
        let deploy = detail.tasks.iter().find(|t| t.title == "Deploy").unwrap();
        assert_eq!(
            deploy.status, "done",
            "unanimous PASS → downstream proceeds"
        );

        // Second: unanimous with one FAIL → gate.
        let (svc2, engine2, _w2, run_id2) =
            verify_harness(vec![true, true, false], Some(r#"{"vote":"unanimous"}"#)).await;
        engine2.start(run_id2.clone());
        let detail2 = drive_to_completion(&svc2, &run_id2).await;
        assert_eq!(
            detail2.run.status, "completed",
            "still completes (done/skipped)"
        );
        let gate2 = detail2.tasks.iter().find(|t| t.title == "Gate").unwrap();
        assert!(
            gate2
                .output_summary
                .as_deref()
                .unwrap_or("")
                .starts_with("VERDICT: FAIL"),
            "unanimous broken by one fail"
        );
        let deploy2 = detail2.tasks.iter().find(|t| t.title == "Deploy").unwrap();
        assert_eq!(deploy2.status, "skipped", "unanimous FAIL gates downstream");
    }

    #[tokio::test]
    async fn verify_threshold_policy_tallies() {
        // Threshold(2): exactly 2 of 3 skeptics pass → PASS (≥ 2) → Deploy runs.
        let (svc, engine, _w, run_id) =
            verify_harness(vec![true, true, false], Some(r#"{"vote":{"threshold":2}}"#)).await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed");
        let gate = detail.tasks.iter().find(|t| t.title == "Gate").unwrap();
        let summary = gate.output_summary.as_deref().unwrap_or("");
        assert!(
            summary.starts_with("VERDICT: PASS"),
            "2 ≥ threshold 2: {summary}"
        );
        assert!(
            summary.contains("threshold:2"),
            "policy surfaced: {summary}"
        );
        let deploy = detail.tasks.iter().find(|t| t.title == "Deploy").unwrap();
        assert_eq!(deploy.status, "done", "threshold met → downstream proceeds");
    }

    #[tokio::test]
    async fn verify_zero_regression_plain_agent_chain_still_runs() {
        // ZERO-REGRESSION: a plain agent chain (no verify task) drives to
        // completion exactly as before — the verify branch must not perturb the
        // ordinary path.
        let h = harness().await;
        let run_id = seed_run(&h).await;
        h.run_service.plan(&run_id).await.expect("plan");
        h.engine.start(run_id.clone());
        let detail = drive_to_completion(&h.run_service, &run_id).await;
        assert_eq!(
            detail.run.status, "completed",
            "plain agent chain unaffected"
        );
        for t in &detail.tasks {
            assert_eq!(t.status, "done", "task {} done", t.title);
            assert_eq!(t.kind, "agent", "no verify kind injected");
        }
    }

    // -------------------------------------------------------------------------
    // judge 模式 (UC-1c): end-to-end engine drive. A judge aggregator settles in
    // the FILL step (no worker dispatch, no in-flight slot), parses its N judges'
    // ballots, aggregates them (mean/borda) to pick a winner, and writes a
    // parseable WINNER marker. NO downstream gate — it reports the winner.
    // -------------------------------------------------------------------------

    /// A worker whose output is keyed by the task SPEC so a test can make a judge
    /// emit a specific ballot deterministically. Convention:
    /// - spec contains "BALLOT:<json>" → output the `<json>` after the marker
    ///   (lets a test inject `{"scores":[..]}` verbatim, or garbage to be dropped);
    /// - otherwise → a plain "did <spec>" output (a normal candidate worker).
    ///
    /// Records its per-task start order + seen specs so the test can assert the
    /// judge aggregator NEVER reached a worker (no spin, no dispatch).
    struct BallotWorkerRunner {
        start_order: Mutex<Vec<String>>,
        seen_specs: Mutex<Vec<String>>,
    }
    impl BallotWorkerRunner {
        fn new() -> Self {
            Self {
                start_order: Mutex::new(vec![]),
                seen_specs: Mutex::new(vec![]),
            }
        }
    }
    #[async_trait]
    impl WorkerRunner for BallotWorkerRunner {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            task_id: &str,
            _brief: &str,
            task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            self.start_order.lock().unwrap().push(task_id.to_string());
            self.seen_specs.lock().unwrap().push(task_spec.to_string());
            on_started(900);
            let text = if let Some(idx) = task_spec.find("BALLOT:") {
                task_spec[idx + "BALLOT:".len()..].trim().to_string()
            } else {
                format!("did {task_spec}")
            };
            Ok(WorkerOutcome {
                conversation_id: 900,
                text: Some(text),
                ok: true,
                tokens: None,
            })
        }
    }

    /// Plan: M candidate agent tasks (0..M) → N judge agent tasks (each dep on ALL
    /// candidates, emitting `judge_ballots[j]` as its BALLOT) → one `judge`
    /// aggregator (dep on all judges) → one downstream Consumer agent (dep on the
    /// judge, to prove no gate / downstream proceeds). `aggregate_config` is the
    /// judge task's raw pattern_config (or None for the default mean).
    struct JudgePlanProducer {
        candidates: usize,
        judge_ballots: Vec<String>,
        aggregate_config: Option<String>,
    }
    #[async_trait]
    impl PlanProducer for JudgePlanProducer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<PlannedDag, AppError> {
            let mut tasks = vec![];
            // M candidate tasks (independent, share a fan-out group tag).
            let mut candidate_indices = vec![];
            for c in 0..self.candidates {
                candidate_indices.push(tasks.len());
                tasks.push(PlannedTask {
                    title: format!("Candidate {c}"),
                    spec: format!("produce alternative {c}"),
                    task_profile: None,
                    depends_on: vec![],
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                    kind: "agent".to_string(),
                    pattern_config: Some("{\"group\":\"candidates\"}".to_string()),
                });
            }
            // N judge tasks, each depending on ALL candidates, emitting its ballot.
            let mut judge_indices = vec![];
            for (j, ballot) in self.judge_ballots.iter().enumerate() {
                judge_indices.push(tasks.len());
                tasks.push(PlannedTask {
                    title: format!("Judge {}", j + 1),
                    spec: format!("score every candidate. BALLOT:{ballot}"),
                    task_profile: None,
                    depends_on: candidate_indices.clone(),
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                    kind: "agent".to_string(),
                    pattern_config: None,
                });
            }
            // The judge aggregator depending on every judge.
            let judge_idx = tasks.len();
            tasks.push(PlannedTask {
                title: "Pick".to_string(),
                spec: "aggregate judge ballots".to_string(),
                task_profile: None,
                depends_on: judge_indices,
                member_index: Some(0),
                rationale: None,
                role: None,
                kind: "judge".to_string(),
                pattern_config: self.aggregate_config.clone(),
            });
            // Downstream consumer depending on the judge (proves NO gate).
            tasks.push(PlannedTask {
                title: "Consumer".to_string(),
                spec: "build on the winning candidate".to_string(),
                task_profile: None,
                depends_on: vec![judge_idx],
                member_index: Some(0),
                rationale: None,
                role: None,
                kind: "agent".to_string(),
                pattern_config: None,
            });
            Ok(PlannedDag { tasks })
        }
    }

    /// Seed + plan a judge run over a single-member fleet. Returns
    /// (RunService, RunEngine, the ballot worker, run id).
    async fn judge_harness(
        candidates: usize,
        judge_ballots: Vec<&str>,
        aggregate_config: Option<&str>,
    ) -> (RunService, RunEngine, Arc<BallotWorkerRunner>, String) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let planner: Arc<dyn PlanProducer> = Arc::new(JudgePlanProducer {
            candidates,
            judge_ballots: judge_ballots.iter().map(|s| s.to_string()).collect(),
            aggregate_config: aggregate_config.map(str::to_string),
        });
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let worker = Arc::new(BallotWorkerRunner::new());
        let worker_dyn: Arc<dyn WorkerRunner> = worker.clone();
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker_dyn, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(10);
        engine_deps.default_max_parallel = 4;
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "judge fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "judge ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "generate candidates, judge, pick".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: Some(4),
                },
            )
            .await
            .expect("run");
        run_service.plan(&run.id).await.expect("plan");
        (run_service, engine, worker, run.id)
    }

    #[tokio::test]
    async fn judge_mean_picks_winner_and_downstream_proceeds() {
        // 3 candidates, 2 judges. Means: c0=0.8, c1=0.3, c2=0.55 → winner c0.
        let (svc, engine, worker, run_id) = judge_harness(
            3,
            vec![r#"{"scores":[0.9,0.2,0.5]}"#, r#"{"scores":[0.7,0.4,0.6]}"#],
            Some(r#"{"aggregate":"mean"}"#),
        )
        .await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed", "judge run completes");

        let by_title = |t: &str| detail.tasks.iter().find(|x| x.title == t).cloned().unwrap();
        let pick = by_title("Pick");
        // The judge aggregator is `done` with a parseable WINNER marker, no conv.
        assert_eq!(pick.kind, "judge");
        assert_eq!(pick.status, "done", "judge settled done");
        assert_eq!(
            pick.conversation_id, None,
            "judge has no worker conversation"
        );
        let pick_summary = pick.output_summary.as_deref().unwrap_or("");
        assert!(
            pick_summary.starts_with("WINNER: candidate 0"),
            "mean winner is c0: {pick_summary}"
        );
        assert!(
            pick_summary.contains("aggregate=mean"),
            "policy surfaced: {pick_summary}"
        );
        assert!(
            pick_summary.contains("judges=2/2"),
            "both judges counted: {pick_summary}"
        );

        // Downstream Consumer actually ran (NO gate — judge reports, doesn't skip).
        let consumer = by_title("Consumer");
        assert_eq!(consumer.status, "done", "downstream proceeds after a judge");

        // The judge task NEVER reached a worker (no dispatch / no spin): the worker
        // saw 3 candidates + 2 judges + 1 consumer = 6 tasks, never "Pick".
        let started = worker.start_order.lock().unwrap().clone();
        assert_eq!(
            started.len(),
            6,
            "worker ran 6 tasks (judge excluded): {started:?}"
        );
        let specs = worker.seen_specs.lock().unwrap().clone();
        assert!(
            !specs.iter().any(|s| s.contains("aggregate judge ballots")),
            "the judge task's spec must never reach a worker: {specs:?}"
        );
    }

    #[tokio::test]
    async fn judge_borda_picks_winner() {
        // 3 candidates, 2 judges, borda:
        // J1: c1(0.9)>c0(0.5)>c2(0.1) → c1=2,c0=1,c2=0.
        // J2: c1(0.8)>c2(0.6)>c0(0.2) → c1=2,c2=1,c0=0.
        // Totals: c0=1,c1=4,c2=1 → winner c1.
        let (svc, engine, _w, run_id) = judge_harness(
            3,
            vec![r#"{"scores":[0.5,0.9,0.1]}"#, r#"{"scores":[0.2,0.8,0.6]}"#],
            Some(r#"{"aggregate":"borda"}"#),
        )
        .await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed");
        let pick = detail.tasks.iter().find(|t| t.title == "Pick").unwrap();
        let summary = pick.output_summary.as_deref().unwrap_or("");
        assert!(
            summary.starts_with("WINNER: candidate 1"),
            "borda winner is c1: {summary}"
        );
        assert!(
            summary.contains("aggregate=borda"),
            "policy surfaced: {summary}"
        );
    }

    #[tokio::test]
    async fn judge_drops_unparseable_ballot_and_still_picks() {
        // 2 candidates, 3 judges; the middle judge emits garbage → dropped. The
        // two usable judges both favor c0 → winner c0, judges=2/3 in the summary.
        let (svc, engine, _w, run_id) = judge_harness(
            2,
            vec![
                r#"{"scores":[0.9,0.1]}"#,
                "the worker crashed, no ballot here",
                r#"{"scores":[0.8,0.2]}"#,
            ],
            None, // default mean
        )
        .await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            detail.run.status, "completed",
            "run completes despite a dropped judge"
        );
        let pick = detail.tasks.iter().find(|t| t.title == "Pick").unwrap();
        let summary = pick.output_summary.as_deref().unwrap_or("");
        assert!(
            summary.starts_with("WINNER: candidate 0"),
            "c0 from 2 usable ballots: {summary}"
        );
        assert!(
            summary.contains("judges=2/3"),
            "one judge dropped fail-safe: {summary}"
        );
        // Downstream still ran (no gate).
        let consumer = detail.tasks.iter().find(|t| t.title == "Consumer").unwrap();
        assert_eq!(consumer.status, "done");
    }

    #[tokio::test]
    async fn judge_zero_regression_plain_agent_chain_still_runs() {
        // ZERO-REGRESSION: a plain agent chain (no judge task) drives to
        // completion exactly as before — the judge branch must not perturb the
        // ordinary path.
        let h = harness().await;
        let run_id = seed_run(&h).await;
        h.run_service.plan(&run_id).await.expect("plan");
        h.engine.start(run_id.clone());
        let detail = drive_to_completion(&h.run_service, &run_id).await;
        assert_eq!(
            detail.run.status, "completed",
            "plain agent chain unaffected"
        );
        for t in &detail.tasks {
            assert_eq!(t.status, "done", "task {} done", t.title);
            assert_eq!(t.kind, "agent", "no judge kind injected");
        }
    }

    // -------------------------------------------------------------------------
    // B4: deep pattern COMPOSITION e2e — fan-out → judge → synthesis. A fan-out
    // group of candidate agents feeds N judges; a `judge` aggregator (no-LLM)
    // picks the winner; a `synthesis` consumer depends on the judge and produces
    // the final deliverable from the winner. This is a plain task structure on the
    // existing engine (patterns are just tasks + deps) — it proves the engine
    // EXECUTES the composition end-to-end, driving to completion.
    // -------------------------------------------------------------------------

    /// A worker for the composition e2e. Records each task's brief (so the test can
    /// assert the synthesis saw the judge's WINNER marker as upstream context) and
    /// keys judge output by a "BALLOT:" marker in the spec (like BallotWorkerRunner).
    struct CompositionWorkerRunner {
        briefs: Mutex<Vec<(String, String)>>, // (task_spec, brief)
        start_order: Mutex<Vec<String>>,
    }
    impl CompositionWorkerRunner {
        fn new() -> Self {
            Self {
                briefs: Mutex::new(vec![]),
                start_order: Mutex::new(vec![]),
            }
        }
    }
    #[async_trait]
    impl WorkerRunner for CompositionWorkerRunner {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            task_id: &str,
            brief: &str,
            task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            self.start_order.lock().unwrap().push(task_id.to_string());
            self.briefs
                .lock()
                .unwrap()
                .push((task_spec.to_string(), brief.to_string()));
            on_started(900);
            let text = if let Some(idx) = task_spec.find("BALLOT:") {
                task_spec[idx + "BALLOT:".len()..].trim().to_string()
            } else {
                format!("did {task_spec}")
            };
            Ok(WorkerOutcome {
                conversation_id: 900,
                text: Some(text),
                ok: true,
                tokens: None,
            })
        }
    }

    /// Plan a fan-out → judge → synthesis COMPOSITION: M candidate agent siblings
    /// sharing a fan-out group tag → N judge agent tasks (each depends on ALL
    /// candidates, emitting its ballot) → ONE `judge` aggregator (depends on all
    /// judges) → ONE `synthesis` consumer (depends on the judge).
    struct CompositionPlanProducer {
        candidates: usize,
        judge_ballots: Vec<String>,
    }
    #[async_trait]
    impl PlanProducer for CompositionPlanProducer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<PlannedDag, AppError> {
            let mut tasks = vec![];
            let mut candidate_indices = vec![];
            for c in 0..self.candidates {
                candidate_indices.push(tasks.len());
                tasks.push(PlannedTask {
                    title: format!("Candidate {c}"),
                    spec: format!("produce alternative {c}"),
                    task_profile: None,
                    depends_on: vec![],
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                    kind: "agent".to_string(),
                    pattern_config: Some("{\"group\":\"candidates\"}".to_string()),
                });
            }
            let mut judge_indices = vec![];
            for (j, ballot) in self.judge_ballots.iter().enumerate() {
                judge_indices.push(tasks.len());
                tasks.push(PlannedTask {
                    title: format!("Judge {}", j + 1),
                    spec: format!("score every candidate. BALLOT:{ballot}"),
                    task_profile: None,
                    depends_on: candidate_indices.clone(),
                    member_index: Some(0),
                    rationale: None,
                    role: None,
                    kind: "agent".to_string(),
                    pattern_config: None,
                });
            }
            let judge_idx = tasks.len();
            tasks.push(PlannedTask {
                title: "Pick".to_string(),
                spec: "aggregate judge ballots".to_string(),
                task_profile: None,
                depends_on: judge_indices,
                member_index: Some(0),
                rationale: None,
                role: None,
                kind: "judge".to_string(),
                pattern_config: Some("{\"aggregate\":\"mean\"}".to_string()),
            });
            // The COMPOSITION's closing step: a synthesis consuming the judge winner.
            tasks.push(PlannedTask {
                title: "Finalize".to_string(),
                spec: "synthesize the final deliverable from the winning candidate".to_string(),
                task_profile: None,
                depends_on: vec![judge_idx],
                member_index: Some(0),
                rationale: None,
                role: None,
                kind: "synthesis".to_string(),
                pattern_config: None,
            });
            Ok(PlannedDag { tasks })
        }
    }

    async fn composition_harness(
        candidates: usize,
        judge_ballots: Vec<&str>,
    ) -> (RunService, RunEngine, Arc<CompositionWorkerRunner>, String) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let planner: Arc<dyn PlanProducer> = Arc::new(CompositionPlanProducer {
            candidates,
            judge_ballots: judge_ballots.iter().map(|s| s.to_string()).collect(),
        });
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let worker = Arc::new(CompositionWorkerRunner::new());
        let worker_dyn: Arc<dyn WorkerRunner> = worker.clone();
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker_dyn, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(10);
        engine_deps.default_max_parallel = 4;
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "composition fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "composition ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "explore alternatives, pick the best, finalize".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: Some(4),
                },
            )
            .await
            .expect("run");
        run_service.plan(&run.id).await.expect("plan");
        (run_service, engine, worker, run.id)
    }

    #[tokio::test]
    async fn composition_fanout_judge_synthesis_drives_to_completion() {
        // 3 candidates, 2 judges. Means: c0=0.8, c1=0.3, c2=0.55 → winner c0. The
        // judge (no-LLM) settles to a WINNER marker; the synthesis consumer then
        // runs WITH the judge's winner as upstream context. The whole composition
        // drives to completion.
        let (svc, engine, worker, run_id) = composition_harness(
            3,
            vec![r#"{"scores":[0.9,0.2,0.5]}"#, r#"{"scores":[0.7,0.4,0.6]}"#],
        )
        .await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            detail.run.status, "completed",
            "fan-out→judge→synthesis completes"
        );

        let by_title = |t: &str| detail.tasks.iter().find(|x| x.title == t).cloned().unwrap();

        // Every task in the composition reached a terminal `done` state.
        for t in &detail.tasks {
            assert_eq!(t.status, "done", "composition task {} done", t.title);
        }

        // The fan-out candidates are agent siblings sharing the group tag.
        for c in 0..3 {
            let cand = by_title(&format!("Candidate {c}"));
            assert_eq!(cand.kind, "agent");
            assert_eq!(
                cand.pattern_config.as_deref(),
                Some("{\"group\":\"candidates\"}"),
                "candidate {c} carries the fan-out group tag"
            );
        }

        // The judge (no-LLM) picked the winner c0 and never reached a worker.
        let pick = by_title("Pick");
        assert_eq!(pick.kind, "judge");
        assert_eq!(
            pick.conversation_id, None,
            "judge has no worker conversation"
        );
        let pick_summary = pick.output_summary.as_deref().unwrap_or("");
        assert!(
            pick_summary.starts_with("WINNER: candidate 0"),
            "judge picks c0: {pick_summary}"
        );

        // The synthesis consumer ran (real worker) AND saw the judge's WINNER marker
        // as upstream context — proving the judge→synthesis edge actually fed data.
        let finalize = by_title("Finalize");
        assert_eq!(finalize.kind, "synthesis");
        assert_eq!(finalize.status, "done", "synthesis consumer completed");
        let briefs = worker.briefs.lock().unwrap().clone();
        let synth_brief = briefs
            .iter()
            .find(|(spec, _)| spec.contains("synthesize the final deliverable"))
            .map(|(_, brief)| brief.clone())
            .expect("synthesis worker ran");
        assert!(
            synth_brief.contains("WINNER: candidate 0"),
            "synthesis saw the judge's winner marker upstream: {synth_brief}"
        );

        // The judge aggregator NEVER reached a worker: 3 candidates + 2 judges + 1
        // synthesis = 6 worker runs (judge excluded — it settles no-LLM).
        let started = worker.start_order.lock().unwrap().clone();
        assert_eq!(
            started.len(),
            6,
            "worker ran 6 tasks (judge excluded): {started:?}"
        );
    }

    // ── loop 模式 (UC-1d): config parse, stop decision (HARD cap wins), dry state ──

    #[test]
    fn loop_config_parse_is_fail_soft_and_always_bounded() {
        // Explicit shapes.
        let c = LoopConfig::parse(Some(r#"{"max_iter":3,"stop":{"kind":"max_iter"}}"#));
        assert_eq!(c.max_iter, 3);
        assert_eq!(c.stop, StopCriteria::MaxIter);

        let c = LoopConfig::parse(Some(
            r#"{"max_iter":4,"stop":{"kind":"predicate","done_marker":"DONE"}}"#,
        ));
        assert_eq!(c.max_iter, 4);
        assert_eq!(
            c.stop,
            StopCriteria::Predicate {
                done_marker: "DONE".to_string()
            }
        );

        let c = LoopConfig::parse(Some(
            r#"{"max_iter":6,"stop":{"kind":"dry","quiet_rounds":2}}"#,
        ));
        assert_eq!(c.max_iter, 6);
        assert_eq!(c.stop, StopCriteria::Dry { quiet_rounds: 2 });

        // Fail-soft: absent / blank / not-JSON → DEFAULT cap-only (bounded, never
        // unbounded). The default max_iter is the small backstop.
        for raw in [None, Some("   "), Some("not json"), Some(r#"{"foo":1}"#)] {
            let c = LoopConfig::parse(raw);
            assert_eq!(
                c.max_iter, DEFAULT_LOOP_MAX_ITER,
                "fail-soft cap for {raw:?}"
            );
            assert_eq!(c.stop, StopCriteria::MaxIter, "fail-soft stop for {raw:?}");
        }

        // Unknown stop kind → cap-only (still bounded).
        let c = LoopConfig::parse(Some(r#"{"max_iter":2,"stop":{"kind":"weird"}}"#));
        assert_eq!(c.max_iter, 2);
        assert_eq!(c.stop, StopCriteria::MaxIter);

        // max_iter omitted → default; max_iter=0 (invalid) → clamped to default
        // (NEVER 0 → never an unbounded/zero loop).
        assert_eq!(
            LoopConfig::parse(Some(r#"{"stop":{"kind":"max_iter"}}"#)).max_iter,
            DEFAULT_LOOP_MAX_ITER
        );
        assert_eq!(
            LoopConfig::parse(Some(r#"{"max_iter":0}"#)).max_iter,
            DEFAULT_LOOP_MAX_ITER
        );

        // predicate with NO marker → degrades to cap-only (a marker-less predicate
        // could never fire, so the cap is the only stop → still bounded).
        let c = LoopConfig::parse(Some(r#"{"max_iter":3,"stop":{"kind":"predicate"}}"#));
        assert_eq!(c.stop, StopCriteria::MaxIter);

        // dry quiet_rounds omitted → defaults to 1 (clamped >= 1).
        let c = LoopConfig::parse(Some(r#"{"max_iter":3,"stop":{"kind":"dry"}}"#));
        assert_eq!(c.stop, StopCriteria::Dry { quiet_rounds: 1 });

        // B4: the verdict-gated stop — {"kind":"approved"} (+ aliases) parses to
        // StopCriteria::Approved; max_iter stays the hard cap.
        for kind in ["approved", "verdict", "verify"] {
            let raw = format!(r#"{{"max_iter":4,"stop":{{"kind":"{kind}"}}}}"#);
            let c = LoopConfig::parse(Some(&raw));
            assert_eq!(c.max_iter, 4, "approved keeps the cap for kind={kind}");
            assert_eq!(c.stop, StopCriteria::Approved, "kind={kind} → Approved");
        }
    }

    #[test]
    fn predicate_done_marker_and_json() {
        assert!(predicate_done(Some("all polished. DONE"), "DONE"));
        assert!(!predicate_done(Some("still working"), "DONE"));
        // JSON {"done":true} anywhere triggers regardless of the text marker.
        assert!(predicate_done(
            Some(r#"result: {"done":true,"note":"ok"}"#),
            "DONE"
        ));
        assert!(!predicate_done(Some(r#"{"done":false}"#), "DONE"));
        // Missing / blank → not done.
        assert!(!predicate_done(None, "DONE"));
        assert!(!predicate_done(Some("   "), "DONE"));
    }

    #[test]
    fn decide_loop_hard_cap_always_wins_even_when_criterion_never_fires() {
        // The no-spin backstop: with a predicate that NEVER matches, the loop must
        // STOP exactly at the cap. max_iter=3 → iterations_done 1,2 CONTINUE; 3 STOP.
        let cfg = LoopConfig {
            max_iter: 3,
            stop: StopCriteria::Predicate {
                done_marker: "NEVER".to_string(),
            },
        };
        assert_eq!(
            decide_loop(&cfg, 1, Some("nope"), &[]),
            LoopDecision::Continue
        );
        assert_eq!(
            decide_loop(&cfg, 2, Some("nope"), &[]),
            LoopDecision::Continue
        );
        assert_eq!(
            decide_loop(&cfg, 3, Some("nope"), &[]),
            LoopDecision::Stop { reason: "max_iter" },
            "HARD cap forces STOP at max_iter regardless of the criterion"
        );
        // And it can NEVER exceed the cap (defensive: iterations_done > max_iter).
        assert_eq!(
            decide_loop(&cfg, 99, Some("nope"), &[]),
            LoopDecision::Stop { reason: "max_iter" }
        );
    }

    #[test]
    fn decide_loop_predicate_stops_early_under_the_cap() {
        let cfg = LoopConfig {
            max_iter: 10,
            stop: StopCriteria::Predicate {
                done_marker: "DONE".to_string(),
            },
        };
        // Under the cap, no marker → CONTINUE.
        assert_eq!(
            decide_loop(&cfg, 1, Some("round 1"), &[]),
            LoopDecision::Continue
        );
        // Marker present (still under the cap) → STOP early (reason predicate).
        assert_eq!(
            decide_loop(&cfg, 2, Some("round 2 DONE"), &[]),
            LoopDecision::Stop {
                reason: "predicate"
            }
        );
    }

    #[test]
    fn decide_loop_dry_stops_after_k_unchanged_rounds() {
        // quiet_rounds=2: STOP once 2 consecutive rounds are identical (this round
        // equals the single prior one). Hash the same output to simulate "no change".
        let cfg = LoopConfig {
            max_iter: 10,
            stop: StopCriteria::Dry { quiet_rounds: 2 },
        };
        let h_a = round_hash(Some("draft A"));
        let h_b = round_hash(Some("draft B"));
        // Round 1: no prior → CONTINUE.
        assert_eq!(
            decide_loop(&cfg, 1, Some("draft A"), &[]),
            LoopDecision::Continue
        );
        // Round 2 produced a DIFFERENT output than round 1 → CONTINUE.
        assert_eq!(
            decide_loop(&cfg, 2, Some("draft B"), &[h_a]),
            LoopDecision::Continue
        );
        // Round 3 repeats round 2's output → 2 consecutive equal (rounds 2,3) → STOP.
        assert_eq!(
            decide_loop(&cfg, 3, Some("draft B"), &[h_a, h_b]),
            LoopDecision::Stop { reason: "dry" }
        );

        // quiet_rounds=3 over the SAME history needs 3 consecutive equal; rounds 2,3
        // equal is only 2 → still CONTINUE.
        let cfg3 = LoopConfig {
            max_iter: 10,
            stop: StopCriteria::Dry { quiet_rounds: 3 },
        };
        assert_eq!(
            decide_loop(&cfg3, 3, Some("draft B"), &[h_a, h_b]),
            LoopDecision::Continue
        );
        // A 4th identical round (history h_a,h_b,h_b, this=h_b) → rounds 2,3,4 equal → STOP.
        assert_eq!(
            decide_loop(&cfg3, 4, Some("draft B"), &[h_a, h_b, h_b]),
            LoopDecision::Stop { reason: "dry" }
        );
    }

    #[test]
    fn decide_loop_approved_stops_on_verdict_pass_under_the_cap() {
        // B4 verdict-gated stop: REUSE parse_verdict_pass over the body output.
        // Under the cap, a not-yet-approved (or unparseable) output CONTINUEs; a
        // PASS verdict STOPs early (reason "approved").
        let cfg = LoopConfig {
            max_iter: 10,
            stop: StopCriteria::Approved,
        };
        // Body says FAIL → not approved → CONTINUE.
        assert_eq!(
            decide_loop(
                &cfg,
                1,
                Some(r#"{"pass":false,"critique":"needs work"}"#),
                &[]
            ),
            LoopDecision::Continue
        );
        // Fail-safe: an unparseable / verdict-less output is NOT approval → CONTINUE.
        assert_eq!(
            decide_loop(&cfg, 2, Some("still drafting, no verdict yet"), &[]),
            LoopDecision::Continue
        );
        assert_eq!(
            decide_loop(&cfg, 3, None, &[]),
            LoopDecision::Continue,
            "missing output → not approved"
        );
        // Body emits a PASS verdict → APPROVED → STOP early (still under the cap).
        assert_eq!(
            decide_loop(&cfg, 4, Some(r#"{"pass":true,"critique":"good"}"#), &[]),
            LoopDecision::Stop { reason: "approved" }
        );
        // The text PASS marker fallback also approves.
        assert_eq!(
            decide_loop(&cfg, 5, Some("Self-review: PASS — done"), &[]),
            LoopDecision::Stop { reason: "approved" }
        );
    }

    #[test]
    fn decide_loop_approved_hard_cap_wins_when_never_approved() {
        // BREAK-NEVER-SPIN: a body that NEVER approves itself must STOP exactly at
        // the max_iter cap. The cap is checked FIRST in decide_loop, so the
        // verdict-gated stop can never run unbounded. max_iter=3.
        let cfg = LoopConfig {
            max_iter: 3,
            stop: StopCriteria::Approved,
        };
        assert_eq!(
            decide_loop(&cfg, 1, Some(r#"{"pass":false}"#), &[]),
            LoopDecision::Continue
        );
        assert_eq!(
            decide_loop(&cfg, 2, Some(r#"{"pass":false}"#), &[]),
            LoopDecision::Continue
        );
        assert_eq!(
            decide_loop(&cfg, 3, Some(r#"{"pass":false}"#), &[]),
            LoopDecision::Stop { reason: "max_iter" },
            "the hard cap forces STOP even though the body never approves"
        );
        // Defensive: never exceeds the cap.
        assert_eq!(
            decide_loop(&cfg, 99, Some(r#"{"pass":false}"#), &[]),
            LoopDecision::Stop { reason: "max_iter" }
        );
    }

    #[test]
    fn loop_state_hashes_round_trip() {
        let hashes = vec![111u64, 222u64, 333u64];
        let rendered = render_loop_state(&hashes);
        assert!(
            rendered.starts_with(LOOP_STATE_PREFIX),
            "machine prefix present: {rendered}"
        );
        let parsed = parse_loop_state_hashes(Some(&rendered));
        assert_eq!(parsed, hashes, "hashes survive a render→parse round-trip");
        // Absent / no LOOP-STATE line → empty (fail-soft).
        assert!(parse_loop_state_hashes(None).is_empty());
        assert!(parse_loop_state_hashes(Some("some unrelated text")).is_empty());
    }

    #[test]
    fn render_loop_final_leads_with_parseable_marker() {
        let s = render_loop_final("done", "predicate", 2, 5, Some("the final polished draft"));
        assert!(s.starts_with("LOOP: DONE"), "machine marker leads: {s}");
        assert!(s.contains("reason=predicate"), "reason surfaced: {s}");
        assert!(s.contains("iterations=2"), "iteration count surfaced: {s}");
        assert!(s.contains("max_iter=5"), "cap surfaced: {s}");
        assert!(
            s.contains("the final polished draft"),
            "final body output carried: {s}"
        );

        let f = render_loop_final("failed", "body_failed", 1, 3, None);
        assert!(f.starts_with("LOOP: FAILED"), "failed marker: {f}");
        assert!(f.contains("reason=body_failed"), "failure reason: {f}");
    }

    // ── loop 迭代回看 (UC-1d, 评审 Important): prior-round output carry + inject ──

    #[test]
    fn loop_prior_output_parses_only_a_present_nonblank_field() {
        // Present + non-blank → Some.
        assert_eq!(
            loop_prior_output(Some(r#"{"loop_prior_output":"上一轮草稿"}"#)),
            Some("上一轮草稿".to_string())
        );
        // Coexists with an unrelated key (e.g. a fan-out group tag) → still parsed.
        assert_eq!(
            loop_prior_output(Some(r#"{"group":"g","loop_prior_output":"draft"}"#)),
            Some("draft".to_string())
        );
        // Absent / blank config / not-JSON / missing key / blank value / non-string
        // value → None (no carry → fresh brief).
        for raw in [
            None,
            Some("   "),
            Some("not json"),
            Some(r#"{"group":"g"}"#),
            Some(r#"{"loop_prior_output":"   "}"#),
            Some(r#"{"loop_prior_output":123}"#),
        ] {
            assert_eq!(loop_prior_output(raw), None, "no carry for {raw:?}");
        }
    }

    #[test]
    fn build_body_loop_carry_merges_and_preserves_existing_keys() {
        // Fresh body (no existing config) → a new object carrying the two loop
        // fields.
        let carry = build_body_loop_carry(None, Some("round 1 output"), 2).expect("carry");
        let v: serde_json::Value = serde_json::from_str(&carry).unwrap();
        assert_eq!(
            v.get("loop_prior_output").and_then(|x| x.as_str()),
            Some("round 1 output")
        );
        assert_eq!(v.get("loop_iteration").and_then(|x| x.as_u64()), Some(2));

        // Existing config (e.g. a fan-out group) is PRESERVED while the loop fields
        // are merged in.
        let carry =
            build_body_loop_carry(Some(r#"{"group":"cands"}"#), Some("o"), 3).expect("carry");
        let v: serde_json::Value = serde_json::from_str(&carry).unwrap();
        assert_eq!(
            v.get("group").and_then(|x| x.as_str()),
            Some("cands"),
            "existing key kept"
        );
        assert_eq!(
            v.get("loop_prior_output").and_then(|x| x.as_str()),
            Some("o")
        );
        assert_eq!(v.get("loop_iteration").and_then(|x| x.as_u64()), Some(3));

        // A blank prior output → None (nothing useful to carry → fresh next brief).
        assert_eq!(build_body_loop_carry(None, Some("   "), 2), None);
        assert_eq!(build_body_loop_carry(None, None, 2), None);

        // The merged config round-trips through `loop_prior_output` (the injector).
        let carry = build_body_loop_carry(None, Some("the prior"), 2).unwrap();
        assert_eq!(
            loop_prior_output(Some(&carry)),
            Some("the prior".to_string())
        );
    }

    #[test]
    fn compose_brief_loop_body_iter_ge_1_carries_prior_output() {
        // A body re-run carrying `loop_prior_output` gets a clear 上一轮产出 section
        // appended so it refines the prior round.
        let mut body = task_row_with_kind("agent", "Refine draft", "polish it");
        body.pattern_config = build_body_loop_carry(None, Some("草稿第一版"), 2);
        let brief = compose_brief(Some("写手"), &body, &[], &empty_brief_ctx());
        assert!(
            brief.contains("上一轮产出(请在此基础上改进/迭代):"),
            "iter>=1 body brief carries the prior-output section: {brief}"
        );
        assert!(
            brief.contains("草稿第一版"),
            "the prior round's text is injected: {brief}"
        );
        // The normal framing is still present (role/task/spec).
        assert!(brief.contains("ROLE: 写手"));
        assert!(brief.contains("TASK: Refine draft"));
        assert!(brief.contains("polish it"));
    }

    #[test]
    fn compose_brief_loop_body_first_iteration_has_no_prior_section() {
        // The FIRST iteration (attempt 0) has NO carry (pattern_config is None) → a
        // normal fresh brief, identical to a plain agent task (zero carry, zero
        // section). build_body_loop_carry is never invoked for the first run.
        let body = task_row_with_kind("agent", "Refine draft", "polish it");
        assert_eq!(
            body.pattern_config, None,
            "first iteration body has no carry"
        );
        let brief = compose_brief(Some("写手"), &body, &[], &empty_brief_ctx());
        assert!(
            !brief.contains("上一轮产出"),
            "first iteration brief must NOT carry a prior-output section: {brief}"
        );
    }

    #[test]
    fn compose_brief_non_loop_task_is_byte_for_byte_unchanged() {
        // ZERO-REGRESSION: a task WITHOUT loop_prior_output (every normal
        // agent/synthesis/verify/judge task, and the loop body's first iteration)
        // gets the EXACT pre-existing brief. We assert byte-for-byte against the
        // legacy framing AND that a config without the carry key adds nothing.
        let task = task_row_with_kind("agent", "Synthesize", "write the report");
        let upstream = vec![("Gather".to_string(), "found 12 sources".to_string())];
        let expected = "ROLE: writer\n\nTASK: Synthesize\nSPEC:\nwrite the report\n\nUPSTREAM RESULTS (completed dependencies you can build on):\n- Gather: found 12 sources\n";
        // No pattern_config at all.
        assert_eq!(compose_brief(Some("writer"), &task, &upstream, &empty_brief_ctx()), expected);
        // A pattern_config that does NOT carry the loop key (e.g. a fan-out group)
        // is also a no-op for the brief — same bytes.
        let mut tagged = task.clone();
        tagged.pattern_config = Some(r#"{"group":"g"}"#.to_string());
        assert_eq!(
            compose_brief(Some("writer"), &tagged, &upstream, &empty_brief_ctx()),
            expected,
            "a non-carry pattern_config must not perturb the brief"
        );
    }

    // -------------------------------------------------------------------------
    // loop 模式 (UC-1d): end-to-end engine drive. A loop controller settles in the
    // FILL step (no worker dispatch). On CONTINUE it RESETS the body to re-run in
    // place (attempt++); the HARD max_iter cap guarantees termination.
    // -------------------------------------------------------------------------

    /// A worker that drives the loop BODY deterministically by counting how many
    /// times each task id has run (the re-run count tracks the loop iteration).
    /// The body's per-round output is taken from `rounds` by run order (round n →
    /// `rounds[n-1]`, the last entry repeats so `dry` can be exercised). The body
    /// is recognized by its title appearing in the brief (`compose_brief` leads
    /// with `TASK: <title>`). Non-body tasks (the downstream) emit a plain
    /// "did <spec>".
    ///
    /// Records the per-task start order so a test can assert the body ran exactly N
    /// times (the iteration count) and the loop controller NEVER reached a worker.
    struct LoopBodyWorkerRunner {
        /// task_id → how many times it has run so far.
        run_counts: Mutex<std::collections::HashMap<String, usize>>,
        start_order: Mutex<Vec<String>>,
        seen_specs: Mutex<Vec<String>>,
        /// Briefs the BODY saw, in run order (one per body round). Lets a test
        /// assert the prior round's output is carried into a later iteration.
        body_briefs: Mutex<Vec<String>>,
        /// Body title to recognize (the body is identified by title here).
        body_title: String,
        /// Per-ROUND outputs for the body, applied by run order (round n = index n-1).
        rounds: Vec<String>,
        /// If set, the body FAILS (returns ok:false) on the given round number.
        fail_on_round: Option<usize>,
    }
    impl LoopBodyWorkerRunner {
        fn new(body_title: &str, rounds: Vec<&str>, fail_on_round: Option<usize>) -> Self {
            Self {
                run_counts: Mutex::new(std::collections::HashMap::new()),
                start_order: Mutex::new(vec![]),
                seen_specs: Mutex::new(vec![]),
                body_briefs: Mutex::new(vec![]),
                body_title: body_title.to_string(),
                rounds: rounds.into_iter().map(str::to_string).collect(),
                fail_on_round,
            }
        }
    }
    #[async_trait]
    impl WorkerRunner for LoopBodyWorkerRunner {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            task_id: &str,
            brief: &str,
            task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            self.start_order.lock().unwrap().push(task_id.to_string());
            self.seen_specs.lock().unwrap().push(task_spec.to_string());
            on_started(900);
            // The brief identifies the task by title (compose_brief leads with TASK:).
            let is_body = brief.contains(&format!("TASK: {}", self.body_title));
            if is_body {
                self.body_briefs.lock().unwrap().push(brief.to_string());
                let round = {
                    let mut counts = self.run_counts.lock().unwrap();
                    let n = counts.entry(task_id.to_string()).or_insert(0);
                    *n += 1;
                    *n
                };
                if self.fail_on_round == Some(round) {
                    // Body fails this round → ok:false (no final text).
                    return Ok(WorkerOutcome {
                        conversation_id: 900,
                        text: None,
                        ok: false,
                        tokens: None,
                    });
                }
                let idx = round
                    .saturating_sub(1)
                    .min(self.rounds.len().saturating_sub(1));
                let text = self
                    .rounds
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| "body output".to_string());
                Ok(WorkerOutcome {
                    conversation_id: 900,
                    text: Some(text),
                    ok: true,
                    tokens: None,
                })
            } else {
                Ok(WorkerOutcome {
                    conversation_id: 900,
                    text: Some(format!("did {task_spec}")),
                    ok: true,
                    tokens: None,
                })
            }
        }
        // A failing loop body is a DEFINITE failure (a marked error), NOT a transient
        // timeout: report a marker present so `settle_failed_or_retry` classifies it
        // NonRetryable → fail immediately. This keeps
        // `loop_failing_body_stops_loop_and_gates_downstream_no_infinite_iterate`'s
        // "body ran exactly twice / loop stops failed" semantics under the new
        // NoMarker timeout budget.
        async fn last_error_present(&self, _conversation_id: &str) -> bool {
            true
        }
    }

    /// Plan: BODY(0, agent) → Loop(1, loop, depends_on [0], pattern_config) →
    /// Publish(2, agent, depends_on [1] — gated on the LOOP, not the body). The
    /// loop's pattern_config is `loop_config` raw JSON.
    struct LoopPlanProducer {
        loop_config: String,
    }
    #[async_trait]
    impl PlanProducer for LoopPlanProducer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<PlannedDag, AppError> {
            Ok(PlannedDag {
                tasks: vec![
                    PlannedTask {
                        title: "Refine".to_string(),
                        spec: "refine one round".to_string(),
                        task_profile: None,
                        depends_on: vec![],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    },
                    PlannedTask {
                        title: "Loop".to_string(),
                        spec: "iterate".to_string(),
                        task_profile: None,
                        depends_on: vec![0],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                        kind: "loop".to_string(),
                        pattern_config: Some(self.loop_config.clone()),
                    },
                    PlannedTask {
                        title: "Publish".to_string(),
                        spec: "publish the refined result".to_string(),
                        task_profile: None,
                        depends_on: vec![1],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    },
                ],
            })
        }
    }

    /// Seed + plan a loop run over a single-member fleet. Returns
    /// (RunService, RunEngine, the loop-body worker, run id).
    async fn loop_harness(
        loop_config: &str,
        rounds: Vec<&str>,
        fail_on_round: Option<usize>,
    ) -> (RunService, RunEngine, Arc<LoopBodyWorkerRunner>, String) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let planner: Arc<dyn PlanProducer> = Arc::new(LoopPlanProducer {
            loop_config: loop_config.to_string(),
        });
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let worker = Arc::new(LoopBodyWorkerRunner::new("Refine", rounds, fail_on_round));
        let worker_dyn: Arc<dyn WorkerRunner> = worker.clone();
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker_dyn, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(10);
        engine_deps.default_max_parallel = 4;
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "loop fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "loop ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "iteratively refine then publish".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: Some(4),
                },
            )
            .await
            .expect("run");
        run_service.plan(&run.id).await.expect("plan");
        (run_service, engine, worker, run.id)
    }

    /// Count how many times the BODY (title "Refine") ran in the worker's start
    /// order — each entry is a task id, and the body is the only task that re-runs.
    /// The body's task id is the one that appears MORE THAN ONCE (or the single one
    /// whose detail title is Refine). We pass the run detail to resolve the title.
    fn body_run_count(
        worker: &LoopBodyWorkerRunner,
        detail: &nomifun_api_types::RunDetail,
    ) -> usize {
        let body_id = detail
            .tasks
            .iter()
            .find(|t| t.title == "Refine")
            .map(|t| t.id.clone())
            .unwrap_or_default();
        worker
            .start_order
            .lock()
            .unwrap()
            .iter()
            .filter(|id| **id == body_id)
            .count()
    }

    #[tokio::test]
    async fn loop_max_iter_hard_cap_stops_after_exactly_n_iterations() {
        // NO-SPIN BACKSTOP: a predicate that NEVER fires + max_iter=3. The loop must
        // drive to completion in bounded passes, running the body EXACTLY 3 times,
        // then STOP at the cap. This is the termination guarantee.
        let (svc, engine, worker, run_id) = loop_harness(
            r#"{"max_iter":3,"stop":{"kind":"predicate","done_marker":"NEVER-EMITTED"}}"#,
            vec!["round1", "round2", "round3", "round4-should-not-happen"],
            None,
        )
        .await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            detail.run.status, "completed",
            "bounded loop drives to completion"
        );

        let by_title = |t: &str| detail.tasks.iter().find(|x| x.title == t).cloned().unwrap();
        let body = by_title("Refine");
        let ctrl = by_title("Loop");
        // The body ran EXACTLY 3 times (the cap), tracked by attempt (0-based: the
        // last completed round is attempt 2 → 3 iterations).
        assert_eq!(
            body_run_count(&worker, &detail),
            3,
            "body ran exactly max_iter=3 times"
        );
        assert_eq!(
            body.attempt, 2,
            "body attempt is 2 (3rd round, 0-based) at the cap"
        );
        // The controller is done with the max_iter STOP marker + no worker conv.
        assert_eq!(ctrl.kind, "loop");
        assert_eq!(
            ctrl.status, "done",
            "loop controller settled done at the cap"
        );
        assert_eq!(
            ctrl.conversation_id, None,
            "loop controller has no worker conversation"
        );
        let summary = ctrl.output_summary.as_deref().unwrap_or("");
        assert!(
            summary.starts_with("LOOP: DONE"),
            "machine marker leads: {summary}"
        );
        assert!(
            summary.contains("reason=max_iter"),
            "hard cap reason: {summary}"
        );
        assert!(summary.contains("iterations=3"), "3 iterations: {summary}");
        assert!(
            summary.contains("round3"),
            "final body output carried: {summary}"
        );
        // Downstream ran AFTER the loop finished (gated on the loop, not the body).
        assert_eq!(
            by_title("Publish").status,
            "done",
            "downstream runs after the loop"
        );

        // The loop controller NEVER reached a worker (no dispatch / no spin).
        let specs = worker.seen_specs.lock().unwrap().clone();
        assert!(
            !specs.iter().any(|s| s == "iterate"),
            "the loop controller spec must never reach a worker: {specs:?}"
        );
    }

    #[tokio::test]
    async fn loop_predicate_stops_early_when_body_emits_marker() {
        // The body emits DONE on round 2 → the loop stops EARLY (before the cap of
        // 5). Body runs exactly 2 times.
        let (svc, engine, worker, run_id) = loop_harness(
            r#"{"max_iter":5,"stop":{"kind":"predicate","done_marker":"DONE"}}"#,
            vec![
                "still working",
                "polished now DONE",
                "round3-should-not-happen",
            ],
            None,
        )
        .await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed");

        let by_title = |t: &str| detail.tasks.iter().find(|x| x.title == t).cloned().unwrap();
        assert_eq!(
            body_run_count(&worker, &detail),
            2,
            "predicate stops early after 2 rounds"
        );
        let ctrl = by_title("Loop");
        let summary = ctrl.output_summary.as_deref().unwrap_or("");
        assert!(
            summary.contains("reason=predicate"),
            "early predicate stop: {summary}"
        );
        assert!(summary.contains("iterations=2"), "2 iterations: {summary}");
        assert_eq!(
            by_title("Publish").status,
            "done",
            "downstream runs after the loop"
        );
    }

    #[tokio::test]
    async fn loop_dry_stops_after_k_unchanged_rounds() {
        // quiet_rounds=2 + max_iter=5. The body emits a CHANGING output for 2 rounds
        // then the SAME output → 2 consecutive identical rounds → dry STOP. Outputs:
        // r1="a", r2="b", r3="b" → rounds 2,3 identical → stop after round 3.
        let (svc, engine, worker, run_id) = loop_harness(
            r#"{"max_iter":5,"stop":{"kind":"dry","quiet_rounds":2}}"#,
            vec!["a", "b", "b", "b-should-not-happen"],
            None,
        )
        .await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed");

        let by_title = |t: &str| detail.tasks.iter().find(|x| x.title == t).cloned().unwrap();
        assert_eq!(
            body_run_count(&worker, &detail),
            3,
            "dry stops after 3 rounds (r2==r3)"
        );
        let ctrl = by_title("Loop");
        let summary = ctrl.output_summary.as_deref().unwrap_or("");
        assert!(summary.contains("reason=dry"), "dry stop reason: {summary}");
        assert!(summary.contains("iterations=3"), "3 iterations: {summary}");
        assert_eq!(
            by_title("Publish").status,
            "done",
            "downstream runs after the loop"
        );

        // dry-stop is REACHABLE because the body now refines the prior round: each
        // iteration >=1 sees the previous round's output in its brief, so when it
        // repeats that output the round-hash converges (here r2==r3 → dry). Confirm
        // the carry actually happened (round 2 saw round 1's "a"; round 3 saw "b").
        let briefs = worker.body_briefs.lock().unwrap().clone();
        assert_eq!(briefs.len(), 3, "the body ran 3 rounds");
        assert!(
            !briefs[0].contains("上一轮产出"),
            "round 1 (attempt 0) has no carry: {}",
            briefs[0]
        );
        assert!(
            briefs[1].contains("上一轮产出"),
            "round 2 carries the prior round: {}",
            briefs[1]
        );
        assert!(
            briefs[1].contains('a'),
            "round 2 sees round 1's output 'a': {}",
            briefs[1]
        );
        assert!(
            briefs[2].contains("上一轮产出"),
            "round 3 carries the prior round: {}",
            briefs[2]
        );
        assert!(
            briefs[2].contains('b'),
            "round 3 sees round 2's output 'b': {}",
            briefs[2]
        );
    }

    #[tokio::test]
    async fn loop_body_iteration_carries_prior_round_output_into_brief() {
        // 评审 Important: a refinement loop's body must SEE its prior round's output.
        // max_iter=3 (cap-only, never-firing early stop) so the body runs exactly 3
        // rounds with DISTINCT outputs; assert each iteration >=1 carries the
        // PRECEDING round's output text in its brief (a true refinement loop), and
        // the first iteration does NOT (fresh start).
        let (svc, engine, worker, run_id) = loop_harness(
            r#"{"max_iter":3,"stop":{"kind":"max_iter"}}"#,
            vec!["第一版草稿", "第二版改进", "第三版定稿"],
            None,
        )
        .await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed");
        assert_eq!(
            body_run_count(&worker, &detail),
            3,
            "body ran exactly max_iter=3 rounds"
        );

        let briefs = worker.body_briefs.lock().unwrap().clone();
        assert_eq!(briefs.len(), 3, "three body briefs recorded");

        // Iteration 0 (first round): NO prior-output section — a fresh brief.
        assert!(
            !briefs[0].contains("上一轮产出"),
            "first iteration must NOT carry a prior-output section: {}",
            briefs[0]
        );
        assert!(
            !briefs[0].contains("第一版草稿"),
            "first brief has no prior text: {}",
            briefs[0]
        );

        // Iteration 1: carries iteration 0's output ("第一版草稿").
        assert!(
            briefs[1].contains("上一轮产出(请在此基础上改进/迭代):"),
            "iter 1 brief carries the section: {}",
            briefs[1]
        );
        assert!(
            briefs[1].contains("第一版草稿"),
            "iter 1 sees round 0's output: {}",
            briefs[1]
        );
        assert!(
            !briefs[1].contains("第二版改进"),
            "iter 1 cannot see its own future output: {}",
            briefs[1]
        );

        // Iteration 2: carries iteration 1's output ("第二版改进").
        assert!(
            briefs[2].contains("上一轮产出"),
            "iter 2 brief carries the section: {}",
            briefs[2]
        );
        assert!(
            briefs[2].contains("第二版改进"),
            "iter 2 sees round 1's output: {}",
            briefs[2]
        );

        // The body row's final pattern_config carries the LAST reset's prior output
        // (round 1's "第二版改进", written when CONTINUE reset it for round 2) — the
        // carry channel is the body's pattern_config, not upstream.
        let body = detail.tasks.iter().find(|t| t.title == "Refine").unwrap();
        let carried = loop_prior_output(body.pattern_config.as_deref());
        assert_eq!(
            carried,
            Some("第二版改进".to_string()),
            "body pattern_config carries the prior output"
        );
    }

    #[tokio::test]
    async fn loop_body_attempt_increments_per_iteration() {
        // The body's `attempt` must increment per loop iteration (drives the UI
        // iteration/retry badge). With max_iter=4 and a never-firing predicate, the
        // body's final attempt is 3 (0-based: rounds 1..4 → attempts 0..3).
        let (svc, engine, worker, run_id) = loop_harness(
            r#"{"max_iter":4,"stop":{"kind":"max_iter"}}"#,
            vec!["r1", "r2", "r3", "r4"],
            None,
        )
        .await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed");
        let body = detail.tasks.iter().find(|t| t.title == "Refine").unwrap();
        assert_eq!(
            body.attempt, 3,
            "body attempt increments per iteration (0-based, 4 rounds)"
        );
        assert_eq!(body_run_count(&worker, &detail), 4, "body ran 4 times");
    }

    #[tokio::test]
    async fn loop_failing_body_stops_loop_and_gates_downstream_no_infinite_iterate() {
        // A body that FAILS on round 2 must STOP the loop (failed) and gate the
        // downstream — never iterate a failing body forever. max_iter=5, but the
        // body fails on round 2 so it runs only twice (round 1 ok, round 2 fails).
        let (svc, engine, worker, run_id) = loop_harness(
            r#"{"max_iter":5,"stop":{"kind":"max_iter"}}"#,
            vec!["round1-ok", "round2-will-fail"],
            Some(2),
        )
        .await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        // The run is `failed` (the body failed). The loop controller stopped failed,
        // the downstream was gated (skipped).
        assert_eq!(detail.run.status, "failed", "a failing body fails the run");

        let by_title = |t: &str| detail.tasks.iter().find(|x| x.title == t).cloned().unwrap();
        let body = by_title("Refine");
        let ctrl = by_title("Loop");
        assert_eq!(body.status, "failed", "the body is failed");
        // The body ran only twice (round1 ok → continue → round2 fails) — it did NOT
        // iterate forever on the failure.
        assert_eq!(
            body_run_count(&worker, &detail),
            2,
            "failing body did not iterate forever"
        );
        assert_eq!(
            ctrl.status, "failed",
            "loop controller stops failed on a failing body"
        );
        let summary = ctrl.output_summary.as_deref().unwrap_or("");
        assert!(
            summary.starts_with("LOOP: FAILED"),
            "failed marker: {summary}"
        );
        assert!(
            summary.contains("reason=body_failed"),
            "body-failed reason: {summary}"
        );
        // Downstream Publish was GATED → skipped (never ran on a failing loop).
        assert_eq!(
            by_title("Publish").status,
            "skipped",
            "downstream gated on a failing loop"
        );
        let specs = worker.seen_specs.lock().unwrap().clone();
        assert!(
            !specs.iter().any(|s| s == "publish the refined result"),
            "gated downstream must never reach a worker: {specs:?}"
        );
    }

    #[tokio::test]
    async fn loop_zero_regression_plain_agent_chain_still_runs() {
        // ZERO-REGRESSION: a plain agent chain (no loop task) drives to completion
        // exactly as before — the loop branch must not perturb the ordinary path.
        let h = harness().await;
        let run_id = seed_run(&h).await;
        h.run_service.plan(&run_id).await.expect("plan");
        h.engine.start(run_id.clone());
        let detail = drive_to_completion(&h.run_service, &run_id).await;
        assert_eq!(
            detail.run.status, "completed",
            "plain agent chain unaffected"
        );
        for t in &detail.tasks {
            assert_eq!(t.status, "done", "task {} done", t.title);
            assert_eq!(t.kind, "agent", "no loop kind injected");
        }
    }

    // ── B4: verdict-gated loop stop (iterate until approved, max_iter backstop) ──

    #[tokio::test]
    async fn loop_approved_stops_when_body_self_verifies_pass() {
        // VERDICT-GATED STOP: the body emits a FAIL verdict for the first 2 rounds,
        // then a PASS verdict on round 3 → the loop STOPs at approval (round 3),
        // BEFORE the max_iter=6 cap. Reuses parse_verdict_pass over the body output.
        let (svc, engine, worker, run_id) = loop_harness(
            r#"{"max_iter":6,"stop":{"kind":"approved"}}"#,
            vec![
                r#"draft v1 — self-review: {"pass":false,"critique":"rough"}"#,
                r#"draft v2 — self-review: {"pass":false,"critique":"closer"}"#,
                r#"draft v3 — self-review: {"pass":true,"critique":"good enough"}"#,
                "round4-should-not-happen",
            ],
            None,
        )
        .await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            detail.run.status, "completed",
            "verdict-gated loop drives to completion"
        );

        let by_title = |t: &str| detail.tasks.iter().find(|x| x.title == t).cloned().unwrap();
        // Body ran EXACTLY 3 times (stopped at approval, before the cap of 6).
        assert_eq!(
            body_run_count(&worker, &detail),
            3,
            "loop stops at approval after 3 rounds"
        );
        let body = by_title("Refine");
        assert_eq!(
            body.attempt, 2,
            "body attempt is 2 (3rd round, 0-based) at approval"
        );
        let ctrl = by_title("Loop");
        assert_eq!(ctrl.kind, "loop");
        assert_eq!(
            ctrl.status, "done",
            "loop controller settled done on approval"
        );
        let summary = ctrl.output_summary.as_deref().unwrap_or("");
        assert!(
            summary.starts_with("LOOP: DONE"),
            "machine marker leads: {summary}"
        );
        assert!(
            summary.contains("reason=approved"),
            "early verdict-gated stop: {summary}"
        );
        assert!(
            summary.contains("iterations=3"),
            "stopped at 3 iterations: {summary}"
        );
        assert!(
            summary.contains("draft v3"),
            "final approved body output carried: {summary}"
        );
        // Downstream ran after the loop approved.
        assert_eq!(
            by_title("Publish").status,
            "done",
            "downstream runs after the loop approves"
        );
    }

    #[tokio::test]
    async fn loop_approved_never_passes_stops_at_max_iter_hard_cap() {
        // BREAK-NEVER-SPIN BACKSTOP: a body that NEVER approves itself (always emits
        // FAIL) must STOP at the max_iter=3 HARD cap — the verdict-gated stop can
        // never run unbounded. Body runs EXACTLY 3 times, then the cap forces STOP.
        let (svc, engine, worker, run_id) = loop_harness(
            r#"{"max_iter":3,"stop":{"kind":"approved"}}"#,
            vec![
                r#"v1 {"pass":false}"#,
                r#"v2 {"pass":false}"#,
                r#"v3 {"pass":false}"#,
                "round4-should-not-happen",
            ],
            None,
        )
        .await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            detail.run.status, "completed",
            "the cap guarantees termination"
        );

        let by_title = |t: &str| detail.tasks.iter().find(|x| x.title == t).cloned().unwrap();
        // Body ran EXACTLY 3 times (the cap), never a 4th despite never approving.
        assert_eq!(
            body_run_count(&worker, &detail),
            3,
            "never-approving body stops at the cap"
        );
        let body = by_title("Refine");
        assert_eq!(
            body.attempt, 2,
            "body attempt is 2 (3rd round, 0-based) at the cap"
        );
        let ctrl = by_title("Loop");
        assert_eq!(
            ctrl.status, "done",
            "loop controller settled done at the cap"
        );
        let summary = ctrl.output_summary.as_deref().unwrap_or("");
        assert!(
            summary.contains("reason=max_iter"),
            "hard cap is the stop reason: {summary}"
        );
        assert!(
            summary.contains("iterations=3"),
            "exactly 3 iterations at the cap: {summary}"
        );
        assert_eq!(
            by_title("Publish").status,
            "done",
            "downstream still runs after the capped loop"
        );

        // The loop controller NEVER reached a worker (no spin / no dispatch).
        let specs = worker.seen_specs.lock().unwrap().clone();
        assert!(
            !specs.iter().any(|s| s == "iterate"),
            "the loop controller spec must never reach a worker: {specs:?}"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // UC-2a: manual per-node rerun (reset + cascade + re-activate; reject running)
    // + node spec/prompt fine-tune (gates on running, reflected in next brief).
    // These drive a REAL engine through the chain harness so a rerun re-executes
    // to completion, not just resets state.
    // ─────────────────────────────────────────────────────────────────────────

    /// Records every `(task_id, task_spec)` brief it was asked to run, so a test
    /// can prove an amended spec reaches the worker on the re-run. Always succeeds.
    struct SpecRecordingWorkerRunner {
        seen: Arc<Mutex<Vec<(String, String)>>>,
    }
    impl SpecRecordingWorkerRunner {
        fn new() -> Self {
            Self {
                seen: Arc::new(Mutex::new(vec![])),
            }
        }
        fn handle(&self) -> Arc<Mutex<Vec<(String, String)>>> {
            self.seen.clone()
        }
    }
    #[async_trait]
    impl WorkerRunner for SpecRecordingWorkerRunner {
        async fn run(
            &self,
            _member: &FleetMember,
            _workspace_dir: Option<&str>,
            _run_id: &str,
            task_id: &str,
            _brief: &str,
            task_spec: &str,
            _timeout: Duration,
            on_started: Box<dyn FnOnce(i64) + Send>,
        ) -> Result<WorkerOutcome, AppError> {
            self.seen
                .lock()
                .unwrap()
                .push((task_id.to_string(), task_spec.to_string()));
            on_started(900);
            Ok(WorkerOutcome {
                conversation_id: 900,
                text: Some(format!("output of {task_id}")),
                ok: true,
                tokens: None,
            })
        }
    }

    /// Build a chain harness (A→B→C) whose worker is the supplied dyn runner, with
    /// a real engine. Returns (RunService, RunEngine, run_id) after plan.
    async fn rerun_chain_harness(worker: Arc<dyn WorkerRunner>) -> (RunService, RunEngine, String) {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);
        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let mut engine_deps = RunEngineDeps::new(run_repo, worker, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(5);
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "rerun fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet create");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "rerun ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws create");
        let run = run_service
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "rerun chain".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: None,
                },
            )
            .await
            .expect("run create");
        run_service.plan(&run.id).await.expect("plan");
        (run_service, engine, run.id)
    }

    // UC-3a — conversation-driven re-adjust: the WHOLE reconcile + terminal-run
    // re-activation under the per-run lock (`engine.adjust`), then the route gate
    // re-drives the new pending task to completion with a LIVE loop (no strand).
    //
    // A single-task run is driven to `completed` (its loop deregisters under the
    // lock). We then `engine.adjust` with a plan that KEEPS the completed task +
    // ADDS a new task depending on it: reconcile preserves the done task (NOT
    // re-run — its output survives) and inserts+routes the new pending task, the
    // terminal run is re-activated to `running` UNDER the lock, the route starts a
    // fresh loop, and the run re-drives to `completed` with the kept task still
    // done and the new task now done — the run was never stranded
    // `running`-with-pending-and-no-driver.
    #[tokio::test]
    async fn adjust_completed_run_keeps_done_adds_new_redrives_no_strand() {
        // A planner whose `produce` yields ONE task and whose `adjust` returns a
        // plan the test fills in (after it learns the real kept task id).
        struct AdjustEngineProducer {
            adjusted: Mutex<crate::plan::AdjustedPlan>,
        }
        #[async_trait]
        impl PlanProducer for AdjustEngineProducer {
            async fn produce(
                &self,
                _goal: &str,
                _members: &[FleetMember],
                _sink: Option<&crate::plan::LeadThinkingSink>,
            ) -> Result<PlannedDag, AppError> {
                Ok(PlannedDag {
                    tasks: vec![PlannedTask {
                        title: "原始".to_string(),
                        spec: "do the original work".to_string(),
                        task_profile: None,
                        depends_on: vec![],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    }],
                })
            }
            async fn adjust(
                &self,
                _intent: &str,
                _tasks: &[nomifun_api_types::RunTask],
                _deps: &[nomifun_api_types::RunTaskDep],
                _members: &[FleetMember],
                _sink: Option<&crate::plan::LeadThinkingSink>,
            ) -> Result<crate::plan::AdjustedPlan, AppError> {
                Ok(self.adjusted.lock().unwrap().clone())
            }
        }

        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let producer = Arc::new(AdjustEngineProducer {
            adjusted: Mutex::new(crate::plan::AdjustedPlan { tasks: vec![] }),
        });
        let planner: Arc<dyn PlanProducer> = producer.clone();
        let svc = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(900, "out"));
        let mut engine_deps = RunEngineDeps::new(run_repo, worker, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(5);
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "adjust fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "adjust ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = svc
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "adjust chain".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: None,
                },
            )
            .await
            .expect("run");
        svc.plan(&run.id).await.expect("plan");
        let run_id = run.id;

        // Drive the single task to completion.
        engine.start(run_id.clone());
        let first = drive_to_completion(&svc, &run_id).await;
        assert_eq!(first.run.status, "completed", "initial run completes");
        assert_eq!(first.tasks.len(), 1);
        let kept_id = first.tasks[0].id.clone();
        assert_eq!(first.tasks[0].status, "done");
        // The completed loop deregisters its handle under the lock.
        for _ in 0..200 {
            if !engine.is_running(&run_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !engine.is_running(&run_id),
            "completed run's loop deregistered"
        );

        // Stage the adjusted plan: keep the done task + add one depending on it.
        *producer.adjusted.lock().unwrap() = crate::plan::AdjustedPlan {
            tasks: vec![
                crate::plan::AdjustedNode::Keep {
                    keep: kept_id.clone(),
                },
                crate::plan::AdjustedNode::New(crate::plan::AdjustedNewTask {
                    title: "追加".to_string(),
                    spec: "build on the kept work".to_string(),
                    role: None,
                    kind: "agent".to_string(),
                    pattern_config: None,
                    depends_on: vec![crate::plan::AdjustedDepRef::Kept(kept_id.clone())],
                }),
            ],
        };

        // Adjust under the lock, then apply the EXACT route gate.
        let adjusted_run = engine
            .adjust(&svc, "u1", &run_id, "在已完成工作上追加一步")
            .await
            .expect("adjust");
        assert_eq!(
            adjusted_run.status, "running",
            "terminal run re-activated to running"
        );
        let started = adjusted_run.status == "running" && !engine.is_running(&run_id);
        assert!(
            started,
            "route must (re)start the loop for the re-activated run"
        );
        engine.start(run_id.clone());

        // The re-activated run re-drives to completion WITHOUT re-running the kept
        // task (it was already done) and WITH the new task now done — no strand.
        let second = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            second.run.status, "completed",
            "re-activated run completes (no strand)"
        );
        assert_eq!(second.tasks.len(), 2, "kept + new");
        let kept = second
            .tasks
            .iter()
            .find(|t| t.id == kept_id)
            .expect("kept task survives");
        assert_eq!(kept.status, "done", "kept task stayed done");
        assert_eq!(
            kept.attempt, first.tasks[0].attempt,
            "kept task NOT re-run (attempt unchanged)"
        );
        let new_task = second
            .tasks
            .iter()
            .find(|t| t.title == "追加")
            .expect("new task added");
        assert_eq!(new_task.status, "done", "new task re-driven to done");
        // The dep kept→new was wired (the new task only ran after the kept blocker).
        assert!(
            second
                .deps
                .iter()
                .any(|d| d.blocker_task_id == kept_id && d.blocked_task_id == new_task.id),
            "kept→new dep wired: {:?}",
            second.deps
        );
    }

    // -------------------------------------------------------------------------
    // Phase 3a Task 1 — `add_tasks` runtime APPEND primitive (RunService +
    // RunEngine lock wrapper). A conversation's ONE persistent run can have NEW
    // nodes APPENDED at runtime (Claude-Code-style dispatch-while-running); the
    // append inserts pending tasks + deps in one transaction (empty delete list)
    // and re-arms a terminal run so the loop drives the new work — no strand.
    // -------------------------------------------------------------------------

    /// Minimal `PlannedTask` for the append tests — `agent` kind, no deps, no
    /// pre-assignment (the harness's single-member fleet routes it via the pure
    /// `resolve_assignment_pick` fallback).
    fn planned_task(title: &str, spec: &str) -> PlannedTask {
        PlannedTask {
            title: title.to_string(),
            spec: spec.to_string(),
            task_profile: None,
            depends_on: vec![],
            member_index: None,
            rationale: None,
            role: None,
            kind: "agent".to_string(),
            pattern_config: None,
        }
    }

    /// A run driven to a TERMINAL state gets a NEW node APPENDED via
    /// `engine.add_tasks`; the append re-arms the terminal run to `running`, and
    /// the re-started loop drives the appended node to `done` (the run settles
    /// `completed` again) — the appended pending task is never stranded.
    #[tokio::test]
    async fn add_tasks_appends_to_running_run_and_new_node_runs() {
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(1, "x"));
        let (svc, engine, run_id) = single_task_harness(worker).await;
        engine.start(run_id.clone());
        let first = drive_to_completion(&svc, &run_id).await;
        assert_eq!(first.run.status, "completed", "initial run completes");

        // Append one new node (no deps) onto the now-terminal run.
        let new = vec![planned_task("追加节点", "do the extra work")];
        let rearmed = engine
            .add_tasks(&svc, &run_id, new)
            .await
            .expect("add_tasks");
        assert_eq!(
            rearmed.status, "running",
            "terminal run re-armed to running by the append"
        );

        // The route (re)starts the loop for the re-armed run if it isn't live.
        if !engine.is_running(&run_id) {
            engine.start(run_id.clone());
        }
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed");
        assert!(
            detail
                .tasks
                .iter()
                .any(|t| t.title.contains("追加节点") && t.status == "done"),
            "appended node must run to done: {:?}",
            detail.tasks
        );
    }

    /// An empty append batch is a clean `BadRequest` — a no-op append would only
    /// churn the re-arm, so a caller bug surfaces loudly (mirrors `plan_flat`).
    #[tokio::test]
    async fn add_tasks_empty_is_rejected() {
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(1, "x"));
        let (svc, engine, run_id) = single_task_harness(worker).await;
        assert!(
            engine.add_tasks(&svc, &run_id, vec![]).await.is_err(),
            "an empty add_tasks batch must be rejected"
        );
    }

    // B4 CONCURRENCY-REGRESSION GUARD: the lead LLM call in `engine.adjust` is awaited
    // with the per-run lock NOT held — `compute_adjusted_plan` (the LLM await) runs
    // lock-free; only `apply_adjusted_plan` (pure DB) runs under the lock. This pins
    // the invariant by injecting a planner whose `adjust` BLOCKS (parks on a Notify)
    // for the whole "LLM call", spawning `engine.adjust` so it parks inside the
    // lock-free compute, and proving a CONCURRENT `engine.rerun_task` (which DOES take
    // the same per-run lock) is NOT blocked by the in-flight adjust — it acquires the
    // lock, runs, and returns WELL BEFORE the parked adjust is released. If the lock
    // spanned the LLM await (the pre-B4 monolith), the rerun would deadlock until the
    // adjust's LLM unblocked; the bounded `timeout` would then fire and fail the test.
    #[tokio::test]
    async fn adjust_lead_llm_runs_outside_per_run_lock_no_block() {
        use tokio::sync::Notify;

        // A planner whose `produce` yields ONE task and whose `adjust` PARKS on a
        // Notify (simulating a slow/hanging lead LLM) until the test releases it,
        // recording (via a flag) that it entered the call so the test can wait for it.
        struct BlockingAdjustProducer {
            entered: Arc<Notify>,
            release: Arc<Notify>,
        }
        #[async_trait]
        impl PlanProducer for BlockingAdjustProducer {
            async fn produce(
                &self,
                _goal: &str,
                _members: &[FleetMember],
                _sink: Option<&crate::plan::LeadThinkingSink>,
            ) -> Result<PlannedDag, AppError> {
                Ok(PlannedDag {
                    tasks: vec![PlannedTask {
                        title: "原始".to_string(),
                        spec: "do the original work".to_string(),
                        task_profile: None,
                        depends_on: vec![],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    }],
                })
            }
            async fn adjust(
                &self,
                _intent: &str,
                _tasks: &[nomifun_api_types::RunTask],
                _deps: &[nomifun_api_types::RunTaskDep],
                _members: &[FleetMember],
                _sink: Option<&crate::plan::LeadThinkingSink>,
            ) -> Result<crate::plan::AdjustedPlan, AppError> {
                // Signal we are INSIDE the lead call, then PARK until released — this
                // is the window the per-run lock must NOT be held across.
                self.entered.notify_one();
                self.release.notified().await;
                // Return a no-op keep-everything plan (semantics irrelevant here).
                Ok(crate::plan::AdjustedPlan { tasks: vec![] })
            }
        }

        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let planner: Arc<dyn PlanProducer> = Arc::new(BlockingAdjustProducer {
            entered: entered.clone(),
            release: release.clone(),
        });
        let svc = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(900, "out"));
        let mut engine_deps = RunEngineDeps::new(run_repo, worker, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(5);
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "adjust-lock fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "adjust-lock ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = svc
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "adjust lock".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: None,
                },
            )
            .await
            .expect("run");
        svc.plan(&run.id).await.expect("plan");
        let run_id = run.id;

        // Drive the single task to completion so the loop deregisters (a quiescent
        // run — no live worker, no loop — isolates the lock behavior under test).
        engine.start(run_id.clone());
        let first = drive_to_completion(&svc, &run_id).await;
        assert_eq!(first.run.status, "completed", "initial run completes");
        let task_id = first.tasks[0].id.clone();
        for _ in 0..200 {
            if !engine.is_running(&run_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !engine.is_running(&run_id),
            "completed run's loop deregistered"
        );

        // Spawn `engine.adjust` — it enters the LOCK-FREE `compute_adjusted_plan` and
        // PARKS inside the lead `adjust` call (the LLM window).
        let svc_for_adjust = svc.clone();
        let engine_for_adjust = engine.clone();
        let adjust_run_id = run_id.clone();
        let adjust_handle = tokio::spawn(async move {
            engine_for_adjust
                .adjust(&svc_for_adjust, "u1", &adjust_run_id, "锁外验证")
                .await
        });

        // Wait until the lead call is in-flight (the adjust is parked mid-LLM).
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("lead adjust call must have started");

        // WHILE the adjust is parked mid-LLM, a concurrent `rerun_task` (which takes
        // the SAME per-run lock) MUST NOT be blocked. If the lock spanned the LLM
        // await, this would hang until `release` fires below → the timeout fails the
        // test. It completes promptly because the lock is held ONLY around the pure-DB
        // apply, never across the (still-parked) lead call.
        let rerun = tokio::time::timeout(
            Duration::from_secs(3),
            engine.rerun_task(&svc, "u1", &run_id, &task_id),
        )
        .await
        .expect("rerun must not be blocked by an in-flight (lock-free) adjust LLM")
        .expect("rerun succeeds");
        assert_eq!(
            rerun.status, "running",
            "rerun re-activated the completed run"
        );

        // Release the parked lead call so the adjust finishes (no leaked task / no
        // deadlock). Its apply re-reads FRESH under the lock and reconciles the (now
        // re-activated, single-task) run with a keep-everything plan — it succeeds
        // (the rerun reset the task to `pending`, not `running`, so no reject).
        release.notify_one();
        let adjusted = tokio::time::timeout(Duration::from_secs(5), adjust_handle)
            .await
            .expect("adjust must finish after the lead call is released")
            .expect("adjust task did not panic");
        assert!(adjusted.is_ok(), "adjust completes cleanly: {adjusted:?}");
    }

    // B4: `engine.adjust` streams the lead's adjust-phase thought over WS — the
    // `compute_adjusted_plan` half is handed a phase="adjust" throttle sink, so a
    // planner that emits reasoning/text deltas through it produces
    // `orchestrator.run.leadThinking` events carrying `phase:"adjust"`. This pins the
    // B2-sink wiring on the adjust path (the production call site that B2 left as
    // `None` pending this task).
    #[tokio::test]
    async fn adjust_streams_lead_thinking_phase_adjust() {
        // A planner whose `adjust` drives the sink (when present) with a reasoning +
        // text delta, then returns a keep-everything plan.
        struct StreamingAdjustProducer;
        #[async_trait]
        impl PlanProducer for StreamingAdjustProducer {
            async fn produce(
                &self,
                _goal: &str,
                _members: &[FleetMember],
                _sink: Option<&crate::plan::LeadThinkingSink>,
            ) -> Result<PlannedDag, AppError> {
                Ok(PlannedDag {
                    tasks: vec![PlannedTask {
                        title: "原始".to_string(),
                        spec: "do the original work".to_string(),
                        task_profile: None,
                        depends_on: vec![],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                        kind: "agent".to_string(),
                        pattern_config: None,
                    }],
                })
            }
            async fn adjust(
                &self,
                _intent: &str,
                _tasks: &[nomifun_api_types::RunTask],
                _deps: &[nomifun_api_types::RunTaskDep],
                _members: &[FleetMember],
                sink: Option<&crate::plan::LeadThinkingSink>,
            ) -> Result<crate::plan::AdjustedPlan, AppError> {
                if let Some(sink) = sink {
                    // A long-enough chunk to trip the throttle's char threshold so it
                    // flushes synchronously (the test does not rely on the timer).
                    sink(crate::plan::LeadDeltaKind::Reasoning, &"思".repeat(60));
                    sink(crate::plan::LeadDeltaKind::Text, &"x".repeat(60));
                }
                Ok(crate::plan::AdjustedPlan { tasks: vec![] })
            }
        }

        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let emitter = OrchestratorRunEventEmitter::new(broadcaster.clone());
        let planner: Arc<dyn PlanProducer> = Arc::new(StreamingAdjustProducer);
        let svc = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(900, "out"));
        let mut engine_deps = RunEngineDeps::new(run_repo, worker, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(5);
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "adjust-stream fleet".to_string(),
                    description: None,
                    max_parallel: None,
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "adjust-stream ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = svc
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "adjust stream".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: None,
                },
            )
            .await
            .expect("run");
        svc.plan(&run.id).await.expect("plan");
        let run_id = run.id;

        // Adjust (no need to run the engine loop — the run is `running` post-plan with
        // a pending task, which adjust accepts; the lead call streams via the sink).
        engine
            .adjust(&svc, "u1", &run_id, "请优化")
            .await
            .expect("adjust");

        // The recorder captured leadThinking events carrying phase="adjust" — at least
        // one reasoning and one text chunk fanned out through the throttle.
        let events = broadcaster.events.lock().unwrap().clone();
        let adjust_thinking: Vec<&nomifun_api_types::WebSocketMessage<serde_json::Value>> = events
            .iter()
            .filter(|e| {
                e.name == "orchestrator.run.leadThinking"
                    && e.data.get("phase").and_then(|p| p.as_str()) == Some("adjust")
            })
            .collect();
        assert!(
            !adjust_thinking.is_empty(),
            "adjust must emit phase=adjust leadThinking events; got names {:?}",
            events.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        assert!(
            adjust_thinking
                .iter()
                .any(|e| e.data.get("kind").and_then(|k| k.as_str()) == Some("reasoning")),
            "a reasoning chunk must have streamed: {adjust_thinking:?}"
        );
        assert!(
            adjust_thinking
                .iter()
                .any(|e| e.data.get("kind").and_then(|k| k.as_str()) == Some("text")),
            "a text (draft heartbeat) chunk must have streamed: {adjust_thinking:?}"
        );
        // The run id is stamped on the event so the FE can filter by run.
        assert!(
            adjust_thinking
                .iter()
                .all(|e| e.data.get("run_id").and_then(|r| r.as_str()) == Some(run_id.as_str())),
            "every leadThinking event carries the run id"
        );
    }

    // Rerun a `done` task on a completed run: the task AND its downstream dependents
    // reset to `pending` and re-execute to `done`; the run re-activates from
    // `completed` and reaches `completed` again. (Mirrors the route's `engine.start`.)
    #[tokio::test]
    async fn rerun_done_task_resets_with_cascade_and_re_executes() {
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(900, "out"));
        let (svc, engine, run_id) = rerun_chain_harness(worker).await;

        // First drive to completion (A→B→C all done).
        engine.start(run_id.clone());
        let first = drive_to_completion(&svc, &run_id).await;
        assert_eq!(first.run.status, "completed", "initial run completes");
        let a = first
            .tasks
            .iter()
            .find(|t| t.title == "A")
            .expect("A")
            .clone();
        let b = first.tasks.iter().find(|t| t.title == "B").expect("B");
        let c = first.tasks.iter().find(|t| t.title == "C").expect("C");
        let (a_attempt, b_attempt, c_attempt) = (a.attempt, b.attempt, c.attempt);

        // Rerun the ROOT task A → it + its transitive dependents (B, C) reset.
        let run_after = engine
            .rerun_task(&svc, "u1", &run_id, &a.id)
            .await
            .expect("rerun A");
        assert_eq!(
            run_after.status, "running",
            "completed run re-activated to running"
        );

        // Immediately after reset (before the loop re-drives), A/B/C are pending and
        // their attempt bumped. Read the detail right away — the re-activated loop is
        // not started yet (the service does the reset synchronously).
        let reset = svc.get_detail(&run_id).await.expect("detail");
        for title in ["A", "B", "C"] {
            let t = reset.tasks.iter().find(|t| t.title == title).unwrap();
            assert_eq!(t.status, "pending", "{title} reset to pending");
            assert!(t.output_summary.is_none(), "{title} output cleared");
            assert!(t.conversation_id.is_none(), "{title} conversation cleared");
        }
        let a2 = reset.tasks.iter().find(|t| t.title == "A").unwrap();
        let b2 = reset.tasks.iter().find(|t| t.title == "B").unwrap();
        let c2 = reset.tasks.iter().find(|t| t.title == "C").unwrap();
        assert_eq!(a2.attempt, a_attempt + 1, "A attempt bumped");
        assert_eq!(b2.attempt, b_attempt + 1, "B (dependent) attempt bumped");
        assert_eq!(
            c2.attempt,
            c_attempt + 1,
            "C (transitive dependent) attempt bumped"
        );

        // Drive the re-activated run: the engine re-executes the reset tasks to done.
        engine.start(run_id.clone());
        let second = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            second.run.status, "completed",
            "re-activated run completes again"
        );
        for t in &second.tasks {
            assert_eq!(t.status, "done", "task {} re-executed to done", t.title);
        }
    }

    // engine.start re-drives a previously-COMPLETED run (confirmation): after
    // rerun flips it back to `running`, a plain `engine.start` picks up the
    // now-pending tasks and the run reaches `completed` again. Reruns a LEAF (C)
    // so only C resets — proving the cascade only touches dependents, not A/B.
    #[tokio::test]
    async fn rerun_leaf_resets_only_itself_and_re_completes() {
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(900, "out"));
        let (svc, engine, run_id) = rerun_chain_harness(worker).await;
        engine.start(run_id.clone());
        let first = drive_to_completion(&svc, &run_id).await;
        assert_eq!(first.run.status, "completed");
        let a = first.tasks.iter().find(|t| t.title == "A").unwrap().clone();
        let b = first.tasks.iter().find(|t| t.title == "B").unwrap().clone();
        let c = first.tasks.iter().find(|t| t.title == "C").unwrap().clone();

        engine
            .rerun_task(&svc, "u1", &run_id, &c.id)
            .await
            .expect("rerun C");

        let reset = svc.get_detail(&run_id).await.expect("detail");
        let a2 = reset.tasks.iter().find(|t| t.title == "A").unwrap();
        let b2 = reset.tasks.iter().find(|t| t.title == "B").unwrap();
        let c2 = reset.tasks.iter().find(|t| t.title == "C").unwrap();
        // Only the leaf reset; A/B (upstream of C) untouched (still done, attempt same).
        assert_eq!(a2.status, "done", "A upstream untouched");
        assert_eq!(a2.attempt, a.attempt, "A attempt unchanged");
        assert_eq!(b2.status, "done", "B upstream untouched");
        assert_eq!(b2.attempt, b.attempt, "B attempt unchanged");
        assert_eq!(c2.status, "pending", "C (leaf) reset");
        assert_eq!(c2.attempt, c.attempt + 1, "C attempt bumped");

        engine.start(run_id.clone());
        let second = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            second.run.status, "completed",
            "engine.start re-drove the completed run"
        );
        assert_eq!(
            second.tasks.iter().find(|t| t.title == "C").unwrap().status,
            "done",
            "C re-executed"
        );
    }

    // Rerun a FAILED task: it re-runs. Seed a run whose worker fails the first time
    // then succeeds, so the task lands `failed`, then rerun re-executes it to done.
    #[tokio::test]
    async fn rerun_failed_task_re_executes() {
        // A worker that fails the FIRST call then succeeds — so the run fails first,
        // and the rerun (a fresh call) succeeds.
        struct FlakyWorker {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl WorkerRunner for FlakyWorker {
            async fn run(
                &self,
                _member: &FleetMember,
                _workspace_dir: Option<&str>,
                _run_id: &str,
                task_id: &str,
                _brief: &str,
                _task_spec: &str,
                _timeout: Duration,
                on_started: Box<dyn FnOnce(i64) + Send>,
            ) -> Result<WorkerOutcome, AppError> {
                on_started(900);
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // First call: no final text → the engine marks the task failed.
                    Ok(WorkerOutcome {
                        conversation_id: 900,
                        text: None,
                        ok: false,
                        tokens: None,
                    })
                } else {
                    Ok(WorkerOutcome {
                        conversation_id: 900,
                        text: Some(format!("output of {task_id}")),
                        ok: true,
                        tokens: None,
                    })
                }
            }
            // The first-call failure is a DEFINITE failure (marked error), NOT a
            // transient timeout: report a marker present so it fails immediately
            // (NonRetryable) rather than self-healing under the NoMarker timeout
            // budget — the test relies on the FIRST run landing `failed` before the
            // rerun re-executes it.
            async fn last_error_present(&self, _conversation_id: &str) -> bool {
                true
            }
        }
        let worker: Arc<dyn WorkerRunner> = Arc::new(FlakyWorker {
            calls: AtomicUsize::new(0),
        });
        let (svc, engine, run_id) = rerun_chain_harness(worker).await;

        engine.start(run_id.clone());
        let first = drive_to_completion(&svc, &run_id).await;
        assert_eq!(first.run.status, "failed", "first A fails → run fails");
        let a = first.tasks.iter().find(|t| t.title == "A").unwrap().clone();
        assert_eq!(a.status, "failed", "A failed");

        // Rerun the failed A → re-activates the failed run + re-executes.
        let run_after = engine
            .rerun_task(&svc, "u1", &run_id, &a.id)
            .await
            .expect("rerun failed A");
        assert_eq!(
            run_after.status, "running",
            "failed run re-activated to running"
        );
        engine.start(run_id.clone());
        let second = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            second.run.status, "completed",
            "rerun drives the whole chain to done"
        );
        for t in &second.tasks {
            assert_eq!(t.status, "done", "task {} done after rerun", t.title);
        }
    }

    // Reject rerunning a RUNNING task: a live worker is in flight (gated), so the
    // task is `running` — rerun must 400 (no live-worker clobber).
    #[tokio::test]
    async fn rerun_rejects_running_task() {
        // Gated worker keeps the task `running` until released.
        struct GatedWorker {
            gate: Arc<tokio::sync::Notify>,
        }
        #[async_trait]
        impl WorkerRunner for GatedWorker {
            async fn run(
                &self,
                _member: &FleetMember,
                _workspace_dir: Option<&str>,
                _run_id: &str,
                task_id: &str,
                _brief: &str,
                _task_spec: &str,
                _timeout: Duration,
                on_started: Box<dyn FnOnce(i64) + Send>,
            ) -> Result<WorkerOutcome, AppError> {
                on_started(900);
                self.gate.notified().await;
                Ok(WorkerOutcome {
                    conversation_id: 900,
                    text: Some(format!("output of {task_id}")),
                    ok: true,
                    tokens: None,
                })
            }
        }
        let gate = Arc::new(tokio::sync::Notify::new());
        let worker: Arc<dyn WorkerRunner> = Arc::new(GatedWorker { gate: gate.clone() });
        let (svc, engine, run_id) = rerun_chain_harness(worker).await;
        engine.start(run_id.clone());

        // Wait until task A is `running` (its worker is blocked on the gate).
        let mut running_id = None;
        for _ in 0..200 {
            let d = svc.get_detail(&run_id).await.expect("detail");
            running_id = d
                .tasks
                .iter()
                .find(|t| t.status == "running")
                .map(|t| t.id.clone());
            if running_id.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let running_id = running_id.expect("a task is running");

        let err = engine
            .rerun_task(&svc, "u1", &run_id, &running_id)
            .await
            .expect_err("rerun of a running task must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");

        // Release the gated workers (the chain has 3 gated tasks) so the test does
        // not leak blocked tasks — notify repeatedly until the run settles.
        tokio::spawn({
            let gate = gate.clone();
            async move {
                for _ in 0..20 {
                    gate.notify_one();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        });
        let _ = drive_to_completion(&svc, &run_id).await;
    }

    // Fix C (LIVENESS RECOVERY): a `running` task with NO live loop is a phantom
    // ORPHAN (its worker died — the reported「重启后卡在执行中、无法重跑」bug). Unlike
    // `rerun_rejects_running_task` (a LIVE loop → correctly rejected), here the loop
    // is killed mid-flight (`engine.stop`) so `!is_running`, and rerun must RECOVER:
    // normalize the orphan → pending, then reset + re-drive — never wedge it with a 400.
    #[tokio::test]
    async fn rerun_recovers_phantom_running_when_no_live_loop() {
        // A gated worker parks its task in `running` until released.
        struct GatedWorker {
            gate: Arc<tokio::sync::Notify>,
        }
        #[async_trait]
        impl WorkerRunner for GatedWorker {
            async fn run(
                &self,
                _member: &FleetMember,
                _workspace_dir: Option<&str>,
                _run_id: &str,
                task_id: &str,
                _brief: &str,
                _task_spec: &str,
                _timeout: Duration,
                on_started: Box<dyn FnOnce(i64) + Send>,
            ) -> Result<WorkerOutcome, AppError> {
                on_started(900);
                self.gate.notified().await;
                Ok(WorkerOutcome {
                    conversation_id: 900,
                    text: Some(format!("output of {task_id}")),
                    ok: true,
                    tokens: None,
                })
            }
        }
        let gate = Arc::new(tokio::sync::Notify::new());
        let worker: Arc<dyn WorkerRunner> = Arc::new(GatedWorker { gate: gate.clone() });
        let (svc, engine, run_id) = rerun_chain_harness(worker).await;

        // Drive until task A is live-`running` (its worker parked on the gate).
        engine.start(run_id.clone());
        let mut running_id = None;
        for _ in 0..200 {
            let d = svc.get_detail(&run_id).await.expect("detail");
            running_id = d
                .tasks
                .iter()
                .find(|t| t.status == "running")
                .map(|t| t.id.clone());
            if running_id.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let running_id = running_id.expect("a task reached running");

        // Kill the loop mid-flight: the handle is removed (is_running=false) and the
        // worker future aborted BEFORE it can write a terminal status — leaving the
        // task stuck `running` with no live worker. THIS is the reported orphan (a
        // crash / stop that never settled the node). Boot reconcile fixes it on the
        // next restart; here we prove rerun ALSO recovers it live (no restart).
        engine.stop(&run_id);
        assert!(!engine.is_running(&run_id), "loop stopped → no live worker");
        let still = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(
            still
                .tasks
                .iter()
                .find(|t| t.id == running_id)
                .unwrap()
                .status,
            "running",
            "task is a phantom `running` orphan after the loop died"
        );

        // Rerun the phantom: must RECOVER (normalize orphan → pending), NOT reject
        // with a 400 (which is only correct for a genuinely LIVE running task).
        engine
            .rerun_task(&svc, "u1", &run_id, &running_id)
            .await
            .expect("rerun of a phantom-running orphan must recover, not reject");
        let reset = svc.get_detail(&run_id).await.expect("detail");
        assert_eq!(
            reset
                .tasks
                .iter()
                .find(|t| t.id == running_id)
                .unwrap()
                .status,
            "pending",
            "phantom running recovered → pending (re-runnable)"
        );

        // Release the gated workers so the re-driven run settles (no leaked blocks),
        // then confirm end-to-end recovery: the re-activated run drives to completed.
        tokio::spawn({
            let gate = gate.clone();
            async move {
                for _ in 0..30 {
                    gate.notify_one();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        });
        engine.start(run_id.clone());
        let done = drive_to_completion(&svc, &run_id).await;
        assert_eq!(done.run.status, "completed", "recovered run completes");
    }

    // Edit a non-running task's spec → the task's spec changes AND a subsequent
    // rerun's brief reflects the NEW spec (the worker is called with the amended
    // task_spec on the re-run).
    #[tokio::test]
    async fn update_spec_changes_spec_and_rerun_uses_new_spec() {
        let recorder = Arc::new(SpecRecordingWorkerRunner::new());
        let seen = recorder.handle();
        let worker: Arc<dyn WorkerRunner> = recorder;
        let (svc, engine, run_id) = rerun_chain_harness(worker).await;

        engine.start(run_id.clone());
        let first = drive_to_completion(&svc, &run_id).await;
        assert_eq!(first.run.status, "completed");
        let a = first.tasks.iter().find(|t| t.title == "A").unwrap().clone();
        assert_eq!(a.spec, "do A", "initial A spec");
        // The worker saw the original spec on the first run.
        assert!(
            seen.lock().unwrap().iter().any(|(_, s)| s == "do A"),
            "worker ran A with the original spec"
        );

        // Amend A's spec, then rerun it.
        svc.update_task_spec("u1", &run_id, &a.id, "重新做 A（改进版）")
            .await
            .expect("update spec");
        let after_edit = svc.get_detail(&run_id).await.expect("detail");
        let a_edited = after_edit.tasks.iter().find(|t| t.title == "A").unwrap();
        assert_eq!(a_edited.spec, "重新做 A（改进版）", "spec persisted");

        engine
            .rerun_task(&svc, "u1", &run_id, &a.id)
            .await
            .expect("rerun A");
        engine.start(run_id.clone());
        let second = drive_to_completion(&svc, &run_id).await;
        assert_eq!(second.run.status, "completed");
        // The re-run dispatched A with the AMENDED spec.
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .any(|(tid, s)| *tid == a.id && s == "重新做 A（改进版）"),
            "rerun's worker brief must use the amended spec; seen={:?}",
            seen.lock().unwrap()
        );
    }

    // update_task_spec rejects a blank spec (400) and a running task (400).
    #[tokio::test]
    async fn update_spec_rejects_blank_and_running() {
        // Blank spec → 400 (use the plain mock; the run need not even execute).
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(900, "out"));
        let (svc, _engine, run_id) = rerun_chain_harness(worker).await;
        let detail = svc.get_detail(&run_id).await.expect("detail");
        let a = detail
            .tasks
            .iter()
            .find(|t| t.title == "A")
            .unwrap()
            .clone();
        let err = svc
            .update_task_spec("u1", &run_id, &a.id, "   ")
            .await
            .expect_err("blank spec must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");

        // Running task → 400. Drive with a gated worker so A is provably running.
        struct GatedWorker {
            gate: Arc<tokio::sync::Notify>,
        }
        #[async_trait]
        impl WorkerRunner for GatedWorker {
            async fn run(
                &self,
                _member: &FleetMember,
                _workspace_dir: Option<&str>,
                _run_id: &str,
                task_id: &str,
                _brief: &str,
                _task_spec: &str,
                _timeout: Duration,
                on_started: Box<dyn FnOnce(i64) + Send>,
            ) -> Result<WorkerOutcome, AppError> {
                on_started(900);
                self.gate.notified().await;
                Ok(WorkerOutcome {
                    conversation_id: 900,
                    text: Some(format!("out {task_id}")),
                    ok: true,
                    tokens: None,
                })
            }
        }
        let gate = Arc::new(tokio::sync::Notify::new());
        let worker2: Arc<dyn WorkerRunner> = Arc::new(GatedWorker { gate: gate.clone() });
        let (svc2, engine2, run_id2) = rerun_chain_harness(worker2).await;
        engine2.start(run_id2.clone());
        let mut running_id = None;
        for _ in 0..200 {
            let d = svc2.get_detail(&run_id2).await.expect("detail");
            running_id = d
                .tasks
                .iter()
                .find(|t| t.status == "running")
                .map(|t| t.id.clone());
            if running_id.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let running_id = running_id.expect("a task is running");
        let err = svc2
            .update_task_spec("u1", &run_id2, &running_id, "改不了运行中的")
            .await
            .expect_err("running task spec edit must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "got: {err:?}");
        tokio::spawn({
            let gate = gate.clone();
            async move {
                for _ in 0..20 {
                    gate.notify_one();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        });
        let _ = drive_to_completion(&svc2, &run_id2).await;
    }

    // Both per-node controls are owner-scoped: a wrong user gets 403 (Forbidden)
    // and a missing run 404 (NotFound). The run is left untouched.
    #[tokio::test]
    async fn rerun_and_spec_are_owner_scoped() {
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(900, "out"));
        let (svc, engine, run_id) = rerun_chain_harness(worker).await; // owned by "u1"
        engine.start(run_id.clone());
        let first = drive_to_completion(&svc, &run_id).await;
        let a = first.tasks.iter().find(|t| t.title == "A").unwrap().clone();

        // Wrong user → 403 for both controls.
        let err = engine
            .rerun_task(&svc, "intruder", &run_id, &a.id)
            .await
            .expect_err("cross-user rerun");
        assert!(
            matches!(err, AppError::Forbidden(_)),
            "rerun cross-user is 403, got: {err:?}"
        );
        let err = svc
            .update_task_spec("intruder", &run_id, &a.id, "盗改")
            .await
            .expect_err("cross-user spec edit");
        assert!(
            matches!(err, AppError::Forbidden(_)),
            "spec cross-user is 403, got: {err:?}"
        );

        // Missing run → 404 for both.
        let err = engine
            .rerun_task(&svc, "u1", "run_missing", &a.id)
            .await
            .expect_err("missing run rerun");
        assert!(
            matches!(err, AppError::NotFound(_)),
            "rerun missing is 404, got: {err:?}"
        );
        let err = svc
            .update_task_spec("u1", "run_missing", &a.id, "x")
            .await
            .expect_err("missing run spec edit");
        assert!(
            matches!(err, AppError::NotFound(_)),
            "spec missing is 404, got: {err:?}"
        );

        // The run is untouched: A is still done with its original spec.
        let detail = svc.get_detail(&run_id).await.expect("detail");
        let a_after = detail.tasks.iter().find(|t| t.title == "A").unwrap();
        assert_eq!(a_after.status, "done", "non-owner ops did not reset A");
        assert_eq!(
            a_after.spec, "do A",
            "non-owner edit did not change the spec"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // UC-2a 评审 Critical: the re-activation status-vs-loop race (per-run lock).
    // Plus Important-1 (reset preserves pattern node policy) and Important-2 (the
    // cascade does not descend past a running boundary).
    // ─────────────────────────────────────────────────────────────────────────

    // C1(a) — variant-A reactivation path THROUGH the route-level decision. Rerun a
    // task on a COMPLETED run, then apply the EXACT route gate
    // (`if run.status=="running" && !is_running → engine.start`); the run must
    // re-drive to `completed` with a LIVE loop (no strand). Because `engine.rerun_task`
    // deregisters the finished loop's handle UNDER the run lock as it writes the
    // terminal status, `is_running` is authoritative here and the fresh loop is
    // (re)spawned — the run never sits `running`-with-pending-tasks-and-no-driver.
    #[tokio::test]
    async fn rerun_completed_run_redrives_with_live_loop_no_strand() {
        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(900, "out"));
        let (svc, engine, run_id) = rerun_chain_harness(worker).await;

        engine.start(run_id.clone());
        let first = drive_to_completion(&svc, &run_id).await;
        assert_eq!(first.run.status, "completed");
        // The completed loop has deregistered its handle (terminal under the lock).
        // Poll briefly: the handle is removed under the lock as finish_run runs.
        for _ in 0..200 {
            if !engine.is_running(&run_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !engine.is_running(&run_id),
            "completed run's loop deregistered"
        );
        let a = first.tasks.iter().find(|t| t.title == "A").unwrap().clone();

        // Route path: rerun under the engine lock, then apply the route's gate.
        let run = engine
            .rerun_task(&svc, "u1", &run_id, &a.id)
            .await
            .expect("rerun A");
        assert_eq!(
            run.status, "running",
            "completed run re-activated to running"
        );
        // EXACT route decision — only start when running AND no live loop.
        let started = run.status == "running" && !engine.is_running(&run_id);
        assert!(
            started,
            "route must (re)start the loop for a re-activated run with no live driver"
        );
        engine.start(run_id.clone());

        // The re-activated run re-drives to completion — a LIVE loop drove it, the
        // run was NOT stranded `running`-with-pending-and-no-loop.
        let second = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            second.run.status, "completed",
            "re-activated run completes (no strand)"
        );
        for t in &second.tasks {
            assert_eq!(t.status, "done", "task {} re-executed to done", t.title);
        }
    }

    // C1(b) — logic-level invariant: a rerun that resets a settled task while the
    // run is STILL `running` (a live worker in flight elsewhere) must leave the run
    // running-with-a-live-driver (never terminal-with-a-pending-task). This is the
    // reachable shape of variant B: the live loop is awaiting a gated worker; we
    // rerun a DIFFERENT, already-done task. Under the per-run lock the reset is
    // serialized with the loop's terminal check, so the loop either has not finished
    // (it is awaiting the gated worker — `running`) or, once released, re-picks the
    // reset task. The run must converge to `completed` with every task `done`.
    //
    // COVERAGE LIMIT: a true thread-interleaving where the reset lands in the exact
    // microsecond between the loop's terminal `list_tasks` read and `finish_run` is
    // not deterministically reproducible from the test harness (it depends on the
    // multi-thread scheduler). This test pins the OBSERVABLE invariant — no strand,
    // run converges with a live driver — across the reachable concurrent shape; the
    // exact-interleaving guarantee rests on the lock holding the
    // re-read-statuses-and-finish critical section (asserted by construction in
    // `run_loop`, exercised here end-to-end).
    #[tokio::test]
    async fn rerun_while_running_never_strands_run() {
        // A→B→C chain. We gate B's worker so that when we rerun A, the run is still
        // `running` (B in flight). cap=1 forces strict A→B→C ordering.
        struct GateBWorker {
            gate: Arc<tokio::sync::Notify>,
            calls: Arc<Mutex<Vec<String>>>,
            run_repo: Arc<SqliteRunRepository>,
        }
        #[async_trait]
        impl WorkerRunner for GateBWorker {
            async fn run(
                &self,
                _member: &FleetMember,
                _workspace_dir: Option<&str>,
                run_id: &str,
                task_id: &str,
                _brief: &str,
                _task_spec: &str,
                _timeout: Duration,
                on_started: Box<dyn FnOnce(i64) + Send>,
            ) -> Result<WorkerOutcome, AppError> {
                on_started(900);
                // Block ONLY the SECOND task (B) on its FIRST visit, so the run is
                // still `running` while we rerun A. Resolve the title via the repo.
                let title = self
                    .run_repo
                    .list_tasks(run_id)
                    .await
                    .ok()
                    .and_then(|ts| ts.into_iter().find(|t| t.id == task_id).map(|t| t.title))
                    .unwrap_or_default();
                let first_b = {
                    let mut c = self.calls.lock().unwrap();
                    c.push(title.clone());
                    title == "B" && c.iter().filter(|t| *t == "B").count() == 1
                };
                if first_b {
                    self.gate.notified().await;
                }
                Ok(WorkerOutcome {
                    conversation_id: 900,
                    text: Some(format!("output of {task_id}")),
                    ok: true,
                    tokens: None,
                })
            }
        }

        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);
        let svc = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let gate = Arc::new(tokio::sync::Notify::new());
        let worker: Arc<dyn WorkerRunner> = Arc::new(GateBWorker {
            gate: gate.clone(),
            calls: Arc::new(Mutex::new(vec![])),
            run_repo: run_repo.clone(),
        });
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(10);
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "strand fleet".to_string(),
                    description: None,
                    max_parallel: Some(1), // cap=1 → strict A→B→C
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "strand ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = svc
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "strand chain".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: Some(1),
                },
            )
            .await
            .expect("run");
        svc.plan(&run.id).await.expect("plan");
        let run_id = run.id;

        engine.start(run_id.clone());

        // Wait until B is `running` (A done, B in flight, run still `running`).
        let mut a_id = None;
        for _ in 0..300 {
            let d = svc.get_detail(&run_id).await.expect("detail");
            let b_running = d
                .tasks
                .iter()
                .any(|t| t.title == "B" && t.status == "running");
            if b_running {
                a_id = d
                    .tasks
                    .iter()
                    .find(|t| t.title == "A")
                    .map(|t| t.id.clone());
                assert_eq!(
                    d.run.status, "running",
                    "run is still running while B is in flight"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let a_id = a_id.expect("A resolved while B is running");

        // Rerun the DONE task A while the run is running (B in flight). The cascade
        // resets A (root) → its dependents B,C. B is `running` → it is a BOUNDARY
        // (skipped, not descended): C is NOT reset off B here (Important-2). A is
        // reset to pending. The run stays `running` (re-read status under the lock).
        let run_after = engine
            .rerun_task(&svc, "u1", &run_id, &a_id)
            .await
            .expect("rerun A");
        assert_eq!(
            run_after.status, "running",
            "run stays running (not re-activated)"
        );
        // Route gate: run is running AND a live loop exists → do NOT restart.
        assert!(
            engine.is_running(&run_id),
            "the live loop is still registered (not stranded)"
        );

        // Release B's gate so the run can drain. The live loop must re-pick the
        // reset A and converge — never leaving the run terminal with a pending task.
        tokio::spawn({
            let gate = gate.clone();
            async move {
                for _ in 0..40 {
                    gate.notify_one();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        });
        let final_detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(
            final_detail.run.status, "completed",
            "run converges (no strand)"
        );
        for t in &final_detail.tasks {
            assert_eq!(t.status, "done", "task {} done at convergence", t.title);
        }
    }

    // Important-1: rerunning a `verify` (unanimous) node PRESERVES its
    // `pattern_config` policy — the reset must NOT wipe it back to the
    // majority default.
    #[tokio::test]
    async fn rerun_verify_preserves_vote_policy() {
        let cfg = r#"{"vote":"unanimous"}"#;
        let (svc, engine, _w, run_id) = verify_harness(vec![true, true, true], Some(cfg)).await;
        engine.start(run_id.clone());
        let first = drive_to_completion(&svc, &run_id).await;
        assert_eq!(first.run.status, "completed");
        let gate = first
            .tasks
            .iter()
            .find(|t| t.title == "Gate")
            .unwrap()
            .clone();
        assert_eq!(gate.kind, "verify");
        assert_eq!(
            gate.pattern_config.as_deref(),
            Some(cfg),
            "policy present before rerun"
        );

        // Rerun the verify node: its policy must SURVIVE the reset.
        engine
            .rerun_task(&svc, "u1", &run_id, &gate.id)
            .await
            .expect("rerun Gate");
        let after = svc.get_detail(&run_id).await.expect("detail");
        let gate2 = after.tasks.iter().find(|t| t.title == "Gate").unwrap();
        assert_eq!(gate2.status, "pending", "verify reset to pending");
        assert_eq!(
            gate2.pattern_config.as_deref(),
            Some(cfg),
            "verify pattern_config (VotePolicy) PRESERVED across reset"
        );
    }

    // Important-1: rerunning a `judge` (custom aggregate) node PRESERVES its policy.
    #[tokio::test]
    async fn rerun_judge_preserves_aggregate_policy() {
        let cfg = r#"{"aggregate":"borda"}"#;
        let (svc, engine, _w, run_id) =
            judge_harness(2, vec!["0:1,1:5", "0:2,1:4"], Some(cfg)).await;
        engine.start(run_id.clone());
        let first = drive_to_completion(&svc, &run_id).await;
        assert_eq!(first.run.status, "completed");
        let pick = first
            .tasks
            .iter()
            .find(|t| t.title == "Pick")
            .unwrap()
            .clone();
        assert_eq!(pick.kind, "judge");
        assert_eq!(
            pick.pattern_config.as_deref(),
            Some(cfg),
            "policy present before rerun"
        );

        engine
            .rerun_task(&svc, "u1", &run_id, &pick.id)
            .await
            .expect("rerun Pick");
        let after = svc.get_detail(&run_id).await.expect("detail");
        let pick2 = after.tasks.iter().find(|t| t.title == "Pick").unwrap();
        assert_eq!(pick2.status, "pending", "judge reset to pending");
        assert_eq!(
            pick2.pattern_config.as_deref(),
            Some(cfg),
            "judge pattern_config (JudgePolicy) PRESERVED across reset"
        );
    }

    // Important-1: rerunning a `loop` CONTROLLER node PRESERVES its policy
    // (custom max_iter / stop), while a rerun of the loop BODY (an `agent` task)
    // CLEARS the transient loop carry on pattern_config.
    #[tokio::test]
    async fn rerun_loop_controller_preserves_policy_body_clears_carry() {
        // max_iter=3, cap-only (no early stop) → the loop iterates to its hard cap,
        // so the body re-runs and carries `loop_prior_output` forward each round.
        let cfg = r#"{"max_iter":3}"#;
        let (svc, engine, _w, run_id) =
            loop_harness(cfg, vec!["round-1", "round-2", "round-3"], None).await;
        engine.start(run_id.clone());
        let first = drive_to_completion(&svc, &run_id).await;
        assert_eq!(first.run.status, "completed");
        let controller = first
            .tasks
            .iter()
            .find(|t| t.title == "Loop")
            .unwrap()
            .clone();
        let body = first
            .tasks
            .iter()
            .find(|t| t.title == "Refine")
            .unwrap()
            .clone();
        assert_eq!(controller.kind, "loop");
        assert_eq!(
            controller.pattern_config.as_deref(),
            Some(cfg),
            "loop controller policy present before rerun"
        );
        // The body iterated to the cap → it carries a loop_prior_output on its
        // pattern_config (set by the controller's CONTINUE reset).
        assert!(
            body.pattern_config
                .as_deref()
                .map(|s| s.contains("loop_prior_output"))
                .unwrap_or(false),
            "loop body carries loop_prior_output after iterating: {:?}",
            body.pattern_config
        );

        // Rerun the loop CONTROLLER → its policy must survive.
        engine
            .rerun_task(&svc, "u1", &run_id, &controller.id)
            .await
            .expect("rerun Loop");
        let after = svc.get_detail(&run_id).await.expect("detail");
        let controller2 = after.tasks.iter().find(|t| t.title == "Loop").unwrap();
        assert_eq!(controller2.status, "pending", "loop controller reset");
        assert_eq!(
            controller2.pattern_config.as_deref(),
            Some(cfg),
            "loop controller pattern_config (LoopConfig: max_iter/stop) PRESERVED"
        );
        // The cascade reset the loop BODY too (it is a settled dependent of the
        // controller via the loop's edges) — being an `agent` kind, its stale loop
        // carry was CLEARED.
        let body2 = after.tasks.iter().find(|t| t.title == "Refine").unwrap();
        // Whether or not the body is a downstream of the controller in the cascade,
        // an explicit body rerun must clear the carry. Assert via a direct body
        // rerun to make the contract unambiguous.
        engine
            .rerun_task(&svc, "u1", &run_id, &body2.id)
            .await
            .expect("rerun body");
        let after2 = svc.get_detail(&run_id).await.expect("detail");
        let body3 = after2.tasks.iter().find(|t| t.title == "Refine").unwrap();
        assert_eq!(body3.status, "pending", "loop body reset");
        assert_eq!(
            body3.pattern_config, None,
            "loop body (agent kind) pattern_config CLEARED (stale loop_prior_output dropped): {:?}",
            body3.pattern_config
        );
    }

    // Important-2: a cascade that hits a `running` intermediate dependent SKIPS it
    // AND does NOT descend past it — its downstream is left untouched (no stale-
    // lineage re-run). A→B→C chain; gate B so it is `running`; rerun A. B (running)
    // must be skipped and C (downstream of the running B) must NOT be reset.
    #[tokio::test]
    async fn cascade_stops_at_running_boundary() {
        struct GateMiddleWorker {
            gate: Arc<tokio::sync::Notify>,
            run_repo: Arc<SqliteRunRepository>,
        }
        #[async_trait]
        impl WorkerRunner for GateMiddleWorker {
            async fn run(
                &self,
                _member: &FleetMember,
                _workspace_dir: Option<&str>,
                run_id: &str,
                task_id: &str,
                _brief: &str,
                _task_spec: &str,
                _timeout: Duration,
                on_started: Box<dyn FnOnce(i64) + Send>,
            ) -> Result<WorkerOutcome, AppError> {
                on_started(900);
                let title = self
                    .run_repo
                    .list_tasks(run_id)
                    .await
                    .ok()
                    .and_then(|ts| ts.into_iter().find(|t| t.id == task_id).map(|t| t.title))
                    .unwrap_or_default();
                // Block B forever (until released) so it stays `running` across the
                // rerun. A and C complete immediately.
                if title == "B" {
                    self.gate.notified().await;
                }
                Ok(WorkerOutcome {
                    conversation_id: 900,
                    text: Some(format!("output of {task_id}")),
                    ok: true,
                    tokens: None,
                })
            }
        }

        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let emitter = OrchestratorRunEventEmitter::new(Arc::new(RecordingBroadcaster::new()));
        let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);
        let svc = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );
        let gate = Arc::new(tokio::sync::Notify::new());
        let worker: Arc<dyn WorkerRunner> = Arc::new(GateMiddleWorker {
            gate: gate.clone(),
            run_repo: run_repo.clone(),
        });
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(10);
        let engine = RunEngine::new(Arc::new(engine_deps));

        let fleet = crate::service::FleetService::new(fleet_repo)
            .create(
                "u1",
                CreateFleetRequest {
                    name: "boundary fleet".to_string(),
                    description: None,
                    max_parallel: Some(1),
                    members: vec![sample_member("agent_a")],
                },
            )
            .await
            .expect("fleet");
        let ws = crate::service::WorkspaceService::new(ws_repo)
            .create(
                "u1",
                CreateWorkspaceRequest {
                    name: "boundary ws".to_string(),
                    default_fleet_id: Some(fleet.id.clone()),
                    workspace_dir: None,
                },
            )
            .await
            .expect("ws");
        let run = svc
            .create(
                "u1",
                CreateRunRequest {
                    workspace_id: ws.id,
                    goal: "boundary chain".to_string(),
                    fleet_id: fleet.id,
                    autonomy: None,
                    max_parallel: Some(1),
                },
            )
            .await
            .expect("run");
        svc.plan(&run.id).await.expect("plan");
        let run_id = run.id;

        engine.start(run_id.clone());

        // Wait until A is done and B is running.
        let (mut a_id, mut a_attempt_before, mut c_attempt_before) = (None, None, None);
        for _ in 0..300 {
            let d = svc.get_detail(&run_id).await.expect("detail");
            let a_done = d.tasks.iter().any(|t| t.title == "A" && t.status == "done");
            let b_running = d
                .tasks
                .iter()
                .any(|t| t.title == "B" && t.status == "running");
            if a_done && b_running {
                let a = d.tasks.iter().find(|t| t.title == "A").unwrap();
                a_id = Some(a.id.clone());
                a_attempt_before = Some(a.attempt);
                c_attempt_before = d.tasks.iter().find(|t| t.title == "C").map(|t| t.attempt);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let a_id = a_id.expect("A done while B running");
        let a_attempt_before = a_attempt_before.expect("A attempt read");
        let c_attempt_before = c_attempt_before.expect("C attempt read");

        // Rerun A: cascade reaches B (running → boundary: skipped, NOT descended) so
        // C is NOT reset (it sits beyond the running B).
        engine
            .rerun_task(&svc, "u1", &run_id, &a_id)
            .await
            .expect("rerun A");
        let after = svc.get_detail(&run_id).await.expect("detail");
        let a = after.tasks.iter().find(|t| t.title == "A").unwrap();
        let b = after.tasks.iter().find(|t| t.title == "B").unwrap();
        let c = after.tasks.iter().find(|t| t.title == "C").unwrap();
        // The rerun DID run (target A reset → attempt bumped, pending) — so the
        // cascade machinery executed; C being untouched is a real boundary decision.
        assert_eq!(a.status, "pending", "target A reset to pending");
        assert_eq!(
            a.attempt,
            a_attempt_before + 1,
            "target A attempt bumped (rerun ran)"
        );
        assert_eq!(
            b.status, "running",
            "running B left untouched (skipped, not clobbered)"
        );
        assert_eq!(
            c.attempt, c_attempt_before,
            "C beyond the running boundary was NOT reset (attempt unchanged): no stale-lineage re-run"
        );
        // C is naturally `pending` (it never ran — its blocker B is still running),
        // but it must NOT have been TOUCHED by the cascade: the reset bumps `attempt`
        // and emits a `pending` status event. The unchanged attempt above is the
        // authoritative proof the cascade stopped at the running B boundary and did
        // not descend to C (a reset would have bumped it to `c_attempt_before + 1`).
        assert!(
            c.output_summary.is_none() && c.conversation_id.is_none(),
            "C never ran and was not reset-with-output (clean pending beyond the boundary)"
        );

        // Cleanup: release B so the run can drain and not leak the blocked worker.
        tokio::spawn({
            let gate = gate.clone();
            async move {
                for _ in 0..40 {
                    gate.notify_one();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        });
        let _ = drive_to_completion(&svc, &run_id).await;
    }

    // ── B2: run-level LLM synthesis summary (one-shot lead, fail-soft) ────────

    /// A summarize-only stub producer that returns a FIXED synthesized summary,
    /// recording the goal + digest + member count it was handed. `produce`/`adjust`
    /// are never exercised on the summarizer path (the run is planned by the
    /// harness's ChainPlanProducer), so they error.
    struct StubSummarizer {
        text: String,
        seen: Arc<Mutex<Option<(String, String, usize)>>>,
    }
    #[async_trait]
    impl PlanProducer for StubSummarizer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<PlannedDag, AppError> {
            Err(AppError::BadRequest(
                "stub summarizer does not plan".to_string(),
            ))
        }
        async fn summarize(
            &self,
            goal: &str,
            tasks_digest: &str,
            members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<String, AppError> {
            *self.seen.lock().unwrap() =
                Some((goal.to_string(), tasks_digest.to_string(), members.len()));
            Ok(self.text.clone())
        }
    }

    /// A summarize-only stub producer whose `summarize` always ERRORS — proves the
    /// engine's fail-soft fallback to `aggregate_summary`.
    struct ErroringSummarizer;
    #[async_trait]
    impl PlanProducer for ErroringSummarizer {
        async fn produce(
            &self,
            _goal: &str,
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<PlannedDag, AppError> {
            Err(AppError::BadRequest(
                "stub summarizer does not plan".to_string(),
            ))
        }
        async fn summarize(
            &self,
            _goal: &str,
            _tasks_digest: &str,
            _members: &[FleetMember],
            _sink: Option<&crate::plan::LeadThinkingSink>,
        ) -> Result<String, AppError> {
            Err(AppError::Internal("lead summarize boom".to_string()))
        }
    }

    /// Build the full mock stack like [`harness`], but inject `summarizer` into the
    /// engine deps (the B2 seam `build_orchestrator_state` wires in production).
    async fn harness_with_summarizer(summarizer: Arc<dyn PlanProducer>) -> Harness {
        let db = init_database_memory().await.expect("db init");
        let pool = db.pool().clone();
        let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));
        let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
        let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
        let broadcaster = Arc::new(RecordingBroadcaster::new());
        let emitter = OrchestratorRunEventEmitter::new(broadcaster.clone());
        let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);

        let run_service = RunService::new(
            run_repo.clone(),
            fleet_repo.clone(),
            ws_repo.clone(),
            planner,
            emitter.clone(),
        );

        let worker: Arc<dyn WorkerRunner> =
            Arc::new(MockWorkerRunner::with_text(777, "task output"));
        let mut engine_deps =
            RunEngineDeps::new(run_repo.clone(), worker, emitter, ws_repo.clone());
        engine_deps.worker_timeout = Duration::from_secs(5);
        engine_deps.summarizer = summarizer;
        let engine = RunEngine::new(Arc::new(engine_deps));

        Harness {
            run_service,
            engine,
            run_repo,
            fleet_repo,
            ws_repo,
            broadcaster,
        }
    }

    // A completed run with a STUB summarizer returning a synth summary → the run
    // `summary` is the LLM text (NOT the mechanical `aggregate_summary` concat).
    // The summarizer is also handed the run goal, a task digest, and the fleet
    // members (to derive the lead) — proving the one-shot synthesis wiring.
    #[tokio::test]
    async fn completed_run_uses_llm_synth_summary_over_concat() {
        let seen = Arc::new(Mutex::new(None));
        let summarizer: Arc<dyn PlanProducer> = Arc::new(StubSummarizer {
            text: "综合总结：该运行完成了 A→B→C 链，产出连贯一致。".to_string(),
            seen: seen.clone(),
        });
        let h = harness_with_summarizer(summarizer).await;
        let run_id = seed_run(&h).await;
        h.run_service.plan(&run_id).await.expect("plan");

        h.engine.start(run_id.clone());
        let detail = drive_to_completion(&h.run_service, &run_id).await;
        assert_eq!(detail.run.status, "completed", "run must complete");

        let summary = detail.run.summary.expect("run summary set on completion");
        assert_eq!(
            summary, "综合总结：该运行完成了 A→B→C 链，产出连贯一致。",
            "the run summary must be the LLM synthesis, not the concat"
        );
        assert!(
            !summary.contains("Run complete:"),
            "the mechanical concat header must NOT appear: {summary}"
        );

        // The summarizer saw the run goal, a non-empty digest, and the fleet members.
        let (goal, digest, member_count) = seen.lock().unwrap().clone().expect("summarize called");
        assert_eq!(goal, "build the chain", "goal threaded into summarize");
        assert!(
            digest.contains("status=done"),
            "digest carries the task statuses: {digest}"
        );
        assert!(
            digest.contains("task output"),
            "digest carries the task outputs: {digest}"
        );
        assert_eq!(
            member_count, 1,
            "the run's single fleet member is handed to summarize"
        );
    }

    // A summarizer that ERRORS → the run `summary` FALLS BACK to the mechanical
    // `aggregate_summary` concat, and the run is still `completed` (the summary
    // failure is fail-soft and NEVER fails the run).
    #[tokio::test]
    async fn completed_run_falls_back_to_concat_when_summarize_errors() {
        let summarizer: Arc<dyn PlanProducer> = Arc::new(ErroringSummarizer);
        let h = harness_with_summarizer(summarizer).await;
        let run_id = seed_run(&h).await;
        h.run_service.plan(&run_id).await.expect("plan");

        h.engine.start(run_id.clone());
        let detail = drive_to_completion(&h.run_service, &run_id).await;

        // Fail-soft: the run still COMPLETES despite the summary error.
        assert_eq!(
            detail.run.status, "completed",
            "summary error must NOT fail the run"
        );
        let summary = detail.run.summary.expect("run summary set on completion");
        // The mechanical concat is the fallback (its distinctive count header).
        assert!(
            summary.contains("Run complete: 3/3 tasks done."),
            "summary must fall back to aggregate_summary concat: {summary}"
        );
    }

    // The DEFAULT engine summarizer (NoopSummaryProducer, whose `summarize`
    // errors) → a completed run's summary is the mechanical concat. This pins the
    // zero-regression contract: the default harness behaves exactly as before B2.
    #[tokio::test]
    async fn completed_run_default_noop_summarizer_uses_concat() {
        let h = harness().await; // default RunEngineDeps → NoopSummaryProducer
        let run_id = seed_run(&h).await;
        h.run_service.plan(&run_id).await.expect("plan");

        h.engine.start(run_id.clone());
        let detail = drive_to_completion(&h.run_service, &run_id).await;
        assert_eq!(detail.run.status, "completed");
        let summary = detail.run.summary.expect("summary set");
        assert!(
            summary.contains("Run complete: 3/3 tasks done."),
            "default (noop) summarizer falls back to the concat: {summary}"
        );
    }

    // CONCURRENCY-REGRESSION GUARD (B2 fix): the LLM `summarize` is awaited with the
    // per-run terminal lock NOT held — the loop DROPS the first guard, awaits
    // `compute_completed_summary`, then RE-ACQUIRES the lock and RE-VERIFIES the run
    // is still all-terminal before committing the completion + deregistering.
    //
    // This pins the full drop→summarize(no lock)→re-acquire→re-verify→finish path:
    // a completed run still gets its LLM summary WRITTEN (proving the second-scope
    // commit ran with the summary computed outside the lock) AND its loop handle is
    // deregistered (proving the finish_run + remove_if under the RE-ACQUIRED lock
    // executed atomically — the strand-race closure survives the lock split). With a
    // hanging/slow summarizer this path would previously have stalled any concurrent
    // rerun for the whole summarization; the lock is now released across the await.
    #[tokio::test]
    async fn completed_run_summarizes_outside_lock_then_reverifies_and_finishes() {
        let seen = Arc::new(Mutex::new(None));
        let summarizer: Arc<dyn PlanProducer> = Arc::new(StubSummarizer {
            text: "重取锁重校验后写入的综合总结。".to_string(),
            seen: seen.clone(),
        });
        let h = harness_with_summarizer(summarizer).await;
        let run_id = seed_run(&h).await;
        h.run_service.plan(&run_id).await.expect("plan");

        h.engine.start(run_id.clone());
        let detail = drive_to_completion(&h.run_service, &run_id).await;

        // The completion committed under the RE-ACQUIRED lock (second scope).
        assert_eq!(
            detail.run.status, "completed",
            "run must complete via the re-verify path"
        );
        let summary = detail.run.summary.expect("run summary set on completion");
        assert_eq!(
            summary, "重取锁重校验后写入的综合总结。",
            "the LLM summary computed OUTSIDE the lock must be the committed run summary"
        );

        // `summarize` was actually awaited (the drop→summarize step ran with no lock).
        assert!(
            seen.lock().unwrap().is_some(),
            "summarize must have been called outside the lock"
        );

        // The loop handle was deregistered UNDER the re-acquired lock, atomically
        // with the terminal write — the run is no longer registered as running.
        assert!(
            !h.engine.is_running(&run_id),
            "completed run's loop must be deregistered after the second-scope finish"
        );
    }

    // The B2 SUMMARY_SYSTEM prompt must instruct SYNTHESIS (not concatenation) and
    // forbid JSON/fences, otherwise the lead never produces a usable run summary.
    #[test]
    fn summary_system_teaches_synthesis_not_concat() {
        use crate::plan::SUMMARY_SYSTEM;
        assert!(
            SUMMARY_SYSTEM.contains("SYNTHESIZE") || SUMMARY_SYSTEM.contains("synthesize"),
            "must instruct synthesis: {SUMMARY_SYSTEM}"
        );
        assert!(
            SUMMARY_SYSTEM.contains("not merely concatenate")
                || SUMMARY_SYSTEM.to_ascii_lowercase().contains("concatenate"),
            "must warn against concatenation: {SUMMARY_SYSTEM}"
        );
        assert!(
            SUMMARY_SYSTEM.contains("GOAL"),
            "must reference the goal: {SUMMARY_SYSTEM}"
        );
        assert!(
            SUMMARY_SYSTEM.contains("no JSON") || SUMMARY_SYSTEM.contains("JSON"),
            "must forbid JSON output: {SUMMARY_SYSTEM}"
        );
    }

    // build_summary_digest renders one line per task (title|role|status|output),
    // and truncates a long output_summary CJK-safe.
    #[test]
    fn build_summary_digest_renders_tasks_and_truncates() {
        let long_output = "段".repeat(800); // > SUMMARY_TASK_OUTPUT_LEN (600)
        let tasks = vec![
            summary_task("研究", "done", Some("找到三个来源")),
            summary_task("写作", "done", Some(&long_output)),
            summary_task("校对", "skipped", None),
        ];
        let digest = build_summary_digest(&tasks);
        assert!(
            digest.contains("研究 | role=研究 | status=done | output=找到三个来源"),
            "{digest}"
        );
        assert!(
            digest.contains("校对 | role=研究 | status=skipped | output=-"),
            "no-output → -: {digest}"
        );
        // The 800-char output was truncated with an ellipsis.
        assert!(digest.contains('…'), "long output truncated: {digest}");
        assert!(
            !digest.contains(&"段".repeat(700)),
            "digest must not carry the full 800-char output"
        );
    }

    /// Build a minimal completed-run task row for the digest test.
    fn summary_task(title: &str, status: &str, output: Option<&str>) -> OrchRunTaskRow {
        OrchRunTaskRow {
            pending_question: None,
            last_error: None,
            id: format!("rtask_{title}"),
            run_id: "run_x".to_string(),
            title: title.to_string(),
            spec: "spec".to_string(),
            task_profile: None,
            status: status.to_string(),
            conversation_id: None,
            output_summary: output.map(str::to_string),
            output_files: None,
            attempt: 0,
            tokens: None,
            graph_x: None,
            graph_y: None,
            role: Some("研究".to_string()),
            kind: "agent".to_string(),
            pattern_config: None,
            next_retry_at: None,
            override_provider_id: None,
            override_model: None,
            preset_prompt: None,
            on_fail: None,
            created_at: 0,
            updated_at: 0,
        }
    }
}
