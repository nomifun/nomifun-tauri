//! SQLite database layer: init, migrations, repository traits, and implementations.
pub mod backup_bundle;
mod database;
mod error;
mod id_schema_contract;
pub mod models;
mod repository;

pub use database::{
    Database, init_database, init_database_memory, init_database_memory_with_owner,
    open_database_for_backup,
};
pub use error::DbError;
pub use id_schema_contract::validate_id_schema_contract;
pub use models::{
    AgentExecutionAttemptDetailRow, AgentExecutionAttemptRow, AgentExecutionDetailRows,
    AgentExecutionEventRow, AgentExecutionParticipantRow, AgentExecutionRow,
    AgentExecutionStepDependencyRow, AgentExecutionStepDetailRow, AgentExecutionStepRow,
    AgentExecutionTemplateDetailRows, AgentExecutionTemplateParticipantRow,
    AgentExecutionTemplateRow,
    AgentMetadataRow, ConnectorCredentialRow,
    ConversationArtifactRow,
    CreateKnowledgeTagParams, CreationTaskRow, CronJobRunRow, KnowledgeBaseRow, KnowledgeBindingRow,
    KnowledgeTagRow, SkillTagRow, TagSettingRow, TerminalSessionRow, UpdateAgentHandshakeParams,
    UpdateKnowledgeTagParams,
    UpsertAgentMetadataParams, UpsertSkillTagParams, WebhookRow,
    WorkshopAssetRow, WorkshopCanvasRow, ConversationExecutionLinkRow,
};
pub use models::{
    CreatePresetTagParams, PresetAgentPreferenceRow, PresetExampleRow,
    PresetKnowledgeBaseRow, PresetKnowledgePolicyRow, PresetLocalizationRow,
    PresetModelPreferenceRow, PresetRecord, PresetRow, PresetSkillBindingRow,
    PresetTagBindingRow, PresetTagRow, PresetUserStateRow, PresetWriteParams,
    UpdatePresetTagParams, UpsertPresetStateParams,
};
pub use models::{ModelProfileRow, UpsertModelProfileParams};
pub use repository::channel::UpdatePluginStatusParams;
pub use repository::conversation::{
    ConversationFilters, ConversationMessageProjection, ConversationRowUpdate, MessageRowUpdate,
    MessageSearchRow, SortOrder, TurnArtifactMessageCommit,
};
pub use repository::cron::{CRON_RUN_HISTORY_LIMIT, UpdateCronJobParams};
pub use repository::mcp_server::{CreateMcpServerParams, UpdateMcpServerParams};
pub use repository::oauth_token::UpsertOAuthTokenParams;
pub use repository::provider::{CreateProviderParams, UpdateProviderParams};
pub use repository::remote_agent::{CreateRemoteAgentParams, UpdateRemoteAgentParams};
pub use repository::{
    AdoptAgentExecutionStepOutputParams, AgentExecutionLeaseToken,
    AppendAgentExecutionStepsFromAttemptParams, AppendAgentExecutionStepsFromAttemptResult,
    AppendAgentExecutionStepsParams,
    AttemptConversationEffectParams, CreateAgentExecutionAttemptParams,
    CreateAgentExecutionParams, IAgentExecutionRepository,
    CreateAgentExecutionTemplateParams, IAgentExecutionTemplateRepository,
    NewAgentExecutionEvent, NewAgentExecutionParticipant, NewAgentExecutionStep,
    NewAgentExecutionStepDependency, ReconcileAgentExecutionPlanParams,
    NewAgentExecutionTemplateParticipant, UpdateAgentExecutionTemplateParams,
    LoopRepeatResetParams,
    RetryAgentExecutionStep, SettleAgentExecutionAttemptParams, UpdateAgentExecutionParams,
    CreateAcpSessionParams, CreateTerminalParams, IAcpSessionRepository,
    IAgentMetadataRepository, IAttachmentRepository, IChannelRepository,
    IClientPreferenceRepository, ICompanionTokenRepository, IConnectorCredentialRepository,
    IConversationRepository, ICronRepository, IIdmmInterventionRepository, IKnowledgeRepository,
    IMcpServerRepository, IModelProfileRepository, IOAuthTokenRepository, IProviderRepository,
    IRemoteAgentRepository, IRequirementRepository, ISettingsRepository, ISkillTagRepository,
    ITagSettingRepository, ITerminalRepository, IUserRepository, IWebhookRepository,
    ListRequirementsParams, PER_TARGET_CAP, PER_USER_ACTIVITY_CAP, PersistedSessionState,
    SaveRuntimeStateParams,
    SqliteAcpSessionRepository, SqliteAgentMetadataRepository, SqliteAttachmentRepository,
    SqliteAgentExecutionRepository,
    SqliteAgentExecutionTemplateRepository,
    SqliteChannelRepository, SqliteClientPreferenceRepository, SqliteCompanionTokenRepository,
    SqliteConnectorCredentialRepository, SqliteConversationRepository, SqliteCronRepository,
    SqliteIdmmInterventionRepository, SqliteKnowledgeRepository, SqliteMcpServerRepository,
    SqliteModelProfileRepository, SqliteOAuthTokenRepository, SqliteProviderRepository,
    SqliteRemoteAgentRepository, SqliteRequirementRepository, SqliteSettingsRepository,
    SqliteSkillTagRepository, SqliteTagSettingRepository, SqliteTerminalRepository,
    SqliteUserRepository, SqliteWebhookRepository, TTL_MS,
};
pub use repository::{
    IPresetRepository, IPresetStateRepository, IPresetTagRepository,
    SqlitePresetRepository, SqlitePresetStateRepository, SqlitePresetTagRepository,
};
// 创意工坊 (Creative Workshop) + 生成引擎 (creation) repository traits + sqlite impls + params.
pub use repository::{
    AssetSort, CreateCreationTaskParams, ICreationTaskRepository, IWorkshopRepository, ListAssetsParams,
    ListCreationTasksParams, SqliteCreationTaskRepository, SqliteWorkshopRepository,
    UpdateAssetParams, UpdateCreationTaskParams,
};

// Re-export sqlx (and its pool type) for downstream crates that run ad-hoc
// queries against the pool without declaring their own sqlx dependency
// (e.g. nomifun-app's bootstrap relocation path rewrite).
pub use sqlx;
pub use sqlx::SqlitePool;

/// Resolve the canonical owner user ID for this dataset.
///
/// The identity is stored in the database rather than reconstructed from a
/// global constant. A missing, duplicated, non-canonical, or dangling owner is
/// a database invariant violation and fails closed.
pub async fn installation_owner_id(pool: &SqlitePool) -> Result<String, DbError> {
    let identities: Vec<(String, String)> =
        sqlx::query_as("SELECT singleton_key, owner_user_id FROM installation_identity")
            .fetch_all(pool)
            .await
            .map_err(DbError::Query)?;
    let [(key, owner_user_id)] = identities.as_slice() else {
        return Err(DbError::Init(format!(
            "installation identity must contain exactly one owner, found {}",
            identities.len()
        )));
    };
    if key != "installation" {
        return Err(DbError::Init(format!(
            "installation identity contains invalid singleton key {key:?}"
        )));
    }
    nomifun_common::UserId::parse(owner_user_id.clone()).map_err(|error| {
        DbError::Init(format!(
            "installation owner ID is not canonical: {owner_user_id}: {error}"
        ))
    })?;
    let owner_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE user_id = ?")
        .bind(owner_user_id)
        .fetch_one(pool)
        .await
        .map_err(DbError::Query)?;
    if owner_exists != 1 {
        return Err(DbError::Init(format!(
            "installation identity references missing owner user {owner_user_id}"
        )));
    }
    Ok(owner_user_id.clone())
}
