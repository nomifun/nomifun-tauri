use nomifun_common::TimestampMs;

use crate::error::DbError;
use crate::models::{CronJobRow, CronJobRunRow};

pub const CRON_RUN_HISTORY_LIMIT: i64 = 7;

/// Parameters for updating a cron job.
///
/// All fields are optional; `None` means "keep the current value".
#[derive(Debug, Clone, Default)]
pub struct UpdateCronJobParams {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub schedule_kind: Option<String>,
    pub schedule_value: Option<String>,
    pub schedule_tz: Option<Option<String>>,
    pub schedule_description: Option<Option<String>>,
    pub payload_message: Option<String>,
    pub execution_mode: Option<String>,
    pub agent_config: Option<Option<String>>,
    pub preset_id: Option<Option<String>>,
    pub preset_revision: Option<Option<i64>>,
    pub preset_snapshot: Option<Option<String>>,
    /// Target conversation. `Some(Some(id))` binds a conversation, `Some(None)`
    /// clears it to NULL, `None` leaves it unchanged.
    pub conversation_id: Option<Option<String>>,
    pub conversation_title: Option<Option<String>>,
    pub agent_type: Option<String>,
    pub skill_content: Option<Option<String>>,
    pub description: Option<Option<String>>,
    pub next_run_at: Option<Option<TimestampMs>>,
    pub last_run_at: Option<Option<TimestampMs>>,
    pub last_status: Option<Option<String>>,
    pub last_error: Option<Option<String>>,
    pub run_count: Option<i64>,
    pub retry_count: Option<i64>,
    pub max_retries: Option<i64>,
}

/// Data access abstraction for the `cron_jobs` table.
#[async_trait::async_trait]
pub trait ICronRepository: Send + Sync {
    /// Inserts a new cron job row.
    async fn insert(&self, row: &CronJobRow) -> Result<(), DbError>;

    /// Updates a cron job by its stable business ID with the provided fields.
    /// Returns `DbError::NotFound` if absent.
    async fn update(
        &self,
        user_id: &str,
        cron_job_id: &str,
        params: &UpdateCronJobParams,
    ) -> Result<(), DbError>;

    /// Deletes a cron job by business ID. Returns `DbError::NotFound` if absent.
    async fn delete(&self, user_id: &str, cron_job_id: &str) -> Result<(), DbError>;

    /// Returns a single cron job by business ID, or `None` if not found.
    async fn get_by_cron_job_id(
        &self,
        user_id: &str,
        cron_job_id: &str,
    ) -> Result<Option<CronJobRow>, DbError>;

    /// Returns all cron jobs ordered by creation time ascending.
    async fn list_all(&self, user_id: &str) -> Result<Vec<CronJobRow>, DbError>;

    /// Process-internal scheduler lookup. The returned row carries the
    /// authoritative non-empty owner; callers must preserve it through the
    /// execution path rather than supplying or deriving a fallback owner.
    async fn get_by_cron_job_id_for_scheduler(
        &self,
        cron_job_id: &str,
    ) -> Result<Option<CronJobRow>, DbError>;

    /// Process-internal scheduler scan across owners.
    async fn list_enabled_for_scheduler(&self) -> Result<Vec<CronJobRow>, DbError>;

    /// Returns all cron jobs for a given conversation.
    async fn list_by_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<CronJobRow>, DbError>;

    /// Deletes all cron jobs associated with a conversation.
    /// Returns the number of deleted rows.
    async fn delete_by_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<u64, DbError>;

    /// Inserts one execution record and prunes older rows for the same job so
    /// each job retains at most [`CRON_RUN_HISTORY_LIMIT`] rows.
    async fn insert_run_pruned(
        &self,
        user_id: &str,
        row: &CronJobRunRow,
    ) -> Result<(), DbError>;

    /// Returns recent execution records for one job, newest first.
    async fn list_runs_by_job(
        &self,
        user_id: &str,
        cron_job_id: &str,
        limit: i64,
    ) -> Result<Vec<CronJobRunRow>, DbError>;
}
