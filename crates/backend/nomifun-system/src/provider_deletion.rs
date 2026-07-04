//! Cross-subsystem provider-deletion guard hook. Implemented at the app layer
//! (the only place that sees companion/idmm/orchestrator), injected into
//! `ProviderService` so deletion can refuse in-use providers.

use nomifun_common::{AppError, ProviderUsage};
use std::sync::Arc;

#[async_trait::async_trait]
pub trait ProviderDeletionCoordinator: Send + Sync {
    /// Returns every hard-binding usage of `provider_id`; empty ⇒ safe to delete.
    async fn usages(&self, provider_id: &str) -> Result<Vec<ProviderUsage>, AppError>;

    /// Best-effort cleanup of soft references after a successful delete.
    async fn cleanup_soft_refs(&self, provider_id: &str) -> Result<(), AppError>;
}

pub type SharedProviderDeletionCoordinator = Arc<dyn ProviderDeletionCoordinator>;
