use std::collections::HashMap;
use std::str::FromStr;

use nomifun_api_types::{
    CronAgentConfigDto, CronJobMetadataDto, CronJobResponse, CronJobStateDto, CronScheduleDto,
};
use nomifun_common::{ConversationId, CronJobId, PresetId, ProviderId, TimestampMs, UserId};
use nomifun_db::models::CronJobRow;
use serde::{Deserialize, Serialize};

use crate::error::CronError;

// ---------------------------------------------------------------------------
// Domain enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum CronSchedule {
    At {
        at_ms: TimestampMs,
        description: Option<String>,
    },
    Every {
        every_ms: i64,
        description: Option<String>,
    },
    Cron {
        expr: String,
        tz: Option<String>,
        description: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Existing,
    NewConversation,
}

impl ExecutionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Existing => "existing",
            Self::NewConversation => "new_conversation",
        }
    }
}

impl FromStr for ExecutionMode {
    type Err = CronError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "existing" => Ok(Self::Existing),
            "new_conversation" => Ok(Self::NewConversation),
            other => Err(CronError::InvalidExecutionMode(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CreatedBy {
    User,
    Agent,
}

impl CreatedBy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
        }
    }
}

impl FromStr for CreatedBy {
    type Err = CronError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(Self::User),
            "agent" => Ok(Self::Agent),
            other => Err(CronError::InvalidCreatedBy(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Ok,
    Error,
    Skipped,
    Missed,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Skipped => "skipped",
            Self::Missed => "missed",
        }
    }
}

impl FromStr for JobStatus {
    type Err = CronError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ok" => Ok(Self::Ok),
            "error" => Ok(Self::Error),
            "skipped" => Ok(Self::Skipped),
            "missed" => Ok(Self::Missed),
            other => Err(CronError::InvalidJobStatus(other.to_owned())),
        }
    }
}

// ---------------------------------------------------------------------------
// Agent configuration (domain model)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CronAgentConfig {
    /// ACP/agent backend. Nomi jobs must leave this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_snapshot: Option<nomifun_api_types::ResolvedPresetSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Logical provider reference used by Nomi jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// When `true` and the job reuses an existing conversation
    /// (`ExecutionMode::Existing`), the agent's context is cleared before each
    /// scheduled run so accumulated history does not pile up across ticks.
    /// Visible message records are kept.
    #[serde(default)]
    pub clear_context_each_run: bool,
}

// ---------------------------------------------------------------------------
// CronJob — the core domain model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct CronJob {
    /// Stable business identity exposed by the API and used by all runtime
    /// lookups. The SQLite `cron_jobs.id` column remains an internal index only.
    pub cron_job_id: String,
    /// Immutable aggregate owner and the sole authority for execution,
    /// Conversation creation, persistence and private events.
    pub user_id: String,
    pub name: String,
    pub enabled: bool,
    pub schedule: CronSchedule,
    pub message: String,
    pub execution_mode: ExecutionMode,
    pub agent_config: Option<CronAgentConfig>,
    pub conversation_id: Option<String>,
    pub conversation_title: Option<String>,
    pub agent_type: String,
    pub created_by: CreatedBy,
    pub skill_content: Option<String>,
    pub description: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub next_run_at: Option<TimestampMs>,
    pub last_run_at: Option<TimestampMs>,
    pub last_status: Option<JobStatus>,
    pub last_error: Option<String>,
    pub run_count: i64,
    pub retry_count: i64,
    pub max_retries: i64,
}

// ---------------------------------------------------------------------------
// DB ↔ Domain conversion
// ---------------------------------------------------------------------------

pub fn cron_job_from_row(row: CronJobRow) -> Result<CronJob, CronError> {
    CronJobId::parse(&row.cron_job_id).map_err(|error| {
        CronError::Scheduler(format!("cron job {} has invalid business id: {error}", row.cron_job_id))
    })?;
    UserId::parse(&row.user_id).map_err(|error| {
        CronError::Scheduler(format!("cron job {} has invalid owner: {error}", row.cron_job_id))
    })?;
    if let Some(conversation_id) = row.conversation_id.as_deref() {
        ConversationId::parse(conversation_id).map_err(|error| {
            CronError::Scheduler(format!(
                "cron job {} has invalid conversation id: {error}",
                row.cron_job_id
            ))
        })?;
    }
    let schedule = parse_schedule(
        &row.schedule_kind,
        &row.schedule_value,
        row.schedule_tz.as_deref(),
        row.schedule_description.as_deref(),
    )?;

    let execution_mode = ExecutionMode::from_str(&row.execution_mode)?;
    let created_by = CreatedBy::from_str(&row.created_by)?;

    let mut agent_config = row
        .agent_config
        .as_deref()
        .map(serde_json::from_str::<CronAgentConfig>)
        .transpose()?;
    if let Some(config) = agent_config.as_mut() {
        config.preset_id = row.preset_id.clone();
        config.preset_revision = row.preset_revision;
        config.preset_snapshot = row
            .preset_snapshot
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        validate_agent_config_ids(&row.cron_job_id, &row.agent_type, config)?;
    }

    let last_status = row
        .last_status
        .as_deref()
        .map(JobStatus::from_str)
        .transpose()?;

    Ok(CronJob {
        cron_job_id: row.cron_job_id,
        user_id: row.user_id,
        name: row.name,
        enabled: row.enabled,
        schedule,
        message: row.payload_message,
        execution_mode,
        agent_config,
        conversation_id: row.conversation_id,
        conversation_title: row.conversation_title,
        agent_type: row.agent_type,
        created_by,
        skill_content: row.skill_content,
        description: row.description,
        created_at: row.created_at,
        updated_at: row.updated_at,
        next_run_at: row.next_run_at,
        last_run_at: row.last_run_at,
        last_status,
        last_error: row.last_error,
        run_count: row.run_count,
        retry_count: row.retry_count,
        max_retries: row.max_retries,
    })
}

fn parse_schedule(
    kind: &str,
    value: &str,
    tz: Option<&str>,
    description: Option<&str>,
) -> Result<CronSchedule, CronError> {
    match kind {
        "at" => {
            let at_ms = value
                .parse::<TimestampMs>()
                .map_err(|e| CronError::InvalidSchedule(format!("invalid at_ms '{value}': {e}")))?;
            Ok(CronSchedule::At {
                at_ms,
                description: description.map(String::from),
            })
        }
        "every" => {
            let every_ms = value.parse::<i64>().map_err(|e| {
                CronError::InvalidSchedule(format!("invalid every_ms '{value}': {e}"))
            })?;
            Ok(CronSchedule::Every {
                every_ms,
                description: description.map(String::from),
            })
        }
        "cron" => Ok(CronSchedule::Cron {
            expr: value.to_owned(),
            tz: tz.map(String::from),
            description: description.map(String::from),
        }),
        other => Err(CronError::InvalidSchedule(format!(
            "unknown schedule kind: {other}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Domain ↔ DB conversion
// ---------------------------------------------------------------------------

pub fn cron_job_to_row(job: &CronJob) -> Result<CronJobRow, CronError> {
    CronJobId::parse(&job.cron_job_id).map_err(|error| {
        CronError::Scheduler(format!("cron job {} has invalid business id: {error}", job.cron_job_id))
    })?;
    UserId::parse(&job.user_id).map_err(|error| {
        CronError::Scheduler(format!("cron job {} has invalid owner: {error}", job.cron_job_id))
    })?;
    if let Some(conversation_id) = job.conversation_id.as_deref() {
        ConversationId::parse(conversation_id).map_err(|error| {
            CronError::Scheduler(format!(
                "cron job {} has invalid conversation id: {error}",
                job.cron_job_id
            ))
        })?;
    }
    if let Some(config) = job.agent_config.as_ref() {
        validate_agent_config_ids(&job.cron_job_id, &job.agent_type, config)?;
    }
    let (schedule_kind, schedule_value, schedule_tz, schedule_description) =
        schedule_to_row_fields(&job.schedule);

    let agent_config_json = job
        .agent_config
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    Ok(CronJobRow {
        id: 0,
        cron_job_id: job.cron_job_id.clone(),
        user_id: job.user_id.clone(),
        name: job.name.clone(),
        enabled: job.enabled,
        schedule_kind,
        schedule_value,
        schedule_tz,
        schedule_description,
        payload_message: job.message.clone(),
        execution_mode: job.execution_mode.as_str().to_owned(),
        agent_config: agent_config_json,
        preset_id: job.agent_config.as_ref().and_then(|config| config.preset_id.clone()),
        preset_revision: job.agent_config.as_ref().and_then(|config| config.preset_revision),
        preset_snapshot: job
            .agent_config
            .as_ref()
            .and_then(|config| config.preset_snapshot.as_ref())
            .map(serde_json::to_string)
            .transpose()?,
        conversation_id: job.conversation_id.clone(),
        conversation_title: job.conversation_title.clone(),
        agent_type: job.agent_type.clone(),
        created_by: job.created_by.as_str().to_owned(),
        skill_content: job.skill_content.clone(),
        description: job.description.clone(),
        created_at: job.created_at,
        updated_at: job.updated_at,
        next_run_at: job.next_run_at,
        last_run_at: job.last_run_at,
        last_status: job.last_status.map(|s| s.as_str().to_owned()),
        last_error: job.last_error.clone(),
        run_count: job.run_count,
        retry_count: job.retry_count,
        max_retries: job.max_retries,
    })
}

fn validate_agent_config_ids(
    job_id: &str,
    agent_type: &str,
    config: &CronAgentConfig,
) -> Result<(), CronError> {
    if let Some(preset_id) = config.preset_id.as_deref() {
        PresetId::try_from(preset_id).map_err(|error| {
            CronError::Scheduler(format!(
                "cron job {job_id} has invalid preset_id: {error}"
            ))
        })?;
    }
    if agent_type == "nomi" {
        if config.backend.is_some() {
            return Err(CronError::Scheduler(format!(
                "cron job {job_id} uses legacy agent_config.backend for a Nomi provider; use provider_id"
            )));
        }
        let provider_id = config.provider_id.as_deref().ok_or_else(|| {
            CronError::Scheduler(format!(
                "cron job {job_id} is missing nomi provider_id"
            ))
        })?;
        ProviderId::try_from(provider_id).map_err(|error| {
            CronError::Scheduler(format!(
                "cron job {job_id} has invalid nomi provider id: {error}"
            ))
        })?;
        let model = config
            .model
            .as_deref()
            .ok_or_else(|| {
                CronError::Scheduler(format!(
                    "cron job {job_id} is missing nomi model"
                ))
            })?;
        if model.is_empty() || model.trim() != model {
            return Err(CronError::Scheduler(format!(
                "cron job {job_id} has invalid nomi model"
            )));
        }
    }
    Ok(())
}

fn schedule_to_row_fields(
    schedule: &CronSchedule,
) -> (String, String, Option<String>, Option<String>) {
    match schedule {
        CronSchedule::At { at_ms, description } => (
            "at".to_owned(),
            at_ms.to_string(),
            None,
            description.clone(),
        ),
        CronSchedule::Every {
            every_ms,
            description,
        } => (
            "every".to_owned(),
            every_ms.to_string(),
            None,
            description.clone(),
        ),
        CronSchedule::Cron {
            expr,
            tz,
            description,
        } => (
            "cron".to_owned(),
            expr.clone(),
            tz.clone(),
            description.clone(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Domain ↔ DTO conversion
// ---------------------------------------------------------------------------

pub fn cron_job_to_response(job: &CronJob) -> CronJobResponse {
    let schedule = match &job.schedule {
        CronSchedule::At { at_ms, description } => CronScheduleDto::At {
            at_ms: *at_ms,
            description: description.clone(),
        },
        CronSchedule::Every {
            every_ms,
            description,
        } => CronScheduleDto::Every {
            every_ms: *every_ms,
            description: description.clone(),
        },
        CronSchedule::Cron {
            expr,
            tz,
            description,
        } => CronScheduleDto::Cron {
            expr: expr.clone(),
            tz: tz.clone(),
            description: description.clone(),
        },
    };

    let agent_config_dto = job.agent_config.as_ref().map(|c| CronAgentConfigDto {
        backend: c.backend.clone(),
        name: c.name.clone(),
        cli_path: c.cli_path.clone(),
        custom_agent_id: c.custom_agent_id.clone(),
        preset_id: c.preset_id.clone(),
        preset_revision: c.preset_revision,
        preset_snapshot: c.preset_snapshot.clone(),
        mode: c.mode.clone(),
        model: c.model.clone(),
        provider_id: c.provider_id.clone(),
        config_options: c.config_options.clone(),
        workspace: c.workspace.clone(),
        clear_context_each_run: c.clear_context_each_run,
    });

    CronJobResponse {
        cron_job_id: job.cron_job_id.clone(),
        name: job.name.clone(),
        description: job.description.clone(),
        enabled: job.enabled,
        schedule,
        message: job.message.clone(),
        execution_mode: job.execution_mode.as_str().to_owned(),
        metadata: CronJobMetadataDto {
            conversation_id: job.conversation_id.clone(),
            conversation_title: job.conversation_title.clone(),
            agent_type: job.agent_type.clone(),
            created_by: job.created_by.as_str().to_owned(),
            created_at: job.created_at,
            updated_at: job.updated_at,
            agent_config: agent_config_dto,
        },
        state: CronJobStateDto {
            next_run_at_ms: job.next_run_at,
            last_run_at_ms: job.last_run_at,
            last_status: job.last_status.map(|s| s.as_str().to_owned()),
            last_error: job.last_error.clone(),
            run_count: job.run_count,
            retry_count: job.retry_count,
            max_retries: job.max_retries,
        },
    }
}

// ---------------------------------------------------------------------------
// DTO ↔ Domain schedule conversion (for create/update)
// ---------------------------------------------------------------------------

pub fn schedule_from_dto(dto: &CronScheduleDto) -> CronSchedule {
    match dto {
        CronScheduleDto::At { at_ms, description } => CronSchedule::At {
            at_ms: *at_ms,
            description: description.clone(),
        },
        CronScheduleDto::Every {
            every_ms,
            description,
        } => CronSchedule::Every {
            every_ms: *every_ms,
            description: description.clone(),
        },
        CronScheduleDto::Cron {
            expr,
            tz,
            description,
        } => CronSchedule::Cron {
            expr: expr.clone(),
            tz: tz.clone(),
            description: description.clone(),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
    const USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    // -- Enum parsing ---------------------------------------------------------

    #[test]
    fn execution_mode_from_str_valid() {
        assert_eq!(
            ExecutionMode::from_str("existing").unwrap(),
            ExecutionMode::Existing
        );
        assert_eq!(
            ExecutionMode::from_str("new_conversation").unwrap(),
            ExecutionMode::NewConversation
        );
    }

    #[test]
    fn execution_mode_from_str_invalid() {
        let err = ExecutionMode::from_str("other").unwrap_err();
        assert!(matches!(err, CronError::InvalidExecutionMode(_)));
    }

    #[test]
    fn execution_mode_as_str_roundtrip() {
        for mode in [ExecutionMode::Existing, ExecutionMode::NewConversation] {
            assert_eq!(ExecutionMode::from_str(mode.as_str()).unwrap(), mode);
        }
    }

    #[test]
    fn created_by_from_str_valid() {
        assert_eq!(CreatedBy::from_str("user").unwrap(), CreatedBy::User);
        assert_eq!(CreatedBy::from_str("agent").unwrap(), CreatedBy::Agent);
    }

    #[test]
    fn created_by_from_str_invalid() {
        let err = CreatedBy::from_str("robot").unwrap_err();
        assert!(matches!(err, CronError::InvalidCreatedBy(_)));
    }

    #[test]
    fn created_by_as_str_roundtrip() {
        for cb in [CreatedBy::User, CreatedBy::Agent] {
            assert_eq!(CreatedBy::from_str(cb.as_str()).unwrap(), cb);
        }
    }

    #[test]
    fn job_status_from_str_all() {
        assert_eq!(JobStatus::from_str("ok").unwrap(), JobStatus::Ok);
        assert_eq!(JobStatus::from_str("error").unwrap(), JobStatus::Error);
        assert_eq!(JobStatus::from_str("skipped").unwrap(), JobStatus::Skipped);
        assert_eq!(JobStatus::from_str("missed").unwrap(), JobStatus::Missed);
    }

    #[test]
    fn job_status_from_str_invalid() {
        let err = JobStatus::from_str("running").unwrap_err();
        assert!(matches!(err, CronError::InvalidJobStatus(_)));
    }

    #[test]
    fn job_status_as_str_roundtrip() {
        for s in [
            JobStatus::Ok,
            JobStatus::Error,
            JobStatus::Skipped,
            JobStatus::Missed,
        ] {
            assert_eq!(JobStatus::from_str(s.as_str()).unwrap(), s);
        }
    }

    // -- Schedule parsing -----------------------------------------------------

    #[test]
    fn parse_schedule_at() {
        let s = parse_schedule("at", "1700000000000", None, Some("once")).unwrap();
        assert_eq!(
            s,
            CronSchedule::At {
                at_ms: 1700000000000,
                description: Some("once".into()),
            }
        );
    }

    #[test]
    fn parse_schedule_at_invalid_value() {
        let err = parse_schedule("at", "not_a_number", None, None).unwrap_err();
        assert!(matches!(err, CronError::InvalidSchedule(_)));
    }

    #[test]
    fn parse_schedule_every() {
        let s = parse_schedule("every", "60000", None, Some("every minute")).unwrap();
        assert_eq!(
            s,
            CronSchedule::Every {
                every_ms: 60000,
                description: Some("every minute".into()),
            }
        );
    }

    #[test]
    fn parse_schedule_every_invalid_value() {
        let err = parse_schedule("every", "abc", None, None).unwrap_err();
        assert!(matches!(err, CronError::InvalidSchedule(_)));
    }

    #[test]
    fn parse_schedule_cron() {
        let s = parse_schedule(
            "cron",
            "0 0 9 * * *",
            Some("Asia/Shanghai"),
            Some("daily 9am"),
        )
        .unwrap();
        assert_eq!(
            s,
            CronSchedule::Cron {
                expr: "0 0 9 * * *".into(),
                tz: Some("Asia/Shanghai".into()),
                description: Some("daily 9am".into()),
            }
        );
    }

    #[test]
    fn parse_schedule_cron_no_tz() {
        let s = parse_schedule("cron", "*/5 * * * *", None, None).unwrap();
        assert_eq!(
            s,
            CronSchedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
                description: None,
            }
        );
    }

    #[test]
    fn parse_schedule_unknown_kind() {
        let err = parse_schedule("weekly", "7", None, None).unwrap_err();
        assert!(matches!(err, CronError::InvalidSchedule(_)));
    }

    // -- DB ↔ Domain roundtrip ------------------------------------------------

    fn sample_row() -> CronJobRow {
        CronJobRow {
            id: 1,
            cron_job_id: JOB_ID.into(),
            user_id: USER_ID.into(),
            name: "Test Job".into(),
            enabled: true,
            schedule_kind: "every".into(),
            schedule_value: "60000".into(),
            schedule_tz: None,
            schedule_description: Some("every minute".into()),
            payload_message: "do something".into(),
            execution_mode: "existing".into(),
            agent_config: Some(r#"{"backend":"acp","name":"Claude"}"#.into()),
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            conversation_id: Some("0190f5fe-7c00-7a00-8abc-012345678901".into()),
            conversation_title: Some("Test Conv".into()),
            agent_type: "acp".into(),
            created_by: "user".into(),
            skill_content: Some("---\nname: test\n---\nContent".into()),
            description: None,
            created_at: 1000,
            updated_at: 2000,
            next_run_at: Some(3000),
            last_run_at: Some(1500),
            last_status: Some("ok".into()),
            last_error: None,
            run_count: 5,
            retry_count: 0,
            max_retries: 3,
        }
    }

    fn sample_job() -> CronJob {
        CronJob {
            cron_job_id: JOB_ID.into(),
            user_id: USER_ID.into(),
            name: "Test Job".into(),
            enabled: true,
            schedule: CronSchedule::Every {
                every_ms: 60000,
                description: Some("every minute".into()),
            },
            message: "do something".into(),
            execution_mode: ExecutionMode::Existing,
            agent_config: Some(CronAgentConfig {
                backend: Some("acp".into()),
                name: "Claude".into(),
                cli_path: None,
                custom_agent_id: None,
                preset_id: None,
                preset_revision: None,
                preset_snapshot: None,
                mode: None,
                model: None,
                provider_id: None,
                config_options: None,
                workspace: None,
                clear_context_each_run: false,
            }),
            conversation_id: Some("0190f5fe-7c00-7a00-8abc-012345678901".into()),
            conversation_title: Some("Test Conv".into()),
            agent_type: "acp".into(),
            created_by: CreatedBy::User,
            skill_content: Some("---\nname: test\n---\nContent".into()),
            description: None,
            created_at: 1000,
            updated_at: 2000,
            next_run_at: Some(3000),
            last_run_at: Some(1500),
            last_status: Some(JobStatus::Ok),
            last_error: None,
            run_count: 5,
            retry_count: 0,
            max_retries: 3,
        }
    }

    #[test]
    fn row_to_domain_roundtrip() {
        let row = sample_row();
        let job = cron_job_from_row(row).unwrap();
        assert_eq!(job.cron_job_id, JOB_ID);
        assert_eq!(job.name, "Test Job");
        assert!(job.enabled);
        assert_eq!(
            job.schedule,
            CronSchedule::Every {
                every_ms: 60000,
                description: Some("every minute".into()),
            }
        );
        assert_eq!(job.execution_mode, ExecutionMode::Existing);
        assert_eq!(job.created_by, CreatedBy::User);
        assert_eq!(job.last_status, Some(JobStatus::Ok));
        assert!(job.agent_config.is_some());
        assert_eq!(
            job.agent_config.as_ref().unwrap().backend.as_deref(),
            Some("acp")
        );
    }

    #[test]
    fn domain_to_row_roundtrip() {
        let job = sample_job();
        let row = cron_job_to_row(&job).unwrap();
        assert_eq!(row.id, 0);
        assert_eq!(row.cron_job_id, JOB_ID);
        assert_eq!(row.schedule_kind, "every");
        assert_eq!(row.schedule_value, "60000");
        assert_eq!(row.execution_mode, "existing");
        assert_eq!(row.created_by, "user");
        assert_eq!(row.last_status.as_deref(), Some("ok"));
        assert!(row.agent_config.is_some());

        let restored = cron_job_from_row(row).unwrap();
        assert_eq!(restored.cron_job_id, job.cron_job_id);
        assert_eq!(restored.schedule, job.schedule);
        assert_eq!(restored.execution_mode, job.execution_mode);
        assert_eq!(restored.created_by, job.created_by);
        assert_eq!(restored.last_status, job.last_status);
    }

    #[test]
    fn row_to_domain_at_type() {
        let row = CronJobRow {
            schedule_kind: "at".into(),
            schedule_value: "1700000000000".into(),
            schedule_tz: None,
            schedule_description: Some("once".into()),
            ..sample_row()
        };
        let job = cron_job_from_row(row).unwrap();
        assert_eq!(
            job.schedule,
            CronSchedule::At {
                at_ms: 1700000000000,
                description: Some("once".into()),
            }
        );
    }

    #[test]
    fn row_to_domain_cron_type_with_tz() {
        let row = CronJobRow {
            schedule_kind: "cron".into(),
            schedule_value: "0 0 9 * * *".into(),
            schedule_tz: Some("Asia/Shanghai".into()),
            schedule_description: Some("daily 9am".into()),
            ..sample_row()
        };
        let job = cron_job_from_row(row).unwrap();
        assert_eq!(
            job.schedule,
            CronSchedule::Cron {
                expr: "0 0 9 * * *".into(),
                tz: Some("Asia/Shanghai".into()),
                description: Some("daily 9am".into()),
            }
        );
    }

    #[test]
    fn row_to_domain_no_optional_fields() {
        let row = CronJobRow {
            agent_config: None,
            conversation_title: None,
            skill_content: None,
            next_run_at: None,
            last_run_at: None,
            last_status: None,
            last_error: None,
            ..sample_row()
        };
        let job = cron_job_from_row(row).unwrap();
        assert!(job.agent_config.is_none());
        assert!(job.conversation_title.is_none());
        assert!(job.last_status.is_none());
    }

    #[test]
    fn row_to_domain_invalid_execution_mode() {
        let row = CronJobRow {
            execution_mode: "parallel".into(),
            ..sample_row()
        };
        let err = cron_job_from_row(row).unwrap_err();
        assert!(matches!(err, CronError::InvalidExecutionMode(_)));
    }

    #[test]
    fn row_to_domain_invalid_created_by() {
        let row = CronJobRow {
            created_by: "system".into(),
            ..sample_row()
        };
        let err = cron_job_from_row(row).unwrap_err();
        assert!(matches!(err, CronError::InvalidCreatedBy(_)));
    }

    #[test]
    fn row_to_domain_invalid_status() {
        let row = CronJobRow {
            last_status: Some("running".into()),
            ..sample_row()
        };
        let err = cron_job_from_row(row).unwrap_err();
        assert!(matches!(err, CronError::InvalidJobStatus(_)));
    }

    #[test]
    fn row_to_domain_invalid_agent_config_json() {
        let row = CronJobRow {
            agent_config: Some("not json".into()),
            ..sample_row()
        };
        let err = cron_job_from_row(row).unwrap_err();
        assert!(matches!(err, CronError::Json(_)));
    }

    #[test]
    fn nomi_agent_config_roundtrips_with_provider_id_and_model_only() {
        let row = CronJobRow {
            agent_type: "nomi".into(),
            agent_config: Some(
                serde_json::json!({
                    "name": "Nomi",
                    "provider_id": "0190f5fe-7c00-7a00-8000-000000000002",
                    "model": "model-safe"
                })
                .to_string(),
            ),
            ..sample_row()
        };

        let job = cron_job_from_row(row).unwrap();
        let config = job.agent_config.as_ref().unwrap();
        assert_eq!(
            config.provider_id.as_deref(),
            Some("0190f5fe-7c00-7a00-8000-000000000002")
        );
        assert_eq!(config.model.as_deref(), Some("model-safe"));
        assert!(config.backend.is_none());

        let persisted = cron_job_to_row(&job).unwrap().agent_config.unwrap();
        let persisted: serde_json::Value = serde_json::from_str(&persisted).unwrap();
        assert_eq!(
            persisted
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            ["name", "model", "provider_id"].into_iter().collect()
        );
    }

    #[test]
    fn nomi_agent_config_rejects_backend_as_provider() {
        let row = CronJobRow {
            agent_type: "nomi".into(),
            agent_config: Some(
                serde_json::json!({
                    "name": "Nomi",
                    "backend": "openai",
                    "model": "model-safe"
                })
                .to_string(),
            ),
            ..sample_row()
        };

        let error = cron_job_from_row(row).unwrap_err();
        assert!(error.to_string().contains("use provider_id"), "{error}");
    }

    #[test]
    fn row_to_domain_rejects_non_uuidv7_preset_id() {
        for preset_id in [
            "word-creator",
            "7",
            "preset_0190f5fe-7c00-7a00-8abc-012345678901",
            "550e8400-e29b-41d4-a716-446655440000",
            "0190F5FE-7C00-7A00-8ABC-012345678901",
        ] {
            let row = CronJobRow {
                preset_id: Some(preset_id.into()),
                ..sample_row()
            };
            let error = cron_job_from_row(row)
                .expect_err("cron preset_id must be a canonical bare lowercase UUIDv7");
            assert!(
                error.to_string().contains("invalid preset_id"),
                "unexpected error for {preset_id:?}: {error}"
            );
        }
    }

    // -- Domain ↔ DTO ---------------------------------------------------------

    #[test]
    fn domain_to_dto_every() {
        let job = sample_job();
        let resp = cron_job_to_response(&job);
        assert_eq!(resp.cron_job_id, JOB_ID);
        assert_eq!(resp.name, "Test Job");
        assert!(resp.enabled);
        assert!(matches!(
            resp.schedule,
            CronScheduleDto::Every {
                every_ms: 60000,
                ..
            }
        ));
        assert_eq!(resp.message, "do something");
        assert_eq!(resp.execution_mode, "existing");
        assert_eq!(
            resp.metadata.conversation_id.as_deref(),
            Some("0190f5fe-7c00-7a00-8abc-012345678901")
        );
        assert_eq!(resp.metadata.agent_type, "acp");
        assert_eq!(resp.metadata.created_by, "user");
        assert_eq!(resp.state.run_count, 5);
        assert_eq!(resp.state.last_status.as_deref(), Some("ok"));
    }

    #[test]
    fn domain_to_dto_at_type() {
        let job = CronJob {
            schedule: CronSchedule::At {
                at_ms: 1700000000000,
                description: Some("once".into()),
            },
            ..sample_job()
        };
        let resp = cron_job_to_response(&job);
        assert!(matches!(
            resp.schedule,
            CronScheduleDto::At {
                at_ms: 1700000000000,
                ..
            }
        ));
    }

    #[test]
    fn domain_to_dto_cron_type() {
        let job = CronJob {
            schedule: CronSchedule::Cron {
                expr: "0 0 9 * * *".into(),
                tz: Some("UTC".into()),
                description: Some("daily".into()),
            },
            ..sample_job()
        };
        let resp = cron_job_to_response(&job);
        assert!(matches!(resp.schedule, CronScheduleDto::Cron { .. }));
    }

    #[test]
    fn domain_to_dto_no_agent_config() {
        let job = CronJob {
            agent_config: None,
            ..sample_job()
        };
        let resp = cron_job_to_response(&job);
        assert!(resp.metadata.agent_config.is_none());
    }

    #[test]
    fn domain_to_dto_new_conversation_mode() {
        let job = CronJob {
            execution_mode: ExecutionMode::NewConversation,
            ..sample_job()
        };
        let resp = cron_job_to_response(&job);
        assert_eq!(resp.execution_mode, "new_conversation");
    }

    // -- DTO ↔ Domain schedule ------------------------------------------------

    #[test]
    fn schedule_from_dto_at() {
        let dto = CronScheduleDto::At {
            at_ms: 5000,
            description: Some("once".into()),
        };
        let s = schedule_from_dto(&dto);
        assert_eq!(
            s,
            CronSchedule::At {
                at_ms: 5000,
                description: Some("once".into()),
            }
        );
    }

    #[test]
    fn schedule_from_dto_every() {
        let dto = CronScheduleDto::Every {
            every_ms: 30000,
            description: None,
        };
        let s = schedule_from_dto(&dto);
        assert_eq!(
            s,
            CronSchedule::Every {
                every_ms: 30000,
                description: None,
            }
        );
    }

    #[test]
    fn schedule_from_dto_cron() {
        let dto = CronScheduleDto::Cron {
            expr: "0 */5 * * * *".into(),
            tz: Some("UTC".into()),
            description: Some("every 5m".into()),
        };
        let s = schedule_from_dto(&dto);
        assert_eq!(
            s,
            CronSchedule::Cron {
                expr: "0 */5 * * * *".into(),
                tz: Some("UTC".into()),
                description: Some("every 5m".into()),
            }
        );
    }

    #[test]
    fn agent_job_roundtrips_through_storage() {
        let original = sample_job();
        let row = cron_job_to_row(&sample_job()).unwrap();
        let restored = cron_job_from_row(row).unwrap();
        assert_eq!(restored, original);
    }
}
