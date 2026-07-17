//! All HTTP request/response DTOs shared across the API surface.
mod acp;
mod acp_prompt_hook;
mod agent_build_extra;
mod agent_discovery;
mod agent_error;
mod agent_execution;
mod agent_execution_template;
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
mod managed_model;
mod mcp;
pub mod dispatch_target;
pub mod model_capability;
pub mod model_catalog;
pub mod model_task;
mod office;
mod provider;
mod preset;
mod remote_agent;
mod requirement;
mod response;
mod serde_util;
mod secret;
mod shell;
mod skill;
mod system;
mod mcp_bridge;
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
pub use agent_execution::{
    AddExecutionStepsRequest, AdoptExecutionStepOutputRequest, AdjustAgentExecutionRequest, AgentExecution,
    AgentExecutionChangedEvent, AgentExecutionDetail, AgentExecutionEvent,
    AgentExecutionEventsQuery, AnswerExecutionDecisionRequest, ConfigureExecutionStepRequest,
    CreateAgentExecutionRequest, ExecutionAttempt,
    ExecutionModelPool, ExecutionModelRef, ExecutionParticipant, ExecutionStep,
    ExecutionStepDependency, ExecutionStepProfile, JudgeAggregation, LoopStopPolicy,
    ParticipantCapability, ParticipantConstraints, PlannedExecution, PlannedExecutionStep,
    ReassignExecutionStepRequest, RenameAgentExecutionRequest, ReplanAgentExecutionRequest,
    RetryExecutionStepRequest, SteerExecutionStepRequest, StepControlPolicy,
    UpdateExecutionStepRequest, VerificationPolicy, VersionedAgentExecutionCommand,
};
pub use agent_execution_template::{
    AgentExecutionTemplate, AgentExecutionTemplateDetail,
    AgentExecutionTemplateParticipant, AgentExecutionTemplateParticipantInput,
    CreateAgentExecutionTemplateRequest, CreateExecutionFromTemplateRequest,
    UpdateAgentExecutionTemplateRequest,
};
pub use exposure::{ExposureClamp, ExposureMode, SAFE_PUBLIC_SERVICE_TOOLS, exposure_clamp};
pub use preset::{
    AgentPreference, CreatePresetRequest, CreatePresetTagRequest, ImportPresetsRequest,
    ImportPresetsResult, KnowledgeBaseBinding, ModelPreference, PresetImportError,
    PresetKnowledgePolicy, PresetOverrides, PresetResponse, PresetSource, PresetTagDimension,
    PresetTagResponse, PresetTarget, ResolvePresetRequest, ResolvedPresetSnapshot,
    SetPresetStateRequest, SkillBinding, UpdatePresetRequest, UpdatePresetTagRequest,
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
    CronJobRemovedPayload, CronJobResponse, CronJobRunResponse, CronJobStateDto, CronScheduleDto,
    HasSkillResponse, ListCronJobsQuery, RunNowResponse,
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
pub use managed_model::{
    ManagedModel, ManagedModelHealthBatchResult, ManagedModelHealthErrorKind,
    ManagedModelHealthResult, ManagedModelHealthStatus, ManagedModelServiceAvailability,
    ManagedModelServiceStatus, SetManagedModelEnabledRequest,
    SetManagedModelServiceEnabledRequest,
};
pub use mcp::{
    BatchImportMcpServersRequest, CreateMcpServerRequest, DetectedMcpServerEntry,
    DetectedMcpServerResponse, ImportMcpServerRequest, McpAuthMethod, McpConnectionTestErrorCode,
    McpConnectionTestResult, McpServerResponse, McpToolResponse, McpTransport,
    OAuthCheckStatusRequest, OAuthLoginRequest, OAuthLoginResponse, OAuthLogoutRequest,
    OAuthStatusResponse, TestMcpConnectionRequest, UpdateMcpServerRequest,
};
pub use model_capability::{infer_generation_capabilities, infer_model_modalities};
pub use dispatch_target::{resolve_dispatch_target, DispatchTarget, RequestShape};
pub use model_catalog::{resolve_models, CatalogModelRef, ResolveModelsRequest, ResolveModelsResponse};
pub use model_task::{
    derive_tasks_and_traits, ModelProfile, ModelProfileKeyRequest, ModelProfileUpsertRequest,
    ModelTask, ModelTrait, ProfileSource,
};
pub use office::{
    is_preview_capability, CellCoord, CellRange, ConversionResultDto, ConversionTarget, DetectStarOfficeRequest,
    DocumentConversionRequest, DocumentConversionResponse, ExcelSheetData, ExcelSheetImage,
    ExcelWorkbookData, GetSnapshotContentRequest, ListSnapshotsRequest, PptJsonData, PptSlideData,
    PreviewHistoryTargetDto, PreviewSnapshotInfoDto, PreviewState, PreviewStatusEvent,
    PreviewUrlResponse, SaveSnapshotRequest, SnapshotContentResponse, StarOfficeDetectResponse,
    StartPreviewRequest, StopPreviewRequest, PREVIEW_CAPABILITY_BYTES, PREVIEW_CAPABILITY_HEX_LEN,
};
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
    MaterializeSkillsResponse, MaterializedSkillRef, NamedPathResponse, ReadPresetRuleRequest,
    ReadBuiltinResourceRequest, ReadSkillInfoRequest, ReadSkillInfoResponse,
    RemoveExternalPathRequest, ScanForSkillsRequest, ScanForSkillsResponse, ScannedSkillResponse,
    SetSkillTagsRequest, SkillListItemResponse, SkillMarketItemResponse, SkillMarketSyncRequest,
    SkillMarketSyncResponse, SkillPathsResponse, SkillSourceResponse, WritePresetRuleRequest,
};
pub use system::{
    ClientPreferencesResponse, SystemSettingsResponse, UpdateClientPreferencesRequest,
    UpdateSettingsRequest,
};
pub use mcp_bridge::{
    BrowserMcpConfig, ComputerMcpConfig, GATEWAY_CALL_TOOL_OPERATION,
    GATEWAY_CAPABILITY_DOMAIN, GATEWAY_CREATE_CONVERSATION_TOOL,
    GATEWAY_LIST_TOOLS_OPERATION,
    GatewayCapabilityClaims, GatewayCapabilityScope, GatewayMcpChildConfig,
    GatewayMcpConfig,
    KNOWLEDGE_CAPABILITY_DOMAIN, KNOWLEDGE_READ_TOOL, KNOWLEDGE_SEARCH_TOOL,
    KNOWLEDGE_WRITE_TOOL, KnowledgeCapabilityClaims, KnowledgeCapabilityScope,
    KnowledgeMcpChildConfig, KnowledgeMcpConfig, OpenMcpConfig,
    REQUIREMENT_CAPABILITY_DOMAIN, REQUIREMENT_COMPLETE_TOOL,
    REQUIREMENT_UPDATE_STATUS_TOOL, RequirementCapabilityClaims,
    RequirementCapabilityScope, RequirementMcpChildConfig, RequirementMcpConfig,
    ScopedMcpChildBootstrap, ScopedMcpChildConfig,
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
