use agent_client_protocol::{Error as SdkError, ErrorCode};
use nomifun_common::{AgentKillReason, AppError};

/// Why an ACP session was closed / terminated.
///
/// Captured at the close site (cancel / kill / send-message-error) so the
/// next user-facing toast can render something better than "session closed"
/// or "Bad gateway". `summary` is the redacted, user-safe message — stderr
/// MUST be filtered through `stderr_error_extractor::extract_error_message`
/// before reaching this type. Raw stderr is logged via `tracing` only and
/// must never land here.
///
/// Lifecycle:
/// - writer: `AcpSession::record_close_reason`, called by the manager when
///   a close path runs (`send_message` Err, `cancel`, `kill`, post-init
///   process exit detection).
/// - reader: `AcpSession::last_close_reason`, drained by the manager when
///   composing the user-facing error message for the next toast.
/// - invalidation: cleared on `clear_session_id` and on
///   `record_close_reason(None)` so a rebuilt session starts fresh.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // `Killed`/`UserCancel` are wired by the manager close path; kept for completeness across all close sites.
pub enum CloseReason {
    /// User cancelled the in-flight prompt via `cancel()`. Distinct from
    /// `Killed` so the toast can say "cancelled" instead of "killed".
    UserCancel,

    /// Manager invoked `kill()` (idle timeout, conversation deletion, …).
    /// Carries the structured reason so the toast text is actionable.
    Killed { reason: Option<AgentKillReason> },

    /// CLI process exited unexpectedly. Mirrors `AcpError::Disconnected`
    /// but with a redacted summary; stderr stays in tracing logs only.
    ProcessExited {
        exit_code: Option<i32>,
        signal: Option<String>,
        /// User-safe summary derived from `extract_error_message` over the
        /// stderr tail. Empty when the extractor's allowlist did not match.
        redacted_summary: String,
    },

    /// Generic upstream / protocol failure that closed the turn but where
    /// the process is still alive. `display` is the
    /// `user_facing_message`-stripped form of the originating `AppError`,
    /// so it never starts with "Bad gateway: ".
    Failed { display: String },
}

impl CloseReason {
    /// Render a single-line, user-facing summary safe to broadcast over
    /// WebSocket / put into HTTP responses. stderr never leaves the
    /// `redacted_summary` field, which is itself allowlist-filtered.
    pub fn user_facing_message(&self) -> String {
        match self {
            CloseReason::UserCancel => "Conversation cancelled".to_owned(),
            CloseReason::Killed { reason } => match reason {
                Some(AgentKillReason::UserCancelled) => "Conversation cancelled".to_owned(),
                Some(AgentKillReason::IdleTimeout) => "Agent killed: idle timeout".to_owned(),
                Some(AgentKillReason::AgentErrorRecovery) => "Agent killed: error recovery".to_owned(),
                Some(AgentKillReason::KnowledgeBindingChanged) => "Agent killed: knowledge binding changed".to_owned(),
                Some(AgentKillReason::ConversationDeleted) => "Agent killed: conversation deleted".to_owned(),
                None => "Agent killed".to_owned(),
            },
            CloseReason::ProcessExited {
                exit_code,
                signal,
                redacted_summary,
            } => {
                let detail = format_exit_detail(*exit_code, signal.as_deref());
                if redacted_summary.is_empty() {
                    format!("Agent process exited{detail}")
                } else {
                    format!("Agent process exited{detail}: {redacted_summary}")
                }
            }
            CloseReason::Failed { display } => display.clone(),
        }
    }
}

/// ACP-specific error type for protocol and process lifecycle errors.
///
/// This error is internal to the `nomifun-ai-agent` crate. External callers
/// see it only after conversion to [`AppError`] via the `From` impl.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // Variants constructed as error paths mature; kept for complete ACP error model.
pub(crate) enum AcpError {
    // ── Process lifecycle ──────────────────────────────────────────
    /// CLI binary not found or not executable.
    SpawnFailed { message: String },

    /// Process exited before the initialize handshake completed.
    StartupCrash {
        exit_code: Option<i32>,
        signal: Option<String>,
        stderr: String,
    },

    /// Process crashed while a request was in flight.
    Disconnected {
        exit_code: Option<i32>,
        signal: Option<String>,
        stderr: String,
    },

    // ── ACP protocol errors (from SDK ErrorCode) ──────────────────
    /// Agent requires authentication first.
    AuthRequired,

    /// Agent-side session not found.
    SessionNotFound { session_id: String },

    /// Agent does not support the requested method.
    MethodNotFound { method: String },

    /// Invalid request parameters.
    InvalidParams { message: String },

    /// Agent reported an internal error. `data` carries the optional JSON-RPC
    /// `error.data` payload from the agent — see the [`Display`] impl for how
    /// it is rendered.
    ///
    /// [`Display`]: std::fmt::Display
    AgentInternal {
        message: String,
        code: i32,
        data: Option<serde_json::Value>,
    },

    // ── Local errors ──────────────────────────────────────────────
    /// Protocol not connected (used before connect or after disconnect).
    NotConnected,

    /// Initialize handshake timed out.
    InitTimeout { timeout_secs: u64 },
}

/// Format the human-readable suffix for `StartupCrash` / `Disconnected`.
/// stderr is deliberately omitted — see the `From<AcpError> for AppError`
/// security note.
fn format_exit_detail(exit_code: Option<i32>, signal: Option<&str>) -> String {
    match (exit_code, signal) {
        (Some(code), Some(sig)) => format!(" (exit code {code}, {sig})"),
        (Some(code), None) => format!(" (exit code {code})"),
        (None, Some(sig)) => format!(" ({sig})"),
        (None, None) => String::new(),
    }
}

/// JSON-RPC default message strings that carry no useful information.
/// When `AgentInternal` arrives with one of these as its `message`, we fall
/// back to a diagnostic display ("Agent internal error (code -32603)").
///
/// These strings are copied from `ErrorCode`'s `strum::Display` attributes in
/// `agent-client-protocol-schema`. If the SDK changes them, update this list
/// to avoid silently reverting to the diagnostic fallback.
const SDK_DEFAULT_MESSAGES: &[&str] = &[
    "Parse error",
    "Invalid request",
    "Method not found",
    "Invalid params",
    "Internal error",
];

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpError::SpawnFailed { message } => {
                write!(f, "Failed to spawn agent process: {message}")
            }
            AcpError::StartupCrash { exit_code, signal, .. } => {
                // stderr intentionally NOT included — may carry secrets.
                let detail = format_exit_detail(*exit_code, signal.as_deref());
                write!(f, "Agent process exited before initialize handshake completed{detail}")
            }
            AcpError::Disconnected { exit_code, signal, .. } => {
                let detail = format_exit_detail(*exit_code, signal.as_deref());
                write!(f, "Agent process disconnected{detail}")
            }
            AcpError::AuthRequired => f.write_str("Authentication required"),
            AcpError::SessionNotFound { session_id } => {
                write!(f, "Session not found: {session_id}")
            }
            AcpError::MethodNotFound { method } => {
                write!(f, "Method not supported: {method}")
            }
            AcpError::InvalidParams { message } => {
                write!(f, "Invalid parameters: {message}")
            }
            AcpError::AgentInternal { message, code, data } => {
                let trimmed = message.trim();
                let is_default =
                    trimmed.is_empty() || SDK_DEFAULT_MESSAGES.iter().any(|d| d.eq_ignore_ascii_case(trimmed));
                if is_default {
                    write!(f, "Agent internal error (code {code})")?;
                } else {
                    f.write_str(trimmed)?;
                }
                if let Some(data) = data {
                    // serde_json::to_string on a Value cannot actually fail;
                    // the fallback exists only because Display must be infallible.
                    let compact = serde_json::to_string(data).unwrap_or_else(|_| "<unserializable data>".to_owned());
                    write!(f, " ({compact})")?;
                }
                Ok(())
            }
            AcpError::NotConnected => f.write_str("ACP protocol not connected"),
            AcpError::InitTimeout { timeout_secs } => {
                write!(f, "Initialize handshake timed out after {timeout_secs}s")
            }
        }
    }
}

impl AcpError {
    /// Whether the caller may retry the operation.
    #[allow(dead_code)] // Will be used once retry logic is wired into the send path.
    pub(crate) fn is_retryable(&self) -> bool {
        matches!(
            self,
            AcpError::SpawnFailed { .. }
                | AcpError::StartupCrash { .. }
                | AcpError::Disconnected { .. }
                | AcpError::AgentInternal { .. }
                | AcpError::InitTimeout { .. }
        )
    }

    /// Convert an SDK [`Error`](SdkError) into an [`AcpError`].
    ///
    /// Mapping is by [`ErrorCode`], never by message text. The single
    /// exception is `data.error == "Session not found: ..."`: OpenCode
    /// (and likely others) return a stale-session failure as
    /// `code = InvalidParams (-32602)` with the real reason buried in
    /// the `data` field, so we re-classify those into `SessionNotFound`
    /// to keep crash detection / recovery paths uniform across agents.
    /// See ELECTRON-1HQ.
    /// `context` carries the session ID or method name for diagnostics.
    pub fn from_sdk(err: SdkError, context: &str) -> Self {
        match err.code {
            ErrorCode::AuthRequired => AcpError::AuthRequired,
            ErrorCode::ResourceNotFound => AcpError::SessionNotFound {
                session_id: context.to_owned(),
            },
            ErrorCode::MethodNotFound => AcpError::MethodNotFound {
                method: context.to_owned(),
            },
            ErrorCode::InvalidParams => {
                if let Some(sid) = extract_session_not_found(err.data.as_ref()) {
                    AcpError::SessionNotFound { session_id: sid }
                } else {
                    AcpError::InvalidParams { message: err.message }
                }
            }
            ErrorCode::ParseError | ErrorCode::InvalidRequest | ErrorCode::InternalError => {
                if let Some(sid) = extract_session_not_found(err.data.as_ref()) {
                    AcpError::SessionNotFound { session_id: sid }
                } else {
                    AcpError::AgentInternal {
                        message: err.message,
                        code: i32::from(err.code),
                        data: err.data,
                    }
                }
            }
            _ => {
                let code = i32::from(err.code);
                // -32001, -32002: additional session-not-found codes used by some agents
                if code == -32001 || code == -32002 {
                    AcpError::SessionNotFound {
                        session_id: context.to_owned(),
                    }
                } else if let Some(sid) = extract_session_not_found(err.data.as_ref()) {
                    AcpError::SessionNotFound { session_id: sid }
                } else {
                    AcpError::AgentInternal {
                        message: err.message,
                        code,
                        data: err.data,
                    }
                }
            }
        }
    }
}

/// If `data` carries a `{"error": "Session not found: <sid>"}` payload
/// (either as a JSON object or as a JSON-string-of-JSON, which is what
/// OpenCode actually emits — see ELECTRON-1HQ), return the session id.
/// Returns `None` for any other shape so callers can fall through to
/// the default `code`-based mapping.
fn extract_session_not_found(data: Option<&serde_json::Value>) -> Option<String> {
    let value = data?;
    let obj = match value {
        serde_json::Value::Object(_) => value.clone(),
        serde_json::Value::String(s) => serde_json::from_str(s).ok()?,
        _ => return None,
    };
    let msg = obj.get("error")?.as_str()?;
    let prefix = "Session not found: ";
    let sid = msg.strip_prefix(prefix)?.trim();
    if sid.is_empty() { None } else { Some(sid.to_owned()) }
}

/// Conversion from [`AcpError`] to [`AppError`] — the only way `AcpError`
/// leaves this crate.
///
/// **Security:** `StartupCrash` and `Disconnected` contain `stderr` which may
/// hold sensitive data. The `Display` impl only includes
/// `exit_code` and `signal`. `stderr` is available for structured logging
/// (`tracing`) but never serialized into HTTP responses.
impl From<AcpError> for AppError {
    fn from(err: AcpError) -> Self {
        match &err {
            // Process lifecycle → 502 Bad Gateway (upstream failure)
            AcpError::SpawnFailed { .. } | AcpError::StartupCrash { .. } | AcpError::Disconnected { .. } => {
                AppError::BadGateway(err.to_string())
            }

            // Authentication → 401
            AcpError::AuthRequired => AppError::Unauthorized("Agent requires authentication".into()),

            // Session not found → 404
            AcpError::SessionNotFound { .. } => AppError::NotFound(err.to_string()),

            // Method not found → 400
            AcpError::MethodNotFound { .. } => AppError::BadRequest(err.to_string()),

            // Invalid parameters → 400
            AcpError::InvalidParams { .. } => AppError::BadRequest(err.to_string()),

            // Agent internal error → 502 (upstream failure)
            AcpError::AgentInternal { .. } => AppError::BadGateway(err.to_string()),

            // Not connected → 500 (our bug)
            AcpError::NotConnected => AppError::Internal("ACP protocol not connected".into()),

            // Init timeout → 502
            AcpError::InitTimeout { .. } => AppError::BadGateway(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    // ── CloseReason ─────────────────────────────────────────────────────

    #[test]
    fn close_reason_user_cancel_is_user_friendly() {
        let msg = CloseReason::UserCancel.user_facing_message();
        assert_eq!(msg, "Conversation cancelled");
    }

    #[test]
    fn close_reason_killed_renders_each_kill_reason() {
        assert_eq!(
            CloseReason::Killed {
                reason: Some(AgentKillReason::IdleTimeout)
            }
            .user_facing_message(),
            "Agent killed: idle timeout"
        );
        assert_eq!(
            CloseReason::Killed {
                reason: Some(AgentKillReason::ConversationDeleted)
            }
            .user_facing_message(),
            "Agent killed: conversation deleted"
        );
        assert_eq!(
            CloseReason::Killed { reason: None }.user_facing_message(),
            "Agent killed"
        );
    }

    #[test]
    fn close_reason_process_exited_renders_exit_code_and_summary() {
        let msg = CloseReason::ProcessExited {
            exit_code: Some(127),
            signal: None,
            redacted_summary: "usage limit exceeded".into(),
        }
        .user_facing_message();
        assert!(msg.contains("exit code 127"), "got {msg}");
        assert!(msg.contains("usage limit exceeded"), "got {msg}");
    }

    #[test]
    fn close_reason_process_exited_omits_summary_when_empty() {
        // No allowlist match → no trailing colon, no stray noise.
        let msg = CloseReason::ProcessExited {
            exit_code: Some(1),
            signal: None,
            redacted_summary: String::new(),
        }
        .user_facing_message();
        assert!(msg.contains("exit code 1"), "got {msg}");
        assert!(!msg.ends_with(": "), "must not have a dangling colon; got {msg}");
    }

    #[test]
    fn close_reason_process_exited_includes_signal() {
        let msg = CloseReason::ProcessExited {
            exit_code: None,
            signal: Some("signal:9".into()),
            redacted_summary: String::new(),
        }
        .user_facing_message();
        assert!(msg.contains("signal:9"), "got {msg}");
    }

    #[test]
    fn close_reason_user_facing_message_is_safe_to_redisplay() {
        // The helper produced a synthetic stderr containing fake credentials.
        // The allowlist filter is responsible for keeping that out of the
        // `redacted_summary` field — the user_facing_message helper itself
        // must not invent or re-fetch any non-allowlisted content.
        let reason = CloseReason::ProcessExited {
            exit_code: Some(2),
            signal: None,
            redacted_summary: "rate limit exceeded".into(),
        };
        let msg = reason.user_facing_message();
        assert!(!msg.contains("Bearer"), "must not include synthetic secret material");
        assert!(!msg.contains("api_key="), "must not include synthetic secret material");
        assert!(msg.contains("rate limit exceeded"));
    }

    #[test]
    fn close_reason_failed_carries_through_user_facing_text() {
        let reason = CloseReason::Failed {
            display: "API Error: Internal server error".into(),
        };
        assert_eq!(reason.user_facing_message(), "API Error: Internal server error");
    }

    #[test]
    fn retryable_variants() {
        assert!(
            AcpError::SpawnFailed {
                message: "not found".into()
            }
            .is_retryable()
        );
        assert!(
            AcpError::StartupCrash {
                exit_code: Some(1),
                signal: None,
                stderr: String::new(),
            }
            .is_retryable()
        );
        assert!(
            AcpError::Disconnected {
                exit_code: None,
                signal: Some("SIGKILL".into()),
                stderr: String::new(),
            }
            .is_retryable()
        );
        assert!(
            AcpError::AgentInternal {
                message: "oops".into(),
                code: -32603,
                data: None,
            }
            .is_retryable()
        );
        assert!(AcpError::InitTimeout { timeout_secs: 30 }.is_retryable());
    }

    #[test]
    fn non_retryable_variants() {
        assert!(!AcpError::AuthRequired.is_retryable());
        assert!(
            !AcpError::SessionNotFound {
                session_id: "s1".into()
            }
            .is_retryable()
        );
        assert!(!AcpError::MethodNotFound { method: "foo".into() }.is_retryable());
        assert!(!AcpError::InvalidParams { message: "bad".into() }.is_retryable());
        assert!(!AcpError::NotConnected.is_retryable());
    }

    #[test]
    fn from_sdk_auth_required() {
        let sdk_err = SdkError::auth_required();
        let acp = AcpError::from_sdk(sdk_err, "sess-1");
        assert!(matches!(acp, AcpError::AuthRequired));
    }

    #[test]
    fn from_sdk_resource_not_found() {
        let sdk_err = SdkError::resource_not_found(None);
        let acp = AcpError::from_sdk(sdk_err, "sess-42");
        match acp {
            AcpError::SessionNotFound { session_id } => assert_eq!(session_id, "sess-42"),
            other => panic!("Expected SessionNotFound, got {other:?}"),
        }
    }

    #[test]
    fn from_sdk_method_not_found() {
        let sdk_err = SdkError::method_not_found();
        let acp = AcpError::from_sdk(sdk_err, "session/magic");
        match acp {
            AcpError::MethodNotFound { method } => assert_eq!(method, "session/magic"),
            other => panic!("Expected MethodNotFound, got {other:?}"),
        }
    }

    #[test]
    fn from_sdk_invalid_params() {
        let sdk_err = SdkError::invalid_params();
        let acp = AcpError::from_sdk(sdk_err, "ignored");
        assert!(matches!(acp, AcpError::InvalidParams { .. }));
    }

    /// OpenCode reports a stale session as
    /// `code: -32602 InvalidParams` with the real reason wrapped in a
    /// JSON-string `data` payload (see ELECTRON-1HQ wire dump). Re-classify
    /// to `SessionNotFound` so downstream crash detection /
    /// recovery treats it uniformly with agents that return -32600 / -32001.
    #[test]
    fn from_sdk_invalid_params_with_session_not_found_data() {
        let sdk_err = SdkError::invalid_params().data(serde_json::Value::String(
            r#"{"error":"Session not found: ses_21859c95dffefejNiDf1VYXMgU"}"#.to_owned(),
        ));
        let acp = AcpError::from_sdk(sdk_err, "session/set_mode");
        match acp {
            AcpError::SessionNotFound { session_id } => {
                assert_eq!(session_id, "ses_21859c95dffefejNiDf1VYXMgU");
            }
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    /// Object-shaped data should also be recognised — some agents skip the
    /// extra string-encoding round-trip and emit a JSON object directly.
    #[test]
    fn from_sdk_invalid_params_with_object_data_session_not_found() {
        let sdk_err = SdkError::invalid_params().data(serde_json::json!({
            "error": "Session not found: sess-direct"
        }));
        let acp = AcpError::from_sdk(sdk_err, "ctx");
        match acp {
            AcpError::SessionNotFound { session_id } => assert_eq!(session_id, "sess-direct"),
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    /// Internal-error code carrying the same payload should also re-classify;
    /// don't tie the rescue to any single `ErrorCode`.
    #[test]
    fn from_sdk_internal_with_session_not_found_data() {
        let sdk_err = SdkError::internal_error().data(serde_json::json!({
            "error": "Session not found: sess-ie"
        }));
        let acp = AcpError::from_sdk(sdk_err, "ctx");
        match acp {
            AcpError::SessionNotFound { session_id } => assert_eq!(session_id, "sess-ie"),
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    /// Unrelated `data` payloads must not trigger the rescue path —
    /// otherwise we'd silently rewrite `InvalidParams` for genuinely
    /// malformed requests.
    #[test]
    fn from_sdk_invalid_params_with_unrelated_data_stays_invalid_params() {
        let sdk_err = SdkError::invalid_params().data(serde_json::json!({
            "error": "Workspace path must be absolute"
        }));
        let acp = AcpError::from_sdk(sdk_err, "ctx");
        assert!(matches!(acp, AcpError::InvalidParams { .. }));
    }

    #[test]
    fn from_sdk_internal_error() {
        let sdk_err = SdkError::internal_error();
        let acp = AcpError::from_sdk(sdk_err, "context");
        match acp {
            AcpError::AgentInternal { code, .. } => assert_eq!(code, -32603),
            other => panic!("Expected AgentInternal, got {other:?}"),
        }
    }

    #[test]
    fn from_sdk_other_code_session_related() {
        let sdk_err = SdkError::new(-32001, "session expired");
        let acp = AcpError::from_sdk(sdk_err, "sess-old");
        assert!(matches!(acp, AcpError::SessionNotFound { .. }));
    }

    #[test]
    fn from_sdk_other_code_unknown() {
        let sdk_err = SdkError::new(-32099, "custom error");
        let acp = AcpError::from_sdk(sdk_err, "ctx");
        match acp {
            AcpError::AgentInternal { code, message, .. } => {
                assert_eq!(code, -32099);
                assert_eq!(message, "custom error");
            }
            other => panic!("Expected AgentInternal, got {other:?}"),
        }
    }

    #[test]
    fn to_app_error_status_codes() {
        let cases: Vec<(AcpError, StatusCode)> = vec![
            (AcpError::SpawnFailed { message: "x".into() }, StatusCode::BAD_GATEWAY),
            (AcpError::AuthRequired, StatusCode::UNAUTHORIZED),
            (
                AcpError::SessionNotFound { session_id: "s".into() },
                StatusCode::NOT_FOUND,
            ),
            (AcpError::MethodNotFound { method: "m".into() }, StatusCode::BAD_REQUEST),
            (AcpError::InvalidParams { message: "p".into() }, StatusCode::BAD_REQUEST),
            (
                AcpError::AgentInternal {
                    message: "e".into(),
                    code: -1,
                    data: None,
                },
                StatusCode::BAD_GATEWAY,
            ),
            (AcpError::NotConnected, StatusCode::INTERNAL_SERVER_ERROR),
            (AcpError::InitTimeout { timeout_secs: 30 }, StatusCode::BAD_GATEWAY),
        ];

        for (acp_err, expected_status) in cases {
            let app_err: AppError = acp_err.into();
            assert_eq!(app_err.status_code(), expected_status, "Mismatch for {app_err:?}");
        }
    }

    #[test]
    fn display_does_not_contain_stderr() {
        let err = AcpError::StartupCrash {
            exit_code: Some(1),
            signal: None,
            stderr: "SUPER SECRET API KEY abc123".into(),
        };
        let display = err.to_string();
        assert!(
            !display.contains("SUPER SECRET"),
            "Display should not leak stderr: {display}"
        );
    }

    #[test]
    fn startup_crash_display_includes_exit_code() {
        let err = AcpError::StartupCrash {
            exit_code: Some(1),
            signal: None,
            stderr: String::new(),
        };
        let display = err.to_string();
        assert!(display.contains("exit code 1"), "got {display}");
        assert!(
            display.contains("before initialize handshake"),
            "must explain when in lifecycle the crash happened; got {display}"
        );
    }

    #[test]
    fn startup_crash_display_omits_detail_when_unknown() {
        let err = AcpError::StartupCrash {
            exit_code: None,
            signal: None,
            stderr: String::new(),
        };
        let display = err.to_string();
        assert!(!display.contains("None"), "must not surface raw `None`; got {display}");
        assert!(!display.contains("()"), "must not produce empty parens; got {display}");
    }

    #[test]
    fn disconnected_display_includes_signal_when_present() {
        let err = AcpError::Disconnected {
            exit_code: None,
            signal: Some("signal:9".into()),
            stderr: String::new(),
        };
        let display = err.to_string();
        assert!(display.contains("signal:9"), "got {display}");
    }

    #[test]
    fn from_sdk_captures_data_payload() {
        let sdk_err = SdkError::internal_error().data(serde_json::json!({"reason": "rate_limited", "retry_after": 30}));
        let acp = AcpError::from_sdk(sdk_err, "context");
        match acp {
            AcpError::AgentInternal { code, message, data } => {
                assert_eq!(code, -32603);
                assert_eq!(message, "Internal error");
                let data = data.expect("data must be preserved");
                assert_eq!(data["reason"], "rate_limited");
                assert_eq!(data["retry_after"], 30);
            }
            other => panic!("Expected AgentInternal, got {other:?}"),
        }
    }

    #[test]
    fn from_sdk_no_data_yields_none() {
        let sdk_err = SdkError::internal_error();
        let acp = AcpError::from_sdk(sdk_err, "context");
        match acp {
            AcpError::AgentInternal { data, .. } => assert!(data.is_none()),
            other => panic!("Expected AgentInternal, got {other:?}"),
        }
    }

    #[test]
    fn agent_internal_display_uses_message_only_when_no_data() {
        let err = AcpError::AgentInternal {
            message: "API Error: Internal server error".into(),
            code: -32603,
            data: None,
        };
        assert_eq!(
            err.to_string(),
            "API Error: Internal server error",
            "Display must NOT prefix with 'Agent internal error:' when message carries upstream context"
        );
    }

    #[test]
    fn agent_internal_display_falls_back_when_message_is_sdk_default() {
        // SDK default for ErrorCode::InternalError is the plain string "Internal error".
        // When that's all we have, the user sees nothing useful, so add a hint.
        let err = AcpError::AgentInternal {
            message: "Internal error".into(),
            code: -32603,
            data: None,
        };
        let display = err.to_string();
        assert!(
            display.contains("Agent internal error"),
            "Display must include 'Agent internal error' hint when SDK gave us its default message; got {display}"
        );
        assert!(
            display.contains("-32603"),
            "Display must include the JSON-RPC code as a diagnostic when message is empty/default; got {display}"
        );
    }

    #[test]
    fn agent_internal_display_appends_data_when_message_is_sdk_default() {
        // Real-world shape: SDK returned its default `"Internal error"` but
        // attached structured data. Display must use the diagnostic header
        // AND append the data.
        let err = AcpError::AgentInternal {
            message: "Internal error".into(),
            code: -32603,
            data: Some(serde_json::json!({"retry_after": 30})),
        };
        let display = err.to_string();
        assert!(
            display.contains("Agent internal error"),
            "header must use diagnostic fallback when message is the SDK default; got {display}"
        );
        assert!(
            display.contains("-32603"),
            "header must include the code; got {display}"
        );
        assert!(display.contains("retry_after"), "data must be appended; got {display}");
        assert!(display.contains("30"), "data value must be appended; got {display}");
        assert!(!display.contains('\n'), "data must be inline; got {display}");
    }

    #[test]
    fn agent_internal_display_appends_data_inline() {
        let err = AcpError::AgentInternal {
            message: "API Error".into(),
            code: -32603,
            data: Some(serde_json::json!({"upstream_status": 503})),
        };
        let display = err.to_string();
        assert!(display.contains("API Error"), "got {display}");
        assert!(display.contains("upstream_status"), "got {display}");
        assert!(display.contains("503"), "got {display}");
        assert!(
            !display.contains('\n'),
            "data must be appended on a single line, not pretty-printed; got {display}"
        );
    }
}
