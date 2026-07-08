//! Router state for the workshop domain: the `Arc`-wrapped service.

use std::sync::Arc;

use crate::service::WorkshopService;

#[derive(Clone)]
pub struct WorkshopRouterState {
    pub service: Arc<WorkshopService>,
}

impl WorkshopRouterState {
    pub fn new(service: Arc<WorkshopService>) -> Self {
        Self { service }
    }
}
