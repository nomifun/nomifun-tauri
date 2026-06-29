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
use nomifun_common::AppError;
use nomifun_db::models::OrchRunTaskRow;
use nomifun_db::{IOrchWorkspaceRepository, IRunRepository};
use nomifun_db::{UpdateRunParams, UpdateTaskParams};
use tracing::{info, warn};

use crate::events::OrchestratorRunEventEmitter;
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
        Err(AppError::BadRequest("steering is not wired in this engine".to_owned()))
    }
}

/// Hard ceiling on a single worker task's turn.
pub const DEFAULT_WORKER_TIMEOUT: Duration = Duration::from_secs(1800);

/// Fallback concurrency cap when neither the run nor the fleet snapshot pins one.
pub const DEFAULT_MAX_PARALLEL: usize = 4;

/// How long the run loop idles (between paused-status re-checks) when the run is
/// `paused` and has no in-flight workers. A bounded sleep — NOT a busy-spin: the
/// loop yields the runtime each tick, then re-reads the status so a `resume` is
/// observed within ~one interval.
const PAUSE_POLL_INTERVAL: Duration = Duration::from_millis(500);

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
                handles,
                run_id: guard_run_id,
                generation,
            };
            info!(run_id = %loop_run_id, "Run engine loop started");
            run_loop(deps, &loop_run_id, cancelled_for_task).await;
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
        if let Some((_, handle)) = self.handles.remove(run_id) {
            handle.cancelled.store(true, Ordering::SeqCst);
            handle.join.abort();
        }
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

    /// Resume every persisted `running` run at boot. The running set (`handles`)
    /// is in-memory, but run status is persisted — on a process restart nothing
    /// would drive a `running` run until... never. This makes the backend the
    /// single source of truth: a `running` run resumes from boot. Idempotent via
    /// `is_running`. Detached + best-effort (mirrors `resume_persisted_bindings`).
    pub fn resume_persisted_runs(&self, run_repo: Arc<dyn IRunRepository>) {
        let this = self.clone();
        tokio::spawn(async move {
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
            return Err(AppError::BadRequest("steer text must not be empty".to_owned()));
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
            return Err(AppError::NotFound(format!("task {task_id} in run {run_id}")));
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
}

/// The bounded-parallel run loop: dispatch up to `cap` ready tasks concurrently,
/// awaiting in-flight workers, until the run reaches a terminal state, then
/// settle the run row + emit and exit.
async fn run_loop(deps: Arc<RunEngineDeps>, run_id: &str, cancelled: Arc<AtomicBool>) {
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

    // In-flight worker futures, each resolving to (task_id, outcome). The set's
    // length is the live concurrency; we never exceed `cap`.
    let mut inflight: FuturesUnordered<WorkerFuture> = FuturesUnordered::new();
    // Tasks currently in-flight — so a re-query of the ready set does not
    // re-dispatch a task whose worker is still running (list_ready_tasks keys off
    // persisted status, and a task is marked `running` before its future is
    // pushed, so this is belt-and-suspenders against a status read race).
    let mut in_progress: HashSet<String> = HashSet::new();

    loop {
        // (a) Cancelled → stop scheduling. Task 3 adds in-flight cancel
        // propagation; here we simply stop dispatching and let the loop unwind
        // (the spawned loop task is also aborted by `stop`).
        if cancelled.load(Ordering::SeqCst) {
            info!(run_id, "Run loop cancelled — exiting");
            break;
        }

        // (a') Paused gate (P3b): re-read the persisted run status each iteration.
        // When `paused` the loop must NOT dispatch new workers — but it keeps
        // processing any in-flight workers to completion (pause ≠ cancel). With
        // no in-flight work it idle-waits (a short sleep, NOT a busy-spin) and
        // re-checks, so a `resume` (status → `running`) is observed on the next
        // iteration and filling resumes. A read error is treated as not-paused
        // (fail-open: better to keep driving than to wedge on a transient error).
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
            info!(run_id, "Run loop: run awaits plan approval — not dispatching (exiting until approved)");
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
                                all.iter().any(|b| {
                                    b.id == d.blocker_task_id && b.status == "failed"
                                })
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
            // Not paused: the task statuses are conclusive (with zero workers in
            // flight they cannot change underneath us — no busy-spin).
            match deps.run_repo.list_tasks(run_id).await {
                Ok(tasks) => {
                    let all_terminal = tasks
                        .iter()
                        .all(|t| t.status == "done" || t.status == "skipped");
                    let any_failed = tasks.iter().any(|t| t.status == "failed");
                    if !tasks.is_empty() && all_terminal {
                        finish_run(&deps, run_id, "completed", Some(aggregate_summary(&tasks)))
                            .await;
                    } else if any_failed {
                        finish_run(&deps, run_id, "failed", None).await;
                    } else {
                        // Stuck (no ready, no in-flight, not terminal) — break,
                        // never spin.
                        warn!(
                            run_id,
                            task_count = tasks.len(),
                            "Run loop: no ready tasks and run not terminal — exiting to avoid spin"
                        );
                    }
                }
                Err(e) => warn!(run_id, error = %e, "Run loop: list_tasks failed — exiting"),
            }
            break;
        }

        // (d) Park on the next worker to finish (NOT a poll — this awaits). The
        // completion may unblock downstream tasks, so the loop re-fills.
        if let Some((task_id, outcome)) = inflight.next().await {
            in_progress.remove(&task_id);
            settle_task_outcome(&deps, run_id, &task_id, outcome).await;
        }
        // Loop again — re-evaluate the ready set (newly unblocked) and the
        // terminal condition.
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
        info!(run_id, cancelled, "Run stop: cancelled in-flight worker conversations");
    }
}

/// The future a single in-flight worker resolves to: its task id paired with the
/// worker outcome. Boxed because each closure type differs by captured task id.
type WorkerFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = (String, Result<WorkerOutcome, AppError>)> + Send>,
>;

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
            mark_task_failed(deps, run_id, &task.id, None).await;
            return None;
        }
    };

    // Mark running + emit BEFORE pushing the future, so a concurrent re-query of
    // list_ready_tasks (keyed on persisted status) never re-dispatches it.
    update_task_status(deps, &task.id, "running").await;
    deps.emitter.emit_task_status(run_id, &task.id, "running");

    // Compose the brief: role hint + the task + completed upstream outputs.
    let upstream = collect_upstream_outputs(deps, run_id, &task.id).await;
    let brief = compose_brief(member.role_hint.as_deref(), &task, &upstream);

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
                            status: None,
                            conversation_id: Some(Some(conv_id)),
                            output_summary: None,
                            output_files: None,
                            attempt: None,
                            tokens: None,
                            graph_x: None,
                            graph_y: None,
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
            .run(
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
/// error) → mark `failed` + emit. Completion is what unblocks downstream tasks.
async fn settle_task_outcome(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    task_id: &str,
    outcome: Result<WorkerOutcome, AppError>,
) {
    match outcome {
        Ok(o) if o.ok => {
            let _ = deps
                .run_repo
                .update_task(
                    task_id,
                    UpdateTaskParams {
                        status: Some("done".to_string()),
                        conversation_id: Some(Some(o.conversation_id)),
                        output_summary: Some(o.text),
                        output_files: None,
                        attempt: None,
                        // TODO(迁移 023 token 接通): write the worker's token usage here
                        // once it is surfaced. `WorkerOutcome` and
                        // `ConversationRuntimeSummary` currently expose NO token count
                        // (only conversation_id / final text / is_processing), so there
                        // is nothing real to write yet — wiring it would require
                        // threading per-turn usage out of the conversation/agent layer
                        // into `WorkerOutcome.tokens`. Deferred (don't fabricate); the
                        // `orch_run_tasks.tokens` column + the RunTask DTO/UI field are
                        // already plumbed and will light up once that source exists.
                        tokens: None,
                        graph_x: None,
                        graph_y: None,
                    },
                )
                .await;
            deps.emitter.emit_task_status(run_id, task_id, "done");
        }
        Ok(o) => {
            // Worker returned but did not produce a final text (timeout / empty).
            mark_task_failed(deps, run_id, task_id, Some(o.conversation_id)).await;
        }
        Err(e) => {
            warn!(run_id, task_id, error = %e, "Run loop: worker errored — failing task");
            mark_task_failed(deps, run_id, task_id, None).await;
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
    match assignment {
        Some(a) => members
            .into_iter()
            .find(|m| m.id == a.member_id)
            .ok_or("assigned member not in snapshot"),
        // No assignment → default to member[0] (single-member fleet path).
        None => members.into_iter().next().ok_or("fleet snapshot empty"),
    }
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

/// Compose the worker's brief: role hint + task title/spec + completed upstream
/// outputs (injected as context). Sent as the conversation `system_prompt`.
///
/// **Kind-aware (迁移 023).** For `kind == "synthesis"` the brief is framed as an
/// explicit synthesis instruction — the upstream dependency outputs are the
/// PRIMARY material to merge, not just background context — while the `agent`
/// kind (the default, and anything unknown) keeps the exact previous framing
/// (zero regression). The upstream gathering is identical for both kinds; only
/// the framing differs.
fn compose_brief(
    role_hint: Option<&str>,
    task: &OrchRunTaskRow,
    upstream: &[(String, String)],
) -> String {
    if task.kind == "synthesis" {
        return compose_synthesis_brief(role_hint, task, upstream);
    }
    compose_agent_brief(role_hint, task, upstream)
}

/// The unchanged `agent`-kind brief: role hint + task title/spec + completed
/// upstream outputs as build-on context. This is byte-for-byte the pre-023
/// `compose_brief` body — the agent path must not regress.
fn compose_agent_brief(
    role_hint: Option<&str>,
    task: &OrchRunTaskRow,
    upstream: &[(String, String)],
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
    if !upstream.is_empty() {
        out.push_str("\nUPSTREAM RESULTS (completed dependencies you can build on):\n");
        for (title, summary) in upstream {
            out.push_str("- ");
            out.push_str(title);
            out.push_str(": ");
            out.push_str(summary);
            out.push('\n');
        }
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
            out.push_str(summary);
            out.push('\n');
        }
    }
    out
}

/// Aggregate the run summary from completed task outputs (P1: concatenation;
/// TODO: a lead-model summarization pass). Always non-empty when there is at
/// least one task (falls back to a count line).
fn aggregate_summary(tasks: &[OrchRunTaskRow]) -> String {
    let mut out = String::new();
    let done = tasks.iter().filter(|t| t.status == "done").count();
    out.push_str(&format!("Run complete: {done}/{} tasks done.\n", tasks.len()));
    for t in tasks {
        if let Some(summary) = t.output_summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            out.push_str("\n## ");
            out.push_str(&t.title);
            out.push('\n');
            out.push_str(summary);
            out.push('\n');
        }
    }
    out
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
                status: Some("done".to_string()),
                conversation_id: None,
                output_summary: Some(Some(summary)),
                output_files: None,
                attempt: None,
                tokens: None,
                graph_x: None,
                graph_y: None,
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
    if any {
        Some(ballot)
    } else {
        None
    }
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
        let Some(json) = first_json_object(text) else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) else { continue };
        let Some(scores) = value.get("scores") else { continue };
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
fn aggregate_judge(ballots: &[Vec<Option<f64>>], candidates: usize, policy: JudgePolicy) -> JudgeResult {
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
                status: Some("done".to_string()),
                conversation_id: None,
                output_summary: Some(Some(summary)),
                output_files: None,
                attempt: None,
                tokens: None,
                graph_x: None,
                graph_y: None,
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
        let kind = stop.get("kind").and_then(serde_json::Value::as_str).unwrap_or("");
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
                    Some(m) => StopCriteria::Predicate { done_marker: m.to_string() },
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
                LoopDecision::Stop { reason: "predicate" }
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
    let Some(line) = text.lines().find(|l| l.trim_start().starts_with(LOOP_STATE_PREFIX)) else {
        return vec![];
    };
    let after = line.trim_start().trim_start_matches(LOOP_STATE_PREFIX).trim();
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
    let csv = hashes.iter().map(|h| h.to_string()).collect::<Vec<_>>().join(",");
    format!("{LOOP_STATE_PREFIX}{csv}")
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
                        status: None,
                        conversation_id: None,
                        output_summary: Some(Some(state_summary)),
                        output_files: None,
                        attempt: None,
                        tokens: None,
                        graph_x: None,
                        graph_y: None,
                    },
                )
                .await;
            // RESET the body: pending + clear output_summary/conversation_id +
            // attempt+1. This un-`done`s the controller's only blocker, so the
            // controller leaves the ready set until the body re-completes (a real
            // worker round) — the monotonic progress that bounds the loop.
            let _ = deps
                .run_repo
                .update_task(
                    &body.id,
                    UpdateTaskParams {
                        status: Some("pending".to_string()),
                        conversation_id: Some(None), // clear the prior round's conv
                        output_summary: Some(None),  // clear the prior round's output
                        output_files: Some(None),
                        attempt: Some(body.attempt + 1),
                        tokens: None,
                        graph_x: None,
                        graph_y: None,
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
                status: Some(status.to_string()),
                conversation_id: None,
                output_summary: Some(Some(summary)),
                output_files: None,
                attempt: None,
                tokens: None,
                graph_x: None,
                graph_y: None,
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
                status: Some(status.to_string()),
                conversation_id: None,
                output_summary: None,
                output_files: None,
                attempt: None,
                tokens: None,
                graph_x: None,
                graph_y: None,
            },
        )
        .await;
}

async fn mark_task_failed(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    task_id: &str,
    conversation_id: Option<i64>,
) {
    let _ = deps
        .run_repo
        .update_task(
            task_id,
            UpdateTaskParams {
                status: Some("failed".to_string()),
                conversation_id: conversation_id.map(Some),
                output_summary: None,
                output_files: None,
                attempt: None,
                tokens: None,
                graph_x: None,
                graph_y: None,
            },
        )
        .await;
    deps.emitter.emit_task_status(run_id, task_id, "failed");
}

/// Settle the run row to a terminal status (with an optional summary) and emit
/// `run.completed`. Best-effort: a persistence error is logged, not propagated
/// (the loop is exiting regardless).
async fn finish_run(deps: &Arc<RunEngineDeps>, run_id: &str, status: &str, summary: Option<String>) {
    if let Err(e) = deps
        .run_repo
        .update_run(
            run_id,
            UpdateRunParams {
                status: Some(status.to_string()),
                summary: summary.map(Some),
                lead_conv_id: None,
                total_tokens: None,
                goal: None,
                autonomy: None,
                fleet_snapshot: None,
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
            self.events.lock().unwrap().iter().map(|e| e.name.clone()).collect()
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

        let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(777, "task output"));
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
        assert!(completed, "run must reach completed within the bounded poll");

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
            assert_eq!(t.conversation_id, Some(777), "worker conversation id recorded");
        }
        // Run summary non-empty.
        let summary = detail.run.summary.expect("run summary set on completion");
        assert!(!summary.trim().is_empty(), "run summary must be non-empty");
        assert!(summary.contains("3/3"), "summary reflects 3/3 done, got: {summary}");

        // The realtime trail includes a run.completed event.
        let names = h.broadcaster.names();
        assert!(
            names.iter().any(|n| n == "orchestrator.run.completed"),
            "run.completed must be emitted; got {names:?}"
        );
        assert!(
            names.iter().filter(|n| *n == "orchestrator.task.statusChanged").count() >= 6,
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
        assert!(deregistered, "engine loop should deregister after the run completes");
    }

    // ── P6 Task 1: interactive ad-hoc run parks before the engine ─────────────

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
            detail.tasks.iter().map(|t| (&t.title, &t.status)).collect::<Vec<_>>()
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

        assert!(!h.engine.is_running(&run_id), "stopped loop is no longer registered");
        let detail = h.run_service.get_detail(&run_id).await.expect("detail");
        assert_eq!(detail.run.status, "cancelled", "run persisted as cancelled");
    }

    #[test]
    fn compose_brief_includes_role_task_and_upstream() {
        let task = OrchRunTaskRow {
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
            created_at: 0,
            updated_at: 0,
        };
        let upstream = vec![("Gather".to_string(), "found 12 sources".to_string())];
        let brief = compose_brief(Some("writer"), &task, &upstream);
        assert!(brief.contains("ROLE: writer"));
        assert!(brief.contains("TASK: Synthesize"));
        assert!(brief.contains("write the report"));
        assert!(brief.contains("Gather: found 12 sources"));
    }

    /// Build an `OrchRunTaskRow` with the given `kind` (other fields fixed) — used
    /// by the kind-aware compose_brief tests.
    fn task_row_with_kind(kind: &str, title: &str, spec: &str) -> OrchRunTaskRow {
        OrchRunTaskRow {
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
        let synth_brief = compose_brief(Some("写手"), &synth_task, &upstream);

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
        assert!(synth_brief.contains("Draft A: 草稿A的要点"), "dep A output present: {synth_brief}");
        assert!(synth_brief.contains("Draft B: 草稿B的要点"), "dep B output present: {synth_brief}");
        // The role + spec are still surfaced.
        assert!(synth_brief.contains("ROLE: 写手"));
        assert!(synth_brief.contains("写出最终稿"));

        // The SAME task as an `agent` kind produces the OLD framing (no synthesis
        // instruction, the plain TASK/UPSTREAM RESULTS labels) — proving the
        // branches diverge and agent is unchanged.
        let agent_task = task_row_with_kind("agent", "合并草稿", "写出最终稿");
        let agent_brief = compose_brief(Some("写手"), &agent_task, &upstream);
        assert_ne!(synth_brief, agent_brief, "synthesis framing must differ from agent");
        assert!(agent_brief.contains("TASK: 合并草稿"), "agent keeps TASK framing: {agent_brief}");
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
        let brief = compose_brief(Some("writer"), &task, &upstream);
        let expected = "ROLE: writer\n\nTASK: Synthesize\nSPEC:\nwrite the report\n\nUPSTREAM RESULTS (completed dependencies you can build on):\n- Gather: found 12 sources\n";
        assert_eq!(brief, expected, "agent-kind brief must match the pre-023 framing exactly");
    }

    #[test]
    fn aggregate_summary_is_non_empty_and_counts_done() {
        let mk = |title: &str, status: &str, summary: Option<&str>| OrchRunTaskRow {
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
        assert_eq!(VotePolicy::parse(Some(r#"{"vote":"majority"}"#)), VotePolicy::Majority);
        assert_eq!(VotePolicy::parse(Some(r#"{"vote":"unanimous"}"#)), VotePolicy::Unanimous);
        assert_eq!(VotePolicy::parse(Some(r#"{"vote":"UNANIMOUS"}"#)), VotePolicy::Unanimous);
        assert_eq!(
            VotePolicy::parse(Some(r#"{"vote":{"threshold":2}}"#)),
            VotePolicy::Threshold(2)
        );
        // Fail-soft: absent / blank / not-JSON / no `vote` / unknown string → Majority.
        assert_eq!(VotePolicy::parse(None), VotePolicy::Majority);
        assert_eq!(VotePolicy::parse(Some("   ")), VotePolicy::Majority);
        assert_eq!(VotePolicy::parse(Some("not json")), VotePolicy::Majority);
        assert_eq!(VotePolicy::parse(Some(r#"{"group":"x"}"#)), VotePolicy::Majority);
        assert_eq!(VotePolicy::parse(Some(r#"{"vote":"weird"}"#)), VotePolicy::Majority);
    }

    #[test]
    fn vote_policy_passes_thresholds() {
        // Majority = strictly more than half.
        assert!(VotePolicy::Majority.passes(2, 3), "2/3 is a majority");
        assert!(!VotePolicy::Majority.passes(1, 3), "1/3 is not");
        assert!(!VotePolicy::Majority.passes(1, 2), "1/2 is a TIE, not a majority");
        assert!(VotePolicy::Majority.passes(2, 2), "2/2 is a majority");
        assert!(!VotePolicy::Majority.passes(0, 0), "0 skeptics never passes (fail-safe)");
        // Unanimous = all, with at least one skeptic.
        assert!(VotePolicy::Unanimous.passes(3, 3));
        assert!(!VotePolicy::Unanimous.passes(2, 3));
        assert!(!VotePolicy::Unanimous.passes(0, 0), "unanimous with 0 skeptics fails");
        // Threshold(n) = at least n.
        assert!(VotePolicy::Threshold(2).passes(2, 5));
        assert!(VotePolicy::Threshold(2).passes(3, 5));
        assert!(!VotePolicy::Threshold(2).passes(1, 5));
    }

    #[test]
    fn parse_verdict_prefers_json_pass_field() {
        assert!(parse_verdict_pass(Some(r#"{"pass": true, "critique": "looks good"}"#)));
        assert!(!parse_verdict_pass(Some(r#"{"pass": false, "critique": "broken auth"}"#)));
        // JSON embedded in prose still found.
        assert!(parse_verdict_pass(Some(
            "After review: {\"pass\": true, \"critique\": \"ok\"} done."
        )));
    }

    #[test]
    fn parse_verdict_text_fallback_fail_beats_pass() {
        // No JSON pass field → text scan. FAIL is conservative and wins.
        assert!(!parse_verdict_pass(Some("Verdict: FAIL — the logic is wrong")));
        assert!(parse_verdict_pass(Some("Verdict: PASS — all good")));
        // Both present → FAIL wins (conservative).
        assert!(!parse_verdict_pass(Some("It could PASS but I must FAIL it")));
    }

    #[test]
    fn parse_verdict_unparseable_defaults_to_fail() {
        // The fail-safe invariant: missing / blank / unrecognizable → FAIL.
        assert!(!parse_verdict_pass(None), "missing output → fail-safe");
        assert!(!parse_verdict_pass(Some("   ")), "blank output → fail-safe");
        assert!(!parse_verdict_pass(Some("hmm, not sure")), "no verdict token → fail-safe");
        // Malformed JSON with no PASS/FAIL token → fail-safe.
        assert!(!parse_verdict_pass(Some(r#"{"pas": tru"#)), "broken JSON → fail-safe");
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
        assert!(tally_verify(&all_pass, VotePolicy::Unanimous).pass, "all pass → unanimous");

        let one_fail = vec![
            skeptic_with("S1", Some(r#"{"pass":true}"#)),
            skeptic_with("S2", Some("FAIL")),
        ];
        assert!(!tally_verify(&one_fail, VotePolicy::Unanimous).pass, "one fail breaks unanimous");

        // Threshold(1): a single pass among 3 satisfies it.
        let one_pass = vec![
            skeptic_with("S1", Some(r#"{"pass":true}"#)),
            skeptic_with("S2", Some("FAIL")),
            skeptic_with("S3", Some(r#"{"pass":false}"#)),
        ];
        assert!(tally_verify(&one_pass, VotePolicy::Threshold(1)).pass, "1 pass ≥ threshold 1");
        assert!(!tally_verify(&one_pass, VotePolicy::Threshold(2)).pass, "1 pass < threshold 2");
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
        assert!(!v.pass, "1/2 (tie) fails majority — unvalidated output never approves");
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
        assert!(s.starts_with("VERDICT: FAIL"), "summary leads with the verdict: {s}");
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
        assert_eq!(JudgePolicy::parse(Some(r#"{"aggregate":"mean"}"#)), JudgePolicy::Mean);
        assert_eq!(JudgePolicy::parse(Some(r#"{"aggregate":"borda"}"#)), JudgePolicy::Borda);
        assert_eq!(JudgePolicy::parse(Some(r#"{"aggregate":"BORDA"}"#)), JudgePolicy::Borda);
        // candidates pin alongside the policy still resolves the policy.
        assert_eq!(
            JudgePolicy::parse(Some(r#"{"aggregate":"borda","candidates":3}"#)),
            JudgePolicy::Borda
        );
        // Fail-soft: absent / blank / not-JSON / no `aggregate` / unknown → Mean.
        assert_eq!(JudgePolicy::parse(None), JudgePolicy::Mean);
        assert_eq!(JudgePolicy::parse(Some("   ")), JudgePolicy::Mean);
        assert_eq!(JudgePolicy::parse(Some("not json")), JudgePolicy::Mean);
        assert_eq!(JudgePolicy::parse(Some(r#"{"group":"x"}"#)), JudgePolicy::Mean);
        assert_eq!(JudgePolicy::parse(Some(r#"{"aggregate":"weird"}"#)), JudgePolicy::Mean);
    }

    #[test]
    fn parse_judge_ballot_array_and_object_forms() {
        // Array form: positional by candidate index.
        let arr = parse_judge_ballot(Some(r#"{"scores":[0.8,0.3,0.6]}"#), 3).expect("array ballot");
        assert_eq!(arr, vec![Some(0.8), Some(0.3), Some(0.6)]);

        // Object form: keyed by candidate index (sparse OK — missing → None).
        let obj = parse_judge_ballot(Some(r#"{"scores":{"0":0.8,"2":0.6}}"#), 3).expect("object ballot");
        assert_eq!(obj, vec![Some(0.8), None, Some(0.6)]);

        // Embedded in prose still found.
        let prose = parse_judge_ballot(
            Some("After review: {\"scores\":[0.1,0.9]} done."),
            2,
        )
        .expect("prose-embedded ballot");
        assert_eq!(prose, vec![Some(0.1), Some(0.9)]);

        // Extra array entries beyond M are ignored; the ballot is sized to M.
        let extra = parse_judge_ballot(Some(r#"{"scores":[0.1,0.2,0.3,0.4]}"#), 2).expect("extra");
        assert_eq!(extra, vec![Some(0.1), Some(0.2)]);

        // Out-of-range / non-index object keys are ignored.
        let oor = parse_judge_ballot(Some(r#"{"scores":{"0":0.5,"9":0.9,"x":0.1}}"#), 2).expect("oor");
        assert_eq!(oor, vec![Some(0.5), None]);
    }

    #[test]
    fn parse_judge_ballot_unparseable_is_dropped_no_panic() {
        // The fail-safe invariant: missing / blank / unparseable / no usable
        // scores → None (the judge is DROPPED), never a panic.
        assert!(parse_judge_ballot(None, 3).is_none(), "missing output → dropped");
        assert!(parse_judge_ballot(Some("   "), 3).is_none(), "blank output → dropped");
        assert!(parse_judge_ballot(Some("no json here"), 3).is_none(), "no JSON → dropped");
        assert!(parse_judge_ballot(Some(r#"{"scor: ["#), 3).is_none(), "broken JSON → dropped");
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
        assert!(parse_judge_ballot(Some(r#"{"scores":[]}"#), 3).is_none(), "empty array → dropped");
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
        assert!((r.aggregate[0] - 0.8).abs() < 1e-9, "mean c0: {:?}", r.aggregate);
        assert!((r.aggregate[2] - 0.55).abs() < 1e-9, "mean c2: {:?}", r.aggregate);
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
            assert!((ra.aggregate[c] - rb.aggregate[c]).abs() < 1e-9, "candidate {c} aggregate stable");
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
        assert!((r.aggregate[0] - 3.0).abs() < 1e-9, "c0 borda=3: {:?}", r.aggregate);
        assert!((r.aggregate[1] - 0.0).abs() < 1e-9, "c1 borda=0: {:?}", r.aggregate);
        assert!((r.aggregate[2] - 3.0).abs() < 1e-9, "c2 borda=3: {:?}", r.aggregate);
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
        assert!((r.aggregate[1] - 4.0).abs() < 1e-9, "c1 borda=4: {:?}", r.aggregate);

        // Order independence: reverse the ballots → same winner + same totals.
        let b = vec![a[1].clone(), a[0].clone()];
        let rb = aggregate_judge(&b, 3, JudgePolicy::Borda);
        assert_eq!(rb.winner, Some(1));
        for c in 0..3 {
            assert!((r.aggregate[c] - rb.aggregate[c]).abs() < 1e-9, "candidate {c} borda stable");
        }
    }

    #[test]
    fn aggregate_judge_borda_ties_within_a_judge_share_points() {
        // One judge, 3 candidates, c0 and c1 TIED at 0.5, c2 at 0.1.
        // Ranks for points 2,1,0: the tied block {c0,c1} occupies ranks 0,1 →
        // share (2+1)/2 = 1.5 each; c2 gets 0. Deterministic (no index drift).
        let ballots = vec![vec![Some(0.5), Some(0.5), Some(0.1)]];
        let r = aggregate_judge(&ballots, 3, JudgePolicy::Borda);
        assert!((r.aggregate[0] - 1.5).abs() < 1e-9, "c0 shares tie: {:?}", r.aggregate);
        assert!((r.aggregate[1] - 1.5).abs() < 1e-9, "c1 shares tie: {:?}", r.aggregate);
        assert!((r.aggregate[2] - 0.0).abs() < 1e-9, "c2 last: {:?}", r.aggregate);
        // Final tie c0/c1 → lowest index wins.
        assert_eq!(r.winner, Some(0));
    }

    #[test]
    fn settle_judge_drops_missing_ballot_fail_safe() {
        // Two usable judges + one dropped (unparseable). The drop must not crash;
        // the winner is computed from the two usable ballots only.
        let candidates = 2;
        let raw = vec![
            Some(r#"{"scores":[0.9,0.1]}"#),       // c0 strong
            Some("worker timed out, no ballot"),   // unparseable → dropped
            Some(r#"{"scores":[0.8,0.2]}"#),       // c0 strong
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
        let outs = vec![Some(r#"{"scores":[0.1,0.2]}"#), Some(r#"{"scores":{"3":0.9}}"#)];
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
            &[vec![Some(0.9), Some(0.2), Some(0.5)], vec![Some(0.7), Some(0.4), Some(0.6)]],
            3,
            JudgePolicy::Mean,
        );
        let s = render_judge_summary(&r, JudgePolicy::Mean);
        assert!(s.starts_with("WINNER: candidate 0"), "summary leads with the winner: {s}");
        assert!(s.contains("aggregate=mean"), "policy surfaced: {s}");
        assert!(s.contains("scores=["), "per-candidate aggregates surfaced: {s}");
        assert!(s.contains("judges=2/2"), "judge count surfaced: {s}");

        // No winner → leads with `WINNER: none`.
        let none = render_judge_summary(&aggregate_judge(&[], 3, JudgePolicy::Mean), JudgePolicy::Mean);
        assert!(none.starts_with("WINNER: none"), "no-winner marker: {none}");
    }

    // -------------------------------------------------------------------------
    // P2: bounded-parallel scheduling (concurrency, dependency strictness,
    // workspace_dir injection). All-mock: a delay-and-counting WorkerRunner +
    // a diamond DAG (A,B independent → C depends on both).
    // -------------------------------------------------------------------------

    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

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
        let mut engine_deps = RunEngineDeps::new(run_repo.clone(), worker_dyn, emitter, ws_repo.clone());
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
        assert!(summary.contains("3/3"), "summary reflects 3/3 done: {summary}");

        // CONCURRENCY PROOF: peak concurrency reached 2 (A and B overlapped).
        let peak = worker.max_concurrent.load(Ordering::SeqCst);
        assert_eq!(peak, 2, "with cap=2, A and B must run concurrently (peak=2), got {peak}");

        // DEPENDENCY STRICTNESS: C started only after both A and B. The first two
        // starts are A,B (in some order); the third start is C.
        let order = worker.start_order.lock().unwrap().clone();
        assert_eq!(order.len(), 3, "all three tasks ran exactly once");
        let title_of = |id: &str| {
            detail.tasks.iter().find(|t| t.id == id).map(|t| t.title.clone()).unwrap_or_default()
        };
        let titles: Vec<String> = order.iter().map(|id| title_of(id)).collect();
        assert_eq!(titles[2], "C", "C must be the LAST task to start (after A+B done), got {titles:?}");
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
        // Independent A,B + dependent C, each 100ms. Concurrent (cap=2): A‖B ≈
        // 100ms, then C ≈ 100ms → ≈200ms total. Serial would be ≈300ms. Assert
        // the elapsed is comfortably under the serial sum (proves overlap), with
        // generous headroom for scheduler jitter on a loaded CI box.
        let (svc, engine, _worker, run_id) =
            diamond_harness(2, None, Duration::from_millis(100)).await;

        let start = Instant::now();
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        let elapsed = start.elapsed();

        assert_eq!(detail.run.status, "completed");
        assert!(
            elapsed < Duration::from_millis(290),
            "A‖B overlap should keep total well under the 300ms serial sum, got {elapsed:?}"
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
        assert_eq!(peak, 1, "with cap=1, no two workers may overlap (peak=1), got {peak}");

        // C is still strictly last; A/B order between them is not constrained.
        let order = worker.start_order.lock().unwrap().clone();
        let title_of = |id: &str| {
            detail.tasks.iter().find(|t| t.id == id).map(|t| t.title.clone()).unwrap_or_default()
        };
        let titles: Vec<String> = order.iter().map(|id| title_of(id)).collect();
        assert_eq!(titles.len(), 3);
        assert_eq!(titles[2], "C", "C must be last even serially, got {titles:?}");
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
        assert_eq!(peak, 2, "absent run cap → default_max_parallel=2 governs (peak=2), got {peak}");
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
            self.steered.lock().unwrap().push((conversation_id, text.to_owned()));
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
        assert!(worker.started.load(Ordering::SeqCst) >= 1, "first worker must start");
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
        assert_eq!(worker.started.load(Ordering::SeqCst), 3, "all 3 tasks eventually run");
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
        assert_eq!(detail.run.status, "completed", "PASS verdict → run completes");

        let by_title = |t: &str| detail.tasks.iter().find(|x| x.title == t).cloned().unwrap();
        let gate = by_title("Gate");
        // The verify aggregator is `done` with a PASS verdict and NO worker conv.
        assert_eq!(gate.kind, "verify");
        assert_eq!(gate.status, "done", "verify settled done");
        assert_eq!(gate.conversation_id, None, "verify has no worker conversation");
        let gate_summary = gate.output_summary.as_deref().unwrap_or("");
        assert!(gate_summary.starts_with("VERDICT: PASS"), "PASS verdict: {gate_summary}");
        assert!(gate_summary.contains("3/3"), "3/3 tally: {gate_summary}");

        // Downstream Deploy actually ran (it is `done`, not skipped).
        let deploy = by_title("Deploy");
        assert_eq!(deploy.status, "done", "downstream proceeds on PASS");

        // The verify task NEVER reached the worker (no dispatch / no spin): the
        // worker saw exactly Build + 3 skeptics + Deploy = 5 tasks, never "Gate".
        let started = worker.start_order.lock().unwrap().clone();
        assert_eq!(started.len(), 5, "worker ran 5 tasks (verify excluded): {started:?}");
        let specs = worker.seen_specs.lock().unwrap().clone();
        assert!(
            !specs.iter().any(|s| s.contains("aggregate skeptic verdicts")),
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
        assert_eq!(gate.status, "done", "verify itself is done (it computed a verdict)");
        assert_eq!(gate.conversation_id, None, "verify has no worker conversation");
        let gate_summary = gate.output_summary.as_deref().unwrap_or("");
        assert!(gate_summary.starts_with("VERDICT: FAIL"), "FAIL verdict: {gate_summary}");
        assert!(gate_summary.contains("1/3"), "1/3 tally: {gate_summary}");

        // Downstream Deploy was GATED → skipped, and never reached the worker.
        let deploy = by_title("Deploy");
        assert_eq!(deploy.status, "skipped", "downstream gated (skipped) on FAIL");
        assert_eq!(deploy.conversation_id, None, "skipped downstream never dispatched");

        let specs = worker.seen_specs.lock().unwrap().clone();
        assert!(
            !specs.iter().any(|s| s.contains("deploy the validated feature")),
            "gated downstream must never reach a worker: {specs:?}"
        );
        // Build + 3 skeptics ran (4 tasks); verify + Deploy did not.
        assert_eq!(worker.start_order.lock().unwrap().len(), 4, "Build + 3 skeptics only");
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
        assert_eq!(deploy.status, "done", "unanimous PASS → downstream proceeds");

        // Second: unanimous with one FAIL → gate.
        let (svc2, engine2, _w2, run_id2) =
            verify_harness(vec![true, true, false], Some(r#"{"vote":"unanimous"}"#)).await;
        engine2.start(run_id2.clone());
        let detail2 = drive_to_completion(&svc2, &run_id2).await;
        assert_eq!(detail2.run.status, "completed", "still completes (done/skipped)");
        let gate2 = detail2.tasks.iter().find(|t| t.title == "Gate").unwrap();
        assert!(
            gate2.output_summary.as_deref().unwrap_or("").starts_with("VERDICT: FAIL"),
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
        assert!(summary.starts_with("VERDICT: PASS"), "2 ≥ threshold 2: {summary}");
        assert!(summary.contains("threshold:2"), "policy surfaced: {summary}");
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
        assert_eq!(detail.run.status, "completed", "plain agent chain unaffected");
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
            Self { start_order: Mutex::new(vec![]), seen_specs: Mutex::new(vec![]) }
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
            Ok(WorkerOutcome { conversation_id: 900, text: Some(text), ok: true })
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
        assert_eq!(pick.conversation_id, None, "judge has no worker conversation");
        let pick_summary = pick.output_summary.as_deref().unwrap_or("");
        assert!(
            pick_summary.starts_with("WINNER: candidate 0"),
            "mean winner is c0: {pick_summary}"
        );
        assert!(pick_summary.contains("aggregate=mean"), "policy surfaced: {pick_summary}");
        assert!(pick_summary.contains("judges=2/2"), "both judges counted: {pick_summary}");

        // Downstream Consumer actually ran (NO gate — judge reports, doesn't skip).
        let consumer = by_title("Consumer");
        assert_eq!(consumer.status, "done", "downstream proceeds after a judge");

        // The judge task NEVER reached a worker (no dispatch / no spin): the worker
        // saw 3 candidates + 2 judges + 1 consumer = 6 tasks, never "Pick".
        let started = worker.start_order.lock().unwrap().clone();
        assert_eq!(started.len(), 6, "worker ran 6 tasks (judge excluded): {started:?}");
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
        assert!(summary.starts_with("WINNER: candidate 1"), "borda winner is c1: {summary}");
        assert!(summary.contains("aggregate=borda"), "policy surfaced: {summary}");
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
        assert_eq!(detail.run.status, "completed", "run completes despite a dropped judge");
        let pick = detail.tasks.iter().find(|t| t.title == "Pick").unwrap();
        let summary = pick.output_summary.as_deref().unwrap_or("");
        assert!(summary.starts_with("WINNER: candidate 0"), "c0 from 2 usable ballots: {summary}");
        assert!(summary.contains("judges=2/3"), "one judge dropped fail-safe: {summary}");
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
        assert_eq!(detail.run.status, "completed", "plain agent chain unaffected");
        for t in &detail.tasks {
            assert_eq!(t.status, "done", "task {} done", t.title);
            assert_eq!(t.kind, "agent", "no judge kind injected");
        }
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
        assert_eq!(c.stop, StopCriteria::Predicate { done_marker: "DONE".to_string() });

        let c = LoopConfig::parse(Some(r#"{"max_iter":6,"stop":{"kind":"dry","quiet_rounds":2}}"#));
        assert_eq!(c.max_iter, 6);
        assert_eq!(c.stop, StopCriteria::Dry { quiet_rounds: 2 });

        // Fail-soft: absent / blank / not-JSON → DEFAULT cap-only (bounded, never
        // unbounded). The default max_iter is the small backstop.
        for raw in [None, Some("   "), Some("not json"), Some(r#"{"foo":1}"#)] {
            let c = LoopConfig::parse(raw);
            assert_eq!(c.max_iter, DEFAULT_LOOP_MAX_ITER, "fail-soft cap for {raw:?}");
            assert_eq!(c.stop, StopCriteria::MaxIter, "fail-soft stop for {raw:?}");
        }

        // Unknown stop kind → cap-only (still bounded).
        let c = LoopConfig::parse(Some(r#"{"max_iter":2,"stop":{"kind":"weird"}}"#));
        assert_eq!(c.max_iter, 2);
        assert_eq!(c.stop, StopCriteria::MaxIter);

        // max_iter omitted → default; max_iter=0 (invalid) → clamped to default
        // (NEVER 0 → never an unbounded/zero loop).
        assert_eq!(LoopConfig::parse(Some(r#"{"stop":{"kind":"max_iter"}}"#)).max_iter, DEFAULT_LOOP_MAX_ITER);
        assert_eq!(LoopConfig::parse(Some(r#"{"max_iter":0}"#)).max_iter, DEFAULT_LOOP_MAX_ITER);

        // predicate with NO marker → degrades to cap-only (a marker-less predicate
        // could never fire, so the cap is the only stop → still bounded).
        let c = LoopConfig::parse(Some(r#"{"max_iter":3,"stop":{"kind":"predicate"}}"#));
        assert_eq!(c.stop, StopCriteria::MaxIter);

        // dry quiet_rounds omitted → defaults to 1 (clamped >= 1).
        let c = LoopConfig::parse(Some(r#"{"max_iter":3,"stop":{"kind":"dry"}}"#));
        assert_eq!(c.stop, StopCriteria::Dry { quiet_rounds: 1 });
    }

    #[test]
    fn predicate_done_marker_and_json() {
        assert!(predicate_done(Some("all polished. DONE"), "DONE"));
        assert!(!predicate_done(Some("still working"), "DONE"));
        // JSON {"done":true} anywhere triggers regardless of the text marker.
        assert!(predicate_done(Some(r#"result: {"done":true,"note":"ok"}"#), "DONE"));
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
            stop: StopCriteria::Predicate { done_marker: "NEVER".to_string() },
        };
        assert_eq!(decide_loop(&cfg, 1, Some("nope"), &[]), LoopDecision::Continue);
        assert_eq!(decide_loop(&cfg, 2, Some("nope"), &[]), LoopDecision::Continue);
        assert_eq!(
            decide_loop(&cfg, 3, Some("nope"), &[]),
            LoopDecision::Stop { reason: "max_iter" },
            "HARD cap forces STOP at max_iter regardless of the criterion"
        );
        // And it can NEVER exceed the cap (defensive: iterations_done > max_iter).
        assert_eq!(decide_loop(&cfg, 99, Some("nope"), &[]), LoopDecision::Stop { reason: "max_iter" });
    }

    #[test]
    fn decide_loop_predicate_stops_early_under_the_cap() {
        let cfg = LoopConfig {
            max_iter: 10,
            stop: StopCriteria::Predicate { done_marker: "DONE".to_string() },
        };
        // Under the cap, no marker → CONTINUE.
        assert_eq!(decide_loop(&cfg, 1, Some("round 1"), &[]), LoopDecision::Continue);
        // Marker present (still under the cap) → STOP early (reason predicate).
        assert_eq!(
            decide_loop(&cfg, 2, Some("round 2 DONE"), &[]),
            LoopDecision::Stop { reason: "predicate" }
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
        assert_eq!(decide_loop(&cfg, 1, Some("draft A"), &[]), LoopDecision::Continue);
        // Round 2 produced a DIFFERENT output than round 1 → CONTINUE.
        assert_eq!(decide_loop(&cfg, 2, Some("draft B"), &[h_a]), LoopDecision::Continue);
        // Round 3 repeats round 2's output → 2 consecutive equal (rounds 2,3) → STOP.
        assert_eq!(
            decide_loop(&cfg, 3, Some("draft B"), &[h_a, h_b]),
            LoopDecision::Stop { reason: "dry" }
        );

        // quiet_rounds=3 over the SAME history needs 3 consecutive equal; rounds 2,3
        // equal is only 2 → still CONTINUE.
        let cfg3 = LoopConfig { max_iter: 10, stop: StopCriteria::Dry { quiet_rounds: 3 } };
        assert_eq!(decide_loop(&cfg3, 3, Some("draft B"), &[h_a, h_b]), LoopDecision::Continue);
        // A 4th identical round (history h_a,h_b,h_b, this=h_b) → rounds 2,3,4 equal → STOP.
        assert_eq!(
            decide_loop(&cfg3, 4, Some("draft B"), &[h_a, h_b, h_b]),
            LoopDecision::Stop { reason: "dry" }
        );
    }

    #[test]
    fn loop_state_hashes_round_trip() {
        let hashes = vec![111u64, 222u64, 333u64];
        let rendered = render_loop_state(&hashes);
        assert!(rendered.starts_with(LOOP_STATE_PREFIX), "machine prefix present: {rendered}");
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
        assert!(s.contains("the final polished draft"), "final body output carried: {s}");

        let f = render_loop_final("failed", "body_failed", 1, 3, None);
        assert!(f.starts_with("LOOP: FAILED"), "failed marker: {f}");
        assert!(f.contains("reason=body_failed"), "failure reason: {f}");
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
                let round = {
                    let mut counts = self.run_counts.lock().unwrap();
                    let n = counts.entry(task_id.to_string()).or_insert(0);
                    *n += 1;
                    *n
                };
                if self.fail_on_round == Some(round) {
                    // Body fails this round → ok:false (no final text).
                    return Ok(WorkerOutcome { conversation_id: 900, text: None, ok: false });
                }
                let idx = round.saturating_sub(1).min(self.rounds.len().saturating_sub(1));
                let text = self.rounds.get(idx).cloned().unwrap_or_else(|| "body output".to_string());
                Ok(WorkerOutcome { conversation_id: 900, text: Some(text), ok: true })
            } else {
                Ok(WorkerOutcome {
                    conversation_id: 900,
                    text: Some(format!("did {task_spec}")),
                    ok: true,
                })
            }
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
    fn body_run_count(worker: &LoopBodyWorkerRunner, detail: &nomifun_api_types::RunDetail) -> usize {
        let body_id = detail
            .tasks
            .iter()
            .find(|t| t.title == "Refine")
            .map(|t| t.id.clone())
            .unwrap_or_default();
        worker.start_order.lock().unwrap().iter().filter(|id| **id == body_id).count()
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
        assert_eq!(detail.run.status, "completed", "bounded loop drives to completion");

        let by_title = |t: &str| detail.tasks.iter().find(|x| x.title == t).cloned().unwrap();
        let body = by_title("Refine");
        let ctrl = by_title("Loop");
        // The body ran EXACTLY 3 times (the cap), tracked by attempt (0-based: the
        // last completed round is attempt 2 → 3 iterations).
        assert_eq!(body_run_count(&worker, &detail), 3, "body ran exactly max_iter=3 times");
        assert_eq!(body.attempt, 2, "body attempt is 2 (3rd round, 0-based) at the cap");
        // The controller is done with the max_iter STOP marker + no worker conv.
        assert_eq!(ctrl.kind, "loop");
        assert_eq!(ctrl.status, "done", "loop controller settled done at the cap");
        assert_eq!(ctrl.conversation_id, None, "loop controller has no worker conversation");
        let summary = ctrl.output_summary.as_deref().unwrap_or("");
        assert!(summary.starts_with("LOOP: DONE"), "machine marker leads: {summary}");
        assert!(summary.contains("reason=max_iter"), "hard cap reason: {summary}");
        assert!(summary.contains("iterations=3"), "3 iterations: {summary}");
        assert!(summary.contains("round3"), "final body output carried: {summary}");
        // Downstream ran AFTER the loop finished (gated on the loop, not the body).
        assert_eq!(by_title("Publish").status, "done", "downstream runs after the loop");

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
            vec!["still working", "polished now DONE", "round3-should-not-happen"],
            None,
        )
        .await;
        engine.start(run_id.clone());
        let detail = drive_to_completion(&svc, &run_id).await;
        assert_eq!(detail.run.status, "completed");

        let by_title = |t: &str| detail.tasks.iter().find(|x| x.title == t).cloned().unwrap();
        assert_eq!(body_run_count(&worker, &detail), 2, "predicate stops early after 2 rounds");
        let ctrl = by_title("Loop");
        let summary = ctrl.output_summary.as_deref().unwrap_or("");
        assert!(summary.contains("reason=predicate"), "early predicate stop: {summary}");
        assert!(summary.contains("iterations=2"), "2 iterations: {summary}");
        assert_eq!(by_title("Publish").status, "done", "downstream runs after the loop");
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
        assert_eq!(body_run_count(&worker, &detail), 3, "dry stops after 3 rounds (r2==r3)");
        let ctrl = by_title("Loop");
        let summary = ctrl.output_summary.as_deref().unwrap_or("");
        assert!(summary.contains("reason=dry"), "dry stop reason: {summary}");
        assert!(summary.contains("iterations=3"), "3 iterations: {summary}");
        assert_eq!(by_title("Publish").status, "done", "downstream runs after the loop");
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
        assert_eq!(body.attempt, 3, "body attempt increments per iteration (0-based, 4 rounds)");
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
        assert_eq!(body_run_count(&worker, &detail), 2, "failing body did not iterate forever");
        assert_eq!(ctrl.status, "failed", "loop controller stops failed on a failing body");
        let summary = ctrl.output_summary.as_deref().unwrap_or("");
        assert!(summary.starts_with("LOOP: FAILED"), "failed marker: {summary}");
        assert!(summary.contains("reason=body_failed"), "body-failed reason: {summary}");
        // Downstream Publish was GATED → skipped (never ran on a failing loop).
        assert_eq!(by_title("Publish").status, "skipped", "downstream gated on a failing loop");
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
        assert_eq!(detail.run.status, "completed", "plain agent chain unaffected");
        for t in &detail.tasks {
            assert_eq!(t.status, "done", "task {} done", t.title);
            assert_eq!(t.kind, "agent", "no loop kind injected");
        }
    }
}
