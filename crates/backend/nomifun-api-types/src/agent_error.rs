use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentErrorOwnership {
    Nomifun,
    UserAgent,
    UserLlmProvider,
    UnknownUpstream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentErrorCode {
    NomifunConversationBusy,
    NomifunStreamBroken,
    NomifunStateInconsistent,
    NomifunPermissionError,
    NomifunInternalError,
    WorkspacePathEdgeWhitespaceRuntimeUnsupported,
    UserAgentHandshakeFailed,
    UserAgentHandshakeTimeout,
    UserAgentAcpInitFailed,
    UserAgentProtocolMismatch,
    UserAgentNotInstalled,
    UserAgentStartupFailed,
    UserAgentDisconnected,
    UserAgentAuthRequired,
    UserAgentSessionNotFound,
    UserAgentNoPreviousSession,
    UserAgentCommandNotFound,
    UserAgentMissingEnv,
    UserAgentUnsupportedMethod,
    UserAgentInvalidParams,
    UserLlmProviderAuthFailed,
    UserLlmProviderPermissionDenied,
    UserLlmProviderBillingRequired,
    UserLlmProviderConfigError,
    UserLlmProviderModelNotFound,
    UserLlmProviderUnsupportedModel,
    UserLlmProviderEndpointNotFound,
    UserLlmProviderInvalidRequest,
    /// 模型不支持图片输入(收到 image_url 类 400)。会话服务据此剔图重跑,
    /// 故意 **不** 计入 is_provider_fault(不触发换模型)。
    UserLlmProviderImageUnsupported,
    UserLlmProviderInvalidToolSchema,
    UserLlmProviderContextTooLarge,
    UserLlmProviderRateLimited,
    UserLlmProviderTimeout,
    UserLlmProviderNetworkError,
    UserLlmProviderEmptyResponse,
    UserLlmProviderGatewayError,
    UnknownUpstreamError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentErrorResolutionKind {
    Retry,
    WaitForCurrentResponse,
    StartNewSession,
    ReconnectAgent,
    CheckAgentLogin,
    CheckAgentInstallation,
    CheckAgentVersion,
    CheckLocalCommand,
    CheckProviderCredentials,
    CheckProviderBilling,
    CheckProviderBaseUrl,
    ChangeModel,
    ReduceContext,
    SendFeedback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentErrorResolutionTarget {
    ProviderSettings,
    AgentSettings,
    NewConversation,
    Feedback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentErrorResolution {
    pub kind: AgentErrorResolutionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<AgentErrorResolutionTarget>,
}

impl AgentErrorResolution {
    pub fn new(kind: AgentErrorResolutionKind, target: Option<AgentErrorResolutionTarget>) -> Self {
        Self { kind, target }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStreamErrorData {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<AgentErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership: Option<AgentErrorOwnership>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "workspacePath",
        alias = "workspace_path"
    )]
    pub workspace_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_recommended: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<AgentErrorResolution>,
}

impl AgentStreamErrorData {
    pub fn legacy(message: impl Into<String>, code: Option<AgentErrorCode>) -> Self {
        Self {
            message: message.into(),
            code,
            ownership: None,
            detail: None,
            workspace_path: None,
            retryable: None,
            feedback_recommended: None,
            resolution: None,
        }
    }

    pub fn classified(
        message: impl Into<String>,
        code: AgentErrorCode,
        ownership: AgentErrorOwnership,
        detail: Option<String>,
        retryable: bool,
        feedback_recommended: bool,
        resolution: Option<AgentErrorResolution>,
    ) -> Self {
        Self {
            message: message.into(),
            code: Some(code),
            ownership: Some(ownership),
            detail,
            workspace_path: None,
            retryable: Some(retryable),
            feedback_recommended: Some(feedback_recommended),
            resolution,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_unsupported_serde_roundtrip() {
        let code = AgentErrorCode::UserLlmProviderImageUnsupported;
        let json = serde_json::to_string(&code).unwrap();
        assert_eq!(json, "\"USER_LLM_PROVIDER_IMAGE_UNSUPPORTED\"");
        let back: AgentErrorCode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, code);
    }

    #[test]
    fn classified_error_serializes_as_public_contract() {
        let payload = AgentStreamErrorData::classified(
            "The model provider rejected the request",
            AgentErrorCode::UserLlmProviderAuthFailed,
            AgentErrorOwnership::UserLlmProvider,
            Some("Provider returned 401.".into()),
            false,
            false,
            None,
        );

        let json = serde_json::to_value(payload).unwrap();
        assert_eq!(json["message"], "The model provider rejected the request");
        assert_eq!(json["code"], "USER_LLM_PROVIDER_AUTH_FAILED");
        assert_eq!(json["ownership"], "user_llm_provider");
        assert!(json.get("workspacePath").is_none());
        assert_eq!(json["retryable"], false);
        assert_eq!(json["feedback_recommended"], false);
        assert!(json.get("resolution").is_none());
    }

    #[test]
    fn classified_error_serializes_resolution() {
        let payload = AgentStreamErrorData::classified(
            "The current response is still running",
            AgentErrorCode::NomifunConversationBusy,
            AgentErrorOwnership::Nomifun,
            Some("Conflict: Conversation is already processing a message".into()),
            true,
            false,
            Some(AgentErrorResolution::new(
                AgentErrorResolutionKind::WaitForCurrentResponse,
                Some(AgentErrorResolutionTarget::NewConversation),
            )),
        );

        let json = serde_json::to_value(payload).unwrap();
        assert_eq!(json["code"], "NOMIFUN_CONVERSATION_BUSY");
        assert_eq!(json["resolution"]["kind"], "wait_for_current_response");
        assert_eq!(json["resolution"]["target"], "new_conversation");
    }

    #[test]
    fn legacy_error_payload_deserializes() {
        let json = serde_json::json!({
            "message": "legacy failure",
            "code": "UNKNOWN_UPSTREAM_ERROR"
        });

        let payload: AgentStreamErrorData = serde_json::from_value(json).unwrap();
        assert_eq!(payload.message, "legacy failure");
        assert_eq!(payload.code, Some(AgentErrorCode::UnknownUpstreamError));
        assert_eq!(payload.ownership, None);
        assert_eq!(payload.workspace_path, None);
        assert_eq!(payload.retryable, None);
        assert_eq!(payload.feedback_recommended, None);
    }

    #[test]
    fn legacy_error_payload_has_no_resolution() {
        let json = serde_json::json!({
            "message": "legacy failure",
            "code": "UNKNOWN_UPSTREAM_ERROR"
        });

        let payload: AgentStreamErrorData = serde_json::from_value(json).unwrap();
        assert_eq!(payload.resolution, None);
    }

    #[test]
    fn workspace_path_field_serializes_and_deserializes() {
        let payload = AgentStreamErrorData {
            message: "workspace path rejected".into(),
            code: Some(AgentErrorCode::WorkspacePathEdgeWhitespaceRuntimeUnsupported),
            ownership: Some(AgentErrorOwnership::Nomifun),
            detail: Some("workspace detail".into()),
            workspace_path: Some("/tmp/Archive ".into()),
            retryable: Some(false),
            feedback_recommended: Some(false),
            resolution: None,
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["code"], "WORKSPACE_PATH_EDGE_WHITESPACE_RUNTIME_UNSUPPORTED");
        assert_eq!(json["workspacePath"], "/tmp/Archive ");

        let roundtrip: AgentStreamErrorData = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip.workspace_path.as_deref(), Some("/tmp/Archive "));
    }
}
