use nomifun_common::AppError;

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("file not found: {0}")]
    FileNotFound(String),

    #[error("directory not found: {0}")]
    DirectoryNotFound(String),

    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("invalid target: {0}")]
    InvalidTarget(String),

    #[error("tool not installed: {0}")]
    ToolNotInstalled(String),

    #[error("command failed: {0}")]
    CommandFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<ShellError> for AppError {
    fn from(err: ShellError) -> Self {
        match err {
            ShellError::FileNotFound(path) => AppError::BadRequest(format!("file not found: {path}")),
            ShellError::DirectoryNotFound(path) => AppError::BadRequest(format!("directory not found: {path}")),
            ShellError::InvalidUrl(msg) => AppError::BadRequest(format!("invalid URL: {msg}")),
            ShellError::InvalidTarget(msg) => AppError::BadRequest(format!("invalid target: {msg}")),
            ShellError::ToolNotInstalled(tool) => AppError::BadRequest(format!("tool not installed: {tool}")),
            ShellError::CommandFailed(msg) => AppError::Internal(format!("command failed: {msg}")),
            ShellError::Io(e) => AppError::Internal(format!("IO error: {e}")),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("STT is not enabled")]
    Disabled,

    #[error("OpenAI STT is not configured: missing API key")]
    OpenaiNotConfigured,

    #[error("Deepgram STT is not configured: missing API key")]
    DeepgramNotConfigured,

    #[error("STT request failed: {0}")]
    RequestFailed(String),

    #[error("STT unknown error: {0}")]
    Unknown(String),
}

impl SttError {
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::Disabled => "STT_DISABLED",
            Self::OpenaiNotConfigured => "STT_OPENAI_NOT_CONFIGURED",
            Self::DeepgramNotConfigured => "STT_DEEPGRAM_NOT_CONFIGURED",
            Self::RequestFailed(_) => "STT_REQUEST_FAILED",
            Self::Unknown(_) => "STT_UNKNOWN",
        }
    }

    pub fn status_code(&self) -> u16 {
        match self {
            Self::Disabled
            | Self::OpenaiNotConfigured
            | Self::DeepgramNotConfigured => 400,
            Self::RequestFailed(_) => 502,
            Self::Unknown(_) => 500,
        }
    }
}

impl From<SttError> for AppError {
    fn from(err: SttError) -> Self {
        match &err {
            SttError::Disabled
            | SttError::OpenaiNotConfigured
            | SttError::DeepgramNotConfigured => {
                AppError::BadRequest(err.to_string())
            }
            SttError::RequestFailed(_) => AppError::BadGateway(err.to_string()),
            SttError::Unknown(_) => AppError::Internal(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_not_found_maps_to_bad_request() {
        let err: AppError = ShellError::FileNotFound("/tmp/missing.txt".into()).into();
        assert!(matches!(err, AppError::BadRequest(msg) if msg.contains("/tmp/missing.txt")));
    }

    #[test]
    fn directory_not_found_maps_to_bad_request() {
        let err: AppError = ShellError::DirectoryNotFound("/tmp/nodir".into()).into();
        assert!(matches!(err, AppError::BadRequest(msg) if msg.contains("/tmp/nodir")));
    }

    #[test]
    fn invalid_url_maps_to_bad_request() {
        let err: AppError = ShellError::InvalidUrl("not a url".into()).into();
        assert!(matches!(err, AppError::BadRequest(msg) if msg.contains("not a url")));
    }

    #[test]
    fn tool_not_installed_maps_to_bad_request() {
        let err: AppError = ShellError::ToolNotInstalled("vscode".into()).into();
        assert!(matches!(err, AppError::BadRequest(msg) if msg.contains("vscode")));
    }

    #[test]
    fn command_failed_maps_to_internal() {
        let err: AppError = ShellError::CommandFailed("exit code 1".into()).into();
        assert!(matches!(err, AppError::Internal(msg) if msg.contains("exit code 1")));
    }

    #[test]
    fn io_error_maps_to_internal() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");
        let err: AppError = ShellError::Io(io_err).into();
        assert!(matches!(err, AppError::Internal(msg) if msg.contains("permission denied")));
    }

    #[test]
    fn shell_error_display_messages() {
        assert_eq!(
            ShellError::FileNotFound("/a.txt".into()).to_string(),
            "file not found: /a.txt"
        );
        assert_eq!(
            ShellError::DirectoryNotFound("/dir".into()).to_string(),
            "directory not found: /dir"
        );
        assert_eq!(ShellError::InvalidUrl("bad".into()).to_string(), "invalid URL: bad");
        assert_eq!(
            ShellError::ToolNotInstalled("code".into()).to_string(),
            "tool not installed: code"
        );
        assert_eq!(
            ShellError::CommandFailed("oops".into()).to_string(),
            "command failed: oops"
        );
    }

    #[test]
    fn stt_disabled_maps_to_bad_request() {
        let err: AppError = SttError::Disabled.into();
        assert!(matches!(err, AppError::BadRequest(msg) if msg.contains("not enabled")));
    }

    #[test]
    fn stt_openai_not_configured_maps_to_bad_request() {
        let err: AppError = SttError::OpenaiNotConfigured.into();
        assert!(matches!(err, AppError::BadRequest(msg) if msg.contains("OpenAI")));
    }

    #[test]
    fn stt_deepgram_not_configured_maps_to_bad_request() {
        let err: AppError = SttError::DeepgramNotConfigured.into();
        assert!(matches!(err, AppError::BadRequest(msg) if msg.contains("Deepgram")));
    }

    #[test]
    fn stt_request_failed_maps_to_bad_gateway() {
        let err: AppError = SttError::RequestFailed("HTTP 401".into()).into();
        assert!(matches!(err, AppError::BadGateway(msg) if msg.contains("HTTP 401")));
    }

    #[test]
    fn stt_unknown_maps_to_internal() {
        let err: AppError = SttError::Unknown("unexpected".into()).into();
        assert!(matches!(err, AppError::Internal(msg) if msg.contains("unexpected")));
    }

    #[test]
    fn stt_error_codes() {
        assert_eq!(SttError::Disabled.error_code(), "STT_DISABLED");
        assert_eq!(SttError::OpenaiNotConfigured.error_code(), "STT_OPENAI_NOT_CONFIGURED");
        assert_eq!(
            SttError::DeepgramNotConfigured.error_code(),
            "STT_DEEPGRAM_NOT_CONFIGURED"
        );
        assert_eq!(SttError::RequestFailed("x".into()).error_code(), "STT_REQUEST_FAILED");
        assert_eq!(SttError::Unknown("x".into()).error_code(), "STT_UNKNOWN");
    }

    #[test]
    fn stt_status_codes() {
        assert_eq!(SttError::Disabled.status_code(), 400);
        assert_eq!(SttError::OpenaiNotConfigured.status_code(), 400);
        assert_eq!(SttError::DeepgramNotConfigured.status_code(), 400);
        assert_eq!(SttError::RequestFailed("x".into()).status_code(), 502);
        assert_eq!(SttError::Unknown("x".into()).status_code(), 500);
    }

    #[test]
    fn stt_error_display_messages() {
        assert_eq!(SttError::Disabled.to_string(), "STT is not enabled");
        assert_eq!(
            SttError::OpenaiNotConfigured.to_string(),
            "OpenAI STT is not configured: missing API key"
        );
        assert_eq!(
            SttError::DeepgramNotConfigured.to_string(),
            "Deepgram STT is not configured: missing API key"
        );
        assert_eq!(
            SttError::RequestFailed("timeout".into()).to_string(),
            "STT request failed: timeout"
        );
        assert_eq!(SttError::Unknown("oops".into()).to_string(), "STT unknown error: oops");
    }
}
