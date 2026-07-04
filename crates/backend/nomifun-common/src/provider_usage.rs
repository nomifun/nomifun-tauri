//! Provider-in-use reporting types shared between the deletion guard,
//! the `AppError::ProviderInUse` variant, and the HTTP error body.

use serde::{Deserialize, Serialize};

/// Which feature holds a live reference to a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderUsageFeature {
    DesktopCompanion,
    PublicCompanion,
    SmartDecision,
    Orchestrator,
}

/// One concrete usage of a provider by a feature (for the "cannot delete" UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsage {
    pub feature: ProviderUsageFeature,
    /// Human-readable name of the referencing entity (companion name, fleet name, …).
    pub label: String,
    /// Optional id to deep-link the user to the unbind location.
    pub target_id: Option<String>,
}

/// Structured payload for `AppError::ProviderInUse` → HTTP `details`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInUseDetails {
    pub usages: Vec<ProviderUsage>,
}
