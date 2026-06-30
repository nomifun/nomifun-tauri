//! SQLite database layer: init, migrations, repository traits, and implementations.
mod database;
mod error;
pub mod models;
mod repository;

pub use database::{Database, init_database, init_database_memory};
pub use error::DbError;
pub use models::{
    AgentMetadataRow, AssistantOverrideRow, AssistantRow, AssistantTagRow, ConnectorCredentialRow,
    ConversationArtifactRow, CreateAssistantParams, CreateAssistantTagParams,
    CreateKnowledgeTagParams, CronJobRunRow, KnowledgeBaseRow, KnowledgeBindingRow,
    KnowledgeTagRow, SkillTagRow, TagSettingRow, TerminalSessionRow, UpdateAgentHandshakeParams,
    UpdateAssistantParams, UpdateAssistantTagParams, UpdateKnowledgeTagParams,
    UpsertAgentMetadataParams, UpsertOverrideParams, UpsertSkillTagParams, WebhookRow,
};
pub use repository::channel::UpdatePluginStatusParams;
pub use repository::conversation::{
    ConversationFilters, ConversationRowUpdate, MessageRowUpdate, MessageSearchRow, SortOrder,
};
pub use repository::cron::{CRON_RUN_HISTORY_LIMIT, UpdateCronJobParams};
pub use repository::mcp_server::{CreateMcpServerParams, UpdateMcpServerParams};
pub use repository::oauth_token::UpsertOAuthTokenParams;
pub use repository::provider::{CreateProviderParams, UpdateProviderParams};
pub use repository::remote_agent::{CreateRemoteAgentParams, UpdateRemoteAgentParams};
pub use repository::{
    CreateAcpSessionParams, CreateTerminalParams, GLOBAL_CAP, IAcpSessionRepository,
    IAgentMetadataRepository, IAssistantOverrideRepository, IAssistantRepository,
    IAssistantTagRepository, IAttachmentRepository, IChannelRepository,
    IClientPreferenceRepository, ICompanionTokenRepository, IConnectorCredentialRepository,
    IConversationRepository, ICronRepository, IIdmmInterventionRepository, IKnowledgeRepository,
    IMcpServerRepository, IOAuthTokenRepository, IProviderRepository, IRemoteAgentRepository,
    IRequirementRepository, ISettingsRepository, ISkillTagRepository, ITagSettingRepository,
    ITerminalRepository, IUserRepository, IWebhookRepository,
    ListRequirementsParams, PER_TARGET_CAP, PersistedSessionState, SaveRuntimeStateParams,
    SqliteAcpSessionRepository, SqliteAgentMetadataRepository, SqliteAssistantOverrideRepository,
    SqliteAssistantRepository, SqliteAssistantTagRepository, SqliteAttachmentRepository,
    SqliteChannelRepository, SqliteClientPreferenceRepository, SqliteCompanionTokenRepository,
    SqliteConnectorCredentialRepository, SqliteConversationRepository, SqliteCronRepository,
    SqliteIdmmInterventionRepository, SqliteKnowledgeRepository, SqliteMcpServerRepository,
    SqliteOAuthTokenRepository, SqliteProviderRepository, SqliteRemoteAgentRepository,
    SqliteRequirementRepository, SqliteSettingsRepository, SqliteSkillTagRepository,
    SqliteTagSettingRepository, SqliteTerminalRepository,
    SqliteUserRepository, SqliteWebhookRepository, TTL_MS,
};
// Orchestration (智能编排) repository traits + sqlite impls + params.
pub use repository::{
    CreateAssignmentParams, CreateFleetParams, CreateOrchWorkspaceParams, CreateRunParams,
    CreateTaskParams, IFleetRepository, IOrchWorkspaceRepository, IRunRepository, NewFleetMember,
    ReconcileDepRef, ReconcileNewTask, ReconcilePlan, SqliteFleetRepository,
    SqliteOrchWorkspaceRepository, SqliteRunRepository, UpdateFleetParams,
    UpdateOrchWorkspaceParams, UpdateRunParams, UpdateTaskParams,
};

// Re-export sqlx (and its pool type) for downstream crates that run ad-hoc
// queries against the pool without declaring their own sqlx dependency
// (e.g. nomifun-app's bootstrap relocation path rewrite).
pub use sqlx;
pub use sqlx::SqlitePool;
