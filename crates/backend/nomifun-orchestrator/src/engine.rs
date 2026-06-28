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
fn compose_brief(
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
                    },
                    PlannedTask {
                        title: "B".to_string(),
                        spec: "do B".to_string(),
                        task_profile: None,
                        depends_on: vec![0],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                    },
                    PlannedTask {
                        title: "C".to_string(),
                        spec: "do C".to_string(),
                        task_profile: None,
                        depends_on: vec![1],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
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
                    },
                    PlannedTask {
                        title: "B".to_string(),
                        spec: "do B".to_string(),
                        task_profile: None,
                        depends_on: vec![],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
                    },
                    PlannedTask {
                        title: "C".to_string(),
                        spec: "do C".to_string(),
                        task_profile: None,
                        depends_on: vec![0, 1],
                        member_index: Some(0),
                        rationale: None,
                        role: None,
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
}
