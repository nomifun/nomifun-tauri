use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, PartialEq, Eq)]
pub struct CronJobRunRow {
    pub id: i64,
    pub cron_job_run_id: String,
    pub cron_job_id: String,
    pub executed_at_ms: TimestampMs,
    pub status: String,
    pub created_at_ms: TimestampMs,
}

/// Durable identity and lifecycle of one cron trigger.
///
/// This table is intentionally separate from the seven-row presentation
/// history: pruning UI history must never erase the evidence that absorbs an
/// at-least-once scheduler or run-now replay.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, PartialEq, Eq)]
pub struct CronRunReservationRow {
    pub id: i64,
    pub cron_job_run_id: String,
    pub cron_job_id: String,
    pub trigger_kind: String,
    pub operation_key: String,
    pub request_fingerprint: String,
    pub schedule_revision: Option<i64>,
    pub planned_at_ms: Option<TimestampMs>,
    pub status: String,
    pub conversation_id: Option<String>,
    pub result_error: Option<String>,
    pub created_at_ms: TimestampMs,
    pub updated_at_ms: TimestampMs,
    pub settled_at_ms: Option<TimestampMs>,
    /// `pending` is minted only by the current exact-projection protocol.
    /// `legacy_unknown` is quarantined because older versions could update the
    /// job before settling this row without persisting the applied run id.
    pub job_projection_state: String,
    pub job_projected_at_ms: Option<TimestampMs>,
}
