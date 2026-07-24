use std::sync::Arc;

use nomifun_api_types::{CronJobExecutedEvent, CronJobRemovedPayload, CronJobResponse, WebSocketMessage};
use nomifun_conversation::ConversationService;
use nomifun_common::{ConversationId, UserId};
use nomifun_realtime::UserEventSink;
use serde_json::json;
use tracing::error;

#[derive(Clone)]
pub struct CronEventEmitter {
    user_events: Arc<dyn UserEventSink>,
}

impl CronEventEmitter {
    pub fn new(user_events: Arc<dyn UserEventSink>) -> Self {
        Self { user_events }
    }

    pub fn emit_job_created(&self, owner_id: &str, job: &CronJobResponse) {
        self.emit_to_user(owner_id, "cron.job-created", job);
    }

    pub fn emit_job_updated(&self, owner_id: &str, job: &CronJobResponse) {
        self.emit_to_user(owner_id, "cron.job-updated", job);
    }

    pub fn emit_job_removed(&self, owner_id: &str, cron_job_id: &str) {
        self.emit_to_user(
            owner_id,
            "cron.job-removed",
            &CronJobRemovedPayload {
                cron_job_id: cron_job_id.to_owned(),
            },
        );
    }

    pub fn emit_job_executed(
        &self,
        owner_id: &str,
        cron_job_id: &str,
        status: &str,
        err: Option<&str>,
    ) {
        self.emit_to_user(
            owner_id,
            "cron.job-executed",
            &CronJobExecutedEvent {
                cron_job_id: cron_job_id.to_owned(),
                status: status.to_owned(),
                error: err.map(|s| s.to_owned()),
            },
        );
    }

    pub fn emit_conversation_tips(
        &self,
        owner_id: &str,
        conversation_id: &str,
        content: &str,
        tip_type: &str,
    ) {
        if let Err(error) = ConversationId::try_from(conversation_id) {
            error!(conversation_id, error = %error, "Refusing to emit cron tips for an invalid conversation id");
            return;
        }
        let payload = json!({
            "conversation_id": conversation_id,
            "msg_id": ConversationService::mint_msg_id(),
            "type": "tips",
            "data": {
                "content": content,
                "type": tip_type,
            },
            "hidden": false,
        });
        self.user_events.send_to_user(
            owner_id,
            WebSocketMessage::new("message.stream", payload),
        );
    }

    fn emit_to_user<T: serde::Serialize>(&self, owner_id: &str, event_name: &str, payload: &T) {
        if let Err(error) = UserId::try_from(owner_id) {
            error!(owner_id, event_name, error = %error, "Refusing to emit a cron event for an invalid user id");
            return;
        }
        let value = match serde_json::to_value(payload) {
            Ok(v) => v,
            Err(e) => {
                error!(event_name, error = %e, "Failed to serialize event payload");
                return;
            }
        };
        self.user_events
            .send_to_user(owner_id, WebSocketMessage::new(event_name, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::{
        CronJobExecutedEvent, CronJobMetadataDto, CronJobRemovedPayload, CronJobResponse,
        CronJobStateDto, CronScheduleDto,
    };

    const USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const USER_ID_2: &str = "0190f5fe-7c00-7a00-8000-000000000002";
    const JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
    const JOB_ID_2: &str = "0190f5fe-7c00-7a00-8abc-012345678902";
    const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    struct RecordingUserEvents {
        deliveries: std::sync::Mutex<Vec<(String, WebSocketMessage<serde_json::Value>)>>,
    }

    impl RecordingUserEvents {
        fn new() -> Self {
            Self {
                deliveries: std::sync::Mutex::new(vec![]),
            }
        }

        fn events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
            self.deliveries
                .lock()
                .unwrap()
                .iter()
                .map(|(_, event)| event.clone())
                .collect()
        }

        fn owners(&self) -> Vec<String> {
            self.deliveries
                .lock()
                .unwrap()
                .iter()
                .map(|(owner, _)| owner.clone())
                .collect()
        }
    }

    impl UserEventSink for RecordingUserEvents {
        fn send_to_user(&self, user_id: &str, event: WebSocketMessage<serde_json::Value>) {
            self.deliveries
                .lock()
                .unwrap()
                .push((user_id.to_owned(), event));
        }
    }

    fn make_emitter() -> (CronEventEmitter, Arc<RecordingUserEvents>) {
        let user_events = Arc::new(RecordingUserEvents::new());
        let emitter = CronEventEmitter::new(user_events.clone());
        (emitter, user_events)
    }

    fn sample_response() -> CronJobResponse {
        CronJobResponse {
            cron_job_id: JOB_ID.into(),
            name: "Test Job".into(),
            description: Some("Test description".into()),
            enabled: true,
            schedule: CronScheduleDto::Every {
                every_ms: 60000,
                description: Some("every minute".into()),
            },
            message: "hello".into(),
            execution_mode: "existing".into(),
            metadata: CronJobMetadataDto {
                conversation_id: Some("0190f5fe-7c00-7a00-8abc-012345678901".into()),
                conversation_title: None,
                agent_type: "acp".into(),
                created_by: "user".into(),
                created_at: 1000,
                updated_at: 2000,
                agent_config: None,
            },
            state: CronJobStateDto {
                next_run_at_ms: Some(61000),
                last_run_at_ms: None,
                last_status: None,
                last_error: None,
                run_count: 0,
                retry_count: 0,
                max_retries: 3,
            },
        }
    }

    #[test]
    fn job_created_event_shape() {
        let (emitter, bc) = make_emitter();
        let resp = sample_response();
        emitter.emit_job_created(USER_ID, &resp);

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "cron.job-created");

        let parsed: CronJobResponse = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(parsed.cron_job_id, JOB_ID);
        assert_eq!(parsed.name, "Test Job");
    }

    #[test]
    fn job_updated_event_shape() {
        let (emitter, bc) = make_emitter();
        let resp = sample_response();
        emitter.emit_job_updated(USER_ID, &resp);

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "cron.job-updated");

        let parsed: CronJobResponse = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(parsed.cron_job_id, JOB_ID);
    }

    #[test]
    fn job_removed_event_shape() {
        let (emitter, bc) = make_emitter();
        emitter.emit_job_removed(USER_ID, JOB_ID_2);

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "cron.job-removed");

        let parsed: CronJobRemovedPayload = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(parsed.cron_job_id, JOB_ID_2);
    }

    #[test]
    fn job_executed_success_event() {
        let (emitter, bc) = make_emitter();
        emitter.emit_job_executed(USER_ID, JOB_ID, "ok", None);

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "cron.job-executed");

        let parsed: CronJobExecutedEvent = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(parsed.cron_job_id, JOB_ID);
        assert_eq!(parsed.status, "ok");
        assert!(parsed.error.is_none());
    }

    #[test]
    fn job_executed_error_event() {
        let (emitter, bc) = make_emitter();
        emitter.emit_job_executed(USER_ID, JOB_ID, "error", Some("timeout"));

        let events = bc.events();
        assert_eq!(events.len(), 1);

        let parsed: CronJobExecutedEvent = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(parsed.status, "error");
        assert_eq!(parsed.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn job_executed_skipped_event() {
        let (emitter, bc) = make_emitter();
        emitter.emit_job_executed(USER_ID, JOB_ID, "skipped", None);

        let events = bc.events();
        let parsed: CronJobExecutedEvent = serde_json::from_value(events[0].data.clone()).unwrap();
        assert_eq!(parsed.status, "skipped");
    }

    #[test]
    fn multiple_events_accumulate() {
        let (emitter, bc) = make_emitter();
        let resp = sample_response();
        emitter.emit_job_created(USER_ID, &resp);
        emitter.emit_job_updated(USER_ID, &resp);
        emitter.emit_job_removed(USER_ID, JOB_ID);
        emitter.emit_job_executed(USER_ID, JOB_ID, "ok", None);

        let events = bc.events();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].name, "cron.job-created");
        assert_eq!(events[1].name, "cron.job-updated");
        assert_eq!(events[2].name, "cron.job-removed");
        assert_eq!(events[3].name, "cron.job-executed");
    }

    #[test]
    fn private_cron_events_are_scoped_to_the_explicit_owner() {
        let (emitter, user_events) = make_emitter();
        let resp = sample_response();

        emitter.emit_job_created(USER_ID, &resp);
        emitter.emit_conversation_tips(USER_ID_2, CONVERSATION_ID, "missed", "warning");

        assert_eq!(user_events.owners(), vec![USER_ID, USER_ID_2]);
        let events = user_events.events();
        assert_eq!(events[0].name, "cron.job-created");
        assert_eq!(events[1].name, "message.stream");
        assert_eq!(events[1].data["conversation_id"], CONVERSATION_ID);
    }
}
