use serde::{Deserialize, Serialize};

/// Coarse readiness exposed to the model hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManagedModelServiceAvailability {
    /// A seed catalog is loaded, but live inference has not been verified.
    Unverified,
    Ready,
    Degraded,
}

/// A model projected by a managed model service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedModel {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub source: String,
}

/// Result state for a low-cost inference probe through a managed model supply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManagedModelHealthStatus {
    Unknown,
    Healthy,
    Unhealthy,
}

/// Stable, source-neutral failure category for managed-model health probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedModelHealthErrorKind {
    ServiceDisabled,
    ModelDisabled,
    Busy,
    Timeout,
    Unavailable,
    InvalidResponse,
    Unknown,
}

/// Result of checking one managed model through the public NomiFun adapter.
///
/// The contract intentionally contains no upstream URL or vendor error body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedModelHealthResult {
    pub model_id: String,
    pub status: ManagedModelHealthStatus,
    pub latency_ms: Option<u64>,
    /// Unix epoch milliseconds at which the probe completed.
    pub checked_at: i64,
    pub error_kind: Option<ManagedModelHealthErrorKind>,
    pub message: Option<String>,
}

/// Aggregate returned after checking the managed free-model list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedModelHealthBatchResult {
    pub results: Vec<ManagedModelHealthResult>,
    pub total: usize,
    pub healthy: usize,
    pub unhealthy: usize,
    pub unknown: usize,
}

/// Status returned by the managed free-model service and its mutations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedModelServiceStatus {
    pub protocol_version: String,
    pub provider_id: Option<String>,
    pub enabled: bool,
    pub ready: bool,
    pub upstream: String,
    pub models: Vec<ManagedModel>,
    /// Unix epoch milliseconds for the most recent successful live refresh.
    pub last_refresh: Option<i64>,
    /// Whether this process is automatically refreshing the managed catalog.
    pub automatic_refresh: bool,
    /// Nominal successful-refresh interval in milliseconds (before jitter).
    pub refresh_interval_ms: u64,
    /// Unix epoch milliseconds for the next scheduled refresh attempt.
    pub next_refresh: Option<i64>,
    pub last_error: Option<String>,
    pub privacy_notice: String,
    pub availability: ManagedModelServiceAvailability,
}

/// Request body for `POST /api/model-services/free/activate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetManagedModelServiceEnabledRequest {
    pub enabled: bool,
}

/// Request body for `PATCH /api/model-services/free/models/:id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetManagedModelEnabledRequest {
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_uses_camel_case_wire_contract() {
        let provider_id = nomifun_common::ProviderId::new().into_string();
        let status = ManagedModelServiceStatus {
            protocol_version: "1".into(),
            provider_id: Some(provider_id.clone()),
            enabled: true,
            ready: true,
            upstream: "oc".into(),
            models: vec![ManagedModel {
                id: "example-free".into(),
                name: "Example".into(),
                enabled: true,
                source: "oc".into(),
            }],
            last_refresh: Some(1_700_000_000_000),
            automatic_refresh: true,
            refresh_interval_ms: 21_600_000,
            next_refresh: Some(1_700_021_600_000),
            last_error: None,
            privacy_notice: "Requests leave this device.".into(),
            availability: ManagedModelServiceAvailability::Ready,
        };

        let json = serde_json::to_value(status).unwrap();
        assert_eq!(json["protocolVersion"], "1");
        assert_eq!(json["providerId"], provider_id);
        assert_eq!(json["lastRefresh"], 1_700_000_000_000_i64);
        assert_eq!(json["automaticRefresh"], true);
        assert_eq!(json["refreshIntervalMs"], 21_600_000_u64);
        assert_eq!(json["nextRefresh"], 1_700_021_600_000_i64);
        assert!(json.get("protocol_version").is_none());
    }

    #[test]
    fn availability_uses_stable_lowercase_wire_values() {
        assert_eq!(
            serde_json::to_value(ManagedModelServiceAvailability::Unverified).unwrap(),
            serde_json::json!("unverified")
        );
    }

    #[test]
    fn health_result_uses_source_neutral_camel_case_contract() {
        let result = ManagedModelHealthResult {
            model_id: "example-free".into(),
            status: ManagedModelHealthStatus::Unhealthy,
            latency_ms: Some(123),
            checked_at: 1_700_000_000_000,
            error_kind: Some(ManagedModelHealthErrorKind::Unavailable),
            message: Some("The free model is temporarily unavailable.".into()),
        };

        let json = serde_json::to_value(result).unwrap();
        assert_eq!(json["modelId"], "example-free");
        assert_eq!(json["status"], "unhealthy");
        assert_eq!(json["latencyMs"], 123);
        assert_eq!(json["checkedAt"], 1_700_000_000_000_i64);
        assert_eq!(json["errorKind"], "unavailable");
        assert!(json.get("model_id").is_none());
    }
}
