//! Per-session persistence consumer driven by domain events.
//!
//! Subscribes to `mpsc::Receiver<AcpSessionEvent>` (not the UI broadcast)
//! and writes CLI-observed state to `acp_session.session_config.runtime`.
//!
//! The consumer listens to `Observed*` events (mode, model, config,
//! context_usage). The `session_config.runtime` columns record what the
//! session last had —i.e. what resume needs to restore —which is by
//! definition observation-shaped, not intent-shaped. Desired events are
//! kept inside the aggregate for reconcile/UI broadcast only; they are
//! intentionally not persisted so that an invalid user pick (which the
//! CLI rejects) does not leave stale desired values in the DB.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use nomifun_common::AppError;
use nomifun_db::{IAcpSessionRepository, SaveRuntimeStateParams};
use tokio::sync::{RwLock, mpsc, watch};
use tokio::time::sleep_until;
use tracing::{debug, warn};

use crate::manager::acp::agent_event_tracker::AcpSessionEvent;
use crate::session::{ConfigKey, ConfigValue, ModeId, ModelId, PersistedSessionState};

const DEBOUNCE_WINDOW: Duration = Duration::from_millis(500);

/// Global service that loads and persists ACP per-session runtime
/// state on behalf of the conversation route. One instance per
/// process, held by `AppServices`.
pub struct AcpSessionSyncService {
    repo: Arc<dyn IAcpSessionRepository>,
    active: RwLock<HashMap<String, AcpSessionPersistenceBarrier>>,
}

/// Cloneable, idempotent shutdown barrier for one exact ACP persistence
/// consumer.
///
/// Agent process exit alone is not a persistence boundary: `SessionAssigned`
/// bypasses the debounce and an `Observed*` update may still be inside its
/// 500-ms window. Reset callers clear `acp_session` only after runtime teardown,
/// so `AcpAgentManager::kill_and_wait` must also wait for this barrier. Once it
/// resolves successfully, the old consumer has completed every DB future it
/// had already entered, flushed its pending update, and dropped its receiver;
/// no late event from that runtime can repopulate the cleared session.
#[derive(Clone)]
pub struct AcpSessionPersistenceBarrier {
    shutdown_tx: watch::Sender<bool>,
    completion_rx: watch::Receiver<Option<Result<(), String>>>,
}

impl std::fmt::Debug for AcpSessionPersistenceBarrier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcpSessionPersistenceBarrier")
            .field("shutdown_requested", &*self.shutdown_tx.borrow())
            .field("completed", &self.completion_rx.borrow().is_some())
            .finish()
    }
}

impl AcpSessionPersistenceBarrier {
    /// Request an orderly stop without waiting. Safe to call repeatedly and
    /// from `Drop`; every clone observes the same completion state.
    pub fn request_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Stop the consumer and wait until it can no longer write persisted ACP
    /// session state. Concurrent waiters join the same exact consumer.
    pub async fn shutdown_and_wait(&self) -> Result<(), AppError> {
        self.request_shutdown();
        let mut completion_rx = self.completion_rx.clone();
        loop {
            if let Some(result) = completion_rx.borrow().clone() {
                return result.map_err(AppError::Internal);
            }
            completion_rx.changed().await.map_err(|_| {
                AppError::Internal(
                    "ACP session persistence consumer exited without publishing completion"
                        .to_owned(),
                )
            })?;
        }
    }
}

impl AcpSessionSyncService {
    pub fn new(repo: Arc<dyn IAcpSessionRepository>) -> Arc<Self> {
        Arc::new(Self {
            repo,
            active: RwLock::new(HashMap::new()),
        })
    }

    /// Read the persisted per-session state for `conversation_id`.
    pub async fn load_persisted(&self, conversation_id: &str) -> Option<nomifun_db::PersistedSessionState> {
        match self.repo.load_runtime_state(conversation_id).await {
            Ok(state) => state,
            Err(err) => {
                warn!(
                    conversation_id,
                    error = %err,
                    "AcpSessionSyncService::load_persisted failed"
                );
                None
            }
        }
    }

    /// Read the decoded per-session runtime state, mapped into the
    /// aggregate's value-object shape. Returns `None` when the row does
    /// not exist or the JSON payload is empty. Errors are logged and
    /// swallowed so the caller can proceed with a fresh session.
    pub async fn load_snapshot_state(&self, conversation_id: &str) -> Option<PersistedSessionState> {
        let row = match self.repo.load_runtime_state(conversation_id).await {
            Ok(Some(row)) => row,
            Ok(None) => return None,
            Err(err) => {
                warn!(
                    conversation_id,
                    error = %err,
                    "load_snapshot_state: repository failed; skipping preload"
                );
                return None;
            }
        };

        let mut state = PersistedSessionState {
            current_mode_id: row.current_mode_id.map(ModeId::new),
            current_model_id: row.current_model_id.map(ModelId::new),
            ..Default::default()
        };
        if let Some(raw) = row.config_selections_json
            && let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&raw)
        {
            state.config_selections = map
                .into_iter()
                .map(|(k, v)| (ConfigKey::new(k), ConfigValue::new(v)))
                .collect();
        }
        if let Some(raw) = row.context_usage_json
            && let Ok(usage) = serde_json::from_str(&raw)
        {
            state.context_usage = Some(usage);
        }
        Some(state)
    }

    /// Read the persisted CLI-assigned session id, if any.
    /// Used by the factory on resume paths to seed the aggregate before
    /// the first prompt.
    pub async fn load_session_id(&self, conversation_id: &str) -> Option<String> {
        match self.repo.get(conversation_id).await {
            Ok(Some(row)) => row.acp_session_id,
            Ok(None) => None,
            Err(err) => {
                warn!(
                    conversation_id,
                    error = %err,
                    "load_session_id: repository failed"
                );
                None
            }
        }
    }

    /// Take ownership of the manager's domain event receiver and spawn the
    /// per-conversation persistence consumer.
    ///
    /// The returned barrier must be installed on the exact manager that owns
    /// `domain_rx`; its result-bearing teardown joins persistence with process
    /// teardown. Replacing an older consumer first waits for that consumer to
    /// quiesce, so two generations never write the same `acp_session` row.
    pub async fn attach(
        &self,
        conversation_id: String,
        domain_rx: mpsc::Receiver<AcpSessionEvent>,
    ) -> Result<AcpSessionPersistenceBarrier, AppError> {
        let mut active = self.active.write().await;
        if let Some(previous) = active.remove(&conversation_id) {
            previous.shutdown_and_wait().await?;
        }

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (completion_tx, completion_rx) = watch::channel(None);
        let barrier = AcpSessionPersistenceBarrier {
            shutdown_tx,
            completion_rx,
        };
        let repo = self.repo.clone();
        let cid = conversation_id.clone();
        tokio::spawn(async move {
            let outcome = AssertUnwindSafe(domain_event_consumer_until_shutdown(
                cid,
                domain_rx,
                repo,
                shutdown_rx,
            ))
            .catch_unwind()
            .await;
            let completion = match outcome {
                Ok(()) => Ok(()),
                Err(_) => Err("ACP session persistence consumer panicked".to_owned()),
            };
            let _ = completion_tx.send(Some(completion));
        });

        active.insert(conversation_id, barrier.clone());
        Ok(barrier)
    }
}

/// Pending DB update fields accumulated from domain events.
#[derive(Debug, Clone, Default)]
struct PendingUpdate {
    current_mode_id: Option<Option<String>>,
    current_model_id: Option<Option<String>>,
    config_selections_json: Option<Option<String>>,
    context_usage_json: Option<Option<String>>,
}

impl PendingUpdate {
    fn is_empty(&self) -> bool {
        self.current_mode_id.is_none()
            && self.current_model_id.is_none()
            && self.config_selections_json.is_none()
            && self.context_usage_json.is_none()
    }

    fn as_save_params(&self) -> SaveRuntimeStateParams<'_> {
        SaveRuntimeStateParams {
            current_mode_id: self.current_mode_id.as_ref().map(Option::as_deref),
            current_model_id: self.current_model_id.as_ref().map(Option::as_deref),
            config_selections_json: self.config_selections_json.as_ref().map(Option::as_deref),
            context_usage_json: self.context_usage_json.as_ref().map(Option::as_deref),
        }
    }

    fn merge_from_domain_event(&mut self, event: &AcpSessionEvent) -> bool {
        match event {
            AcpSessionEvent::ObservedModeSynced { mode } => {
                self.current_mode_id = Some(Some(mode.as_str().to_owned()));
                true
            }
            AcpSessionEvent::ObservedModelSynced { model } => {
                self.current_model_id = Some(Some(model.as_str().to_owned()));
                true
            }
            AcpSessionEvent::ObservedConfigSynced { selections } => {
                let string_map: HashMap<String, String> = selections
                    .iter()
                    .map(|(k, v)| (k.as_str().to_owned(), v.as_str().to_owned()))
                    .collect();
                let json = serde_json::to_string(&string_map).unwrap_or_default();
                self.config_selections_json = Some(Some(json));
                true
            }
            AcpSessionEvent::ObservedContextUsageChanged { usage_json } => {
                self.context_usage_json = Some(Some(usage_json.clone()));
                true
            }
            _ => false,
        }
    }
}

/// Consume domain events from the session aggregate and persist user
/// intent changes with a debounce window.
///
/// `SessionAssigned` bypasses the debounce: the CLI-issued id must be
/// written immediately so the next turn can take the resume path even
/// if the process crashes before any other event fires.
#[cfg(test)]
async fn domain_event_consumer(
    conversation_id: String,
    rx: mpsc::Receiver<AcpSessionEvent>,
    repo: Arc<dyn IAcpSessionRepository>,
) {
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    domain_event_consumer_until_shutdown(conversation_id, rx, repo, shutdown_rx).await;
}

async fn domain_event_consumer_until_shutdown(
    conversation_id: String,
    mut rx: mpsc::Receiver<AcpSessionEvent>,
    repo: Arc<dyn IAcpSessionRepository>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut pending = PendingUpdate::default();
    let mut flush_at: Option<Instant> = None;

    loop {
        if *shutdown_rx.borrow() {
            flush(&repo, &conversation_id, &mut pending).await;
            debug!(conversation_id, "session-sync persistence barrier completed");
            return;
        }

        let recv = match flush_at {
            Some(deadline) => {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.changed() => {
                        flush(&repo, &conversation_id, &mut pending).await;
                        debug!(conversation_id, "session-sync persistence barrier completed");
                        return;
                    }
                    maybe_event = rx.recv() => maybe_event,
                    () = sleep_until(deadline.into()) => {
                        flush(&repo, &conversation_id, &mut pending).await;
                        flush_at = None;
                        continue;
                    }
                }
            }
            None => {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.changed() => {
                        flush(&repo, &conversation_id, &mut pending).await;
                        debug!(conversation_id, "session-sync persistence barrier completed");
                        return;
                    }
                    maybe_event = rx.recv() => maybe_event,
                }
            }
        };

        match recv {
            Some(event) => {
                if let AcpSessionEvent::SessionAssigned { session_id } = &event {
                    match repo
                        .update_session_id(&conversation_id, session_id.as_str())
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => debug!(
                            conversation_id,
                            "session-sync: acp_session row missing; session_id not written"
                        ),
                        Err(err) => warn!(
                            conversation_id,
                            error = %err,
                            "session-sync: update_session_id failed"
                        ),
                    }
                    continue;
                }
                if pending.merge_from_domain_event(&event) {
                    flush_at = Some(Instant::now() + DEBOUNCE_WINDOW);
                }
            }
            None => {
                flush(&repo, &conversation_id, &mut pending).await;
                debug!(conversation_id, "session-sync domain consumer exiting");
                return;
            }
        }
    }
}

async fn flush(repo: &Arc<dyn IAcpSessionRepository>, conversation_id: &str, pending: &mut PendingUpdate) {
    if pending.is_empty() {
        return;
    }
    let params = pending.as_save_params();
    match repo.save_runtime_state(conversation_id, &params).await {
        Ok(true) => {}
        Ok(false) => {
            debug!(conversation_id, "session sync: acp_session row missing; update dropped");
        }
        Err(err) => {
            warn!(
                conversation_id,
                error = %err,
                "session sync: save_runtime_state failed"
            );
        }
    }
    *pending = PendingUpdate::default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionId;
    use nomifun_db::models::AcpSessionRow;
    use nomifun_db::{
        CreateAcpSessionParams, DbError, PersistedSessionState as DbPersistedSessionState,
        SqliteAcpSessionRepository, init_database_memory,
    };
    use tokio::sync::Semaphore;
    use tokio::time::{sleep, timeout};

    #[derive(Default)]
    struct BlockingSessionState {
        session_id: Option<String>,
        context_usage_json: Option<String>,
    }

    struct BlockingSessionRepo {
        state: std::sync::Mutex<BlockingSessionState>,
        update_started: Semaphore,
        update_release: Semaphore,
        save_started: Semaphore,
        save_release: Semaphore,
    }

    impl BlockingSessionRepo {
        fn new() -> Self {
            Self {
                state: std::sync::Mutex::new(BlockingSessionState::default()),
                update_started: Semaphore::new(0),
                update_release: Semaphore::new(0),
                save_started: Semaphore::new(0),
                save_release: Semaphore::new(0),
            }
        }

        fn session_id(&self) -> Option<String> {
            self.state.lock().unwrap().session_id.clone()
        }

        fn context_usage_json(&self) -> Option<String> {
            self.state.lock().unwrap().context_usage_json.clone()
        }
    }

    #[async_trait::async_trait]
    impl IAcpSessionRepository for BlockingSessionRepo {
        async fn get(&self, conversation_id: &str) -> Result<Option<AcpSessionRow>, DbError> {
            let state = self.state.lock().unwrap();
            Ok(Some(AcpSessionRow {
                id: 1,
                conversation_id: conversation_id.to_owned(),
                agent_backend: "test".to_owned(),
                agent_source: "test".to_owned(),
                agent_id: "test".to_owned(),
                acp_session_id: state.session_id.clone(),
                session_status: "idle".to_owned(),
                session_config: "{}".to_owned(),
                last_active_at: None,
                suspended_at: None,
            }))
        }

        async fn create(
            &self,
            _params: &CreateAcpSessionParams<'_>,
        ) -> Result<AcpSessionRow, DbError> {
            Err(DbError::Init(
                "BlockingSessionRepo does not create rows".to_owned(),
            ))
        }

        async fn update_session_id(
            &self,
            _conversation_id: &str,
            session_id: &str,
        ) -> Result<bool, DbError> {
            let session_id = session_id.to_owned();
            self.update_started.add_permits(1);
            self.update_release
                .acquire()
                .await
                .map_err(|_| DbError::Init("update release semaphore closed".to_owned()))?
                .forget();
            self.state.lock().unwrap().session_id = Some(session_id);
            Ok(true)
        }

        async fn clear_session_id(&self, _conversation_id: &str) -> Result<bool, DbError> {
            let mut state = self.state.lock().unwrap();
            state.session_id = None;
            state.context_usage_json = None;
            Ok(true)
        }

        async fn delete(&self, _conversation_id: &str) -> Result<bool, DbError> {
            Ok(true)
        }

        async fn load_runtime_state(
            &self,
            _conversation_id: &str,
        ) -> Result<Option<DbPersistedSessionState>, DbError> {
            Ok(Some(DbPersistedSessionState {
                context_usage_json: self.state.lock().unwrap().context_usage_json.clone(),
                ..Default::default()
            }))
        }

        async fn save_runtime_state(
            &self,
            _conversation_id: &str,
            params: &SaveRuntimeStateParams<'_>,
        ) -> Result<bool, DbError> {
            let context_usage_json = params
                .context_usage_json
                .flatten()
                .map(ToOwned::to_owned);
            self.save_started.add_permits(1);
            self.save_release
                .acquire()
                .await
                .map_err(|_| DbError::Init("save release semaphore closed".to_owned()))?
                .forget();
            self.state.lock().unwrap().context_usage_json = context_usage_json;
            Ok(true)
        }
    }

    async fn setup() -> (Arc<AcpSessionSyncService>, Arc<dyn IAcpSessionRepository>) {
        let db = init_database_memory().await.unwrap();
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        // Seed the logical conversation target before create() inserts the
        // session row. The dynamically resolved installation owner satisfies
        // the test's owner-scoping checks. The session linkage is resolved
        // logically and cleanup is repository-owned.
        sqlx::query(
            "INSERT INTO conversations (conversation_id, user_id, name, type, status, created_at, updated_at) \
             VALUES ('0190f5fe-7c00-7a00-8000-000000000001', ?, 'c', 'acp', 'pending', 1, 1)",
        )
        .bind(&installation_owner)
        .execute(db.pool())
        .await
        .unwrap();
        let repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(db.pool().clone()));
        repo.create(&CreateAcpSessionParams {
            conversation_id: "0190f5fe-7c00-7a00-8000-000000000001",
            agent_backend: "claude",
            agent_source: "builtin",
            agent_id: "0190f5fe-7c00-7a00-8000-000000000101",
        })
        .await
        .unwrap();
        let svc = AcpSessionSyncService::new(repo.clone());
        (svc, repo)
    }

    #[tokio::test]
    async fn load_persisted_round_trips() {
        let (svc, repo) = setup().await;
        repo.save_runtime_state(
            "0190f5fe-7c00-7a00-8000-000000000001",
            &SaveRuntimeStateParams {
                current_mode_id: Some(Some("plan")),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let state = svc.load_persisted("0190f5fe-7c00-7a00-8000-000000000001").await.unwrap();
        assert_eq!(state.current_mode_id.as_deref(), Some("plan"));
    }

    /// Domain event ObservedModeSynced flushes after debounce.
    #[tokio::test(flavor = "current_thread")]
    async fn domain_event_flushes_after_debounce() {
        let (_svc, repo) = setup().await;
        let (tx, rx) = mpsc::channel(64);

        let cid = "0190f5fe-7c00-7a00-8000-000000000001".to_owned();
        tokio::spawn(domain_event_consumer(cid, rx, repo.clone()));

        tx.send(AcpSessionEvent::ObservedModeSynced { mode: "plan".into() })
            .await
            .unwrap();

        sleep(Duration::from_millis(200)).await;
        let state = repo.load_runtime_state(&"0190f5fe-7c00-7a00-8000-000000000001").await.unwrap().unwrap();
        assert!(state.current_mode_id.is_none(), "debounce not yet elapsed");

        sleep(Duration::from_millis(400)).await;
        let state = repo.load_runtime_state(&"0190f5fe-7c00-7a00-8000-000000000001").await.unwrap().unwrap();
        assert_eq!(state.current_mode_id.as_deref(), Some("plan"));
    }

    /// Burst of events coalesces into a single write.
    #[tokio::test(flavor = "current_thread")]
    async fn coalesces_burst_into_single_write() {
        let (_svc, repo) = setup().await;
        let (tx, rx) = mpsc::channel(64);

        let cid = "0190f5fe-7c00-7a00-8000-000000000001".to_owned();
        tokio::spawn(domain_event_consumer(cid, rx, repo.clone()));

        for label in ["code", "plan", "ask"] {
            tx.send(AcpSessionEvent::ObservedModeSynced { mode: label.into() })
                .await
                .unwrap();
            sleep(Duration::from_millis(100)).await;
        }
        sleep(Duration::from_millis(600)).await;

        let state = repo.load_runtime_state(&"0190f5fe-7c00-7a00-8000-000000000001").await.unwrap().unwrap();
        assert_eq!(state.current_mode_id.as_deref(), Some("ask"));
    }

    /// Unrelated events (SessionOpened) never trigger a DB write.
    #[tokio::test(flavor = "current_thread")]
    async fn unrelated_events_are_ignored() {
        let (_svc, repo) = setup().await;
        let (tx, rx) = mpsc::channel(64);

        let cid = "0190f5fe-7c00-7a00-8000-000000000001".to_owned();
        tokio::spawn(domain_event_consumer(cid, rx, repo.clone()));

        tx.send(AcpSessionEvent::SessionOpened).await.unwrap();
        sleep(Duration::from_millis(600)).await;

        let state = repo.load_runtime_state(&"0190f5fe-7c00-7a00-8000-000000000001").await.unwrap().unwrap();
        assert!(state.current_mode_id.is_none());
    }

    /// When the sender drops, consumer flushes and exits.
    #[tokio::test(flavor = "current_thread")]
    async fn flushes_and_exits_on_channel_close() {
        let (_svc, repo) = setup().await;
        let (tx, rx) = mpsc::channel(64);

        let cid = "0190f5fe-7c00-7a00-8000-000000000001".to_owned();
        tokio::spawn(domain_event_consumer(cid, rx, repo.clone()));

        tx.send(AcpSessionEvent::ObservedModeSynced { mode: "plan".into() })
            .await
            .unwrap();
        drop(tx);
        sleep(Duration::from_millis(50)).await;

        let state = repo.load_runtime_state(&"0190f5fe-7c00-7a00-8000-000000000001").await.unwrap().unwrap();
        assert_eq!(
            state.current_mode_id.as_deref(),
            Some("plan"),
            "pending update must flush on channel close"
        );
    }

    /// ObservedModelSynced drives current_model_id persistence (mirrors
    /// ObservedModeSynced). DesiredModelChanged must NOT write the DB —    /// the DB stores what the CLI actually ran with, not what the user
    /// asked for (an invalid desired value the CLI rejects must not
    /// leave a stale row).
    #[tokio::test(flavor = "current_thread")]
    async fn observed_model_synced_persists_current_model_id() {
        let (_svc, repo) = setup().await;
        let (tx, rx) = mpsc::channel(64);

        let cid = "0190f5fe-7c00-7a00-8000-000000000001".to_owned();
        tokio::spawn(domain_event_consumer(cid, rx, repo.clone()));

        tx.send(AcpSessionEvent::ObservedModelSynced {
            model: "claude-opus-4".into(),
        })
        .await
        .unwrap();

        sleep(Duration::from_millis(700)).await;
        let state = repo.load_runtime_state(&"0190f5fe-7c00-7a00-8000-000000000001").await.unwrap().unwrap();
        assert_eq!(state.current_model_id.as_deref(), Some("claude-opus-4"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn desired_model_changed_does_not_persist() {
        let (_svc, repo) = setup().await;
        let (tx, rx) = mpsc::channel(64);

        let cid = "0190f5fe-7c00-7a00-8000-000000000001".to_owned();
        tokio::spawn(domain_event_consumer(cid, rx, repo.clone()));

        tx.send(AcpSessionEvent::DesiredModelChanged {
            model: "claude-opus-4".into(),
        })
        .await
        .unwrap();

        sleep(Duration::from_millis(700)).await;
        let state = repo.load_runtime_state(&"0190f5fe-7c00-7a00-8000-000000000001").await.unwrap().unwrap();
        assert!(
            state.current_model_id.is_none(),
            "DesiredModelChanged is reconcile/UI-only; persistence only follows Observed*",
        );
    }

    /// ObservedContextUsageChanged persists the usage blob so resume
    /// paths can preload `advertised.context_usage` before the CLI's
    /// first notification arrives.
    #[tokio::test(flavor = "current_thread")]
    async fn observed_context_usage_persists() {
        let (_svc, repo) = setup().await;
        let (tx, rx) = mpsc::channel(64);

        let cid = "0190f5fe-7c00-7a00-8000-000000000001".to_owned();
        tokio::spawn(domain_event_consumer(cid, rx, repo.clone()));

        tx.send(AcpSessionEvent::ObservedContextUsageChanged {
            usage_json: r#"{"used":12345,"size":200000}"#.to_owned(),
        })
        .await
        .unwrap();

        sleep(Duration::from_millis(700)).await;
        let state = repo.load_runtime_state(&"0190f5fe-7c00-7a00-8000-000000000001").await.unwrap().unwrap();
        let raw = state.context_usage_json.expect("usage must be persisted");
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["used"], 12345);
        assert_eq!(parsed["size"], 200000);
    }

    /// SessionAssigned must write the session_id immediately, bypassing
    /// the debounce window used for runtime-state updates.
    #[tokio::test(flavor = "current_thread")]
    async fn session_assigned_writes_session_id_immediately() {
        let (_svc, repo) = setup().await;
        let (tx, rx) = mpsc::channel(64);

        let cid = "0190f5fe-7c00-7a00-8000-000000000001".to_owned();
        tokio::spawn(domain_event_consumer(cid, rx, repo.clone()));

        tx.send(AcpSessionEvent::SessionAssigned {
            session_id: SessionId::new("sess-42"),
        })
        .await
        .unwrap();

        // Well under the debounce window —the event must have already
        // been written.
        sleep(Duration::from_millis(100)).await;
        let row = repo.get(&"0190f5fe-7c00-7a00-8000-000000000001").await.unwrap().unwrap();
        assert_eq!(row.acp_session_id.as_deref(), Some("sess-42"));
    }

    #[tokio::test]
    async fn shutdown_barrier_waits_for_inflight_session_assignment_before_clear() {
        let repo = Arc::new(BlockingSessionRepo::new());
        let service = AcpSessionSyncService::new(repo.clone());
        let (tx, rx) = mpsc::channel(8);
        let barrier = service
            .attach("conversation".to_owned(), rx)
            .await
            .expect("attach persistence consumer");

        tx.send(AcpSessionEvent::SessionAssigned {
            session_id: SessionId::new("old-session"),
        })
        .await
        .unwrap();
        repo.update_started
            .acquire()
            .await
            .expect("update started semaphore")
            .forget();

        let waiter_barrier = barrier.clone();
        let mut waiter = tokio::spawn(async move { waiter_barrier.shutdown_and_wait().await });
        assert!(
            timeout(Duration::from_millis(50), &mut waiter)
                .await
                .is_err(),
            "shutdown must not return while update_session_id is still in flight"
        );

        repo.update_release.add_permits(1);
        timeout(Duration::from_secs(1), &mut waiter)
            .await
            .expect("persistence shutdown bound")
            .expect("persistence waiter task")
            .expect("persistence shutdown result");
        assert_eq!(repo.session_id().as_deref(), Some("old-session"));

        repo.clear_session_id("conversation").await.unwrap();
        assert!(
            tx.send(AcpSessionEvent::SessionAssigned {
                session_id: SessionId::new("late-session"),
            })
            .await
            .is_err(),
            "consumer receiver must be closed after the barrier"
        );
        sleep(Duration::from_millis(50)).await;
        assert_eq!(
            repo.session_id(),
            None,
            "no old runtime event may repopulate the cleared session id"
        );
    }

    #[tokio::test]
    async fn shutdown_barrier_joins_debounced_context_usage_write_before_clear() {
        let repo = Arc::new(BlockingSessionRepo::new());
        let service = AcpSessionSyncService::new(repo.clone());
        let (tx, rx) = mpsc::channel(8);
        let barrier = service
            .attach("conversation".to_owned(), rx)
            .await
            .expect("attach persistence consumer");

        tx.send(AcpSessionEvent::ObservedContextUsageChanged {
            usage_json: r#"{"used":17,"size":100}"#.to_owned(),
        })
        .await
        .unwrap();
        repo.save_started
            .acquire()
            .await
            .expect("debounced save started semaphore")
            .forget();

        let waiter_barrier = barrier.clone();
        let mut waiter = tokio::spawn(async move { waiter_barrier.shutdown_and_wait().await });
        assert!(
            timeout(Duration::from_millis(50), &mut waiter)
                .await
                .is_err(),
            "shutdown must join a debounced save that already entered the repository"
        );

        repo.save_release.add_permits(1);
        timeout(Duration::from_secs(1), &mut waiter)
            .await
            .expect("persistence shutdown bound")
            .expect("persistence waiter task")
            .expect("persistence shutdown result");
        assert!(repo.context_usage_json().is_some());

        repo.clear_session_id("conversation").await.unwrap();
        sleep(DEBOUNCE_WINDOW + Duration::from_millis(100)).await;
        assert_eq!(
            repo.context_usage_json(),
            None,
            "the old debounce timer must not repopulate cleared usage"
        );
    }
}
