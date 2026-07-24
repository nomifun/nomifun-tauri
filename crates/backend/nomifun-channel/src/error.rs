use nomifun_common::AppError;

/// Current owner of a bot identity, carried by [`ChannelError::BotAlreadyBound`]
/// so the HTTP layer can resolve a friendly owner NAME (it holds the roster;
/// the manager does not). `Display` is the id-based fallback used everywhere the
/// name can't be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelOwner {
    Companion(String),
    PublicAgent(String),
    Channel(i64),
}

impl std::fmt::Display for ChannelOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelOwner::Companion(id) => write!(f, "companion '{id}'"),
            ChannelOwner::PublicAgent(id) => write!(f, "public agent '{id}'"),
            ChannelOwner::Channel(id) => write!(f, "channel '{id}'"),
        }
    }
}

/// Channel crate-level errors.
///
/// Uses `thiserror` (library crate convention).
/// Converts to `AppError` for HTTP response mapping.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("Plugin not found: {0}")]
    PluginNotFound(String),

    #[error("Invalid plugin type: {0}")]
    InvalidPluginType(String),

    #[error("Plugin already running: {0}")]
    PluginAlreadyRunning(String),

    #[error("This bot is already bound to {0}")]
    BotAlreadyBound(ChannelOwner),

    #[error("Invalid plugin configuration: {0}")]
    InvalidConfig(String),

    #[error("Plugin connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Pairing code not found: {0}")]
    PairingNotFound(String),

    #[error("Pairing code expired: {0}")]
    PairingExpired(String),

    #[error("Pairing code already processed: {0}")]
    PairingAlreadyProcessed(String),

    #[error("User not found: {0}")]
    UserNotFound(String),

    #[error("User not authorized: {0}")]
    UserNotAuthorized(String),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Credential encryption failed: {0}")]
    EncryptionFailed(String),

    #[error("Credential decryption failed: {0}")]
    DecryptionFailed(String),

    #[error("Platform API error: {0}")]
    PlatformApi(String),

    #[error("Message send failed: {0}")]
    MessageSendFailed(String),

    /// The bound conversation is already running a turn. For companion sessions
    /// (now shared by the desktop bubble, chat tab, and every IM chat) a
    /// concurrent turn surfaces as a turn-claim `Conflict`; the message loop
    /// answers the user with the friendly "still processing" notice rather than
    /// a raw error.
    #[error("conversation busy")]
    ConversationBusy,

    /// A companion is bound to this channel but has no chat model configured, so
    /// its single session can't be created. The message loop relays this as a
    /// plain notice instead of the generic ❌ failure line.
    #[error("{0}")]
    CompanionNotReady(String),

    #[error("{0}")]
    Database(#[from] nomifun_db::DbError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<ChannelError> for AppError {
    fn from(err: ChannelError) -> Self {
        match err {
            ChannelError::PluginNotFound(msg) => AppError::NotFound(msg),
            ChannelError::InvalidPluginType(msg) => AppError::BadRequest(msg),
            ChannelError::PluginAlreadyRunning(msg) => AppError::Conflict(msg),
            err @ ChannelError::BotAlreadyBound(_) => AppError::Conflict(err.to_string()),
            ChannelError::InvalidConfig(msg) => AppError::BadRequest(msg),
            ChannelError::ConnectionFailed(msg) => AppError::BadGateway(msg),
            ChannelError::PairingNotFound(msg) => AppError::NotFound(msg),
            ChannelError::PairingExpired(msg) => AppError::BadRequest(msg),
            ChannelError::PairingAlreadyProcessed(msg) => AppError::BadRequest(msg),
            ChannelError::UserNotFound(msg) => AppError::NotFound(msg),
            ChannelError::UserNotAuthorized(msg) => AppError::Forbidden(msg),
            ChannelError::SessionNotFound(msg) => AppError::NotFound(msg),
            ChannelError::EncryptionFailed(msg) => AppError::Internal(msg),
            ChannelError::DecryptionFailed(msg) => AppError::Internal(msg),
            ChannelError::PlatformApi(msg) => AppError::BadGateway(msg),
            ChannelError::MessageSendFailed(msg) => AppError::Internal(msg),
            ChannelError::ConversationBusy => AppError::Conflict("conversation busy".into()),
            ChannelError::CompanionNotReady(msg) => AppError::BadRequest(msg),
            ChannelError::Database(db_err) => AppError::from(db_err),
            ChannelError::Json(e) => AppError::Internal(format!("JSON error: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_not_found_maps_to_app_not_found() {
        let err: AppError = ChannelError::PluginNotFound("telegram".into()).into();
        assert!(matches!(err, AppError::NotFound(msg) if msg == "telegram"));
    }

    #[test]
    fn invalid_plugin_type_maps_to_bad_request() {
        let err: AppError = ChannelError::InvalidPluginType("unknown".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn plugin_already_running_maps_to_conflict() {
        let err: AppError = ChannelError::PluginAlreadyRunning("telegram".into()).into();
        assert!(matches!(err, AppError::Conflict(_)));
    }

    #[test]
    fn invalid_config_maps_to_bad_request() {
        let err: AppError = ChannelError::InvalidConfig("missing token".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn connection_failed_maps_to_bad_gateway() {
        let err: AppError = ChannelError::ConnectionFailed("timeout".into()).into();
        assert!(matches!(err, AppError::BadGateway(_)));
    }

    #[test]
    fn pairing_not_found_maps_to_not_found() {
        let err: AppError = ChannelError::PairingNotFound("123456".into()).into();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[test]
    fn pairing_expired_maps_to_bad_request() {
        let err: AppError = ChannelError::PairingExpired("123456".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn pairing_already_processed_maps_to_bad_request() {
        let err: AppError = ChannelError::PairingAlreadyProcessed("123456".into()).into();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn user_not_found_maps_to_not_found() {
        let err: AppError = ChannelError::UserNotFound("user-1".into()).into();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[test]
    fn user_not_authorized_maps_to_forbidden() {
        let err: AppError = ChannelError::UserNotAuthorized("tg_42".into()).into();
        assert!(matches!(err, AppError::Forbidden(_)));
    }

    #[test]
    fn session_not_found_maps_to_not_found() {
        let err: AppError = ChannelError::SessionNotFound("sess-1".into()).into();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[test]
    fn encryption_failed_maps_to_internal() {
        let err: AppError = ChannelError::EncryptionFailed("bad key".into()).into();
        assert!(matches!(err, AppError::Internal(_)));
    }

    #[test]
    fn decryption_failed_maps_to_internal() {
        let err: AppError = ChannelError::DecryptionFailed("corrupt".into()).into();
        assert!(matches!(err, AppError::Internal(_)));
    }

    #[test]
    fn platform_api_maps_to_bad_gateway() {
        let err: AppError = ChannelError::PlatformApi("429 rate limited".into()).into();
        assert!(matches!(err, AppError::BadGateway(_)));
    }

    #[test]
    fn message_send_failed_maps_to_internal() {
        let err: AppError = ChannelError::MessageSendFailed("chat not found".into()).into();
        assert!(matches!(err, AppError::Internal(_)));
    }

    #[test]
    fn json_error_maps_to_internal() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let err: AppError = ChannelError::Json(json_err).into();
        assert!(matches!(err, AppError::Internal(_)));
    }

    #[test]
    fn display_messages() {
        assert_eq!(
            ChannelError::PluginNotFound("tg".into()).to_string(),
            "Plugin not found: tg"
        );
        assert_eq!(
            ChannelError::PairingExpired("123456".into()).to_string(),
            "Pairing code expired: 123456"
        );
        assert_eq!(
            ChannelError::InvalidConfig("bad".into()).to_string(),
            "Invalid plugin configuration: bad"
        );
    }
}
