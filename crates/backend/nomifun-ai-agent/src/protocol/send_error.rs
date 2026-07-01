use nomifun_api_types::{
    AgentErrorCode, AgentErrorOwnership, AgentErrorResolution, AgentErrorResolutionKind, AgentErrorResolutionTarget,
    AgentStreamErrorData,
};
use nomifun_common::AppError;

use super::error::AcpError;

const MAX_DETAIL_CHARS: usize = 1000;

#[derive(Debug, Clone)]
pub struct AgentSendError {
    stream_error: AgentStreamErrorData,
}

#[derive(Debug, Clone, Copy)]
struct ClassifiedError {
    message: &'static str,
    code: AgentErrorCode,
    ownership: AgentErrorOwnership,
    retryable: bool,
    feedback_recommended: bool,
    resolution_kind: AgentErrorResolutionKind,
    resolution_target: Option<AgentErrorResolutionTarget>,
}

impl ClassifiedError {
    fn into_send_error(self, detail: String) -> AgentSendError {
        AgentSendError::new(
            self.message,
            self.code,
            self.ownership,
            Some(detail),
            self.retryable,
            self.feedback_recommended,
            resolution(self.resolution_kind, self.resolution_target),
        )
    }
}

impl AgentSendError {
    pub fn new(
        message: impl Into<String>,
        code: AgentErrorCode,
        ownership: AgentErrorOwnership,
        detail: Option<String>,
        retryable: bool,
        feedback_recommended: bool,
        resolution: Option<AgentErrorResolution>,
    ) -> Self {
        Self {
            stream_error: AgentStreamErrorData::classified(
                message,
                code,
                ownership,
                detail.map(|d| sanitize_error_detail(&d)),
                retryable,
                feedback_recommended,
                resolution,
            ),
        }
    }

    pub fn from_app_error(err: AppError) -> Self {
        Self::from_app_error_ref(&err)
    }

    pub fn from_app_error_ref(err: &AppError) -> Self {
        let detail = strip_error_prefix(&err.to_string());
        match err {
            AppError::WorkspacePathEdgeWhitespaceRuntimeUnsupported(path) => Self {
                stream_error: AgentStreamErrorData {
                    message: "This workspace path is no longer supported for execution".into(),
                    code: Some(AgentErrorCode::WorkspacePathEdgeWhitespaceRuntimeUnsupported),
                    ownership: Some(AgentErrorOwnership::Nomifun),
                    detail: Some(sanitize_error_detail(&detail)),
                    workspace_path: Some(path.clone()),
                    retryable: Some(false),
                    feedback_recommended: Some(false),
                    resolution: Some(AgentErrorResolution::new(
                        AgentErrorResolutionKind::StartNewSession,
                        Some(AgentErrorResolutionTarget::NewConversation),
                    )),
                },
            },
            AppError::Internal(_) => Self::new(
                "Nomi failed while sending the message",
                AgentErrorCode::NomifunInternalError,
                AgentErrorOwnership::Nomifun,
                Some(detail),
                true,
                true,
                resolution(
                    AgentErrorResolutionKind::SendFeedback,
                    Some(AgentErrorResolutionTarget::Feedback),
                ),
            ),
            AppError::Forbidden(_) => Self::new(
                "Nomi blocked the request before it reached the Agent",
                AgentErrorCode::NomifunPermissionError,
                AgentErrorOwnership::Nomifun,
                Some(detail),
                false,
                true,
                resolution(
                    AgentErrorResolutionKind::SendFeedback,
                    Some(AgentErrorResolutionTarget::Feedback),
                ),
            ),
            AppError::Unauthorized(_) => Self::new(
                "The selected Agent requires authentication",
                AgentErrorCode::UserAgentAuthRequired,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                resolution(
                    AgentErrorResolutionKind::CheckAgentLogin,
                    Some(AgentErrorResolutionTarget::AgentSettings),
                ),
            ),
            AppError::NotFound(msg) if msg.starts_with("Session not found") => Self::new(
                "The Agent session was not found",
                AgentErrorCode::UserAgentSessionNotFound,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                true,
                false,
                resolution(
                    AgentErrorResolutionKind::StartNewSession,
                    Some(AgentErrorResolutionTarget::NewConversation),
                ),
            ),
            AppError::BadRequest(msg) if msg.contains("Method not supported") => Self::new(
                "The selected Agent does not support this operation",
                AgentErrorCode::UserAgentUnsupportedMethod,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                resolution(
                    AgentErrorResolutionKind::CheckAgentVersion,
                    Some(AgentErrorResolutionTarget::AgentSettings),
                ),
            ),
            AppError::BadRequest(msg) if msg.contains("Invalid parameters") => Self::new(
                "The selected Agent rejected the request parameters",
                AgentErrorCode::UserAgentInvalidParams,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                true,
                resolution(
                    AgentErrorResolutionKind::SendFeedback,
                    Some(AgentErrorResolutionTarget::Feedback),
                ),
            ),
            AppError::Timeout(_) => Self::new(
                "The model provider did not respond in time",
                AgentErrorCode::UserLlmProviderTimeout,
                AgentErrorOwnership::UserLlmProvider,
                Some(detail),
                true,
                false,
                resolution(AgentErrorResolutionKind::Retry, None),
            ),
            AppError::RateLimited => Self::new(
                "The model provider rate limited the request",
                AgentErrorCode::UserLlmProviderRateLimited,
                AgentErrorOwnership::UserLlmProvider,
                Some(detail),
                true,
                false,
                resolution(AgentErrorResolutionKind::Retry, None),
            ),
            AppError::BadGateway(_) => classify_upstream_detail(&detail),
            _ => Self::new(
                "The upstream Agent failed while handling the request",
                AgentErrorCode::UnknownUpstreamError,
                AgentErrorOwnership::UnknownUpstream,
                Some(detail),
                true,
                true,
                resolution(
                    AgentErrorResolutionKind::SendFeedback,
                    Some(AgentErrorResolutionTarget::Feedback),
                ),
            ),
        }
    }

    pub fn stream_error(&self) -> &AgentStreamErrorData {
        &self.stream_error
    }

    pub fn into_stream_error(self) -> AgentStreamErrorData {
        self.stream_error
    }

    pub fn code(&self) -> Option<AgentErrorCode> {
        self.stream_error.code
    }

    pub fn ownership(&self) -> Option<AgentErrorOwnership> {
        self.stream_error.ownership
    }
}

impl std::fmt::Display for AgentSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.stream_error.message)
    }
}

impl std::error::Error for AgentSendError {}

impl From<AppError> for AgentSendError {
    fn from(err: AppError) -> Self {
        Self::from_app_error(err)
    }
}

impl From<AcpError> for AgentSendError {
    fn from(err: AcpError) -> Self {
        let detail = err.to_string();
        match &err {
            AcpError::SpawnFailed { .. } => Self::new(
                "The selected Agent executable could not be started",
                AgentErrorCode::UserAgentNotInstalled,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                resolution(
                    AgentErrorResolutionKind::CheckAgentInstallation,
                    Some(AgentErrorResolutionTarget::AgentSettings),
                ),
            ),
            AcpError::StartupCrash { .. } | AcpError::InitTimeout { .. } => Self::new(
                "The selected Agent failed to start",
                AgentErrorCode::UserAgentStartupFailed,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                true,
                false,
                resolution(
                    AgentErrorResolutionKind::CheckAgentInstallation,
                    Some(AgentErrorResolutionTarget::AgentSettings),
                ),
            ),
            AcpError::Disconnected { .. } => Self::new(
                "The selected Agent disconnected",
                AgentErrorCode::UserAgentDisconnected,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                true,
                false,
                resolution(
                    AgentErrorResolutionKind::ReconnectAgent,
                    Some(AgentErrorResolutionTarget::AgentSettings),
                ),
            ),
            AcpError::AuthRequired => Self::new(
                "The selected Agent requires authentication",
                AgentErrorCode::UserAgentAuthRequired,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                resolution(
                    AgentErrorResolutionKind::CheckAgentLogin,
                    Some(AgentErrorResolutionTarget::AgentSettings),
                ),
            ),
            AcpError::SessionNotFound { .. } => Self::new(
                "The Agent session was not found",
                AgentErrorCode::UserAgentSessionNotFound,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                true,
                false,
                resolution(
                    AgentErrorResolutionKind::StartNewSession,
                    Some(AgentErrorResolutionTarget::NewConversation),
                ),
            ),
            AcpError::MethodNotFound { .. } => Self::new(
                "The selected Agent does not support this operation",
                AgentErrorCode::UserAgentUnsupportedMethod,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                resolution(
                    AgentErrorResolutionKind::CheckAgentVersion,
                    Some(AgentErrorResolutionTarget::AgentSettings),
                ),
            ),
            AcpError::InvalidParams { .. } => Self::new(
                "The selected Agent rejected the request parameters",
                AgentErrorCode::UserAgentInvalidParams,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                true,
                resolution(
                    AgentErrorResolutionKind::SendFeedback,
                    Some(AgentErrorResolutionTarget::Feedback),
                ),
            ),
            AcpError::NotConnected => Self::new(
                "Nomi lost its Agent protocol connection",
                AgentErrorCode::NomifunInternalError,
                AgentErrorOwnership::Nomifun,
                Some(detail),
                true,
                true,
                resolution(
                    AgentErrorResolutionKind::SendFeedback,
                    Some(AgentErrorResolutionTarget::Feedback),
                ),
            ),
            AcpError::AgentInternal { .. } => classify_upstream_detail(&detail),
        }
    }
}

fn classify_upstream_detail(detail: &str) -> AgentSendError {
    let lower = detail.to_ascii_lowercase();
    let classified = classify_agent_lifecycle(&lower)
        .or_else(|| classify_provider_api(&lower))
        .or_else(|| classify_nomifun_state(&lower))
        .unwrap_or(ClassifiedError {
            message: "The upstream Agent failed while handling the request",
            code: AgentErrorCode::UnknownUpstreamError,
            ownership: AgentErrorOwnership::UnknownUpstream,
            retryable: true,
            feedback_recommended: true,
            resolution_kind: AgentErrorResolutionKind::SendFeedback,
            resolution_target: Some(AgentErrorResolutionTarget::Feedback),
        });

    classified.into_send_error(detail.to_owned())
}

fn classify_agent_lifecycle(lower: &str) -> Option<ClassifiedError> {
    if lower.contains("agent process exited before initialize handshake completed") {
        return Some(agent_error(
            "The selected Agent exited before it finished starting",
            AgentErrorCode::UserAgentHandshakeFailed,
            true,
            AgentErrorResolutionKind::CheckAgentInstallation,
        ));
    }
    if lower.contains("process exited with code") {
        return Some(agent_error(
            "The selected Agent process exited unexpectedly",
            AgentErrorCode::UserAgentDisconnected,
            true,
            AgentErrorResolutionKind::ReconnectAgent,
        ));
    }
    if lower.contains("initialize handshake timed out") {
        return Some(agent_error(
            "The selected Agent did not finish starting in time",
            AgentErrorCode::UserAgentHandshakeTimeout,
            true,
            AgentErrorResolutionKind::ReconnectAgent,
        ));
    }
    if lower.contains("cli found but acp initialization failed") || lower.contains("找到 cli 但 acp 初始化失败")
    {
        return Some(agent_error(
            "The selected Agent CLI was found but could not initialize ACP",
            AgentErrorCode::UserAgentAcpInitFailed,
            false,
            AgentErrorResolutionKind::CheckAgentInstallation,
        ));
    }
    if lower.contains("protocol mismatch") || lower.contains("max reconnect attempts") {
        return Some(agent_error(
            "The selected Agent protocol is incompatible",
            AgentErrorCode::UserAgentProtocolMismatch,
            false,
            AgentErrorResolutionKind::CheckAgentVersion,
        ));
    }
    if lower.contains("no previous sessions found") {
        return Some(agent_session_error(
            "No previous Agent session was found for this project",
            AgentErrorCode::UserAgentNoPreviousSession,
        ));
    }
    if lower.contains("session not found") {
        return Some(agent_session_error(
            "The Agent session was not found",
            AgentErrorCode::UserAgentSessionNotFound,
        ));
    }
    if lower.contains("command not found") {
        return Some(agent_error(
            "The selected Agent command was not found",
            AgentErrorCode::UserAgentCommandNotFound,
            false,
            AgentErrorResolutionKind::CheckLocalCommand,
        ));
    }
    if lower.contains("missing environment variable") {
        return Some(agent_error(
            "The selected Agent is missing a required environment variable",
            AgentErrorCode::UserAgentMissingEnv,
            false,
            AgentErrorResolutionKind::CheckAgentInstallation,
        ));
    }

    None
}

fn classify_provider_api(lower: &str) -> Option<ClassifiedError> {
    if contains_any(
        lower,
        &[
            "402",
            "insufficient balance",
            "credit balance is too low",
            "purchase credits",
            "plans & billing",
            "plans and billing",
        ],
    ) {
        return Some(provider_error(
            "The model provider account requires billing attention",
            AgentErrorCode::UserLlmProviderBillingRequired,
            false,
            AgentErrorResolutionKind::CheckProviderBilling,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(lower, &["403", "forbidden", "permission denied"]) {
        return Some(provider_error(
            "The model provider denied access to the request",
            AgentErrorCode::UserLlmProviderPermissionDenied,
            false,
            AgentErrorResolutionKind::CheckProviderCredentials,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(
        lower,
        &[
            "401",
            "unauthorized",
            "invalid api key",
            "invalid_api_key",
            "invalid x-api-key",
            "invalid authentication credentials",
        ],
    ) {
        return Some(provider_error(
            "The model provider rejected the request",
            AgentErrorCode::UserLlmProviderAuthFailed,
            false,
            AgentErrorResolutionKind::CheckProviderCredentials,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(
        lower,
        &[
            "signable request",
            "canonical request",
            "signature",
            "access key",
            "secret key",
            "base url",
            "base_url",
        ],
    ) {
        return Some(provider_error(
            "The model provider configuration is invalid",
            AgentErrorCode::UserLlmProviderConfigError,
            false,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(
        lower,
        &[
            "function calling is not enabled",
            "function calling disabled",
            "unsupported model",
        ],
    ) {
        return Some(provider_error(
            "The configured model does not support this request",
            AgentErrorCode::UserLlmProviderUnsupportedModel,
            false,
            AgentErrorResolutionKind::ChangeModel,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(
        lower,
        &[
            "context window exceeds",
            "context length",
            "context too large",
            "maximum context",
            "prompt is too long",
        ],
    ) {
        return Some(provider_error(
            "The request is too large for the configured model context window",
            AgentErrorCode::UserLlmProviderContextTooLarge,
            false,
            AgentErrorResolutionKind::ReduceContext,
            None,
        ));
    }
    if contains_any(
        lower,
        &[
            "invalid schema",
            "schema for function",
            "tool schema",
            "function schema",
        ],
    ) {
        return Some(provider_error(
            "The model provider rejected an internal tool schema",
            AgentErrorCode::UserLlmProviderInvalidToolSchema,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ));
    }
    if contains_any(
        lower,
        &[
            "model not found",
            "model does not exist",
            "unknown model",
            "invalid model",
            "model_not_found",
            "model identifier is invalid",
        ],
    ) {
        return Some(provider_error(
            "The configured model was not found by the provider",
            AgentErrorCode::UserLlmProviderModelNotFound,
            false,
            AgentErrorResolutionKind::ChangeModel,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if lower.contains("404") || lower.contains("not found") {
        return Some(provider_error(
            "The model provider endpoint was not found",
            AgentErrorCode::UserLlmProviderEndpointNotFound,
            false,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(lower, &["429", "rate limit", "rate_limit", "quota"]) {
        return Some(provider_error(
            "The model provider rate limited the request",
            AgentErrorCode::UserLlmProviderRateLimited,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ));
    }
    if contains_any(
        lower,
        &["empty response from llm", "empty response", "response body was empty"],
    ) {
        return Some(provider_error(
            "The model provider returned an empty response",
            AgentErrorCode::UserLlmProviderEmptyResponse,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ));
    }
    if contains_any(lower, &["504", "timeout", "deadline exceeded", "gateway timeout"]) {
        return Some(provider_error(
            "The model provider did not respond in time",
            AgentErrorCode::UserLlmProviderTimeout,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ));
    }
    if contains_any(
        lower,
        &[
            "dns",
            "connection refused",
            "connection reset",
            "tls",
            "certificate",
            "connection error",
            "connect error",
            "error decoding response body",
            "decoding response body",
            "error sending request",
        ],
    ) {
        return Some(provider_error(
            "The model provider could not be reached",
            AgentErrorCode::UserLlmProviderNetworkError,
            true,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    // 图片不支持:上游对 image_url 内容反序列化失败 / 明确拒绝图片。必须排在
    // 通用 invalid_request 分支之前(那条会吞掉 "invalid_request_error")。
    // retryable=false + 不在 is_provider_fault → 由会话服务发送环专门"剔图重跑"。
    if contains_any(
        lower,
        &[
            "image_url",
            "unknown variant `image_url`",
            "unknown variant 'image_url'",
            "does not support image",
            "image input",
            "multimodal",
        ],
    ) {
        return Some(provider_error(
            "当前模型不支持图片输入",
            AgentErrorCode::UserLlmProviderImageUnsupported,
            false,
            AgentErrorResolutionKind::SendFeedback,
            Some(AgentErrorResolutionTarget::Feedback),
        ));
    }
    if contains_any(
        lower,
        &[
            "invalid request",
            "invalid_request",
            "invalid_request_error",
            "invalid assistant message",
            "content is required",
            "invalid input",
        ],
    ) {
        return Some(provider_error(
            "The model provider rejected the request",
            AgentErrorCode::UserLlmProviderInvalidRequest,
            false,
            AgentErrorResolutionKind::SendFeedback,
            Some(AgentErrorResolutionTarget::Feedback),
        ));
    }
    if contains_any(lower, &["500", "502", "503", "bad gateway", "service unavailable"]) {
        return Some(provider_error(
            "The model provider returned a server error",
            AgentErrorCode::UserLlmProviderGatewayError,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ));
    }
    if lower.contains("provider error") {
        return Some(provider_error(
            "The model provider returned an error",
            AgentErrorCode::UserLlmProviderGatewayError,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ));
    }

    None
}

fn classify_nomifun_state(lower: &str) -> Option<ClassifiedError> {
    if lower.contains("conversation is already processing") {
        return Some(ClassifiedError {
            message: "The current response is still running",
            code: AgentErrorCode::NomifunConversationBusy,
            ownership: AgentErrorOwnership::Nomifun,
            retryable: true,
            feedback_recommended: false,
            resolution_kind: AgentErrorResolutionKind::WaitForCurrentResponse,
            resolution_target: Some(AgentErrorResolutionTarget::NewConversation),
        });
    }

    None
}

fn agent_error(
    message: &'static str,
    code: AgentErrorCode,
    retryable: bool,
    resolution_kind: AgentErrorResolutionKind,
) -> ClassifiedError {
    ClassifiedError {
        message,
        code,
        ownership: AgentErrorOwnership::UserAgent,
        retryable,
        feedback_recommended: false,
        resolution_kind,
        resolution_target: Some(AgentErrorResolutionTarget::AgentSettings),
    }
}

fn agent_session_error(message: &'static str, code: AgentErrorCode) -> ClassifiedError {
    ClassifiedError {
        message,
        code,
        ownership: AgentErrorOwnership::UserAgent,
        retryable: true,
        feedback_recommended: false,
        resolution_kind: AgentErrorResolutionKind::StartNewSession,
        resolution_target: Some(AgentErrorResolutionTarget::NewConversation),
    }
}

fn provider_error(
    message: &'static str,
    code: AgentErrorCode,
    retryable: bool,
    resolution_kind: AgentErrorResolutionKind,
    resolution_target: Option<AgentErrorResolutionTarget>,
) -> ClassifiedError {
    ClassifiedError {
        message,
        code,
        ownership: AgentErrorOwnership::UserLlmProvider,
        retryable,
        feedback_recommended: false,
        resolution_kind,
        resolution_target,
    }
}

fn resolution(
    kind: AgentErrorResolutionKind,
    target: Option<AgentErrorResolutionTarget>,
) -> Option<AgentErrorResolution> {
    Some(AgentErrorResolution::new(kind, target))
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn strip_error_prefix(message: &str) -> String {
    message
        .split_once(": ")
        .map(|(_, rest)| rest)
        .unwrap_or(message)
        .to_owned()
}

pub(crate) fn sanitize_error_detail(input: &str) -> String {
    let stripped = strip_markup(input);
    let without_query = redact_url_queries(&stripped);
    let redacted = redact_lines(&without_query);
    truncate_chars(redacted.trim(), MAX_DETAIL_CHARS)
}

fn strip_markup(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match (ch, in_tag) {
            ('<', _) => in_tag = true,
            ('>', true) => {
                in_tag = false;
                out.push(' ');
            }
            (c, false) => out.push(c),
            _ => {}
        }
    }
    out
}

fn redact_lines(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let cleaned = if is_sensitive_header_line(line) {
            "<redacted header>".to_owned()
        } else {
            redact_secret_words(line)
        };
        push_bounded_line(&mut out, &cleaned);
        if out.chars().count() >= MAX_DETAIL_CHARS {
            break;
        }
    }
    out
}

fn push_bounded_line(out: &mut String, line: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(line);
}

fn is_sensitive_header_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("authorization:")
        || lower.contains("x-api-key:")
        || lower.contains("api-key:")
        || lower.contains("api_key:")
}

fn redact_secret_words(line: &str) -> String {
    line.split_whitespace()
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            if lower.starts_with("bearer ")
                || lower.starts_with("sk-")
                || lower.contains("api_key=")
                || lower.contains("apikey=")
                || lower.contains("access_token=")
                || lower.contains("token=")
            {
                "<redacted>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_url_queries(input: &str) -> String {
    input
        .split_whitespace()
        .map(|word| {
            if (word.starts_with("http://") || word.starts_with("https://")) && word.contains('?') {
                let end_punct = word
                    .chars()
                    .last()
                    .filter(|c| matches!(c, '.' | ',' | ';' | ')' | ']'))
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                let trimmed = word.trim_end_matches(['.', ',', ';', ')', ']']);
                let base = trimmed.split_once('?').map(|(base, _)| base).unwrap_or(trimmed);
                format!("{base}?<redacted>{end_punct}")
            } else {
                word.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_chars(value: &str, max: usize) -> String {
    let mut out = String::new();
    for ch in value.chars().take(max) {
        out.push(ch);
    }
    if value.chars().count() > max {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::{AgentErrorResolutionKind, AgentErrorResolutionTarget};

    fn assert_classification(
        detail: &str,
        code: AgentErrorCode,
        ownership: AgentErrorOwnership,
        resolution: AgentErrorResolutionKind,
    ) {
        let err = AgentSendError::from_app_error(AppError::BadGateway(detail.into()));
        assert_eq!(err.code(), Some(code));
        assert_eq!(err.ownership(), Some(ownership));
        assert_eq!(err.stream_error().resolution.map(|value| value.kind), Some(resolution));
    }

    fn assert_resolution_target(detail: &str, target: AgentErrorResolutionTarget) {
        let err = AgentSendError::from_app_error(AppError::BadGateway(detail.into()));
        assert_eq!(
            err.stream_error().resolution.and_then(|value| value.target),
            Some(target)
        );
    }

    #[test]
    fn classifies_provider_auth_failure() {
        let err = AgentSendError::from_app_error(AppError::BadGateway("provider returned 401 invalid api key".into()));

        assert_eq!(err.code(), Some(AgentErrorCode::UserLlmProviderAuthFailed));
        assert_eq!(err.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
    }

    #[test]
    fn classifies_unknown_upstream_when_heuristics_do_not_match() {
        let err = AgentSendError::from_app_error(AppError::BadGateway("agent exploded".into()));

        assert_eq!(err.code(), Some(AgentErrorCode::UnknownUpstreamError));
        assert_eq!(err.ownership(), Some(AgentErrorOwnership::UnknownUpstream));
        assert_eq!(err.stream_error().feedback_recommended, Some(true));
    }

    #[test]
    fn preserves_runtime_workspace_validation_as_structured_nomifun_error() {
        let err = AgentSendError::from_app_error(AppError::WorkspacePathEdgeWhitespaceRuntimeUnsupported(
            "/Users/test/Archive ".into(),
        ));

        assert_eq!(
            err.code(),
            Some(AgentErrorCode::WorkspacePathEdgeWhitespaceRuntimeUnsupported)
        );
        assert_eq!(err.ownership(), Some(AgentErrorOwnership::Nomifun));
        assert_eq!(
            err.stream_error().workspace_path.as_deref(),
            Some("/Users/test/Archive ")
        );
        assert_eq!(err.stream_error().retryable, Some(false));
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
        assert_eq!(
            err.stream_error().resolution.map(|value| value.kind),
            Some(AgentErrorResolutionKind::StartNewSession)
        );
    }

    #[test]
    fn classifies_provider_error_without_specific_signal_as_provider_gateway() {
        let err = AgentSendError::from_app_error(AppError::BadGateway("Provider error: upstream failed".into()));

        assert_eq!(err.code(), Some(AgentErrorCode::UserLlmProviderGatewayError));
        assert_eq!(err.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
    }

    #[test]
    fn classifies_provider_config_errors_as_not_retryable() {
        let err = AgentSendError::from_app_error(AppError::BadGateway(
            "Provider error: Connection error: Signable request error: failed to create canonical request".into(),
        ));

        assert_eq!(err.code(), Some(AgentErrorCode::UserLlmProviderConfigError));
        assert_eq!(err.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
        assert_eq!(err.stream_error().retryable, Some(false));
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
    }

    #[test]
    fn sanitizes_secrets_and_query_strings() {
        let detail = sanitize_error_detail(
            "Authorization: Bearer sk-secret\nGET https://example.com/v1?api_key=sk-secret\ninvalid_api_key sk-secret",
        );

        assert!(!detail.contains("sk-secret"));
        assert!(!detail.contains("api_key=sk"));
        assert!(detail.contains("<redacted header>"));
        assert_eq!(
            redact_url_queries("GET https://example.com/v1?api_key=sk-secret"),
            "GET https://example.com/v1?<redacted>"
        );
    }

    #[test]
    fn strip_markup_removes_html_tags_keeping_visible_text() {
        let detail = sanitize_error_detail(
            "Provider error: API error 504: <html>\r\n<head><title>504 Gateway Time-out</title></head>\r\n<body><center><h1>504 Gateway Time-out</h1></center>\r\n<hr><center>openresty</center></body></html>",
        );

        assert!(!detail.contains('<'));
        assert!(!detail.contains('>'));
        assert!(detail.contains("504 Gateway Time-out"));
        assert!(detail.contains("openresty"));
        assert!(detail.starts_with("Provider error: API error 504:"));
    }

    #[test]
    fn strip_markup_is_identity_for_plain_text() {
        let detail = sanitize_error_detail("Provider error: API error 504: error code: 524");

        assert_eq!(detail, "Provider error: API error 504: error code: 524");
    }

    #[test]
    fn redaction_runs_after_strip_markup() {
        let detail = sanitize_error_detail(
            "<html><body>Authorization: Bearer sk-secret\nGET https://example.com/v1?api_key=sk-secret</body></html>",
        );

        assert!(!detail.contains("sk-secret"));
        assert!(!detail.contains("api_key=sk"));
        assert!(detail.contains("<redacted header>"));
        assert!(!detail.contains("<html"));
        assert!(!detail.contains("</body>"));
    }

    #[test]
    fn classifies_provider_504_html_body_as_timeout_with_stripped_detail() {
        let raw = "Nomi agent error: Provider error: API error 504: <html>\r\n<head><title>504 Gateway Time-out</title></head>\r\n<body>\r\n<center><h1>504 Gateway Time-out</h1></center>\r\n<hr><center>openresty</center>\r\n</body>\r\n</html>";
        let err = AgentSendError::from_app_error(AppError::BadGateway(raw.into()));

        assert_eq!(err.code(), Some(AgentErrorCode::UserLlmProviderTimeout));
        let detail = err.stream_error().detail.clone().expect("detail present");
        assert!(!detail.contains('<'));
        assert!(!detail.contains('>'));
        assert!(detail.chars().count() <= MAX_DETAIL_CHARS);
        assert!(detail.contains("504 Gateway Time-out"));
    }

    #[test]
    fn classifies_agent_lifecycle_before_bad_gateway_wrapper() {
        assert_classification(
            "Bad gateway: Agent process exited before initialize handshake completed (exit code 1)",
            AgentErrorCode::UserAgentHandshakeFailed,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::CheckAgentInstallation,
        );
        assert_classification(
            "Bad gateway: Initialize handshake timed out after 30s",
            AgentErrorCode::UserAgentHandshakeTimeout,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::ReconnectAgent,
        );
    }

    #[test]
    fn classifies_mid_session_cli_exit_as_agent_disconnected() {
        // Mid-session ACP CLI exit (e.g. Claude Code) surfaces as -32603 with
        // `details: "Claude Code process exited with code 1"`. Previously this
        // fell through to UNKNOWN_UPSTREAM_ERROR; it now maps to a retryable
        // agent disconnect.
        assert_classification(
            "Agent internal error (code -32603) ({\"details\":\"Claude Code process exited with code 1\"})",
            AgentErrorCode::UserAgentDisconnected,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::ReconnectAgent,
        );
        let err = AgentSendError::from_app_error(AppError::BadGateway(
            "Agent internal error (code -32603) ({\"details\":\"Claude Code process exited with code 1\"})".into(),
        ));
        assert_eq!(err.stream_error().retryable, Some(true));
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
    }

    #[test]
    fn classifies_agent_protocol_and_session_failures() {
        assert_classification(
            "Connection error: protocol mismatch Connection error: Max reconnect attempts (10) reached",
            AgentErrorCode::UserAgentProtocolMismatch,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::CheckAgentVersion,
        );
        assert_classification(
            "Agent internal error (code -32603) {\"details\":\"Session not found\"}",
            AgentErrorCode::UserAgentSessionNotFound,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::StartNewSession,
        );
        assert_classification(
            "Bad gateway: Agent internal error (code -32603) {\"details\":\"No previous sessions found for this project\"}",
            AgentErrorCode::UserAgentNoPreviousSession,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::StartNewSession,
        );
    }

    #[test]
    fn classifies_agent_setup_failures() {
        assert_classification(
            "CLI found but ACP initialization failed.",
            AgentErrorCode::UserAgentAcpInitFailed,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::CheckAgentInstallation,
        );
        assert_classification(
            "找到 CLI 但 ACP 初始化失败",
            AgentErrorCode::UserAgentAcpInitFailed,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::CheckAgentInstallation,
        );
        assert_classification(
            "filesystem: Command not found: npx",
            AgentErrorCode::UserAgentCommandNotFound,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::CheckLocalCommand,
        );
        assert_classification(
            "Agent internal error (code -32603) {\"message\":\"Missing environment variable: 'OMLX API KEY'\"}",
            AgentErrorCode::UserAgentMissingEnv,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::CheckAgentInstallation,
        );
    }

    #[test]
    fn classifies_provider_billing_auth_and_rate_limit() {
        assert_classification(
            "Nomi agent error: Provider error: API error 402: {\"error\":{\"message\":\"Insufficient Balance\"}}",
            AgentErrorCode::UserLlmProviderBillingRequired,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderBilling,
        );
        assert_classification(
            "Nomi agent error: Provider error: API error 400: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.\"}}",
            AgentErrorCode::UserLlmProviderBillingRequired,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderBilling,
        );
        assert_classification(
            "Nomi agent error: Provider error: API error 401: invalid x-api-key",
            AgentErrorCode::UserLlmProviderAuthFailed,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderCredentials,
        );
        assert_classification(
            "Nomi agent error: Provider error: Rate limited, retry after 5000ms",
            AgentErrorCode::UserLlmProviderRateLimited,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::Retry,
        );
    }

    #[test]
    fn classifies_provider_auth_credentials_before_config() {
        assert_classification(
            "API error 401: Invalid authentication credentials",
            AgentErrorCode::UserLlmProviderAuthFailed,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderCredentials,
        );
    }

    #[test]
    fn classifies_provider_permission_denied_separately_from_auth() {
        for detail in [
            "API error 403: forbidden",
            "Provider error: permission denied",
            "API error 403: You do not have permission to access this resource",
        ] {
            assert_classification(
                detail,
                AgentErrorCode::UserLlmProviderPermissionDenied,
                AgentErrorOwnership::UserLlmProvider,
                AgentErrorResolutionKind::CheckProviderCredentials,
            );
        }
    }

    #[test]
    fn classifies_provider_request_model_and_context_errors() {
        assert_classification(
            "API error 400: Function calling is not enabled for this model",
            AgentErrorCode::UserLlmProviderUnsupportedModel,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::ChangeModel,
        );
        assert_classification(
            "API error 400: invalid params, context window exceeds limit",
            AgentErrorCode::UserLlmProviderContextTooLarge,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::ReduceContext,
        );
        assert_classification(
            "API error 400: Invalid schema for function 'nomi_list_models': None is not of type 'array'",
            AgentErrorCode::UserLlmProviderInvalidToolSchema,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::Retry,
        );
    }

    #[test]
    fn classifies_model_not_found_before_endpoint_404() {
        assert_classification(
            "API error 404: model not found for path /v1/chat/completions",
            AgentErrorCode::UserLlmProviderModelNotFound,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::ChangeModel,
        );
    }

    #[test]
    fn classifies_bedrock_invalid_model_identifier_as_model_not_found() {
        assert_classification(
            "Nomi agent error: Provider error: API error 400: {\"message\":\"The provided model identifier is invalid.\"}",
            AgentErrorCode::UserLlmProviderModelNotFound,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::ChangeModel,
        );
        assert_resolution_target(
            "Nomi agent error: Provider error: API error 400: {\"message\":\"The provided model identifier is invalid.\"}",
            AgentErrorResolutionTarget::ProviderSettings,
        );
    }

    #[test]
    fn classifies_generic_provider_invalid_requests() {
        for detail in [
            "API error 400: Invalid request: Invalid input",
            "API error 400: Invalid assistant message: content or tool calls must be set",
            "API error 400: content is required",
            "Provider error: API error 400: {\"type\":\"invalid_request_error\",\"message\":\"bad payload\"}",
        ] {
            assert_classification(
                detail,
                AgentErrorCode::UserLlmProviderInvalidRequest,
                AgentErrorOwnership::UserLlmProvider,
                AgentErrorResolutionKind::SendFeedback,
            );
            let err = AgentSendError::from_app_error(AppError::BadGateway(detail.into()));
            assert_eq!(err.stream_error().retryable, Some(false));
        }
    }

    #[test]
    fn classifies_image_unsupported_from_serde_variant_error() {
        let detail = "Failed to deserialize the JSON body into the target type: \
            messages[6]: unknown variant `image_url`, expected `text` at line 1 column 169755";
        let err = AgentSendError::from_app_error(AppError::BadGateway(detail.into()));
        assert_eq!(err.code(), Some(AgentErrorCode::UserLlmProviderImageUnsupported));
    }

    #[test]
    fn plain_invalid_request_still_classifies_as_invalid_request() {
        let detail = "invalid_request_error: content is required";
        let err = AgentSendError::from_app_error(AppError::BadGateway(detail.into()));
        assert_eq!(err.code(), Some(AgentErrorCode::UserLlmProviderInvalidRequest));
    }

    #[test]
    fn non_retryable_agent_invalid_params_do_not_suggest_retry() {
        let app_err =
            AgentSendError::from_app_error(AppError::BadRequest("Invalid parameters: malformed request".into()));
        assert_eq!(app_err.code(), Some(AgentErrorCode::UserAgentInvalidParams));
        assert_eq!(app_err.stream_error().retryable, Some(false));
        assert_eq!(app_err.stream_error().feedback_recommended, Some(true));
        assert_eq!(
            app_err.stream_error().resolution.map(|value| value.kind),
            Some(AgentErrorResolutionKind::SendFeedback)
        );

        let acp_err = AgentSendError::from(AcpError::InvalidParams {
            message: "malformed request".into(),
        });
        assert_eq!(acp_err.code(), Some(AgentErrorCode::UserAgentInvalidParams));
        assert_eq!(acp_err.stream_error().retryable, Some(false));
        assert_eq!(acp_err.stream_error().feedback_recommended, Some(true));
        assert_eq!(
            acp_err.stream_error().resolution.map(|value| value.kind),
            Some(AgentErrorResolutionKind::SendFeedback)
        );
    }

    #[test]
    fn classifies_provider_endpoint_network_timeout_and_empty_response() {
        assert_classification(
            "API error 404: {\"status\":404,\"error\":\"Not Found\",\"path\":\"/v4/v1/chat/completions\"}",
            AgentErrorCode::UserLlmProviderEndpointNotFound,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
        );
        assert_resolution_target(
            "API error 404: {\"status\":404,\"error\":\"Not Found\",\"path\":\"/v4/v1/chat/completions\"}",
            AgentErrorResolutionTarget::ProviderSettings,
        );
        assert_classification(
            "Nomi agent error: API error: Connection error: error decoding response body",
            AgentErrorCode::UserLlmProviderNetworkError,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
        );
        assert_classification(
            "Nomi agent error: API error: error sending request for url",
            AgentErrorCode::UserLlmProviderNetworkError,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
        );
        assert_classification(
            "Autocompact failed: Empty response from LLM",
            AgentErrorCode::UserLlmProviderEmptyResponse,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::Retry,
        );
    }

    #[test]
    fn classifies_bare_provider_404_as_endpoint_not_found() {
        let detail = "Nomi agent error: Provider error: API error 404: {\"detail\":\"Not Found\"}";
        assert_classification(
            detail,
            AgentErrorCode::UserLlmProviderEndpointNotFound,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
        );
        assert_resolution_target(detail, AgentErrorResolutionTarget::ProviderSettings);
        let err = AgentSendError::from_app_error(AppError::BadGateway(detail.into()));
        assert_eq!(err.stream_error().retryable, Some(false));
    }

    #[test]
    fn classifies_nomifun_conversation_busy_after_agent_and_provider_checks() {
        assert_classification(
            "Conflict: Conversation is already processing a message",
            AgentErrorCode::NomifunConversationBusy,
            AgentErrorOwnership::Nomifun,
            AgentErrorResolutionKind::WaitForCurrentResponse,
        );
    }
}
