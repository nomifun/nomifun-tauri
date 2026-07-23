use async_trait::async_trait;

use crate::error::DbError;
use crate::models::{AgentExecutionTemplateDetailRows, AgentExecutionTemplateRow};

#[derive(Debug, Clone)]
pub struct NewAgentExecutionTemplateParticipant {
    pub template_participant_id: String,
    pub source_agent_id: String,
    pub preset_id: Option<String>,
    pub preset_revision: Option<i64>,
    pub preset_snapshot: Option<String>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub role: Option<String>,
    pub capability: Option<String>,
    pub constraints: Option<String>,
    pub description: Option<String>,
    pub system_prompt: Option<String>,
    pub enabled_skills: String,
    pub disabled_builtin_skills: String,
    pub sort_order: i64,
}

#[derive(Debug, Clone)]
pub struct CreateAgentExecutionTemplateParams {
    pub name: String,
    pub description: Option<String>,
    /// Authoring and runtime share one ceiling; saved Templates are always
    /// directly executable rather than deferring validation to launch time.
    pub max_parallel: Option<i64>,
    pub work_dir: Option<String>,
    pub context: Option<String>,
    pub participants: Vec<NewAgentExecutionTemplateParticipant>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateAgentExecutionTemplateParams {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub max_parallel: Option<Option<i64>>,
    pub work_dir: Option<Option<String>>,
    pub context: Option<Option<String>>,
    /// Complete replacement of the executable participant set (1..=64).
    pub participants: Option<Vec<NewAgentExecutionTemplateParticipant>>,
}

#[async_trait]
pub trait IAgentExecutionTemplateRepository: Send + Sync {
    async fn create_template(
        &self,
        user_id: &str,
        params: &CreateAgentExecutionTemplateParams,
    ) -> Result<AgentExecutionTemplateDetailRows, DbError>;

    async fn get_template(
        &self,
        user_id: &str,
        template_id: &str,
    ) -> Result<Option<AgentExecutionTemplateDetailRows>, DbError>;

    async fn list_templates(
        &self,
        user_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AgentExecutionTemplateRow>, DbError>;

    async fn update_template(
        &self,
        user_id: &str,
        template_id: &str,
        expected_version: i64,
        params: &UpdateAgentExecutionTemplateParams,
    ) -> Result<AgentExecutionTemplateDetailRows, DbError>;

    async fn delete_template(
        &self,
        user_id: &str,
        template_id: &str,
        expected_version: i64,
    ) -> Result<bool, DbError>;

    /// Narrow owner-independent occupancy read for provider deletion guards.
    /// It exposes only the referencing template identity and display name.
    async fn list_templates_using_provider(
        &self,
        provider_id: &str,
    ) -> Result<Vec<(String, String)>, DbError>;
}
