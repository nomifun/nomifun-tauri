//! Backend implementation of the agent-side `CronSink` trait, delegating to
//! `CronService`. Built per-conversation by the agent factory so the in-process
//! nomi agent can schedule / list / delete its own recurring prompts. Mirrors
//! `nomifun_requirement::RequirementServiceSink`.

use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use nomifun_ai_agent::{CronJobSummary, CronSink};
use nomifun_api_types::{CreateCronJobRequest, CronScheduleDto, ListCronJobsQuery};
use nomifun_common::{ConversationId, UserId};

use crate::service::CronService;

/// Process-wide handle to the single `CronService`, set once at startup. The
/// agent factory builds per-conversation cron sinks long after startup (when a
/// conversation runs), but `CronService` is created late (it depends on the
/// agent/conversation machinery the factory feeds), so a late-bound singleton
/// is the clean way to bridge the two without threading a handle through every
/// service layer. Set exactly once via [`set_process_cron_service`].
static CRON_SERVICE: OnceLock<Arc<CronService>> = OnceLock::new();

/// Register the process `CronService` so the agent's native cron tools can reach
/// it. Call once at startup, right after the service is constructed.
pub fn set_process_cron_service(service: Arc<CronService>) {
    let _ = CRON_SERVICE.set(service);
}

/// Build a conversation-bound [`CronSink`] over the process `CronService`, or an
/// [`UnavailableCronSink`] if it has not been registered yet (only possible
/// before startup finishes — never during a live conversation).
pub fn cron_sink_for(user_id: String, conversation_id: String) -> Arc<dyn CronSink> {
    match (
        CRON_SERVICE.get(),
        UserId::parse(user_id),
        ConversationId::parse(conversation_id),
    ) {
        (Some(service), Ok(user_id), Ok(conversation_id)) => CronServiceSink::into_arc(
            service.clone(),
            user_id.into_string(),
            conversation_id.into_string(),
        ),
        _ => Arc::new(UnavailableCronSink),
    }
}

/// Fallback sink used only if the process `CronService` is not yet registered.
/// Every operation reports the not-ready state instead of panicking.
pub struct UnavailableCronSink;

#[async_trait]
impl CronSink for UnavailableCronSink {
    async fn create(&self, _name: &str, _cron: &str, _prompt: &str) -> Result<String, String> {
        Err("cron service is not available yet".to_string())
    }
    async fn list(&self) -> Result<Vec<CronJobSummary>, String> {
        Err("cron service is not available yet".to_string())
    }
    async fn delete(&self, _job_id: &str) -> Result<(), String> {
        Err("cron service is not available yet".to_string())
    }
}

/// `CronSink` bound to one (nomi) conversation.
pub struct CronServiceSink {
    service: Arc<CronService>,
    user_id: String,
    /// The agent's canonical conversation entity ID.
    conversation_id: String,
}

impl CronServiceSink {
    /// Build the sink as a trait object ready to inject into the agent factory.
    fn into_arc(
        service: Arc<CronService>,
        user_id: String,
        conversation_id: String,
    ) -> Arc<dyn CronSink> {
        Arc::new(Self {
            service,
            user_id,
            conversation_id,
        })
    }

}

#[async_trait]
impl CronSink for CronServiceSink {
    async fn create(&self, name: &str, cron_expr: &str, prompt: &str) -> Result<String, String> {
        // Bound to the agent's own conversation: agent_type "nomi" +
        // execution_mode Existing makes the job re-run this conversation's nomi
        // agent (model resolved from the conversation at run time, so no
        // agent_config needed). Validated by CronService::add_job.
        let req = CreateCronJobRequest {
            name: name.to_string(),
            description: None,
            schedule: CronScheduleDto::Cron {
                expr: cron_expr.to_string(),
                tz: None,
                description: None,
            },
            prompt: Some(prompt.to_string()),
            message: None,
            conversation_id: Some(self.conversation_id.clone()),
            conversation_title: None,
            agent_type: "nomi".to_string(),
            created_by: "agent".to_string(),
            execution_mode: None, // -> Existing
            agent_config: None,
        };
        let job = self
            .service
            .add_job(&self.user_id, req)
            .await
            .map_err(|e| e.to_string())?;
        Ok(job.cron_job_id.to_string())
    }

    async fn list(&self) -> Result<Vec<CronJobSummary>, String> {
        let jobs = self
            .service
            .list_jobs(&self.user_id, &ListCronJobsQuery {
                conversation_id: Some(self.conversation_id.clone()),
            })
            .await
            .map_err(|e| e.to_string())?;
        Ok(jobs
            .into_iter()
            .map(|j| CronJobSummary {
                id: j.cron_job_id.to_string(),
                name: j.name,
                schedule: j
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("{:?}", j.schedule)),
                enabled: j.enabled,
            })
            .collect())
    }

    async fn delete(&self, job_id: &str) -> Result<(), String> {
        self.service
            .remove_job(&self.user_id, job_id)
            .await
            .map_err(|e| e.to_string())
    }
}
