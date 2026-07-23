use std::sync::Arc;

use async_trait::async_trait;
use nomifun_ai_agent::RequirementSink;
use nomifun_api_types::{CreateRequirementRequest, RequirementStatus};
use nomifun_common::RequirementCreator;

use crate::service::RequirementService;

/// Backend implementation of the agent-side `RequirementSink` trait, delegating
/// to `RequirementService`. Injected into the nomi engine via the agent factory.
pub struct RequirementServiceSink {
    service: Arc<RequirementService>,
}

impl RequirementServiceSink {
    /// Build the sink as a trait object ready to inject into the agent factory.
    pub fn into_arc(service: Arc<RequirementService>) -> Arc<dyn RequirementSink> {
        Arc::new(Self { service })
    }

    /// Build the same sink as a [`RequirementCreator`] trait object for the
    /// opt-in IM → requirement pipeline (channel inbound → tracked requirement).
    pub fn creator_arc(service: Arc<RequirementService>) -> Arc<dyn RequirementCreator> {
        Arc::new(Self { service })
    }
}

#[async_trait]
impl RequirementCreator for RequirementServiceSink {
    async fn create_from_message(
        &self,
        title: &str,
        content: &str,
        tag: &str,
        created_by: &str,
    ) -> Result<String, String> {
        let req = CreateRequirementRequest {
            title: title.to_string(),
            content: content.to_string(),
            tag: tag.to_string(),
            order_key: None,
            status: None, // None → Pending → wakes AutoWork
            created_by: Some(created_by.to_string()),
            attachments: Vec::new(),
        };
        self.service
            .create(req)
            .await
            .map(|r| r.requirement_id)
            .map_err(|e| e.to_string())
    }
}

#[async_trait]
impl RequirementSink for RequirementServiceSink {
    async fn complete(&self, requirement_id: &str, note: &str) -> Result<(), String> {
        self.service
            .complete(requirement_id, Some(note.to_string()))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn update_status(&self, requirement_id: &str, status: &str, note: Option<&str>) -> Result<(), String> {
        let parsed = match status {
            "in_progress" => RequirementStatus::InProgress,
            "done" => RequirementStatus::Done,
            "failed" => RequirementStatus::Failed,
            other => return Err(format!("invalid status '{other}'")),
        };
        self.service
            .set_status(requirement_id, parsed, note.map(|s| s.to_string()))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}
