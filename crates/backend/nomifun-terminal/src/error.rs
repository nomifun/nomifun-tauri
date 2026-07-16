use nomifun_common::AppError;

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("Terminal session not found: {0}")]
    NotFound(String),

    #[error("Failed to spawn terminal: {0}")]
    Spawn(String),

    #[error("Invalid terminal input: {0}")]
    InvalidInput(String),

    #[error("Terminal service is shutting down")]
    ShuttingDown,

    #[error("Terminal I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Database(#[from] nomifun_db::DbError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<TerminalError> for AppError {
    fn from(err: TerminalError) -> Self {
        match err {
            TerminalError::NotFound(msg) => AppError::NotFound(msg),
            TerminalError::InvalidInput(msg) => AppError::BadRequest(msg),
            TerminalError::ShuttingDown => {
                AppError::Conflict("terminal service is shutting down".to_owned())
            }
            TerminalError::Spawn(msg) => AppError::Internal(msg),
            TerminalError::Io(e) => AppError::Internal(format!("terminal io: {e}")),
            TerminalError::Database(db_err) => AppError::from(db_err),
            TerminalError::Json(e) => AppError::Internal(format!("JSON error: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_maps_to_not_found() {
        let app: AppError = TerminalError::NotFound(
            nomifun_common::TerminalId::new().into_string(),
        )
        .into();
        assert!(matches!(app, AppError::NotFound(_)));
    }

    #[test]
    fn invalid_input_maps_to_bad_request() {
        let app: AppError = TerminalError::InvalidInput("bad base64".into()).into();
        assert!(matches!(app, AppError::BadRequest(_)));
    }

    #[test]
    fn spawn_maps_to_internal() {
        let app: AppError = TerminalError::Spawn("nope".into()).into();
        assert!(matches!(app, AppError::Internal(_)));
    }

    #[test]
    fn shutting_down_maps_to_conflict() {
        let app: AppError = TerminalError::ShuttingDown.into();
        assert!(matches!(app, AppError::Conflict(_)));
    }
}
