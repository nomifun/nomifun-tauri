//! Router state for the generation engine: the `Arc`-wrapped service.

use std::sync::Arc;

use crate::service::CreationService;

#[derive(Clone)]
pub struct CreationRouterState {
    pub service: Arc<CreationService>,
}

impl CreationRouterState {
    pub fn new(service: Arc<CreationService>) -> Self {
        Self { service }
    }
}
