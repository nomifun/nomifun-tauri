//! Router state for the public-agent domain: the `Arc`-wrapped service.

use std::sync::Arc;

use crate::service::PublicAgentService;

#[derive(Clone)]
pub struct PublicAgentRouterState {
    pub service: Arc<PublicAgentService>,
}

impl PublicAgentRouterState {
    pub fn new(service: Arc<PublicAgentService>) -> Self {
        Self { service }
    }
}
