use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use futures_util::future::BoxFuture;
use nomi_agent::session::SessionManager;
use nomifun_common::{
    AgentKillReason, AgentType, AppError, ConversationStatus, ErrorChain, OnConversationDelete, TimestampMs, now_ms,
};
use tokio::sync::OnceCell;
use tracing::{info, warn};

use crate::agent_task::AgentInstance;
use crate::types::BuildTaskOptions;

/// Factory function that creates an [`AgentInstance`] from build options.
///
/// Async so the factory can do real I/O (spawn a CLI process, negotiate the
/// ACP initialize handshake, etc.) without needing to `block_on` inside the
/// `IWorkerTaskManager` call site. Returning `BoxFuture` keeps the trait
/// object-safe for DI.
pub type AgentFactory =
    Arc<dyn Fn(BuildTaskOptions) -> BoxFuture<'static, Result<AgentInstance, AppError>> + Send + Sync>;

/// Manages the lifecycle of active Agent tasks.
///
/// Each conversation has at most one active task (keyed by conversation ID).
/// The trait is object-safe for dependency injection.
#[async_trait]
pub trait IWorkerTaskManager: Send + Sync {
    /// Get an existing task by conversation ID.
    fn get_task(&self, conversation_id: &str) -> Option<AgentInstance>;

    /// Get an existing task or build a new one if none exists.
    ///
    /// Concurrent callers with the same `conversation_id` block on a shared
    /// [`OnceCell`] so the factory runs at most once per conversation —
    /// avoiding the race where two concurrent HTTP requests (e.g.
    /// `/messages` + `/warmup`) would each spawn their own CLI process and
    /// ACP connection, with one of them leaking.
    async fn get_or_build_task(
        &self,
        conversation_id: &str,
        options: BuildTaskOptions,
    ) -> Result<AgentInstance, AppError>;

    /// Kill and remove a task.
    fn kill(&self, conversation_id: &str, reason: Option<AgentKillReason>) -> Result<(), AppError>;

    /// Kill a task and return a future that resolves when the process has terminated.
    fn kill_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

    /// Kill and remove all active tasks.
    fn clear(&self);

    /// Number of active tasks (useful for diagnostics).
    fn active_count(&self) -> usize;

    /// Collect tasks eligible for idle cleanup.
    ///
    /// Returns conversation IDs of tasks that:
    /// - have `status == Some(Finished)`
    /// - have been idle longer than `idle_threshold_ms`
    fn collect_idle(&self, idle_threshold_ms: TimestampMs) -> Vec<String>;
}

/// Per-conversation slot: an [`OnceCell`] that the first concurrent caller
/// initialises by running the factory, and that every subsequent caller
/// awaits. Failed initialisations leave the cell empty so the next caller
/// may retry; the slot itself is only removed on `kill` / `clear`.
type TaskSlot = Arc<OnceCell<AgentInstance>>;

/// Max crash-evictions within [`RESTART_WINDOW_MS`] before a conversation's
/// respawn is refused. Beyond this the agent is deterministically crash-looping
/// and respawning again just burns a fresh CLI process + ACP handshake to die
/// the same way.
const RESTART_MAX_PER_WINDOW: u32 = 3;
/// Sliding window (ms) over which crash-evictions are counted. A conversation
/// that survives this long without a crash resets its budget, so a single crash
/// deep into a long session never trips the breaker.
const RESTART_WINDOW_MS: i64 = 60_000;
/// Base respawn backoff (ms); doubled per crash within the window (1s, 2s, …)
/// and capped at [`RESTART_MAX_BACKOFF_MS`]. Paces a flapping agent so a
/// zero-delay respawn cannot re-enter the same crashing operation instantly.
const RESTART_BASE_BACKOFF_MS: u64 = 500;
const RESTART_MAX_BACKOFF_MS: u64 = 8_000;

#[derive(Clone, Copy)]
struct RestartRecord {
    /// Crash-evictions inside the current window.
    count: u32,
    /// When the current window began (`now_ms`).
    window_start_ms: i64,
}

/// Crash-loop governor for agent (re)builds.
///
/// A companion ACP agent that repeatedly crashes mid-turn (e.g. a native fault
/// in the Computer/a11y C-FFI, which no Rust error boundary can catch) is
/// evicted with [`AgentKillReason::AgentErrorRecovery`] and lazily respawned on
/// the next drive. With no throttle that respawn is instant, so a deterministic
/// crash re-faults within seconds — the "6-second restart loop" users observe.
///
/// This governor bounds that loop per conversation: it counts crash-evictions
/// within a sliding window, applies exponential backoff before each respawn,
/// and once [`RESTART_MAX_PER_WINDOW`] crashes occur inside [`RESTART_WINDOW_MS`]
/// it refuses to respawn (the caller surfaces a terminal error the UI renders as
/// "paused — crash looping") until the window elapses. Only genuine crashes are
/// counted; deliberate recycles (idle timeout, knowledge-binding rebuild, team
/// rebuild, conversation delete) never consume the budget, so a healthy reopen
/// is never throttled. Modelled on the MCP stdio transport's respawn breaker.
#[derive(Default)]
struct RestartGovernor {
    records: DashMap<String, RestartRecord>,
}

impl RestartGovernor {
    /// Record a crash-eviction for `conversation_id` at `now_ms`; returns the
    /// crash count within the (possibly just-reset) window.
    fn record_crash(&self, conversation_id: &str, now_ms: i64) -> u32 {
        let mut rec = self
            .records
            .entry(conversation_id.to_owned())
            .or_insert(RestartRecord {
                count: 0,
                window_start_ms: now_ms,
            });
        if now_ms - rec.window_start_ms > RESTART_WINDOW_MS {
            rec.count = 1;
            rec.window_start_ms = now_ms;
        } else {
            rec.count += 1;
        }
        rec.count
    }

    /// Decide whether a (re)build for `conversation_id` may proceed at `now_ms`.
    ///
    /// - `Ok(backoff_ms)` — proceed after sleeping `backoff_ms` (0 when there is
    ///   no recent crash history or the window has elapsed).
    /// - `Err(count)` — refuse: the conversation crashed `count` times within
    ///   the window and is crash-looping.
    fn gate(&self, conversation_id: &str, now_ms: i64) -> Result<u64, u32> {
        match self.records.get(conversation_id) {
            Some(rec) if now_ms - rec.window_start_ms <= RESTART_WINDOW_MS => {
                if rec.count >= RESTART_MAX_PER_WINDOW {
                    Err(rec.count)
                } else {
                    let backoff = RESTART_BASE_BACKOFF_MS
                        .saturating_mul(1u64 << rec.count.min(5))
                        .min(RESTART_MAX_BACKOFF_MS);
                    Ok(backoff)
                }
            }
            // No record, or the window has elapsed → unthrottled build.
            _ => Ok(0),
        }
    }

    /// Drop all crash bookkeeping for a conversation (definitive teardown).
    fn forget(&self, conversation_id: &str) {
        self.records.remove(conversation_id);
    }

    fn clear(&self) {
        self.records.clear();
    }
}

/// Default implementation of [`IWorkerTaskManager`] using a concurrent hash map.
pub struct WorkerTaskManagerImpl {
    tasks: DashMap<String, TaskSlot>,
    factory: AgentFactory,
    /// Bounds rapid crash-respawn loops per conversation (see [`RestartGovernor`]).
    governor: RestartGovernor,
}

impl WorkerTaskManagerImpl {
    pub fn new(factory: AgentFactory) -> Self {
        Self {
            tasks: DashMap::new(),
            factory,
            governor: RestartGovernor::default(),
        }
    }

    /// Look up a fully-initialised instance by conversation id.
    fn initialised_instance(&self, conversation_id: &str) -> Option<AgentInstance> {
        self.tasks.get(conversation_id).and_then(|slot| slot.get().cloned())
    }

    /// Feed a kill into the restart governor. Only a crash-recovery eviction
    /// ([`AgentKillReason::AgentErrorRecovery`]) counts against the respawn
    /// budget; every other kill is a deliberate recycle and must not. A
    /// definitive teardown ([`AgentKillReason::ConversationDeleted`]) drops the
    /// bookkeeping so a reused conversation id starts fresh.
    fn note_kill_for_governor(&self, conversation_id: &str, reason: Option<AgentKillReason>) {
        match reason {
            Some(AgentKillReason::AgentErrorRecovery) => {
                let count = self.governor.record_crash(conversation_id, now_ms());
                warn!(
                    conversation_id,
                    crash_count = count,
                    "Recorded agent crash-eviction for the restart governor"
                );
            }
            Some(AgentKillReason::ConversationDeleted) => self.governor.forget(conversation_id),
            _ => {}
        }
    }
}

#[async_trait]
impl IWorkerTaskManager for WorkerTaskManagerImpl {
    fn get_task(&self, conversation_id: &str) -> Option<AgentInstance> {
        self.initialised_instance(conversation_id)
    }

    async fn get_or_build_task(
        &self,
        conversation_id: &str,
        options: BuildTaskOptions,
    ) -> Result<AgentInstance, AppError> {
        // Atomically obtain the per-conversation slot. `DashMap::entry` is
        // synchronous and side-effect-free — only an empty OnceCell is
        // allocated on the miss path, so concurrent callers for the same id
        // all end up holding the same `Arc<OnceCell>`.
        let slot: TaskSlot = self
            .tasks
            .entry(conversation_id.to_owned())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();

        // Fast path: a live instance already exists — hand it back without
        // touching the restart governor (a healthy warm task is never
        // throttled).
        if let Some(instance) = slot.get() {
            return Ok(instance.clone());
        }

        // About to (re)build. Enforce the crash-loop governor so a
        // deterministically-crashing conversation cannot hot-loop respawns.
        match self.governor.gate(conversation_id, now_ms()) {
            Ok(0) => {}
            Ok(backoff_ms) => {
                warn!(
                    conversation_id,
                    backoff_ms, "Backing off before respawning a recently-crashed agent"
                );
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            }
            Err(count) => {
                warn!(
                    conversation_id,
                    crash_count = count,
                    window_ms = RESTART_WINDOW_MS,
                    "Agent is crash-looping; refusing to respawn until the window elapses"
                );
                return Err(AppError::Conflict(format!(
                    "Agent for conversation {conversation_id} crashed {count} times within {}s and \
                     is paused to break a crash loop. Resolve the underlying failure (see the \
                     agent's exit code/signal in the logs), then try again shortly.",
                    RESTART_WINDOW_MS / 1000
                )));
            }
        }

        // `OnceCell::get_or_try_init` serialises concurrent initialisers:
        // the first caller to reach it runs the factory, every other caller
        // awaits the same future and ends up with the same instance. On
        // failure the cell stays empty so a later caller can retry.
        let factory = self.factory.clone();
        let instance = slot.get_or_try_init(|| async move { factory(options).await }).await?;
        Ok(instance.clone())
    }

    fn kill(&self, conversation_id: &str, reason: Option<AgentKillReason>) -> Result<(), AppError> {
        self.note_kill_for_governor(conversation_id, reason);
        if let Some((id, slot)) = self.tasks.remove(conversation_id) {
            info!(conversation_id = %id, ?reason, "Killing agent task");
            if let Some(agent) = slot.get() {
                agent.kill(reason)?;
            }
        }
        Ok(())
    }

    fn kill_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        self.note_kill_for_governor(conversation_id, reason);
        if let Some((id, slot)) = self.tasks.remove(conversation_id) {
            info!(conversation_id = %id, ?reason, "Killing agent task (awaitable)");
            if let Some(agent) = slot.get() {
                return agent.kill_and_wait(reason);
            }
        }
        Box::pin(std::future::ready(()))
    }

    fn clear(&self) {
        self.governor.clear();
        let keys: Vec<String> = self.tasks.iter().map(|r| r.key().clone()).collect();
        for key in keys {
            if let Some((id, slot)) = self.tasks.remove(&key) {
                info!(conversation_id = %id, "Clearing agent task");
                if let Some(agent) = slot.get() {
                    let _ = agent.kill(None);
                }
            }
        }
    }

    fn active_count(&self) -> usize {
        self.tasks.iter().filter(|entry| entry.value().get().is_some()).count()
    }

    fn collect_idle(&self, idle_threshold_ms: TimestampMs) -> Vec<String> {
        let now = now_ms();
        self.tasks
            .iter()
            .filter_map(|entry| {
                let agent = entry.value().get()?;
                // Only ACP agents participate in idle cleanup per API Spec
                (agent.agent_type() == AgentType::Acp
                    && agent.status() == Some(ConversationStatus::Finished)
                    && (now - agent.last_activity_at()) > idle_threshold_ms)
                    .then(|| entry.key().clone())
            })
            .collect()
    }
}

/// Wired up by `nomifun-app` so deleting a conversation tears down its
/// agent process. Without this hook, ACP/nomi/nanobot subprocesses keep
/// streaming events for a `conversation_id` whose DB row is already gone
/// (Sentry ELECTRON-1BD).
#[async_trait]
impl OnConversationDelete for WorkerTaskManagerImpl {
    async fn on_conversation_deleted(&self, conversation_id: i64) {
        // The task manager keys live agents by the String conversation id;
        // bridge the i64 hook key back to that form for `kill`.
        let conversation_id = conversation_id.to_string();
        if let Err(e) = self.kill(&conversation_id, Some(AgentKillReason::ConversationDeleted)) {
            warn!(
                conversation_id,
                error = %ErrorChain(&e),
                "Failed to kill agent task on conversation delete (non-fatal)",
            );
        }
    }
}

/// Conversation-delete hook that removes a conversation's on-disk nomi state:
/// the global `nomi-sessions/*_{id}.json` file (+ index entry) and any legacy
/// id-named temp workspace under `work_dir/conversations`.
///
/// Without this, those files outlive the conversation. The session dir is keyed
/// only by the reusable integer conversation id, so an orphan could later be
/// resumed by a brand-new conversation that reuses the id (e.g. after a DB
/// rebaseline) — the cross-conversation "memory bleed" this guards against,
/// complementing the per-session `owner_token` check in the nomi factory.
/// Best-effort: every failure is logged, never fatal.
pub struct NomiSessionFilesCascade {
    pub data_dir: PathBuf,
    pub work_dir: PathBuf,
}

#[async_trait]
impl OnConversationDelete for NomiSessionFilesCascade {
    async fn on_conversation_deleted(&self, conversation_id: i64) {
        let id = conversation_id.to_string();

        // 1) nomi session transcript file + index entry.
        let mgr = SessionManager::new(self.data_dir.join("nomi-sessions"), 100);
        if let Err(e) = mgr.delete_session(&id) {
            warn!(conversation_id, error = %e, "Failed to delete nomi session file on conversation delete (non-fatal)");
        }

        // 2) legacy auto-provisioned temp workspace(s) named `{label}-temp-{id}`.
        //    Exact suffix match is id-safe (`-temp-3` never matches `-temp-13`).
        //    New token-named managed workspaces are deleted by ConversationService
        //    while it still has the full conversation row.
        let conv_dir = self.work_dir.join("conversations");
        let suffix = format!("-temp-{id}");
        if let Ok(entries) = std::fs::read_dir(&conv_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().ends_with(&suffix) && entry.path().is_dir() {
                    if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                        warn!(conversation_id, error = %e, "Failed to remove temp workspace on conversation delete (non-fatal)");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::{IAgentTask, IMockAgent};
    use crate::protocol::events::AgentStreamEvent;
    use crate::types::SendMessageData;
    use futures_util::FutureExt;
    use nomifun_common::{AgentKillReason, AgentType, ConversationStatus, ProviderWithModel};
    use std::sync::atomic::{AtomicI64, Ordering};
    use tokio::sync::broadcast;

    /// A minimal mock agent for testing task manager logic. Lives behind
    /// the `AgentInstance::Mock` trait-object variant so we don't have to
    /// stand up a real `AcpAgentManager` just to exercise lifecycle
    /// dispatch.
    struct MockAgent {
        agent_type: AgentType,
        conversation_id: String,
        workspace: String,
        status: Option<ConversationStatus>,
        last_activity: AtomicI64,
        event_tx: broadcast::Sender<AgentStreamEvent>,
    }

    impl MockAgent {
        fn new(conversation_id: &str, status: Option<ConversationStatus>) -> Self {
            let (event_tx, _) = broadcast::channel(16);
            Self {
                agent_type: AgentType::Acp,
                conversation_id: conversation_id.to_owned(),
                workspace: "/tmp/test".to_owned(),
                status,
                last_activity: AtomicI64::new(now_ms()),
                event_tx,
            }
        }

        fn with_agent_type(mut self, t: AgentType) -> Self {
            self.agent_type = t;
            self
        }

        fn with_last_activity(mut self, ts: TimestampMs) -> Self {
            self.last_activity = AtomicI64::new(ts);
            self
        }
    }

    #[async_trait::async_trait]
    impl IAgentTask for MockAgent {
        fn agent_type(&self) -> AgentType {
            self.agent_type
        }
        fn conversation_id(&self) -> &str {
            &self.conversation_id
        }
        fn workspace(&self) -> &str {
            &self.workspace
        }
        fn status(&self) -> Option<ConversationStatus> {
            self.status
        }
        fn last_activity_at(&self) -> TimestampMs {
            self.last_activity.load(Ordering::Relaxed)
        }
        fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
            self.event_tx.subscribe()
        }
        async fn send_message(
            &self,
            _data: SendMessageData,
        ) -> Result<(), crate::protocol::send_error::AgentSendError> {
            Ok(())
        }
        async fn cancel(&self) -> Result<(), AppError> {
            Ok(())
        }
        fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
            Ok(())
        }
    }

    impl IMockAgent for MockAgent {}

    fn make_options(conversation_id: &str) -> BuildTaskOptions {
        BuildTaskOptions {
            agent_type: AgentType::Acp,
            workspace: "/tmp/test".into(),
            model: ProviderWithModel {
                provider_id: "p1".into(),
                model: "test".into(),
                use_model: None,
            },
            conversation_id: conversation_id.into(),
            extra: serde_json::Value::Null,
            conversation_created_at: None,
        }
    }

    fn mock_instance(agent: MockAgent) -> AgentInstance {
        AgentInstance::Mock(Arc::new(agent))
    }

    fn make_manager() -> WorkerTaskManagerImpl {
        let factory: AgentFactory = Arc::new(|opts: BuildTaskOptions| {
            async move { Ok(mock_instance(MockAgent::new(&opts.conversation_id, None))) }.boxed()
        });
        WorkerTaskManagerImpl::new(factory)
    }

    /// Two [`AgentInstance`]s point to the same underlying agent iff they
    /// share an `Arc` — check by pointer identity on the inner trait object.
    fn same_mock(a: &AgentInstance, b: &AgentInstance) -> bool {
        match (a, b) {
            (AgentInstance::Mock(x), AgentInstance::Mock(y)) => Arc::ptr_eq(x, y),
            _ => false,
        }
    }

    #[test]
    fn get_task_returns_none_when_empty() {
        let mgr = make_manager();
        assert!(mgr.get_task("nonexistent").is_none());
    }

    #[tokio::test]
    async fn get_or_build_creates_task() {
        let mgr = make_manager();
        let instance = mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        assert_eq!(instance.conversation_id(), "conv-1");
        assert_eq!(mgr.active_count(), 1);
    }

    #[tokio::test]
    async fn get_or_build_returns_existing() {
        let mgr = make_manager();
        let h1 = mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        let h2 = mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        assert!(same_mock(&h1, &h2));
        assert_eq!(mgr.active_count(), 1);
    }

    #[tokio::test]
    async fn get_or_build_is_single_flight_under_concurrency() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_factory = Arc::clone(&calls);
        let factory: AgentFactory = Arc::new(move |opts: BuildTaskOptions| {
            let calls = Arc::clone(&calls_for_factory);
            async move {
                // Simulate a slow build (CLI spawn + initialize handshake).
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(mock_instance(MockAgent::new(&opts.conversation_id, None)))
            }
            .boxed()
        });
        let mgr = Arc::new(WorkerTaskManagerImpl::new(factory));

        // Ten concurrent callers all racing on the same conversation id.
        let mut joins = Vec::new();
        for _ in 0..10 {
            let mgr = Arc::clone(&mgr);
            joins.push(tokio::spawn(async move {
                mgr.get_or_build_task("conv-race", make_options("conv-race")).await
            }));
        }
        let handles: Vec<_> = futures_util::future::join_all(joins)
            .await
            .into_iter()
            .map(|r| r.unwrap().unwrap())
            .collect();

        assert_eq!(calls.load(Ordering::SeqCst), 1, "factory must run only once");
        assert_eq!(mgr.active_count(), 1);
        for h in handles.iter().skip(1) {
            assert!(same_mock(&handles[0], h), "all callers see the same handle");
        }
    }

    #[tokio::test]
    async fn get_or_build_retries_after_failure() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let fail_next = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&fail_next);
        let factory: AgentFactory = Arc::new(move |opts: BuildTaskOptions| {
            let flag = Arc::clone(&flag);
            async move {
                if flag.swap(false, Ordering::SeqCst) {
                    Err(AppError::Internal("first call fails".into()))
                } else {
                    Ok(mock_instance(MockAgent::new(&opts.conversation_id, None)))
                }
            }
            .boxed()
        });
        let mgr = WorkerTaskManagerImpl::new(factory);

        // First call fails, slot stays empty.
        assert!(mgr.get_or_build_task("conv-1", make_options("conv-1")).await.is_err());
        // Second call retries and succeeds.
        let h = mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        assert_eq!(h.conversation_id(), "conv-1");
        assert_eq!(mgr.active_count(), 1);
    }

    #[tokio::test]
    async fn get_task_finds_existing() {
        let mgr = make_manager();
        mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        let handle = mgr.get_task("conv-1");
        assert!(handle.is_some());
        assert_eq!(handle.unwrap().conversation_id(), "conv-1");
    }

    #[tokio::test]
    async fn kill_removes_task() {
        let mgr = make_manager();
        mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        assert_eq!(mgr.active_count(), 1);

        mgr.kill("conv-1", Some(AgentKillReason::IdleTimeout)).unwrap();
        assert_eq!(mgr.active_count(), 0);
        assert!(mgr.get_task("conv-1").is_none());
    }

    #[test]
    fn kill_nonexistent_is_ok() {
        let factory: AgentFactory = Arc::new(|_| async { unreachable!() }.boxed());
        let mgr = WorkerTaskManagerImpl::new(factory);
        assert!(mgr.kill("nothing", None).is_ok());
    }

    #[tokio::test]
    async fn clear_removes_all() {
        let mgr = make_manager();
        mgr.get_or_build_task("conv-1", make_options("conv-1")).await.unwrap();
        mgr.get_or_build_task("conv-2", make_options("conv-2")).await.unwrap();
        assert_eq!(mgr.active_count(), 2);

        mgr.clear();
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn collect_idle_finds_finished_and_stale_acp_tasks() {
        let factory: AgentFactory = Arc::new(|_| async { unreachable!() }.boxed());
        let mgr = WorkerTaskManagerImpl::new(factory);

        // Helper: insert a pre-initialised slot bypassing the async factory path.
        let insert = |id: &str, instance: AgentInstance| {
            let cell: OnceCell<AgentInstance> = OnceCell::new();
            cell.set(instance).ok();
            mgr.tasks.insert(id.into(), Arc::new(cell));
        };

        // ACP + Finished + old activity → should be collected
        insert(
            "conv-stale",
            mock_instance(
                MockAgent::new("conv-stale", Some(ConversationStatus::Finished)).with_last_activity(now_ms() - 600_000),
            ),
        );

        // ACP + Finished + recent activity → should NOT be collected
        insert(
            "conv-recent",
            mock_instance(
                MockAgent::new("conv-recent", Some(ConversationStatus::Finished)).with_last_activity(now_ms()),
            ),
        );

        // ACP + Running + old activity → should NOT be collected
        insert(
            "conv-running",
            mock_instance(
                MockAgent::new("conv-running", Some(ConversationStatus::Running))
                    .with_last_activity(now_ms() - 600_000),
            ),
        );

        // Non-ACP (Nanobot) + Finished + old activity → should NOT be collected
        insert(
            "conv-nanobot",
            mock_instance(
                MockAgent::new("conv-nanobot", Some(ConversationStatus::Finished))
                    .with_agent_type(AgentType::Nanobot)
                    .with_last_activity(now_ms() - 600_000),
            ),
        );

        let idle = mgr.collect_idle(300_000); // 5-min threshold
        assert_eq!(idle.len(), 1);
        assert_eq!(idle[0], "conv-stale");
    }

    #[test]
    fn collect_idle_empty_when_no_tasks() {
        let mgr = make_manager();
        let idle = mgr.collect_idle(300_000);
        assert!(idle.is_empty());
    }

    // ── Restart governor (crash-loop breaker) ───────────────────────

    #[test]
    fn restart_governor_trips_after_max_crashes_in_window() {
        let g = RestartGovernor::default();
        let t0 = 1_000_000;
        // No history → unthrottled.
        assert_eq!(g.gate("c", t0), Ok(0));
        // Crashes within the window accumulate.
        assert_eq!(g.record_crash("c", t0), 1);
        assert_eq!(g.record_crash("c", t0 + 6_000), 2);
        assert_eq!(g.record_crash("c", t0 + 12_000), 3);
        // At the cap the breaker trips.
        assert_eq!(g.gate("c", t0 + 12_000), Err(RESTART_MAX_PER_WINDOW));
    }

    #[test]
    fn restart_governor_backoff_grows_per_crash() {
        let g = RestartGovernor::default();
        let t0 = 5_000;
        g.record_crash("c", t0); // count 1
        assert_eq!(g.gate("c", t0), Ok(1_000));
        g.record_crash("c", t0); // count 2
        assert_eq!(g.gate("c", t0), Ok(2_000));
    }

    #[test]
    fn restart_governor_resets_after_window() {
        let g = RestartGovernor::default();
        let t0 = 0;
        g.record_crash("c", t0);
        g.record_crash("c", t0 + 1_000);
        g.record_crash("c", t0 + 2_000); // count 3 → tripped
        assert!(g.gate("c", t0 + 2_000).is_err());
        // A crash after the window has elapsed starts a fresh budget.
        let after = t0 + RESTART_WINDOW_MS + 3_000;
        assert_eq!(g.record_crash("c", after), 1);
        assert_eq!(g.gate("c", after), Ok(1_000));
    }

    #[test]
    fn restart_governor_forget_clears_history() {
        let g = RestartGovernor::default();
        let t0 = 100;
        g.record_crash("c", t0);
        g.record_crash("c", t0);
        g.record_crash("c", t0); // tripped
        assert!(g.gate("c", t0).is_err());
        g.forget("c");
        assert_eq!(g.gate("c", t0), Ok(0)); // fresh
    }

    #[tokio::test]
    async fn agent_error_recovery_crashes_trip_the_restart_breaker() {
        let mgr = make_manager();
        // Initial build succeeds.
        mgr.get_or_build_task("c", make_options("c")).await.unwrap();
        // Crash-evictions accumulate (recorded whether or not a rebuild
        // intervenes). At the cap, gate() returns Err *before* any backoff
        // sleep, so the test needs no clock control.
        for _ in 0..RESTART_MAX_PER_WINDOW {
            mgr.kill("c", Some(AgentKillReason::AgentErrorRecovery)).unwrap();
        }
        // The next respawn is refused — the loop is broken.
        match mgr.get_or_build_task("c", make_options("c")).await {
            Err(AppError::Conflict(_)) => {}
            Err(other) => panic!("expected Conflict, got {other:?}"),
            Ok(_) => panic!("crash loop must trip the breaker, but the build succeeded"),
        }
    }

    #[tokio::test]
    async fn benign_recycles_never_trip_the_breaker() {
        let mgr = make_manager();
        for _ in 0..6 {
            mgr.get_or_build_task("c", make_options("c")).await.unwrap();
            // Knowledge-binding rebuild is a deliberate recycle, not a crash.
            mgr.kill("c", Some(AgentKillReason::KnowledgeBindingChanged)).unwrap();
        }
        // Never counted as a crash → still builds.
        assert!(mgr.get_or_build_task("c", make_options("c")).await.is_ok());
    }

    #[tokio::test]
    async fn conversation_delete_resets_the_restart_governor() {
        let mgr = make_manager();
        mgr.get_or_build_task("c", make_options("c")).await.unwrap();
        for _ in 0..RESTART_MAX_PER_WINDOW {
            mgr.kill("c", Some(AgentKillReason::AgentErrorRecovery)).unwrap();
        }
        // Deleting the conversation clears the crash history so a reused id starts fresh.
        mgr.kill("c", Some(AgentKillReason::ConversationDeleted)).unwrap();
        assert!(mgr.get_or_build_task("c", make_options("c")).await.is_ok());
    }
}
