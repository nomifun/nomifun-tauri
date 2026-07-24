use nomifun_common::AppError;

#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("Cron job not found: {0}")]
    JobNotFound(String),

    #[error("Invalid schedule: {0}")]
    InvalidSchedule(String),

    #[error("Invalid cron expression: {0}")]
    InvalidCronExpression(String),

    #[error("Invalid execution mode: {0}")]
    InvalidExecutionMode(String),

    #[error("Invalid created-by value: {0}")]
    InvalidCreatedBy(String),

    #[error("Invalid job status: {0}")]
    InvalidJobStatus(String),

    #[error("Invalid timezone: {0}")]
    InvalidTimezone(String),

    #[error("Invalid skill content: {0}")]
    InvalidSkillContent(String),

    #[error("Invalid agent config: {0}")]
    InvalidAgentConfig(String),

    #[error("Scheduler error: {0}")]
    Scheduler(String),

    #[error(transparent)]
    App(#[from] AppError),

    #[error("{0}")]
    Database(#[from] nomifun_db::DbError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<CronError> for AppError {
    fn from(err: CronError) -> Self {
        match err {
            CronError::JobNotFound(msg) => AppError::NotFound(msg),
            CronError::InvalidSchedule(msg) => AppError::BadRequest(msg),
            CronError::InvalidCronExpression(msg) => AppError::BadRequest(msg),
            CronError::InvalidExecutionMode(msg) => AppError::BadRequest(msg),
            CronError::InvalidCreatedBy(msg) => AppError::BadRequest(msg),
            CronError::InvalidJobStatus(msg) => AppError::BadRequest(msg),
            CronError::InvalidTimezone(msg) => AppError::BadRequest(msg),
            CronError::InvalidSkillContent(msg) => AppError::BadRequest(msg),
            CronError::InvalidAgentConfig(msg) => AppError::BadRequest(msg),
            CronError::Scheduler(msg) => AppError::Internal(msg),
            CronError::App(app_err) => app_err,
            CronError::Database(db_err) => AppError::from(db_err),
            CronError::Json(e) => AppError::Internal(format!("JSON error: {e}")),
        }
    }
}

impl CronError {
    pub(crate) fn from_conversation_create(error: AppError) -> Self {
        match error {
            AppError::WorkspacePathEdgeWhitespace(_) => Self::App(error),
            AppError::WorkspacePathEdgeWhitespaceRuntimeUnsupported(_) => Self::App(error),
            other => Self::Scheduler(format!("create conversation: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_not_found_maps_to_not_found() {
        let job_id = "42".to_owned();
        let err: AppError = CronError::JobNotFound(job_id.clone()).into();
        assert!(matches!(err, AppError::NotFound(msg) if msg == job_id));
    }

    #[test]
    fn invalid_schedule_maps_to_bad_request() {
        let err: AppError = CronError::InvalidSchedule("missing kind".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_cron_expression_maps_to_bad_request() {
        let err: AppError = CronError::InvalidCronExpression("bad expr".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_execution_mode_maps_to_bad_request() {
        let err: AppError = CronError::InvalidExecutionMode("unknown".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_created_by_maps_to_bad_request() {
        let err: AppError = CronError::InvalidCreatedBy("robot".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_job_status_maps_to_bad_request() {
        let err: AppError = CronError::InvalidJobStatus("unknown".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_timezone_maps_to_bad_request() {
        let err: AppError = CronError::InvalidTimezone("Mars/Olympus".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_skill_content_maps_to_bad_request() {
        let err: AppError = CronError::InvalidSkillContent("empty".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn invalid_agent_config_maps_to_bad_request() {
        let err: AppError = CronError::InvalidAgentConfig("missing backend".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn scheduler_error_maps_to_internal() {
        let err: AppError = CronError::Scheduler("timer failed".into()).into();
        assert!(matches!(err, AppError::Internal(_)));
    }

    #[test]
    fn app_error_passthrough_preserves_code() {
        let err: AppError = CronError::App(AppError::WorkspacePathEdgeWhitespace("/tmp/a b".into())).into();
        assert!(matches!(err, AppError::WorkspacePathEdgeWhitespace(msg) if msg == "/tmp/a b"));
    }

    #[test]
    fn runtime_workspace_app_error_passthrough_preserves_code() {
        let err: AppError = CronError::App(AppError::WorkspacePathEdgeWhitespaceRuntimeUnsupported(
            "/tmp/a b".into(),
        ))
        .into();
        assert!(matches!(
            err,
            AppError::WorkspacePathEdgeWhitespaceRuntimeUnsupported(msg) if msg == "/tmp/a b"
        ));
    }

    #[test]
    fn json_error_maps_to_internal() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let err: AppError = CronError::Json(json_err).into();
        assert!(matches!(err, AppError::Internal(_)));
    }

    #[test]
    fn display_messages() {
        assert_eq!(
            CronError::JobNotFound("42".into())
                .to_string(),
            "Cron job not found: 42"
        );
        assert_eq!(
            CronError::InvalidSchedule("bad".into()).to_string(),
            "Invalid schedule: bad"
        );
        assert_eq!(
            CronError::InvalidCronExpression("* *".into()).to_string(),
            "Invalid cron expression: * *"
        );
    }
}
