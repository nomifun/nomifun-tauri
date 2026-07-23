use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CronJobRow {
    pub id: i64,
    pub cron_job_id: String,
    /// Immutable aggregate owner. Runtime code must never infer or default it
    /// from an optional Conversation.
    pub user_id: String,
    pub name: String,
    pub enabled: bool,
    pub schedule_kind: String,
    pub schedule_value: String,
    pub schedule_tz: Option<String>,
    pub schedule_description: Option<String>,
    pub payload_message: String,
    pub execution_mode: String,
    /// JSON: serialized `CronAgentConfig`.
    pub agent_config: Option<String>,
    /// Preset lineage and immutable resolved launch configuration.
    pub preset_id: Option<String>,
    pub preset_revision: Option<i64>,
    pub preset_snapshot: Option<String>,
    /// Target conversation; NULL for a new_conversation job before first fire.
    /// This is a business-ID logical reference.
    pub conversation_id: Option<String>,
    pub conversation_title: Option<String>,
    pub agent_type: String,
    pub created_by: String,
    pub skill_content: Option<String>,
    pub description: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub next_run_at: Option<TimestampMs>,
    pub last_run_at: Option<TimestampMs>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    pub run_count: i64,
    pub retry_count: i64,
    pub max_retries: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_job_row_serialization_roundtrip() {
        let row = CronJobRow {
            id: 42,
            cron_job_id: nomifun_common::CronJobId::new().into_string(),
            user_id: nomifun_common::UserId::new().into_string(),
            name: "Daily report".into(),
            enabled: true,
            schedule_kind: "cron".into(),
            schedule_value: "0 0 9 * * *".into(),
            schedule_tz: Some("Asia/Shanghai".into()),
            schedule_description: Some("Every day at 9am".into()),
            payload_message: "Generate daily report".into(),
            execution_mode: "new_conversation".into(),
            agent_config: Some(r#"{"backend":"openai","name":"OpenAI"}"#.into()),
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            conversation_id: Some(nomifun_common::ConversationId::new().into_string()),
            conversation_title: Some("Reports".into()),
            agent_type: "openai".into(),
            created_by: "user".into(),
            skill_content: Some("---\nname: test\n---\nDo something".into()),
            description: Some("A test cron job".into()),
            created_at: 1000,
            updated_at: 2000,
            next_run_at: Some(3000),
            last_run_at: Some(1500),
            last_status: Some("ok".into()),
            last_error: None,
            run_count: 5,
            retry_count: 0,
            max_retries: 3,
        };
        let json = serde_json::to_string(&row).expect("serialize");
        let restored: CronJobRow = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.id, row.id);
        assert_eq!(restored.name, row.name);
        assert!(restored.enabled);
        assert_eq!(restored.schedule_kind, "cron");
        assert_eq!(restored.run_count, 5);
    }

    #[test]
    fn cron_job_row_optional_fields_default_to_none() {
        let row = CronJobRow {
            id: 7,
            cron_job_id: nomifun_common::CronJobId::new().into_string(),
            user_id: nomifun_common::UserId::new().into_string(),
            name: "Minimal".into(),
            enabled: true,
            schedule_kind: "every".into(),
            schedule_value: "60000".into(),
            schedule_tz: None,
            schedule_description: None,
            payload_message: "ping".into(),
            execution_mode: "existing".into(),
            agent_config: None,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            conversation_id: Some(nomifun_common::ConversationId::new().into_string()),
            conversation_title: None,
            agent_type: "acp".into(),
            created_by: "agent".into(),
            skill_content: None,
            description: None,
            created_at: 100,
            updated_at: 100,
            next_run_at: None,
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
        };
        assert!(row.schedule_tz.is_none());
        assert!(row.agent_config.is_none());
        assert!(row.skill_content.is_none());
        assert!(row.next_run_at.is_none());
        assert!(row.last_status.is_none());
    }
}
