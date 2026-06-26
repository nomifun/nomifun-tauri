//! [`RunEngine`]: the串行 (serial) execution loop that drives an orchestration
//! run's task DAG to completion.
//!
//! The engine skeleton — the per-run handle registry, the `start` =
//! stop-then-spawn dance, the generation-guarded [`HandleGuard`] that removes
//! only its own entry on task exit, and [`RunEngine::resume_persisted_runs`] —
//! is a faithful reduction of `nomifun_requirement::Orchestrator` (see
//! `crates/backend/nomifun-requirement/src/orchestrator.rs`). The differences
//! are deliberate: a run is keyed by a single `String` run id (no dual-domain
//! `(kind, id)`), and the loop is **serial** (P1) — it runs exactly ONE ready
//! task at a time, never spawning concurrent workers (parallel scheduling is
//! P2).
//!
//! ## Loop termination (no busy-spin)
//!
//! `run_loop` exits — it does NOT idle forever — the moment the run reaches a
//! terminal shape:
//! - cancelled (the cooperative flag) → break;
//! - no ready tasks AND every task is `done`/`skipped` → mark the run
//!   `completed` (with an aggregated summary), emit `run.completed`, break;
//! - no ready tasks AND some task `failed` → mark `failed`, emit, break;
//! - no ready tasks AND none of the above (a "stuck" graph that cannot happen in
//!   a serial loop, but is guarded against) → break to avoid spinning.
//!
//! Because the loop is serial, there is never an in-flight worker while the
//! ready set is empty, so the "empty ready set" checks are conclusive — the loop
//! can decide the run's fate and exit. It only re-enters the loop body after
//! actually advancing a task (running it to done/failed), so it cannot spin.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use nomifun_api_types::FleetMember;
use nomifun_db::IRunRepository;
use nomifun_db::models::OrchRunTaskRow;
use nomifun_db::{UpdateRunParams, UpdateTaskParams};
use tracing::{info, warn};

use crate::events::OrchestratorRunEventEmitter;
use crate::worker::WorkerRunner;

/// Hard ceiling on a single worker task's turn.
pub const DEFAULT_WORKER_TIMEOUT: Duration = Duration::from_secs(1800);

/// Shared dependencies for all run loops. The `fleet_snapshot` is read off the
/// run row via `run_repo` (no separate fleet handle is needed at runtime — the
/// snapshot is the single source of truth once a run is created).
pub struct RunEngineDeps {
    pub run_repo: Arc<dyn IRunRepository>,
    pub worker: Arc<dyn WorkerRunner>,
    pub emitter: OrchestratorRunEventEmitter,
    /// Max wall-clock budget for one worker task turn.
    pub worker_timeout: Duration,
}

impl RunEngineDeps {
    pub fn new(
        run_repo: Arc<dyn IRunRepository>,
        worker: Arc<dyn WorkerRunner>,
        emitter: OrchestratorRunEventEmitter,
    ) -> Self {
        Self {
            run_repo,
            worker,
            emitter,
            worker_timeout: DEFAULT_WORKER_TIMEOUT,
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

/// Drives per-run serial execution loops.
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

    /// Stop a run's loop: set the cooperative cancel flag and abort the task.
    /// The loop checks the flag between tasks; the abort covers a long in-flight
    /// worker await. Persisting `cancelled` is the service's job
    /// ([`RunService::cancel`](crate::run_service::RunService::cancel)).
    pub fn stop(&self, run_id: &str) {
        if let Some((_, handle)) = self.handles.remove(run_id) {
            handle.cancelled.store(true, Ordering::SeqCst);
            handle.join.abort();
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
}

/// The serial run loop: drive ready tasks one at a time until the run reaches a
/// terminal state, then settle the run row + emit and exit.
async fn run_loop(deps: Arc<RunEngineDeps>, run_id: &str, cancelled: Arc<AtomicBool>) {
    loop {
        if cancelled.load(Ordering::SeqCst) {
            info!(run_id, "Run loop cancelled — exiting");
            break;
        }

        let ready = match deps.run_repo.list_ready_tasks(run_id).await {
            Ok(r) => r,
            Err(e) => {
                warn!(run_id, error = %e, "Run loop: list_ready_tasks failed — exiting");
                break;
            }
        };

        if ready.is_empty() {
            // No runnable task. Because the loop is serial there is never an
            // in-flight worker here, so the task statuses are conclusive.
            match deps.run_repo.list_tasks(run_id).await {
                Ok(tasks) => {
                    let all_terminal = tasks
                        .iter()
                        .all(|t| t.status == "done" || t.status == "skipped");
                    let any_failed = tasks.iter().any(|t| t.status == "failed");
                    if !tasks.is_empty() && all_terminal {
                        finish_run(&deps, run_id, "completed", Some(aggregate_summary(&tasks))).await;
                    } else if any_failed {
                        finish_run(&deps, run_id, "failed", None).await;
                    } else {
                        // Stuck (shouldn't happen serially) — break, never spin.
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

        // SERIAL: take exactly one ready task per iteration.
        let task = &ready[0];
        run_one_task(&deps, run_id, task).await;
        // Loop again — the just-finished task may have unblocked others.
    }
}

/// Run a single task: resolve its assigned member from the run's fleet snapshot,
/// compose the brief (role hint + upstream outputs), run the worker, and persist
/// the outcome. Failure is recorded as `failed` (retry/reassign is P3).
async fn run_one_task(deps: &Arc<RunEngineDeps>, run_id: &str, task: &OrchRunTaskRow) {
    // Resolve the assignment → member from the run's fleet snapshot.
    let member = match resolve_task_member(deps, run_id, &task.id).await {
        Ok(m) => m,
        Err(reason) => {
            warn!(run_id, task_id = %task.id, reason, "Run loop: cannot resolve member — failing task");
            mark_task_failed(deps, run_id, &task.id, None).await;
            return;
        }
    };

    // Mark running + emit.
    update_task_status(deps, &task.id, "running").await;
    deps.emitter.emit_task_status(run_id, &task.id, "running");

    // Compose the brief: role hint + the task + completed upstream outputs.
    let upstream = collect_upstream_outputs(deps, run_id, &task.id).await;
    let brief = compose_brief(member.role_hint.as_deref(), task, &upstream);

    let workspace_dir = current_workspace_dir(deps, run_id).await;
    let outcome = deps
        .worker
        .run(
            &member,
            workspace_dir.as_deref(),
            run_id,
            &task.id,
            &brief,
            &task.spec,
            deps.worker_timeout,
            // Task 1 placeholder: the engine stays SERIAL here, so the early
            // conv_id report is a no-op. Task 2 replaces this with real in-flight
            // recording + immediate task.conversation_id stamping for cancellation
            // and the live transcript.
            Box::new(|_conv_id| {}),
        )
        .await;

    match outcome {
        Ok(o) if o.ok => {
            let _ = deps
                .run_repo
                .update_task(
                    &task.id,
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
            deps.emitter.emit_task_status(run_id, &task.id, "done");
        }
        Ok(o) => {
            // Worker returned but did not produce a final text (timeout / empty).
            mark_task_failed(deps, run_id, &task.id, Some(o.conversation_id)).await;
        }
        Err(e) => {
            warn!(run_id, task_id = %task.id, error = %e, "Run loop: worker errored — failing task");
            mark_task_failed(deps, run_id, &task.id, None).await;
        }
    }
}

/// Resolve the member assigned to `task_id` from the run's `fleet_snapshot`.
/// Returns a short static reason string on failure (for the warn log).
async fn resolve_task_member(
    deps: &Arc<RunEngineDeps>,
    run_id: &str,
    task_id: &str,
) -> Result<FleetMember, &'static str> {
    let assignment = deps
        .run_repo
        .get_assignment_for_task(task_id)
        .await
        .map_err(|_| "assignment query failed")?
        .ok_or("no assignment for task")?;
    let run = deps
        .run_repo
        .get_run(run_id)
        .await
        .map_err(|_| "run query failed")?
        .ok_or("run not found")?;
    let members: Vec<FleetMember> =
        serde_json::from_str(&run.fleet_snapshot).map_err(|_| "fleet snapshot unparseable")?;
    members
        .into_iter()
        .find(|m| m.id == assignment.member_id)
        .ok_or("assigned member not in snapshot")
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

/// The run's workspace directory, if the run's fleet snapshot carried one. (P1:
/// the worker workspace is resolved at the app-wiring layer; the engine passes
/// `None` here — the worker conversation defaults to its own scratch dir. This
/// hook exists so the assembly can later inject a per-run dir without touching
/// the loop.)
async fn current_workspace_dir(_deps: &Arc<RunEngineDeps>, _run_id: &str) -> Option<String> {
    None
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
        CapabilityProfile, CreateFleetRequest, CreateRunRequest, CreateWorkspaceRequest,
        FleetMember, FleetMemberInput, PlannedDag, PlannedTask,
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
                    },
                    PlannedTask {
                        title: "B".to_string(),
                        spec: "do B".to_string(),
                        task_profile: None,
                        depends_on: vec![0],
                        member_index: Some(0),
                        rationale: None,
                    },
                    PlannedTask {
                        title: "C".to_string(),
                        spec: "do C".to_string(),
                        task_profile: None,
                        depends_on: vec![1],
                        member_index: Some(0),
                        rationale: None,
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
        let mut engine_deps = RunEngineDeps::new(run_repo.clone(), worker, emitter);
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
}
