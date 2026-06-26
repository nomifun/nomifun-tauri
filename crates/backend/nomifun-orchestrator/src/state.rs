use std::sync::Arc;

use crate::engine::RunEngine;
use crate::run_service::RunService;
use crate::service::{FleetService, WorkspaceService};

/// Router state for the orchestration endpoints.
///
/// Consumed by `orchestrator_routes` (Task 8). Carries the fleet/workspace CRUD
/// services plus the Run control-plane: the [`RunService`] (create/plan/inspect/
/// cancel) and the [`RunEngine`] (the serial execution loop). `run_service` is an
/// `Arc` so handlers and the engine share one instance; `RunEngine` is itself
/// `Clone` (cheap — `Arc` internals) so the state stays `Clone`.
#[derive(Clone)]
pub struct OrchestratorRouterState {
    pub fleet: FleetService,
    pub workspace: WorkspaceService,
    pub run_service: Arc<RunService>,
    pub engine: RunEngine,
}

impl OrchestratorRouterState {
    pub fn new(
        fleet: FleetService,
        workspace: WorkspaceService,
        run_service: Arc<RunService>,
        engine: RunEngine,
    ) -> Self {
        Self {
            fleet,
            workspace,
            run_service,
            engine,
        }
    }
}
