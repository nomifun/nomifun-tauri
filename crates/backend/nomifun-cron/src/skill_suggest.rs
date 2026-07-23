use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nomifun_common::now_ms;
use nomifun_db::IConversationRepository;
use nomifun_realtime::UserEventSink;
use tokio::fs;
use tokio::time::sleep;
use tracing::{debug, warn};

use crate::artifacts::{build_skill_suggest_artifact, emit_artifact};
use crate::error::CronError;
use crate::prompt::SKILL_SUGGEST_FILENAME;
use crate::skill_file::{content_hash, validate_skill_content};

const RETRY_DELAYS_MS: [u64; 3] = [1000, 2000, 3000];

#[derive(Clone)]
pub struct SkillSuggestDetector {
    user_events: Arc<dyn UserEventSink>,
    conversation_repo: Arc<dyn IConversationRepository>,
    last_hash_by_job: Arc<Mutex<HashMap<String, String>>>,
}

impl SkillSuggestDetector {
    pub fn new(
        user_events: Arc<dyn UserEventSink>,
        conversation_repo: Arc<dyn IConversationRepository>,
        _data_dir: PathBuf,
    ) -> Self {
        Self {
            user_events,
            conversation_repo,
            last_hash_by_job: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn schedule_check(
        &self,
        owner_id: String,
        conversation_id: String,
        job_id: String,
        workspace: String,
    ) {
        let detector = self.clone();
        tokio::spawn(async move {
            detector
                .check_with_retry(&owner_id, &conversation_id, &job_id, &workspace)
                .await;
        });
    }

    async fn check_with_retry(
        &self,
        owner_id: &str,
        conversation_id: &str,
        job_id: &str,
        workspace: &str,
    ) {
        for delay_ms in RETRY_DELAYS_MS {
            sleep(Duration::from_millis(delay_ms)).await;
            match self
                .check_and_emit(owner_id, conversation_id, job_id, workspace)
                .await
            {
                Ok(true) => return,
                Ok(false) => continue,
                Err(err) => {
                    warn!(
                        conversation_id,
                        job_id,
                        error = %err,
                        "Failed checking SKILL_SUGGEST.md"
                    );
                }
            }
        }
    }

    async fn check_and_emit(
        &self,
        owner_id: &str,
        conversation_id: &str,
        job_id: &str,
        workspace: &str,
    ) -> Result<bool, CronError> {
        if workspace.trim().is_empty() {
            return Ok(false);
        }

        let file_path = Path::new(workspace).join(SKILL_SUGGEST_FILENAME);
        let content = match fs::read_to_string(&file_path).await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => {
                return Err(CronError::InvalidSkillContent(err.to_string()));
            }
        };

        if content.trim().is_empty() {
            return Ok(false);
        }

        let validated = match validate_skill_content(&content) {
            Ok(validated) => validated,
            Err(_) => return Ok(false),
        };

        let hash = content_hash(&content);
        if self.last_hash(job_id).as_deref() == Some(hash.as_str()) {
            return Ok(true);
        }

        self.set_last_hash(job_id, hash);
        self.emit(
            owner_id,
            conversation_id,
            job_id,
            &validated.name,
            &validated.description,
            &content,
        )
        .await;
        Ok(true)
    }

    async fn emit(
        &self,
        owner_id: &str,
        conversation_id: &str,
        job_id: &str,
        name: &str,
        description: &str,
        skill_content: &str,
    ) {
        self.persist_and_emit(
            owner_id,
            conversation_id,
            job_id,
            name,
            description,
            skill_content,
        )
            .await;
    }

    async fn persist_and_emit(
        &self,
        owner_id: &str,
        conversation_id: &str,
        job_id: &str,
        name: &str,
        description: &str,
        skill_content: &str,
    ) {
        let row = match build_skill_suggest_artifact(
            conversation_id,
            job_id,
            name,
            description,
            skill_content,
            now_ms(),
        ) {
            Ok(row) => row,
            Err(err) => {
                warn!(conversation_id, job_id, error = %err, "Refusing invalid cron skill suggestion artifact");
                return;
            }
        };

        let row = match self.conversation_repo.upsert_artifact(&row).await {
            Ok(row) => row,
            Err(err) => {
                warn!(
                    conversation_id,
                    job_id,
                    error = %err,
                    "Failed persisting cron skill suggestion artifact"
                );
                return;
            }
        };

        if let Err(err) = emit_artifact(self.user_events.as_ref(), owner_id, &row) {
            warn!(
                conversation_id,
                job_id,
                error = %err,
                "Failed emitting cron skill suggestion artifact"
            );
            return;
        }
        debug!(conversation_id, job_id, owner_id, "Emitted cron skill suggestion artifact");
    }

    fn last_hash(&self, job_id: &str) -> Option<String> {
        self.last_hash_by_job
            .lock()
            .ok()
            .and_then(|hashes| hashes.get(job_id).cloned())
    }

    fn set_last_hash(&self, job_id: &str, hash: String) {
        if let Ok(mut hashes) = self.last_hash_by_job.lock() {
            hashes.insert(job_id.to_owned(), hash);
        }
    }

    fn clear_last_hash(&self, job_id: &str) {
        if let Ok(mut hashes) = self.last_hash_by_job.lock() {
            hashes.remove(job_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::models::{ConversationRow, CronJobRow};
    use nomifun_db::{
        ICronRepository, SqliteConversationRepository, SqliteCronRepository, SqlitePool, init_database_memory,
    };
    use tempfile::tempdir;
    use tokio::sync::broadcast;

    const JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
    const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const CONVERSATION_ID_2: &str = "0190f5fe-7c00-7a00-8000-000000000002";

    struct TestUserEventBus {
        sender: broadcast::Sender<nomifun_api_types::WebSocketMessage<serde_json::Value>>,
    }

    impl TestUserEventBus {
        fn new(capacity: usize) -> Self {
            let (sender, _) = broadcast::channel(capacity);
            Self { sender }
        }

        fn subscribe(
            &self,
        ) -> broadcast::Receiver<nomifun_api_types::WebSocketMessage<serde_json::Value>> {
            self.sender.subscribe()
        }
    }

    impl UserEventSink for TestUserEventBus {
        fn send_to_user(
            &self,
            _user_id: &str,
            event: nomifun_api_types::WebSocketMessage<serde_json::Value>,
        ) {
            let _ = self.sender.send(event);
        }
    }

    fn make_conversation(id: &str, installation_owner: &str) -> ConversationRow {
        ConversationRow {
            id: 0,
            conversation_id: id.to_owned(),
            user_id: installation_owner.to_owned(),
            name: "Cron Conversation".into(),
            r#type: "acp".into(),
            extra: "{}".into(),
            delegation_policy: "automatic".into(),
            execution_model_pool: None,
            decision_policy: "automatic".into(),
            execution_template_id: None,
            model: None,
            status: Some("finished".into()),
            source: Some("nomifun".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            cron_job_id: None,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }

    /// Seed a minimal cron job and return its local SQLite identity.
    async fn seed_cron_job(pool: &SqlitePool, installation_owner: &str) -> String {
        let repo = SqliteCronRepository::new(pool.clone());
        let cron_job_id = nomifun_common::CronJobId::new().into_string();
        repo.insert(&CronJobRow {
            id: 0,
            cron_job_id: cron_job_id.clone(),
            user_id: installation_owner.to_owned(),
            name: "Test Cron".into(),
            enabled: true,
            schedule_kind: "every".into(),
            schedule_value: "60000".into(),
            schedule_tz: None,
            schedule_description: None,
            payload_message: "ping".into(),
            execution_mode: "new_conversation".into(),
            agent_config: None,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            conversation_id: None,
            conversation_title: None,
            agent_type: "acp".into(),
            created_by: "user".into(),
            skill_content: None,
            description: None,
            created_at: now_ms(),
            updated_at: now_ms(),
            next_run_at: None,
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
        })
        .await
        .unwrap();
        cron_job_id
    }

    #[tokio::test]
    async fn emits_skill_suggest_when_file_is_valid() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::write(
            workspace.join(SKILL_SUGGEST_FILENAME),
            "---\nname: daily-report\ndescription: Daily report\n---\n\nCheck sources.\n",
        )
        .await
        .unwrap();

        let db = init_database_memory().await.unwrap();
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        let repo: Arc<dyn IConversationRepository> = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let job_id = seed_cron_job(db.pool(), &installation_owner).await;
        let conversation_id = CONVERSATION_ID;
        repo.create(&make_conversation(conversation_id, &installation_owner)).await.unwrap();

        let bus = Arc::new(TestUserEventBus::new(16));
        let detector = SkillSuggestDetector::new(bus.clone(), repo.clone(), temp.path().to_path_buf());
        let mut rx = bus.subscribe();

        let emitted = detector
            .check_and_emit(
                &installation_owner,
                conversation_id,
                &job_id,
                &workspace.to_string_lossy(),
            )
            .await
            .unwrap();

        assert!(emitted);
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.name, "conversation.artifact");
        assert!(msg.data.get("id").is_none());
        assert_eq!(msg.data["kind"], "skill_suggest");
        assert_eq!(msg.data["status"], "pending");
        assert_eq!(msg.data["payload"]["cron_job_id"], job_id);
        assert_eq!(msg.data["payload"]["name"], "daily-report");

        let rows = repo.list_artifacts(conversation_id).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            msg.data["conversation_artifact_id"].as_str(),
            Some(rows[0].conversation_artifact_id.as_str())
        );
        assert_eq!(rows[0].kind, "skill_suggest");
        assert_eq!(rows[0].status, "pending");
        assert!(rows[0].payload.contains("\"skillContent\""));
    }

    #[tokio::test]
    async fn suppresses_duplicate_skill_suggest_content() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::write(
            workspace.join(SKILL_SUGGEST_FILENAME),
            "---\nname: daily-report\ndescription: Daily report\n---\n\nCheck sources.\n",
        )
        .await
        .unwrap();

        let db = init_database_memory().await.unwrap();
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        let repo: Arc<dyn IConversationRepository> = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let job_id = seed_cron_job(db.pool(), &installation_owner).await;
        repo.create(&make_conversation(CONVERSATION_ID, &installation_owner)).await.unwrap();
        repo.create(&make_conversation(CONVERSATION_ID_2, &installation_owner)).await.unwrap();

        let bus = Arc::new(TestUserEventBus::new(16));
        let detector = SkillSuggestDetector::new(bus.clone(), repo, temp.path().to_path_buf());
        let mut rx = bus.subscribe();

        assert!(
            detector
                .check_and_emit(
                    &installation_owner,
                    CONVERSATION_ID,
                    &job_id,
                    &workspace.to_string_lossy(),
                )
                .await
                .unwrap()
        );
        assert!(rx.try_recv().is_ok());

        assert!(
            detector
                .check_and_emit(
                    &installation_owner,
                    CONVERSATION_ID_2,
                    &job_id,
                    &workspace.to_string_lossy(),
                )
                .await
                .unwrap()
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn suppresses_skill_suggest_when_saved_skill_exists() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::write(
            workspace.join(SKILL_SUGGEST_FILENAME),
            "---\nname: daily-report\ndescription: Daily report\n---\n\nCheck sources.\n",
        )
        .await
        .unwrap();
        let skill_dir = temp.path().join(format!("cron/skills/{JOB_ID}"));
        tokio::fs::create_dir_all(&skill_dir).await.unwrap();
        tokio::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: saved-skill\ndescription: Saved skill\n---\n\nDo the task.\n",
        )
        .await
        .unwrap();

        let db = init_database_memory().await.unwrap();
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        let repo: Arc<dyn IConversationRepository> = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        repo.create(&make_conversation(CONVERSATION_ID, &installation_owner)).await.unwrap();

        let bus = Arc::new(TestUserEventBus::new(16));
        let detector = SkillSuggestDetector::new(bus.clone(), repo, temp.path().to_path_buf());
        let mut rx = bus.subscribe();

        let emitted = detector
            .check_and_emit(
                &installation_owner,
                CONVERSATION_ID,
                JOB_ID,
                &workspace.to_string_lossy(),
            )
            .await
            .unwrap();

        assert!(emitted);
        assert!(rx.try_recv().is_err());
    }
}
