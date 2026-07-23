//! Cross-subsystem provider-deletion guard hook. Implemented at the app layer
//! (the only place that sees companions, IDMM and Agent Executions), injected into
//! `ProviderService` so deletion can refuse in-use providers.

use nomifun_common::{AppError, ProviderLifecycleBarrier, ProviderUsage};
use std::sync::Arc;

#[async_trait::async_trait]
pub trait ProviderDeletionCoordinator: Send + Sync {
    /// Returns every hard-binding usage of `provider_id`; empty ⇒ safe to delete.
    async fn usages(&self, provider_id: &str) -> Result<Vec<ProviderUsage>, AppError>;

    /// Clear soft references stored outside SQLite before deleting the Provider.
    /// The service holds the lifecycle write guard while this hook runs.
    async fn cleanup_soft_references(&self, _provider_id: &str) -> Result<(), AppError> {
        Ok(())
    }

    /// Process-local barrier shared with side-store writers. The Provider
    /// service takes its write guard across the usage scan and DB delete.
    fn provider_lifecycle_barrier(&self) -> Option<Arc<ProviderLifecycleBarrier>> {
        None
    }
}

pub type SharedProviderDeletionCoordinator = Arc<dyn ProviderDeletionCoordinator>;
