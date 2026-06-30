use std::sync::Arc;

use nomifun_conversation::ConversationService;

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
///
/// **Path B (B3):** also carries a [`ConversationService`] handle so the
/// `create_adhoc_run` route can associate the originating conversation as the
/// run's lead (`link_orchestrator_run`) when the request carries a
/// `lead_conv_id`. This is the SAME `ConversationService` the worker runs turns
/// on (cheap `Arc`-internal clone; same DB) — `build_orchestrator_state` threads
/// in the already-constructed instance rather than spinning up a second one.
#[derive(Clone)]
pub struct OrchestratorRouterState {
    pub fleet: FleetService,
    pub workspace: WorkspaceService,
    pub run_service: Arc<RunService>,
    pub engine: RunEngine,
    pub conversation_service: ConversationService,
}

impl OrchestratorRouterState {
    pub fn new(
        fleet: FleetService,
        workspace: WorkspaceService,
        run_service: Arc<RunService>,
        engine: RunEngine,
        conversation_service: ConversationService,
    ) -> Self {
        Self {
            fleet,
            workspace,
            run_service,
            engine,
            conversation_service,
        }
    }
}
