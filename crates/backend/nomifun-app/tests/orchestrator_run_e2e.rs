//! E2E integration test for the 智能编排 Run control-plane (P1a Task 9).
//!
//! Two things are proven here:
//!
//! 1. **Full-run wiring through the HTTP routes + the engine** — with a real
//!    [`PlanProducer`] (a fixed 2-task chain DAG defined below) and a
//!    [`MockWorkerRunner`] (fixed assistant text), so the run is driveable to
//!    `completed` in CI without a live LLM. We mount ONLY `orchestrator_routes`
//!    over a fresh in-memory DB with an injected `CurrentUser` layer (exactly as
//!    the app's auth middleware does), then: create workspace + fleet → `POST
//!    /api/orchestrator/runs` (201) → poll `GET /api/orchestrator/runs/{id}`
//!    until `status == "completed"` → assert every task `done` + a non-empty run
//!    summary. The route's `create_run` handler does create → plan → engine.start
//!    internally, so this exercises the route↔RunService↔RunEngine seam end to
//!    end (the exact contract `build_orchestrator_state` wires in production).
//!
//! 2. **The REAL app wiring compiles** — the keystone gate is `cargo build -p
//!    nomifun-app`. That build only succeeds once `build_orchestrator_state`
//!    constructs a real `LlmPlanProducer` + `ConversationWorkerRunner`-backed
//!    `RunEngine` AND `create_router`'s `GatewayDeps` literal populates the
//!    `orchestrator_run_service` / `orchestrator_run_engine` fields (Task 7). This
//!    test crate is part of `nomifun-app`, so it does not even compile until that
//!    wiring type-checks — making the green build the proof of the real path.
//!
//! Why not drive the *real* `build_orchestrator_state` to completion here? Its
//! planner is the production `LlmPlanProducer`, which needs a configured provider
//! to make its one-shot planning call — not available in CI. The real
//! multi-task-planning path is covered by the real-machine acceptance run with a
//! configured provider; CI proves the route+engine seam with the mock stack.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use nomifun_api_types::{
    CreateAdhocRunRequest, CreateFleetRequest, CreateRunRequest, CreateWorkspaceRequest,
    FleetMember, FleetMemberInput, ModelRange, ModelRef, PlannedDag, PlannedTask, WebSocketMessage,
};
use nomifun_auth::CurrentUser;
use nomifun_common::AppError;
use nomifun_db::{
    SqliteFleetRepository, SqliteOrchWorkspaceRepository, SqliteRunRepository, init_database_memory,
};
use nomifun_orchestrator::{
    ConversationCanceller, FleetService, MockWorkerRunner, OrchestratorRunEventEmitter,
    OrchestratorRouterState, PlanProducer, RunEngine, RunEngineDeps, RunService, WorkerOutcome,
    WorkerRunner, WorkspaceService, orchestrator_routes,
};
use nomifun_realtime::EventBroadcaster;

/// No-op broadcaster: this test asserts on persisted run state, not the WS trail.
struct NoopBroadcaster;
impl EventBroadcaster for NoopBroadcaster {
    fn broadcast(&self, _event: WebSocketMessage<serde_json::Value>) {}
}

/// No-op skill resolver for the route state's `ConversationService` (B3): the
/// orchestrator-route conversation handle only ever serves
/// `link_orchestrator_run` here, so skill resolution is irrelevant.
struct NoopSkillResolver;
#[async_trait]
impl nomifun_conversation::skill_resolver::SkillResolver for NoopSkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        Vec::new()
    }
    async fn resolve_skills(
        &self,
        _names: &[String],
    ) -> Vec<nomifun_conversation::skill_resolver::ResolvedAgentSkill> {
        Vec::new()
    }
    async fn link_workspace_skills(
        &self,
        _workspace: &std::path::Path,
        _rel_dirs: &[&str],
        _skills: &[nomifun_conversation::skill_resolver::ResolvedAgentSkill],
    ) -> usize {
        0
    }
}

/// No-op worker task manager — the route conversation handle never spawns agents.
struct NoopTaskManager;
#[async_trait]
impl nomifun_ai_agent::IWorkerTaskManager for NoopTaskManager {
    fn get_task(&self, _: &str) -> Option<nomifun_ai_agent::AgentInstance> {
        None
    }
    async fn get_or_build_task(
        &self,
        _: &str,
        _: nomifun_ai_agent::types::BuildTaskOptions,
    ) -> Result<nomifun_ai_agent::AgentInstance, AppError> {
        Err(AppError::Internal("noop".into()))
    }
    fn kill(&self, _: &str, _: Option<nomifun_common::AgentKillReason>) -> Result<(), AppError> {
        Ok(())
    }
    fn kill_and_wait(
        &self,
        _: &str,
        _: Option<nomifun_common::AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }
    fn clear(&self) {}
    fn active_count(&self) -> usize {
        0
    }
    fn collect_idle(&self, _: nomifun_common::TimestampMs) -> Vec<String> {
        vec![]
    }
}

/// Build a `ConversationService` over the given pool for the route state (B3).
/// Mirrors the conversation crate's own test recipe (no-op resolver/task manager)
/// — the orchestrator route only uses it for `link_orchestrator_run`.
fn build_test_conv_service(pool: nomifun_db::SqlitePool) -> nomifun_conversation::ConversationService {
    let conv_repo = Arc::new(nomifun_db::SqliteConversationRepository::new(pool.clone()));
    let agent_metadata_repo: Arc<dyn nomifun_db::IAgentMetadataRepository> =
        Arc::new(nomifun_db::SqliteAgentMetadataRepository::new(pool.clone()));
    let acp_session_repo: Arc<dyn nomifun_db::IAcpSessionRepository> =
        Arc::new(nomifun_db::SqliteAcpSessionRepository::new(pool));
    nomifun_conversation::ConversationService::new(
        std::env::temp_dir(),
        Arc::new(NoopBroadcaster),
        Arc::new(NoopSkillResolver),
        Arc::new(NoopTaskManager),
        conv_repo,
        agent_metadata_repo,
        acp_session_repo,
    )
}

/// A fixed 2-task chain DAG: task0 (no dep) → task1 (depends on 0), both
/// pre-assigned to member 0 so a single-member fleet suffices. Mirrors the
/// production `PlanProducer` contract (Task 4) without a live LLM.
struct ChainPlanProducer;

#[async_trait]
impl PlanProducer for ChainPlanProducer {
    async fn produce(&self, _goal: &str, _members: &[FleetMember], _sink: Option<&nomifun_orchestrator::LeadThinkingSink>) -> Result<PlannedDag, AppError> {
        Ok(PlannedDag {
            tasks: vec![
                PlannedTask {
                    title: "Gather".to_string(),
                    spec: "collect sources".to_string(),
                    task_profile: None,
                    depends_on: vec![],
                    member_index: Some(0),
                    rationale: Some("first".to_string()),
                    role: None,
                    kind: "agent".to_string(),
                    pattern_config: None,
                },
                PlannedTask {
                    title: "Synthesize".to_string(),
                    spec: "write the report".to_string(),
                    task_profile: None,
                    depends_on: vec![0],
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

/// Build a test `OrchestratorRouterState` over a fresh in-memory DB, wired with a
/// `ChainPlanProducer` + a fixed-text `MockWorkerRunner`. This is the test analog
/// of `build_orchestrator_state` — same `RunService::new` / `RunEngine::new`
/// public seams the production builder uses, but with mock planner/worker so the
/// run is driveable to completion without a live LLM.
async fn build_run_state() -> OrchestratorRouterState {
    let db = init_database_memory().await.expect("db init");
    let pool = db.pool().clone();
    let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
    let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
    let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));

    let fleet = FleetService::new(fleet_repo.clone());
    let workspace = WorkspaceService::new(ws_repo.clone());
    let emitter = OrchestratorRunEventEmitter::new(Arc::new(NoopBroadcaster));
    let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);

    let run_service = Arc::new(RunService::new(
        run_repo.clone(),
        fleet_repo,
        ws_repo.clone(),
        planner,
        emitter.clone(),
    ));
    let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(4242, "task output"));
    let mut engine_deps = RunEngineDeps::new(run_repo, worker, emitter, ws_repo);
    engine_deps.worker_timeout = Duration::from_secs(5);
    let engine = RunEngine::new(Arc::new(engine_deps));

    OrchestratorRouterState::new(
        fleet,
        workspace,
        run_service,
        engine,
        build_test_conv_service(pool),
    )
}

/// Mount `orchestrator_routes` with a `CurrentUser` extension injected exactly as
/// the app's auth middleware does (mirrors the routes.rs unit-test pattern), so
/// the handlers' `Extension<CurrentUser>` requirement is exercised, not bypassed.
fn router(state: OrchestratorRouterState) -> axum::Router {
    orchestrator_routes(state).layer(axum::Extension(CurrentUser {
        id: "u1".to_string(),
        username: "tester".to_string(),
    }))
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

/// Full run lifecycle through the HTTP routes: create workspace + fleet, POST a
/// run (which the handler create→plan→engine.start), then poll GET until the run
/// completes and assert every task is done with a non-empty summary.
#[tokio::test]
async fn run_create_plan_execute_completes_through_routes() {
    let state = build_run_state().await;

    // Seed a single-member fleet + a workspace directly via the services the
    // state holds (so the seeded rows are visible to the run handlers).
    let fleet = state
        .fleet
        .create(
            "u1",
            CreateFleetRequest {
                name: "e2e fleet".to_string(),
                description: None,
                max_parallel: None,
                members: vec![FleetMemberInput {
                    agent_id: "agent_a".to_string(),
                    // Nomi-engine member: provider+model required by the real
                    // worker, harmless for the mock worker (ignored).
                    provider_id: Some("prov_x".to_string()),
                    model: Some("claude-opus-4-8".to_string()),
                    role_hint: Some("researcher".to_string()),
                    capability_profile: None,
                    constraints: None,
                    sort_order: None,
                }],
            },
        )
        .await
        .expect("seed fleet");
    let ws = state
        .workspace
        .create(
            "u1",
            CreateWorkspaceRequest {
                name: "e2e ws".to_string(),
                default_fleet_id: Some(fleet.id.clone()),
                workspace_dir: None,
            },
        )
        .await
        .expect("seed workspace");

    let app = router(state);

    // POST /api/orchestrator/runs → 201. The handler creates the run, plans it
    // (2-task chain), and starts the engine loop (fire-and-forget).
    let resp = app
        .clone()
        .oneshot(post(
            "/api/orchestrator/runs",
            serde_json::json!({
                "workspace_id": ws.id,
                "goal": "build the chain",
                "fleet_id": fleet.id,
            }),
        ))
        .await
        .expect("create run request");
    assert_eq!(resp.status(), StatusCode::CREATED, "POST /runs should be 201");
    let json = body_json(resp).await;
    let run_id = json["data"]["id"]
        .as_str()
        .expect("run id is a string")
        .to_owned();
    assert!(run_id.starts_with("run_"), "id should be a run_ string, got {run_id}");

    // Poll GET /api/orchestrator/runs/{id} until completed (bounded ~50×100ms).
    let mut completed = false;
    let mut last = serde_json::Value::Null;
    for _ in 0..50 {
        let resp = app
            .clone()
            .oneshot(get(&format!("/api/orchestrator/runs/{run_id}")))
            .await
            .expect("get run request");
        assert_eq!(resp.status(), StatusCode::OK, "GET /runs/{{id}} should be 200");
        last = body_json(resp).await;
        if last["data"]["run"]["status"] == "completed" {
            completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(completed, "run must reach completed within the bounded poll; last detail: {last}");

    // Assert the final shape: 2 tasks, both done with the worker's output, a
    // non-empty run summary reflecting 2/2 done.
    let detail = &last["data"];
    let tasks = detail["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 2, "2 tasks persisted");
    for t in tasks {
        assert_eq!(t["status"], "done", "task {} should be done", t["title"]);
        assert_eq!(
            t["output_summary"], "task output",
            "task {} output_summary should be the worker text",
            t["title"]
        );
        assert_eq!(t["conversation_id"], 4242, "worker conversation id recorded");
    }
    let summary = detail["run"]["summary"].as_str().expect("run summary set on completion");
    assert!(!summary.trim().is_empty(), "run summary must be non-empty");
    assert!(summary.contains("2/2"), "summary reflects 2/2 done, got: {summary}");

    // The dependency edge connects Gather → Synthesize.
    let deps = detail["deps"].as_array().expect("deps array");
    assert_eq!(deps.len(), 1, "one dep edge (Gather→Synthesize)");

    // 2 tasks → 2 auto assignments, both source=auto.
    let assignments = detail["assignments"].as_array().expect("assignments array");
    assert_eq!(assignments.len(), 2, "2 assignments persisted");
    for a in assignments {
        assert_eq!(a["source"], "auto");
        assert_eq!(a["locked"], false);
    }
}

/// Cancelling a run through the route stops the engine and persists `cancelled`.
#[tokio::test]
async fn run_cancel_through_route_persists_cancelled() {
    let state = build_run_state().await;
    let fleet = state
        .fleet
        .create(
            "u1",
            CreateFleetRequest {
                name: "cancel fleet".to_string(),
                description: None,
                max_parallel: None,
                members: vec![FleetMemberInput {
                    agent_id: "agent_a".to_string(),
                    provider_id: Some("prov_x".to_string()),
                    model: Some("claude-opus-4-8".to_string()),
                    role_hint: None,
                    capability_profile: None,
                    constraints: None,
                    sort_order: None,
                }],
            },
        )
        .await
        .expect("seed fleet");
    let ws = state
        .workspace
        .create(
            "u1",
            CreateWorkspaceRequest {
                name: "cancel ws".to_string(),
                default_fleet_id: Some(fleet.id.clone()),
                workspace_dir: None,
            },
        )
        .await
        .expect("seed workspace");

    // Create the run directly (not via the route) so we can cancel it before the
    // tiny mock loop finishes — then drive cancel through the route.
    let run = state
        .run_service
        .create(
            "u1",
            CreateRunRequest {
                workspace_id: ws.id,
                goal: "to be cancelled".to_string(),
                fleet_id: fleet.id,
                autonomy: None,
                max_parallel: None,
            },
        )
        .await
        .expect("seed run");
    let run_id = run.id.clone();

    let app = router(state);
    let resp = app
        .clone()
        .oneshot(post(&format!("/api/orchestrator/runs/{run_id}/cancel"), serde_json::json!({})))
        .await
        .expect("cancel run request");
    assert_eq!(resp.status(), StatusCode::OK, "POST /runs/{{id}}/cancel should be 200");

    let resp = app
        .clone()
        .oneshot(get(&format!("/api/orchestrator/runs/{run_id}")))
        .await
        .expect("get run request");
    let json = body_json(resp).await;
    assert_eq!(json["data"]["run"]["status"], "cancelled", "run persisted as cancelled");
}

// ----------------------------------------------------------------------------
// P2: parallel run completion + cancel propagation to in-flight workers, through
// the real route↔RunService↔RunEngine seam (mock planner/worker/canceller).
// ----------------------------------------------------------------------------

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A diamond DAG: A,B independent → C depends on both. With cap≥2 the engine
/// runs A and B concurrently, so two workers are in flight at once — the shape a
/// cancel-mid-run test needs.
struct DiamondPlanProducer;

#[async_trait]
impl PlanProducer for DiamondPlanProducer {
    async fn produce(&self, _goal: &str, _members: &[FleetMember], _sink: Option<&nomifun_orchestrator::LeadThinkingSink>) -> Result<PlannedDag, AppError> {
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

/// Records every conversation id the engine asked to cancel, so the test can
/// assert cancel propagated to the in-flight workers (the app's production
/// canceller wraps `ConversationService::cancel`; here we record instead).
struct RecordingCanceller {
    cancelled: Arc<Mutex<Vec<i64>>>,
}
#[async_trait]
impl ConversationCanceller for RecordingCanceller {
    async fn cancel(&self, conversation_id: i64) {
        self.cancelled.lock().unwrap().push(conversation_id);
    }
}

/// A worker that reports a distinct conversation id per task (via `on_started`,
/// so the running task row carries it) then blocks for a long delay — keeping
/// workers in flight while the test cancels.
struct LongDelayWorkerRunner {
    delay: Duration,
    next_conv_id: AtomicUsize,
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
        tokio::time::sleep(self.delay).await;
        Ok(WorkerOutcome {
            conversation_id: conv_id,
            text: Some(format!("output of {task_id}")),
            ok: true,
            tokens: None,
        })
    }
}

/// Build a test state over a fresh in-memory DB whose engine has cap=2, a diamond
/// planner, a long-delay worker (so workers stay in-flight), and the given
/// recording canceller wired into `RunEngineDeps` — the same `cancel_conversation`
/// seam `build_orchestrator_state` injects in production.
async fn build_cancel_state(canceller: Arc<dyn ConversationCanceller>) -> OrchestratorRouterState {
    let db = init_database_memory().await.expect("db init");
    let pool = db.pool().clone();
    let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
    let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
    let run_repo = Arc::new(SqliteRunRepository::new(pool.clone()));

    let fleet = FleetService::new(fleet_repo.clone());
    let workspace = WorkspaceService::new(ws_repo.clone());
    let emitter = OrchestratorRunEventEmitter::new(Arc::new(NoopBroadcaster));
    let planner: Arc<dyn PlanProducer> = Arc::new(DiamondPlanProducer);

    let run_service = Arc::new(RunService::new(
        run_repo.clone(),
        fleet_repo,
        ws_repo.clone(),
        planner,
        emitter.clone(),
    ));
    let worker: Arc<dyn WorkerRunner> = Arc::new(LongDelayWorkerRunner {
        delay: Duration::from_secs(30),
        next_conv_id: AtomicUsize::new(6000),
    });
    let mut engine_deps = RunEngineDeps::new(run_repo, worker, emitter, ws_repo);
    engine_deps.worker_timeout = Duration::from_secs(60);
    engine_deps.default_max_parallel = 2;
    engine_deps.cancel_conversation = canceller;
    let engine = RunEngine::new(Arc::new(engine_deps));

    OrchestratorRouterState::new(
        fleet,
        workspace,
        run_service,
        engine,
        build_test_conv_service(pool),
    )
}

/// Seed a single-member fleet + a workspace via the state's services. Returns
/// (fleet_id, workspace_id).
async fn seed_fleet_and_ws(state: &OrchestratorRouterState, prefix: &str) -> (String, String) {
    let fleet = state
        .fleet
        .create(
            "u1",
            CreateFleetRequest {
                name: format!("{prefix} fleet"),
                description: None,
                max_parallel: None,
                members: vec![FleetMemberInput {
                    agent_id: "agent_a".to_string(),
                    provider_id: Some("prov_x".to_string()),
                    model: Some("claude-opus-4-8".to_string()),
                    role_hint: None,
                    capability_profile: None,
                    constraints: None,
                    sort_order: None,
                }],
            },
        )
        .await
        .expect("seed fleet");
    let ws = state
        .workspace
        .create(
            "u1",
            CreateWorkspaceRequest {
                name: format!("{prefix} ws"),
                default_fleet_id: Some(fleet.id.clone()),
                workspace_dir: None,
            },
        )
        .await
        .expect("seed workspace");
    (fleet.id, ws.id)
}

/// Cancelling a run mid-flight (through the route) cancels the in-flight worker
/// conversations: the recording canceller receives the conv ids the running tasks
/// carry, and the run persists `cancelled`.
#[tokio::test]
async fn run_cancel_propagates_to_in_flight_worker_conversations() {
    let cancelled = Arc::new(Mutex::new(Vec::<i64>::new()));
    let canceller: Arc<dyn ConversationCanceller> = Arc::new(RecordingCanceller {
        cancelled: cancelled.clone(),
    });
    let state = build_cancel_state(canceller).await;
    let (fleet_id, ws_id) = seed_fleet_and_ws(&state, "cancel-prop").await;

    // Create + plan + start the run directly so we can observe the running tasks
    // before cancelling. (The route's create_run also starts the engine, but
    // creating directly keeps the run id in hand.)
    let run = state
        .run_service
        .create(
            "u1",
            CreateRunRequest {
                workspace_id: ws_id,
                goal: "cancel mid-flight".to_string(),
                fleet_id,
                autonomy: None,
                max_parallel: Some(2),
            },
        )
        .await
        .expect("seed run");
    let run_id = run.id.clone();
    state.run_service.plan(&run_id).await.expect("plan");
    state.engine.start(run_id.clone());

    // Wait until a task is running with a stamped conversation_id (the in-flight
    // workers). Bounded ~200×10ms.
    let mut in_flight: Vec<i64> = vec![];
    for _ in 0..200 {
        let detail = state.run_service.get_detail(&run_id).await.expect("detail");
        in_flight = detail
            .tasks
            .iter()
            .filter(|t| t.status == "running")
            .filter_map(|t| t.conversation_id)
            .collect();
        if !in_flight.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !in_flight.is_empty(),
        "at least one worker must be in flight (running + conv stamped) before cancel"
    );

    // Cancel through the route: it calls engine.stop (→ cancel in-flight) then
    // run_service.cancel (→ persist cancelled).
    let app = router(state);
    let resp = app
        .clone()
        .oneshot(post(&format!("/api/orchestrator/runs/{run_id}/cancel"), serde_json::json!({})))
        .await
        .expect("cancel run request");
    assert_eq!(resp.status(), StatusCode::OK, "cancel should be 200");

    // The canceller (detached in stop) must record the in-flight conv id(s).
    let mut got: Vec<i64> = vec![];
    for _ in 0..200 {
        got = cancelled.lock().unwrap().clone();
        if !got.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !got.is_empty(),
        "cancel must propagate to the in-flight worker conversation(s); none recorded"
    );
    for c in &got {
        assert!(
            in_flight.contains(c),
            "cancelled conv {c} must be one of this run's in-flight convs {in_flight:?}"
        );
    }

    // Run persisted cancelled.
    let resp = app
        .oneshot(get(&format!("/api/orchestrator/runs/{run_id}")))
        .await
        .expect("get run request");
    let json = body_json(resp).await;
    assert_eq!(json["data"]["run"]["status"], "cancelled", "run persisted as cancelled");
}

// ----------------------------------------------------------------------------
// P1: the create-from-model-range SEAM, end to end through the engine.
//
// Task 2 unit-tested `create_adhoc` (Range → 2 synthetic members snapshotted) and
// Task 3 unit-tested the gateway caps handler's pure functions (parse_lead_extra +
// expand_auto_range). But neither drives a `create_adhoc`-synthesized fleet
// snapshot THROUGH the planner + engine to `completed` — i.e. proves the engine
// can resolve the synthetic `rmbr_*` members from the snapshot, dispatch workers
// for them, and that the run's own `work_dir` (not a workspace dir — there is no
// workspace) reaches the worker. This test closes that seam.
//
// It mirrors `nomi_run_create`'s exact choreography (caps_orchestrator.rs:126-135):
//   create_adhoc(Range)  →  plan  →  engine.start  →  poll to completed
// with the same mock planner/worker stack the other e2e tests use, so it runs in
// CI without a live LLM. (The gateway tool's own create→plan→start wrapping +
// `Auto` expansion are covered by Task 3's unit tests; this proves the service +
// engine seam underneath it.)
// ----------------------------------------------------------------------------

/// Tiny owned snapshot of the fields the adhoc seam test asserts on after the
/// poll, so the borrow on the awaited detail doesn't escape the loop.
struct RunDetailLike {
    tasks_done: usize,
    tasks_total: usize,
    summary: String,
}

/// A worker that records, per call, the `workspace_dir` it was handed and the
/// resolved member's provider+model — so the adhoc seam test can assert the run's
/// own `work_dir` propagated (no workspace exists) and that the synthetic members
/// reached the worker. Returns a fixed conv id + ok outcome so the run completes.
struct RecordingAdhocWorkerRunner {
    seen_workspace_dir: Mutex<Vec<Option<String>>>,
    seen_member: Mutex<Vec<(Option<String>, Option<String>)>>,
}
#[async_trait]
impl WorkerRunner for RecordingAdhocWorkerRunner {
    async fn run(
        &self,
        member: &FleetMember,
        workspace_dir: Option<&str>,
        _run_id: &str,
        _task_id: &str,
        _brief: &str,
        _task_spec: &str,
        _timeout: Duration,
        on_started: Box<dyn FnOnce(i64) + Send>,
    ) -> Result<WorkerOutcome, AppError> {
        self.seen_workspace_dir
            .lock()
            .unwrap()
            .push(workspace_dir.map(str::to_string));
        self.seen_member
            .lock()
            .unwrap()
            .push((member.provider_id.clone(), member.model.clone()));
        on_started(7777);
        Ok(WorkerOutcome {
            conversation_id: 7777,
            text: Some("adhoc task output".to_string()),
            ok: true,
            tokens: None,
        })
    }
}

/// The conversation-native seam: a run created straight from a model range (no
/// workspace, no pre-built fleet) plans + executes to `completed` through the real
/// RunService + RunEngine, with the run's own `work_dir` reaching the worker and
/// the synthetic members resolved from the frozen snapshot.
#[tokio::test]
async fn adhoc_run_from_model_range_completes_through_engine() {
    // Build state exactly like `build_run_state`, but hold the recording worker so
    // we can assert what it received. Single-member chain DAG is enough — the
    // planner pre-assigns both tasks to member 0 (a synthetic `rmbr_*` member).
    let db = init_database_memory().await.expect("db init");
    let pool = db.pool().clone();
    let fleet_repo = Arc::new(SqliteFleetRepository::new(pool.clone()));
    let ws_repo = Arc::new(SqliteOrchWorkspaceRepository::new(pool.clone()));
    let run_repo = Arc::new(SqliteRunRepository::new(pool));

    let _fleet = FleetService::new(fleet_repo.clone());
    let _workspace = WorkspaceService::new(ws_repo.clone());
    let emitter = OrchestratorRunEventEmitter::new(Arc::new(NoopBroadcaster));
    let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);

    let run_service = Arc::new(RunService::new(
        run_repo.clone(),
        fleet_repo,
        ws_repo.clone(),
        planner,
        emitter.clone(),
    ));
    let worker = Arc::new(RecordingAdhocWorkerRunner {
        seen_workspace_dir: Mutex::new(vec![]),
        seen_member: Mutex::new(vec![]),
    });
    let worker_dyn: Arc<dyn WorkerRunner> = worker.clone();
    let mut engine_deps = RunEngineDeps::new(run_repo, worker_dyn, emitter, ws_repo);
    engine_deps.worker_timeout = Duration::from_secs(5);
    let engine = RunEngine::new(Arc::new(engine_deps));

    // The exact choreography `nomi_run_create` performs (caps_orchestrator.rs):
    // create_adhoc(Range of 2 models, with the lead conversation's work_dir) → plan
    // → engine.start. No workspace, no fleet — the fleet is synthesized from the
    // range. `autonomous` so `plan` flips straight to `running` (no approval gate).
    let run = run_service
        .create_adhoc(
            "u1",
            CreateAdhocRunRequest {
                approval_mode: None,
                goal: "ship the conversation-native run".to_string(),
                work_dir: Some("/tmp/adhoc-proj".to_string()),
                model_range: ModelRange::Range {
                    models: vec![
                        ModelRef { provider_id: "prov_a".to_string(), model: "model-a".to_string() },
                        ModelRef { provider_id: "prov_b".to_string(), model: "model-b".to_string() },
                    ],
                },
                pinned_roles: vec![],
                role_members: vec![],
                autonomy: Some("autonomous".to_string()),
                max_parallel: None,
                lead_conv_id: Some(909),
                lead_model: None,
            },
        )
        .await
        .expect("create_adhoc from range");

    // The fleet snapshot must hold two Nomi-runnable synthetic members (this is the
    // create_adhoc → snapshot half of the seam; the engine half is asserted below).
    let detail = run_service.get_detail(&run.id).await.expect("detail after create");
    assert!(detail.run.workspace_id.is_none(), "ad-hoc run has no workspace");
    assert_eq!(detail.run.work_dir.as_deref(), Some("/tmp/adhoc-proj"));
    assert_eq!(detail.fleet_members.len(), 2, "two synthetic members from the range");
    assert!(
        detail.fleet_members.iter().all(|m| m.id.starts_with("rmbr_")),
        "synthetic member ids use the rmbr_ prefix"
    );

    run_service.plan(&run.id).await.expect("plan");
    engine.start(run.id.clone());

    // Poll the service directly (this seam is below the HTTP layer — the gateway
    // tool, not a REST route, drives it) until the run completes (~50×100ms).
    let mut completed = false;
    let mut last: Option<RunDetailLike> = None;
    for _ in 0..50 {
        let d = run_service.get_detail(&run.id).await.expect("detail poll");
        if d.run.status == "completed" {
            last = Some(RunDetailLike {
                tasks_done: d.tasks.iter().filter(|t| t.status == "done").count(),
                tasks_total: d.tasks.len(),
                summary: d.run.summary.clone().unwrap_or_default(),
            });
            completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(completed, "ad-hoc run must reach completed within the bounded poll");
    let last = last.unwrap();

    // Engine half of the seam: both tasks ran to done (the engine resolved the
    // synthetic members from the snapshot + dispatched workers for them).
    assert_eq!(last.tasks_total, 2, "2 tasks planned");
    assert_eq!(last.tasks_done, 2, "both tasks done");
    assert!(last.summary.contains("2/2"), "summary reflects 2/2 done, got: {}", last.summary);

    // The run's OWN work_dir (there is no workspace) reached every worker call.
    let dirs = worker.seen_workspace_dir.lock().unwrap().clone();
    assert_eq!(dirs.len(), 2, "worker invoked once per task");
    for d in &dirs {
        assert_eq!(
            d.as_deref(),
            Some("/tmp/adhoc-proj"),
            "the ad-hoc run's own work_dir must reach the worker (no workspace dir exists)"
        );
    }
    // The synthetic member reached the worker as a Nomi-runnable member. Both tasks
    // are pre-assigned to member 0 ⇒ provider_a/model-a on both calls.
    let members = worker.seen_member.lock().unwrap().clone();
    for (provider, model) in &members {
        assert_eq!(provider.as_deref(), Some("prov_a"), "worker got the synthetic member's provider");
        assert_eq!(model.as_deref(), Some("model-a"), "worker got the synthetic member's model");
    }
}
