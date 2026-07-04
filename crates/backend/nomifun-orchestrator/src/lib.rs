//! 智能编排 (orchestration) services.
//!
//! - [`FleetService`] — CRUD over per-user fleets (编队) and their members,
//!   handling Row↔DTO mapping and JSON (de)serialization of the per-member
//!   `capability_profile` / `constraints` (fail-soft on decode).
//! - [`WorkspaceService`] — CRUD over per-user orchestration workspaces (Row↔DTO
//!   mapping; the DTO omits the internal `user_id` / `context` columns).
//! - [`OrchestratorError`] — service-layer error mapped into `AppError`.
//! - [`OrchestratorRunEventEmitter`] — realtime WS event seam the Run engine
//!   calls to stream run/task lifecycle status to connected frontends.
//! - [`OrchestratorRouterState`] — router state (`fleet` + `workspace`).
//! - [`orchestrator_routes`] — the axum router mounting the fleet/workspace CRUD
//!   endpoints. Auth is layered externally in nomifun-app, so handlers safely
//!   extract `CurrentUser`.

pub mod engine;
pub mod error;
pub mod events;
pub mod plan;
pub mod router;
pub mod routes;
pub mod run_service;
pub mod service;
pub mod state;
pub mod worker;

pub use engine::{
    AUTONOMOUS_MAX_TASKS, AUTONOMOUS_TOKEN_BUDGET, ConversationCanceller, ConversationSteerer,
    DEFAULT_MAX_PARALLEL, DEFAULT_WORKER_TIMEOUT, LeadReporter, NO_PROGRESS_LIMIT,
    NoopConversationCanceller, NoopConversationSteerer, NoopLeadReporter, RunEngine, RunEngineDeps,
    RunOutcome,
};
pub use error::OrchestratorError;
pub use events::OrchestratorRunEventEmitter;
pub use plan::{LeadDeltaKind, LeadThinkingSink, LeadThinkingThrottle, LlmPlanProducer, PlanProducer};
pub use routes::orchestrator_routes;
pub use router::{rank_members, score_member, ScoredCandidate};
pub use run_service::{spawn_plan_and_start, spawn_plan_flat_and_start, RunService};
pub use service::{FleetService, WorkspaceService};
pub use state::OrchestratorRouterState;
pub use worker::{ConversationWorkerRunner, MockWorkerRunner, WorkerOutcome, WorkerRunner};
