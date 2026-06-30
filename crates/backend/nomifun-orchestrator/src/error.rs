use thiserror::Error;

use nomifun_common::AppError;

/// Errors from orchestration services (fleets, workspaces, runs).
///
/// The fleet repository surfaces raw [`sqlx::Error`] (unlike the `DbError`-based
/// repos), so the service layer wraps those here and converts into [`AppError`]
/// via the `From` impl below. Validation failures map to client 4xx; everything
/// else is an opaque 500.
#[derive(Debug, Error)]
pub enum OrchestratorError {
    /// A required entity was not found (→ 404).
    #[error("not found: {0}")]
    NotFound(String),

    /// The request was malformed or failed validation (→ 400).
    #[error("bad request: {0}")]
    BadRequest(String),

    /// An underlying database query failed (→ 500).
    ///
    /// The fleet repository surfaces raw `sqlx::Error` (re-exported from
    /// `nomifun_db`), so the orchestrator crate does not declare its own sqlx
    /// dependency for the library build.
    #[error("database error: {0}")]
    Database(#[from] nomifun_db::sqlx::Error),
}

impl From<OrchestratorError> for AppError {
    fn from(err: OrchestratorError) -> Self {
        match err {
            OrchestratorError::NotFound(msg) => AppError::NotFound(msg),
            OrchestratorError::BadRequest(msg) => AppError::BadRequest(msg),
            OrchestratorError::Database(e) => AppError::Internal(format!("orchestrator database error: {e}")),
        }
    }
}
