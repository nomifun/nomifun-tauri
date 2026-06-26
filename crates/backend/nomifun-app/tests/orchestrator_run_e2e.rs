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
    CreateFleetRequest, CreateRunRequest, CreateWorkspaceRequest, FleetMember, FleetMemberInput,
    PlannedDag, PlannedTask, WebSocketMessage,
};
use nomifun_auth::CurrentUser;
use nomifun_common::AppError;
use nomifun_db::{
    SqliteFleetRepository, SqliteOrchWorkspaceRepository, SqliteRunRepository, init_database_memory,
};
use nomifun_orchestrator::{
    FleetService, MockWorkerRunner, OrchestratorRunEventEmitter, OrchestratorRouterState,
    PlanProducer, RunEngine, RunEngineDeps, RunService, WorkerRunner, WorkspaceService,
    orchestrator_routes,
};
use nomifun_realtime::EventBroadcaster;

/// No-op broadcaster: this test asserts on persisted run state, not the WS trail.
struct NoopBroadcaster;
impl EventBroadcaster for NoopBroadcaster {
    fn broadcast(&self, _event: WebSocketMessage<serde_json::Value>) {}
}

/// A fixed 2-task chain DAG: task0 (no dep) → task1 (depends on 0), both
/// pre-assigned to member 0 so a single-member fleet suffices. Mirrors the
/// production `PlanProducer` contract (Task 4) without a live LLM.
struct ChainPlanProducer;

#[async_trait]
impl PlanProducer for ChainPlanProducer {
    async fn produce(&self, _goal: &str, _members: &[FleetMember]) -> Result<PlannedDag, AppError> {
        Ok(PlannedDag {
            tasks: vec![
                PlannedTask {
                    title: "Gather".to_string(),
                    spec: "collect sources".to_string(),
                    task_profile: None,
                    depends_on: vec![],
                    member_index: Some(0),
                    rationale: Some("first".to_string()),
                },
                PlannedTask {
                    title: "Synthesize".to_string(),
                    spec: "write the report".to_string(),
                    task_profile: None,
                    depends_on: vec![0],
                    member_index: Some(0),
                    rationale: None,
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
    let run_repo = Arc::new(SqliteRunRepository::new(pool));

    let fleet = FleetService::new(fleet_repo.clone());
    let workspace = WorkspaceService::new(ws_repo.clone());
    let emitter = OrchestratorRunEventEmitter::new(Arc::new(NoopBroadcaster));
    let planner: Arc<dyn PlanProducer> = Arc::new(ChainPlanProducer);

    let run_service = Arc::new(RunService::new(
        run_repo.clone(),
        fleet_repo,
        ws_repo,
        planner,
        emitter.clone(),
    ));
    let worker: Arc<dyn WorkerRunner> = Arc::new(MockWorkerRunner::with_text(4242, "task output"));
    let mut engine_deps = RunEngineDeps::new(run_repo, worker, emitter);
    engine_deps.worker_timeout = Duration::from_secs(5);
    let engine = RunEngine::new(Arc::new(engine_deps));

    OrchestratorRouterState::new(fleet, workspace, run_service, engine)
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
