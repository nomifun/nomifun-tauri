//! All HTTP request/response DTOs shared across the API surface.
mod acp;
mod acp_prompt_hook;
mod agent_build_extra;
mod agent_discovery;
mod agent_error;
mod assistant;
mod auth;
mod channel;
mod confirmation;
mod connection_test;
mod conversation;
mod cron;
mod custom_agent;
mod exposure;
mod extension;
mod file;
mod idmm;
mod knowledge;
mod lifecycle;
mod mcp;
mod office;
mod orchestrator;
mod provider;
mod remote_agent;
mod requirement;
mod response;
mod secret;
mod serde_util;
mod shell;
mod skill;
mod system;
mod team_mcp;
mod terminal;
mod webhook;
mod websocket;

pub use acp::{
    AcpHealthCheckRequest, AcpHealthCheckResponse, AgentModeResponse, DetectCliRequest,
    DetectCliResponse, GetModelInfoResponse, ModelInfoEntry, ModelInfoPayload, ProbeModelRequest,
    SetModeRequest, SetModelRequest, SideQuestionRequest, SideQuestionResponse,
    TryConnectCustomAgentRequest, TryConnectCustomAgentResponse, WorkspaceBrowseQuery,
    WorkspaceEntry,
};
pub use acp_prompt_hook::AcpPromptHookWarningPayload;
pub use agent_build_extra::{
    AcpBuildExtra, AcpModelInfo, NomiBuildExtra, NomiGoalSpec, OpenClawBuildExtra,
    OpenClawGatewayConfig, RemoteBuildExtra, SessionMcpServer, SessionMcpTransport,
    SlashCommandItem,
};
pub use agent_discovery::{
    AgentEnvEntry, AgentHandshake, AgentMetadata, AgentSource, AgentSourceInfo, BehaviorPolicy,
};
pub use agent_error::{
    AgentErrorCode, AgentErrorOwnership, AgentErrorResolution, AgentErrorResolutionKind,
    AgentErrorResolutionTarget, AgentStreamErrorData,
};
pub use exposure::{ExposureClamp, ExposureMode, SAFE_PUBLIC_SERVICE_TOOLS, exposure_clamp};
pub use assistant::{
    AssistantResponse, AssistantSource, AssistantTagDimension, AssistantTagResponse,
    CreateAssistantRequest, CreateAssistantTagRequest, ImportAssistantsRequest,
    ImportAssistantsResult, ImportError, SetAssistantStateRequest, UpdateAssistantRequest,
    UpdateAssistantTagRequest,
};
pub use auth::{
    AuthStatusResponse, ChangePasswordRequest, LoginRequest, LoginResponse, PublicUser,
    QrLoginRequest, RefreshResponse, RefreshTokenRequest, UserInfoResponse,
    WebuiChangePasswordRequest, WebuiChangeUsernameRequest, WebuiChangeUsernameResponse,
    WebuiGenerateQrTokenResponse, WebuiResetPasswordResponse, WsTokenResponse,
};
pub use channel::{
    ApprovePairingRequest, BridgeResponse, ChannelSessionResponse, ChannelUserResponse,
    DisablePluginRequest, EnablePluginRequest, PairingRequestResponse, PairingRequestedPayload,
    PluginStatusChangedPayload, PluginStatusResponse, RejectPairingRequest, RevokeUserRequest,
    SyncChannelSettingsRequest, TestPluginExtraConfig, TestPluginRequest, TestPluginResponse,
    UserAuthorizedPayload,
};
pub use confirmation::{
    ApprovalCheckQuery, ApprovalCheckResponse, ConfirmRequest, ConfirmationListResponse,
};
pub use connection_test::TestBedrockConnectionRequest;
pub use conversation::{
    ActiveCountResponse, CloneConversationRequest, ConversationArtifactKind,
    ConversationArtifactListResponse, ConversationArtifactResponse, ConversationArtifactStatus,
    ConversationListResponse, ConversationMcpStatus, ConversationMcpStatusKind,
    ConversationResponse, ConversationRuntimeStateKind, ConversationRuntimeSummary,
    CreateConversationRequest, ListConversationsQuery, ListMessagesQuery, MessageListResponse,
    MessageResponse, MessageSearchItem, MessageSearchResponse, SearchMessagesQuery,
    SendMessageRequest, SendMessageResponse, UpdateConversationArtifactRequest,
    UpdateConversationRequest,
};
pub use cron::{
    CreateCronJobRequest, CronAgentConfigDto, CronJobExecutedEvent, CronJobMetadataDto,
    CronJobPayloadDto, CronJobRemovedPayload, CronJobResponse, CronJobRunResponse, CronJobStateDto,
    CronJobTargetDto, CronScheduleDto, HasSkillResponse, ListCronJobsQuery, RunNowResponse,
    SaveCronSkillRequest, UpdateCronJobRequest,
};
pub use custom_agent::{
    CustomAgentAdvancedOverrides, CustomAgentUpsertRequest, DeleteCustomAgentResponse, SetEnabledRequest,
};
pub use extension::{
    DisableExtensionRequest, EnableExtensionRequest, ExtensionSummaryResponse, GetI18nRequest,
    GetPermissionsRequest, GetRiskLevelRequest, HubExtensionListItem, HubExtensionListResponse,
    HubOperationResponse, HubUpdateInfo, InstallExtensionRequest, PermissionDetailResponse,
    PermissionSummaryResponse,
};
pub use file::{
    BrowseDirectoryQuery, BrowseDirectoryResponse, BrowseEntry, CancelZipRequest, CopyFilesRequest,
    CopyFilesResponse, CreateTempFileRequest, DirOrFileResponse, FetchRemoteImageRequest,
    FileChangeInfoResponse, FileMetadataResponse, FileWatchRequest, GetFileMetadataRequest,
    GetFilesByDirRequest, GetImageBase64Request, ListWorkspaceFilesRequest, ReadFileBufferRequest,
    ReadFileRequest, RemoveEntryRequest, RenameRequest, RenameResponse, SnapshotBaselineRequest,
    SnapshotCompareResponse, SnapshotDiscardRequest, SnapshotInfoResponse, SnapshotMode,
    SnapshotStageRequest, SnapshotWorkspaceRequest, WorkspaceFlatFileResponse,
    WorkspaceOfficeWatchRequest, WriteFileRequest, ZipFileEntry, ZipRequest,
};
pub use idmm::{
    BlockedBehavior, BudgetConfig, BypassModelRef, CategoryMode, CategoryRules, DecisionStrategy,
    DecisionWatchConfig, FaultWatchConfig, IdmmConfig, IdmmRunState, IdmmSettings, IdmmState,
    IdmmTargetKind, InterventionRecord, ModelFailoverConfig, OpenQuestionRule, OptionRule,
    PermissionRule, ScanScope, SetIdmmRequest, Tendency, WakeStrategy, WatchBase, WatchTier,
};
pub use knowledge::{
    ConnectorCredentialSummary, ConnectorSyncState, CreateKnowledgeTagRequest, KnowledgeMountInfo,
    KnowledgeSource, KnowledgeSourceEntry, KnowledgeSourceMode, KnowledgeTag,
    UpdateKnowledgeTagRequest,
};
pub use lifecycle::{
    GitHubReleaseAsset, SystemInfoResponse, UpdateCheckRequest, UpdateCheckResult,
    UpdateReleaseInfo, UpdateWorkDirRequest,
};
pub use mcp::{
    BatchImportMcpServersRequest, CreateMcpServerRequest, DetectedMcpServerEntry,
    DetectedMcpServerResponse, ImportMcpServerRequest, McpAuthMethod, McpConnectionTestErrorCode,
    McpConnectionTestResult, McpServerResponse, McpToolResponse, McpTransport,
    OAuthCheckStatusRequest, OAuthLoginRequest, OAuthLoginResponse, OAuthLogoutRequest,
    OAuthStatusResponse, TestMcpConnectionRequest, UpdateMcpServerRequest,
};
pub use office::{
    CellCoord, CellRange, ConversionResultDto, ConversionTarget, DetectStarOfficeRequest,
    DocumentConversionRequest, DocumentConversionResponse, ExcelSheetData, ExcelSheetImage,
    ExcelWorkbookData, GetSnapshotContentRequest, ListSnapshotsRequest, PptJsonData, PptSlideData,
    PreviewHistoryTargetDto, PreviewSnapshotInfoDto, PreviewState, PreviewStatusEvent,
    PreviewUrlResponse, SaveSnapshotRequest, SnapshotContentResponse, StarOfficeDetectResponse,
    StartPreviewRequest, StopPreviewRequest,
};
pub use orchestrator::{
    Assignment, CapabilityProfile, CreateAdhocRunRequest, CreateFleetRequest, CreateRunRequest,
    CreateWorkspaceRequest, Fleet, FleetMember, FleetMemberInput, MemberConstraints, ModelRange,
    ModelRef, OrchWorkspace, PlannedDag, PlannedTask, ReassignRequest, ReplanRequest, Run,
    RunDetail, RunRenameRequest, RunTask, RunTaskDep, SteerRequest, TaskConfigUpdateRequest,
    TaskProfile, TaskSpecUpdateRequest, UpdateFleetRequest, UpdateWorkspaceRequest, derive_capability,
};
pub use orchestrator::AdjustRunRequest;
pub use provider::{
    BedrockAuthMethod, BedrockConfig, CreateProviderRequest, DetectProtocolRequest,
    DetectedProtocol, DetectionSuggestion, FetchModelsAnonymousRequest, FetchModelsRequest,
    FetchModelsResponse, HealthStatus, KeyTestResult, ModelCapability, ModelHealthStatus,
    ModelInfo, ModelType, MultiKeyResult, ProtocolDetectionResponse, ProviderHealthCheckErrorKind,
    ProviderHealthCheckRequest, ProviderHealthCheckResponse, ProviderResponse, SuggestionType,
    UpdateProviderRequest,
};
pub use remote_agent::{
    CreateRemoteAgentRequest, HandshakeResponse, RemoteAgentListItem, RemoteAgentResponse,
    TestRemoteAgentConnectionRequest, UpdateRemoteAgentRequest,
};
pub use requirement::{
    AttachmentDto, AutoWorkConfigRequest, AutoWorkRunState, AutoWorkState, AutoWorkTargetKind,
    BatchDeleteRequest, BatchDeleteResponse, BoardResponse, ClaimRequest, CompleteRequest,
    CreateRequirementRequest, ListRequirementsQuery, NewAttachmentRef, Requirement,
    RequirementDeletedPayload, RequirementStatus, ResumeTagRequest, TagPausedPayload, TagSummary,
    UpdateRequirementRequest, UpdateStatusRequest,
};
pub use response::{ApiResponse, ErrorResponse};
pub use secret::{RegisterSecretRequest, SecretListItem};
pub use shell::{
    CheckToolInstalledRequest, CheckToolInstalledResponse, DeepgramSpeechToTextConfig,
    OpenAISpeechToTextConfig, OpenExternalRequest, OpenFileRequest, OpenFolderWithRequest,
    ShowItemInFolderRequest, SpeechToTextConfig, SpeechToTextProvider, SpeechToTextResult,
    ToolType,
};
pub use skill::{
    AddExternalPathRequest, BuiltinAutoSkillResponse, DeleteSkillRequest, ExportSkillRequest,
    ExternalSkillSourceResponse, ImportSkillRequest, ImportSkillResponse, MaterializeSkillsRequest,
    MaterializeSkillsResponse, MaterializedSkillRef, NamedPathResponse, ReadAssistantRuleRequest,
    ReadBuiltinResourceRequest, ReadSkillInfoRequest, ReadSkillInfoResponse,
    RemoveExternalPathRequest, ScanForSkillsRequest, ScanForSkillsResponse, ScannedSkillResponse,
    SetSkillTagsRequest, SkillListItemResponse, SkillPathsResponse, SkillSourceResponse,
    WriteAssistantRuleRequest,
};
pub use system::{
    ClientPreferencesResponse, SystemSettingsResponse, UpdateClientPreferencesRequest,
    UpdateSettingsRequest,
};
pub use team_mcp::{
    BrowserMcpConfig, ComputerMcpConfig, GatewayMcpConfig, GuideMcpConfig, KnowledgeMcpConfig,
    OpenMcpConfig, RequirementMcpConfig,
};
pub use terminal::{
    CreateTerminalRequest, TerminalExitEvent, TerminalInputRequest, TerminalOutputEvent,
    TerminalRemovedPayload, TerminalResizeRequest, TerminalSessionResponse, UpdateTerminalRequest,
};
pub use webhook::{
    CreateWebhookRequest, TagBinding, TagBindings, TagSetting, UpdateWebhookRequest,
    UpsertTagSettingRequest, Webhook, WebhookPlatform,
};
pub use websocket::WebSocketMessage;

#[cfg(test)]
mod public_contract_tests {
    use super::{AgentErrorResolution, AgentErrorResolutionKind, AgentErrorResolutionTarget};

    #[test]
    fn error_resolution_types_are_exported_from_crate_root() {
        let resolution = AgentErrorResolution::new(
            AgentErrorResolutionKind::Retry,
            Some(AgentErrorResolutionTarget::Feedback),
        );

        assert_eq!(resolution.kind, AgentErrorResolutionKind::Retry);
        assert_eq!(
            resolution.target,
            Some(AgentErrorResolutionTarget::Feedback)
        );
    }
}
