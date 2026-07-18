use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

use nomifun_api_types::{ConversationRuntimeStateKind, ConversationRuntimeSummary};
use nomifun_common::{AppError, CompanionId, ConversationStatus, now_ms};
use tokio::sync::Notify;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tracing::{info, warn};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct ActiveTurn {
    id: u64,
    /// Verified ownership cached at admission so a user can securely cancel
    /// even when the database actor is the stuck component.
    owner_user_id: Option<String>,
    public_cancellable: bool,
    /// Stable public turn identity shared by `turn.started` and
    /// `turn.completed`. Legacy/test callers that do not publish wire events
    /// leave this unset.
    wire_turn_id: Option<String>,
    /// Current streamed message segment. Unlike `wire_turn_id`, this advances
    /// for internal continuations/failover resends and scopes terminal CAS.
    terminal_msg_id: Option<String>,
    wire_context: TurnWireContext,
    started_at: i64,
    cancellation: CancellationToken,
    /// 0 = no terminal publisher, 1 = one publisher claimed the terminal,
    /// 2 = the cancelled terminal is on the wire and relay finalization ended.
    terminal_observed: Arc<AtomicU8>,
    terminal_notify: Arc<Notify>,
    owner_quiesced: Arc<std::sync::atomic::AtomicBool>,
    owner_quiesced_notify: Arc<Notify>,
    /// Exact-generation owner task. A cooperative cancellation token cannot
    /// interrupt a repository/transport future that is stuck inside an await,
    /// so stop keeps this abort backstop until the owner quiesces.
    owner_abort_handle: Option<AbortHandle>,
    /// A stop request closes this gate before tearing down the captured runtime.
    /// A fast backend terminal therefore cannot release turn admission and let
    /// a replacement process overlap the old process's bounded teardown.
    release_blocked: bool,
}

#[derive(Debug, Clone)]
struct RuntimeBuildEntry {
    cancellation: CancellationToken,
    requester_user_id: Option<String>,
    public_cancellable: bool,
}

#[derive(Debug, Default)]
pub struct ConversationRuntimeStateService {
    /// Conversations with an acquired turn handle, mapped to the wall-clock
    /// time (epoch ms) the handle was acquired. The timestamp is surfaced in
    /// `ConversationRuntimeSummary::processing_started_at` so the frontend's
    /// elapsed-time indicator can anchor to the real turn start and survive
    /// view unmount/remount instead of restarting from zero.
    active_turns: Mutex<HashMap<String, ActiveTurn>>,
    /// Conversation deletion is an admission tombstone, not merely a stop.
    /// The tombstone lock is held through turn insertion, making deletion and
    /// admission one synchronous ordering boundary.
    deletion_tombstones: Mutex<std::collections::HashSet<String>>,
    /// Temporary admission leases held by stop workers until bounded teardown
    /// and exact-generation release finish. This also protects idle-runtime
    /// stops, where there is no active turn release gate to close.
    stop_tombstones: Mutex<HashMap<String, usize>>,
    /// Normal completion also needs an admission fence across release -> DB
    /// finished -> `turn.completed`, but unlike a real stop it must not make
    /// runtime GETs report a synthetic processing state.
    completion_tombstones: Mutex<HashMap<String, usize>>,
    cleanup_fence_notify: Notify,
    /// Synchronous ordering point shared by new stop/completion admissions and
    /// the final `turn.completed` enqueue+fence-drop action.
    cleanup_linearization: Mutex<()>,
    /// Requester-scoped admission fences held while a user stop performs its
    /// bounded repository authorization. They close the empty-scan race where
    /// cancellation sees no active/build work and a same-user public build
    /// registers immediately afterwards. Private/durable work and other users
    /// are intentionally unaffected.
    user_cancel_preflights: Mutex<HashMap<(String, String), usize>>,
    runtime_builds: Mutex<HashMap<String, HashMap<u64, RuntimeBuildEntry>>>,
    runtime_build_notify: Notify,
    /// Monotonic process-local identity for turn ownership. A stale handle is
    /// never allowed to release (or cancel) a newer turn for the same
    /// conversation.
    next_turn_id: AtomicU64,
    next_runtime_build_id: AtomicU64,
    /// Wakes bounded stop requests as soon as the exact generation they
    /// cancelled releases its ownership handle.
    turn_release_notify: Notify,
    /// Admission epoch advanced by every stop request, even when it arrives
    /// before the in-flight send has acquired its turn handle.
    cancellation_epochs: Mutex<HashMap<String, u64>>,
    /// Per-conversation signature of the knowledge mounts the live Agent runtime
    /// was last created with. The Agent bakes the knowledge retrieval-protocol
    /// section at build time and is cached per conversation, so a binding
    /// toggled mid-session does not reach the already-running agent.
    /// `apply_knowledge_mounts` compares the freshly-resolved signature against
    /// this map to decide whether to recycle the cached runtime. In-memory only
    /// (cleared on restart), which is intentional: after a restart the runtime registry
    /// is empty too, so the first build naturally carries the current mounts.
    knowledge_signatures: Mutex<HashMap<String, String>>,
    /// Per-conversation CUMULATIVE token usage (`input + output`) for the turns
    /// run on that conversation, accumulated from the per-turn `TurnCompleted`
    /// metrics event the stream relay sees. Keyed by conversation id string.
    ///
    /// A persisted execution attempt drives a fresh conversation to completion,
    /// then reads and removes this total via [`Self::take_turn_tokens`] for its
    /// per-step usage record. The relay's `add_turn_tokens` write happens before
    /// the [`AgentTurnHandle`] releases (and before `is_processing` flips false),
    /// so an attempt that reads only after turn completion observes the full
    /// total without a race. In-memory only (cleared on restart), like the maps
    /// above; an un-taken entry is dropped on the next `take`. Continuation turns
    /// (cron/autowork follow-ups, model-failover resends) accumulate additively.
    turn_tokens: Mutex<HashMap<String, i64>>,
}

#[derive(Debug)]
pub struct AgentTurnHandle {
    conversation_id: String,
    turn_id: u64,
    cancellation: CancellationToken,
    wire_turn_id: Option<String>,
    terminal_msg_id: Option<String>,
    wire_context: TurnWireContext,
    terminal_observed: Arc<AtomicU8>,
    terminal_notify: Arc<Notify>,
    owner_quiesced: Arc<std::sync::atomic::AtomicBool>,
    owner_quiesced_notify: Arc<Notify>,
    state: Weak<ConversationRuntimeStateService>,
    released: bool,
}

/// Generation-scoped cancellation snapshot. The stop handler captures this
/// before awaiting any agent transport operation, so a late cancel can only
/// signal the turn that was active when the request began.
#[derive(Debug, Clone)]
pub struct AgentTurnCancellation {
    conversation_id: String,
    turn_id: u64,
    wire_turn_id: Option<String>,
    terminal_msg_id: Option<String>,
    wire_context: TurnWireContext,
    cancellation: CancellationToken,
    terminal_observed: Arc<AtomicU8>,
    terminal_notify: Arc<Notify>,
    owner_quiesced: Arc<std::sync::atomic::AtomicBool>,
    owner_quiesced_notify: Arc<Notify>,
    owner_abort_handle: Option<AbortHandle>,
}

#[derive(Debug)]
pub struct ConversationDeletionGuard {
    conversation_id: String,
    state: Weak<ConversationRuntimeStateService>,
    committed: bool,
}

#[derive(Debug)]
pub struct ConversationStopGuard {
    conversation_id: String,
    state: Weak<ConversationRuntimeStateService>,
    cancelled_build_ids: Vec<u64>,
}

#[derive(Debug)]
pub struct ConversationCompletionGuard {
    conversation_id: String,
    state: Weak<ConversationRuntimeStateService>,
}

#[derive(Debug)]
pub struct CancelledTurnReleaseGuard {
    cancellation: AgentTurnCancellation,
    state: Weak<ConversationRuntimeStateService>,
    armed: bool,
}

#[derive(Debug)]
pub struct RuntimeBuildLease {
    conversation_id: String,
    id: u64,
    expected_cancellation_epoch: u64,
    cancellation: CancellationToken,
    state: Weak<ConversationRuntimeStateService>,
    released: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InMemoryCancelAuthority {
    ActiveTurn,
    PublicBuilds(Vec<u64>),
    None,
}

#[derive(Debug)]
pub struct UserCancelPreflightGuard {
    conversation_id: String,
    user_id: String,
    state: Weak<ConversationRuntimeStateService>,
}

#[derive(Debug)]
pub struct InMemoryUserCancelAuthorization {
    pub authority: InMemoryCancelAuthority,
    pub preflight_guard: Option<UserCancelPreflightGuard>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnWireContext {
    pub companion: bool,
    pub companion_id: Option<CompanionId>,
    pub origin: Option<String>,
    pub channel_platform: Option<String>,
}

impl ConversationRuntimeStateService {
    pub fn try_acquire_turn(self: &Arc<Self>, conversation_id: &str) -> Result<AgentTurnHandle, AppError> {
        self.try_acquire_turn_with_wire_id(conversation_id, None)
    }

    /// Acquire one turn and bind its stable public identity up front. Binding
    /// at admission time lets a stop request finalize even when cancellation
    /// lands while the runtime factory is still cold-starting.
    pub fn try_acquire_turn_with_wire_id(
        self: &Arc<Self>,
        conversation_id: &str,
        wire_turn_id: Option<String>,
    ) -> Result<AgentTurnHandle, AppError> {
        self.try_acquire_turn_with_wire_id_at_epoch(conversation_id, wire_turn_id, None)
    }

    pub fn try_acquire_turn_with_wire_id_at_epoch(
        self: &Arc<Self>,
        conversation_id: &str,
        wire_turn_id: Option<String>,
        expected_cancellation_epoch: Option<u64>,
    ) -> Result<AgentTurnHandle, AppError> {
        self.try_acquire_turn_with_wire_context_at_epoch(
            conversation_id,
            wire_turn_id,
            TurnWireContext::default(),
            expected_cancellation_epoch,
        )
    }

    pub fn try_acquire_turn_with_wire_context_at_epoch(
        self: &Arc<Self>,
        conversation_id: &str,
        wire_turn_id: Option<String>,
        wire_context: TurnWireContext,
        expected_cancellation_epoch: Option<u64>,
    ) -> Result<AgentTurnHandle, AppError> {
        self.try_acquire_turn_with_wire_context_at_epoch_and_owner(
            conversation_id,
            wire_turn_id,
            wire_context,
            expected_cancellation_epoch,
            None,
            false,
            None,
        )
    }

    pub fn try_acquire_turn_with_wire_context_at_epoch_and_owner(
        self: &Arc<Self>,
        conversation_id: &str,
        wire_turn_id: Option<String>,
        wire_context: TurnWireContext,
        expected_cancellation_epoch: Option<u64>,
        owner_user_id: Option<String>,
        public_cancellable: bool,
        preparation_cancellation: Option<&CancellationToken>,
    ) -> Result<AgentTurnHandle, AppError> {
        let _linearization = self
            .cleanup_linearization
            .lock()
            .map_err(|_| AppError::Internal("cleanup linearization lock poisoned".into()))?;
        if preparation_cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} runtime preparation was cancelled before turn admission"
            )));
        }
        let deletion_tombstones = self.deletion_tombstones.lock().map_err(|_| {
            AppError::Internal("conversation deletion admission lock poisoned".into())
        })?;
        if deletion_tombstones.contains(conversation_id) {
            return Err(AppError::NotFound(format!(
                "conversation {conversation_id} is being deleted"
            )));
        }
        let stop_tombstones = self.stop_tombstones.lock().map_err(|_| {
            AppError::Internal("conversation stop admission lock poisoned".into())
        })?;
        if stop_tombstones.contains_key(conversation_id) {
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} is stopping"
            )));
        }
        let completion_tombstones = self.completion_tombstones.lock().map_err(|_| {
            AppError::Internal("conversation completion admission lock poisoned".into())
        })?;
        if completion_tombstones.contains_key(conversation_id) {
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} is completing"
            )));
        }
        let mut active_turns = self.active_turns.lock().map_err(|_| {
            warn!(
                conversation_id,
                "conversation runtime state lock poisoned while acquiring turn"
            );
            AppError::Internal("conversation runtime state lock poisoned".into())
        })?;

        if active_turns.contains_key(conversation_id) {
            info!(conversation_id, "conversation runtime turn acquisition rejected");
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} is already running"
            )));
        }

        let turn_id = self.next_turn_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let cancellation = CancellationToken::new();
        let terminal_observed = Arc::new(AtomicU8::new(0));
        let terminal_notify = Arc::new(Notify::new());
        let owner_quiesced = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let owner_quiesced_notify = Arc::new(Notify::new());
        active_turns.insert(
            conversation_id.to_owned(),
            ActiveTurn {
                id: turn_id,
                owner_user_id,
                public_cancellable,
                wire_turn_id: wire_turn_id.clone(),
                terminal_msg_id: wire_turn_id.clone(),
                wire_context: wire_context.clone(),
                started_at: now_ms(),
                cancellation: cancellation.clone(),
                terminal_observed: Arc::clone(&terminal_observed),
                terminal_notify: Arc::clone(&terminal_notify),
                owner_quiesced: Arc::clone(&owner_quiesced),
                owner_quiesced_notify: Arc::clone(&owner_quiesced_notify),
                owner_abort_handle: None,
                release_blocked: false,
            },
        );
        drop(active_turns);
        drop(completion_tombstones);
        drop(stop_tombstones);
        drop(deletion_tombstones);

        if expected_cancellation_epoch.is_some_and(|expected| self.cancellation_epoch(conversation_id) != expected) {
            let cancellation = AgentTurnCancellation {
                conversation_id: conversation_id.to_owned(),
                turn_id,
                wire_turn_id: wire_turn_id.clone(),
                terminal_msg_id: wire_turn_id.clone(),
                wire_context: wire_context.clone(),
                cancellation: cancellation.clone(),
                terminal_observed: Arc::clone(&terminal_observed),
                terminal_notify: Arc::clone(&terminal_notify),
                owner_quiesced: Arc::clone(&owner_quiesced),
                owner_quiesced_notify: Arc::clone(&owner_quiesced_notify),
                owner_abort_handle: None,
            };
            self.force_release_cancelled_turn(&cancellation);
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} send was cancelled before turn admission"
            )));
        }

        info!(conversation_id, "conversation runtime turn acquired");

        Ok(AgentTurnHandle {
            conversation_id: conversation_id.to_owned(),
            turn_id,
            cancellation,
            wire_turn_id: wire_turn_id.clone(),
            terminal_msg_id: wire_turn_id,
            wire_context,
            terminal_observed,
            terminal_notify,
            owner_quiesced,
            owner_quiesced_notify,
            state: Arc::downgrade(self),
            released: false,
        })
    }

    pub fn cancellation_epoch(&self, conversation_id: &str) -> u64 {
        self.cancellation_epochs
            .lock()
            .ok()
            .and_then(|epochs| epochs.get(conversation_id).copied())
            .unwrap_or(0)
    }

    /// Acquire before an initiator's first await and retain until turn
    /// admission. This closes the stale-prebuild window where stop/delete can
    /// finish before an old caller reaches the registry and then resurrect a
    /// runtime from already-loaded options.
    pub fn begin_runtime_build(
        self: &Arc<Self>,
        conversation_id: &str,
    ) -> Result<RuntimeBuildLease, AppError> {
        self.begin_runtime_build_for_requester(conversation_id, None, false)
    }

    pub fn begin_runtime_build_for_requester(
        self: &Arc<Self>,
        conversation_id: &str,
        requester_user_id: Option<String>,
        public_cancellable: bool,
    ) -> Result<RuntimeBuildLease, AppError> {
        let _linearization = self
            .cleanup_linearization
            .lock()
            .map_err(|_| AppError::Internal("cleanup linearization lock poisoned".into()))?;
        let deletion_tombstones = self.deletion_tombstones.lock().map_err(|_| {
            AppError::Internal("conversation deletion admission lock poisoned".into())
        })?;
        if deletion_tombstones.contains(conversation_id) {
            return Err(AppError::NotFound(format!(
                "conversation {conversation_id} is being deleted"
            )));
        }
        let stop_tombstones = self.stop_tombstones.lock().map_err(|_| {
            AppError::Internal("conversation stop admission lock poisoned".into())
        })?;
        if stop_tombstones.contains_key(conversation_id) {
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} is stopping"
            )));
        }
        let completion_tombstones = self.completion_tombstones.lock().map_err(|_| {
            AppError::Internal("conversation completion admission lock poisoned".into())
        })?;
        if completion_tombstones.contains_key(conversation_id) {
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} is completing"
            )));
        }
        if public_cancellable
            && let Some(requester_user_id) = requester_user_id.as_deref()
            && self
                .user_cancel_preflights
                .lock()
                .map_err(|_| {
                    AppError::Internal("conversation user-cancel preflight lock poisoned".into())
                })?
                .contains_key(&(conversation_id.to_owned(), requester_user_id.to_owned()))
        {
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} is being stopped by this requester"
            )));
        }
        let expected_cancellation_epoch = self
            .cancellation_epochs
            .lock()
            .map_err(|_| AppError::Internal("conversation cancellation epoch lock poisoned".into()))?
            .get(conversation_id)
            .copied()
            .unwrap_or(0);
        let id = self
            .next_runtime_build_id
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let cancellation = CancellationToken::new();
        self.runtime_builds
            .lock()
            .map_err(|_| AppError::Internal("conversation runtime build lock poisoned".into()))?
            .entry(conversation_id.to_owned())
            .or_default()
            .insert(
                id,
                RuntimeBuildEntry {
                    cancellation: cancellation.clone(),
                    requester_user_id,
                    public_cancellable,
                },
            );
        Ok(RuntimeBuildLease {
            conversation_id: conversation_id.to_owned(),
            id,
            expected_cancellation_epoch,
            cancellation,
            state: Arc::downgrade(self),
            released: false,
        })
    }

    fn cancel_runtime_builds(&self, conversation_id: &str) -> Vec<u64> {
        self.runtime_builds
            .lock()
            .ok()
            .and_then(|builds| builds.get(conversation_id).cloned())
            .map(|builds| {
                let mut ids = Vec::with_capacity(builds.len());
                for (id, entry) in builds {
                    ids.push(id);
                    entry.cancellation.cancel();
                }
                ids
            })
            .unwrap_or_default()
    }

    pub async fn wait_for_runtime_builds(
        &self,
        conversation_id: &str,
        build_ids: &[u64],
        timeout: Duration,
    ) -> bool {
        if build_ids.is_empty() {
            return true;
        }
        let finished = || {
            self.runtime_builds
                .lock()
                .map(|builds| {
                    builds
                        .get(conversation_id)
                        .is_none_or(|active| build_ids.iter().all(|id| !active.contains_key(id)))
                })
                .unwrap_or(false)
        };
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.runtime_build_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if finished() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, &mut notified).await.is_err() {
                return finished();
            }
        }
    }

    /// Forget only the captured, already-cancelled build IDs after the stop
    /// wait bound. Their owners retain cancelled tokens and an obsolete epoch,
    /// so they still cannot build/admit; removing the bookkeeping prevents a
    /// non-cooperative preflight future from leaking state forever.
    pub fn forget_cancelled_runtime_builds(&self, conversation_id: &str, build_ids: &[u64]) {
        if build_ids.is_empty() {
            return;
        }
        if let Ok(mut builds) = self.runtime_builds.lock() {
            let remove_conversation = if let Some(active) = builds.get_mut(conversation_id) {
                for id in build_ids {
                    active.remove(id);
                }
                active.is_empty()
            } else {
                false
            };
            if remove_conversation {
                builds.remove(conversation_id);
            }
        }
        self.runtime_build_notify.notify_waiters();
    }

    pub fn advance_cancellation_epoch(&self, conversation_id: &str) -> u64 {
        self.cancellation_epochs
            .lock()
            .map(|mut epochs| {
                let epoch = epochs.entry(conversation_id.to_owned()).or_insert(0);
                *epoch = epoch.wrapping_add(1);
                *epoch
            })
            .unwrap_or(0)
    }

    /// Atomically prevent every future turn admission while deletion tears
    /// down the exact active generation and removes persisted state. Dropping
    /// an uncommitted guard reopens admission if the database delete fails;
    /// committing keeps the tombstone permanently for the deleted identity.
    pub fn begin_conversation_deletion(
        self: &Arc<Self>,
        conversation_id: &str,
    ) -> Result<ConversationDeletionGuard, AppError> {
        let _linearization = self
            .cleanup_linearization
            .lock()
            .map_err(|_| AppError::Internal("cleanup linearization lock poisoned".into()))?;
        let inserted = self
            .deletion_tombstones
            .lock()
            .map_err(|_| AppError::Internal("conversation deletion admission lock poisoned".into()))?
            .insert(conversation_id.to_owned());
        if !inserted {
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} deletion is already in progress"
            )));
        }
        self.advance_cancellation_epoch(conversation_id);
        let _ = self.cancel_runtime_builds(conversation_id);
        Ok(ConversationDeletionGuard {
            conversation_id: conversation_id.to_owned(),
            state: Arc::downgrade(self),
            committed: false,
        })
    }

    /// Close admission even when a conversation has only an idle runtime (or
    /// a cold uninitialized registry slot). Stop is single-flight: the leader
    /// owns the guard and every follower joins the same cleanup/event owner.
    pub fn begin_conversation_stop(
        self: &Arc<Self>,
        conversation_id: &str,
    ) -> Result<Option<ConversationStopGuard>, AppError> {
        self.begin_conversation_stop_inner(conversation_id, false)
    }

    pub fn begin_conversation_stop_for_deletion(
        self: &Arc<Self>,
        conversation_id: &str,
    ) -> Result<Option<ConversationStopGuard>, AppError> {
        self.begin_conversation_stop_inner(conversation_id, true)
    }

    fn begin_conversation_stop_inner(
        self: &Arc<Self>,
        conversation_id: &str,
        deletion_owned: bool,
    ) -> Result<Option<ConversationStopGuard>, AppError> {
        let _linearization = self
            .cleanup_linearization
            .lock()
            .map_err(|_| AppError::Internal("cleanup linearization lock poisoned".into()))?;
        let deletion_tombstones = self.deletion_tombstones.lock().map_err(|_| {
            AppError::Internal("conversation deletion admission lock poisoned".into())
        })?;
        let deleting = deletion_tombstones.contains(conversation_id);
        if deleting != deletion_owned {
            return Err(if deleting {
                AppError::NotFound(format!("conversation {conversation_id} is being deleted"))
            } else {
                AppError::Conflict(format!(
                    "conversation {conversation_id} has no owning deletion"
                ))
            });
        }
        let mut tombstones = self
            .stop_tombstones
            .lock()
            .map_err(|_| AppError::Internal("conversation stop admission lock poisoned".into()))?;
        // Stop is single-flight per conversation. A follower joins the
        // existing cleanup instead of acquiring another fence owner and
        // repeating teardown. This also leaves exactly one completion-event
        // publisher, so its linearization cannot be delayed by a follower
        // whose own timeout is as long as the leader's.
        if tombstones.contains_key(conversation_id) {
            return Ok(None);
        }
        let completion_tombstones = self.completion_tombstones.lock().map_err(|_| {
            AppError::Internal("conversation completion admission lock poisoned".into())
        })?;
        if completion_tombstones.contains_key(conversation_id) {
            return Ok(None);
        }
        tombstones.insert(conversation_id.to_owned(), 1);
        drop(completion_tombstones);
        drop(tombstones);
        drop(deletion_tombstones);
        self.advance_cancellation_epoch(conversation_id);
        let cancelled_build_ids = self.cancel_runtime_builds(conversation_id);
        Ok(Some(ConversationStopGuard {
            conversation_id: conversation_id.to_owned(),
            state: Arc::downgrade(self),
            cancelled_build_ids,
        }))
    }

    /// Fence only the exact normal-completion critical section. Admissions and
    /// runtime summaries remain busy until the authoritative idle
    /// `turn.completed` event is synchronously enqueued.
    pub fn begin_turn_completion(
        self: &Arc<Self>,
        conversation_id: &str,
        turn_id: u64,
    ) -> Result<Option<ConversationCompletionGuard>, AppError> {
        let _linearization = self
            .cleanup_linearization
            .lock()
            .map_err(|_| AppError::Internal("cleanup linearization lock poisoned".into()))?;
        let deletion_tombstones = self.deletion_tombstones.lock().map_err(|_| {
            AppError::Internal("conversation deletion admission lock poisoned".into())
        })?;
        if deletion_tombstones.contains(conversation_id) {
            return Ok(None);
        }
        let stop_tombstones = self.stop_tombstones.lock().map_err(|_| {
            AppError::Internal("conversation stop admission lock poisoned".into())
        })?;
        if stop_tombstones.contains_key(conversation_id) {
            return Ok(None);
        }
        let mut tombstones = self.completion_tombstones.lock().map_err(|_| {
            AppError::Internal("conversation completion admission lock poisoned".into())
        })?;
        let active_turns = self.active_turns.lock().map_err(|_| {
            AppError::Internal("conversation runtime state lock poisoned".into())
        })?;
        if !active_turns
            .get(conversation_id)
            .is_some_and(|turn| turn.id == turn_id && !turn.release_blocked)
        {
            return Ok(None);
        }
        let owners = tombstones.entry(conversation_id.to_owned()).or_insert(0);
        *owners = owners.saturating_add(1);
        Ok(Some(ConversationCompletionGuard {
            conversation_id: conversation_id.to_owned(),
            state: Arc::downgrade(self),
        }))
    }

    fn cleanup_fences_clear(&self, conversation_id: &str) -> bool {
        let stop_clear = self
            .stop_tombstones
            .lock()
            .map(|fences| !fences.contains_key(conversation_id))
            .unwrap_or(false);
        let completion_clear = self
            .completion_tombstones
            .lock()
            .map(|fences| !fences.contains_key(conversation_id))
            .unwrap_or(false);
        stop_clear && completion_clear
    }

    /// Deletion calls this after its own stop worker reports. A second stop or
    /// an already-running normal completion can still own side effects, so the
    /// core row must not be removed until every owner has dropped its fence.
    pub async fn wait_for_cleanup_fences(
        &self,
        conversation_id: &str,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.cleanup_fence_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.cleanup_fences_clear(conversation_id) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, &mut notified).await.is_err() {
                return self.cleanup_fences_clear(conversation_id);
            }
        }
    }

    pub async fn wait_for_other_cleanup_fences(
        &self,
        conversation_id: &str,
        allowed_stop_owners: usize,
        allowed_completion_owners: usize,
        timeout: Duration,
    ) -> bool {
        let within_allowance = || {
            let stop_owners = self
                .stop_tombstones
                .lock()
                .ok()
                .and_then(|fences| fences.get(conversation_id).copied())
                .unwrap_or(0);
            let completion_owners = self
                .completion_tombstones
                .lock()
                .ok()
                .and_then(|fences| fences.get(conversation_id).copied())
                .unwrap_or(0);
            stop_owners <= allowed_stop_owners
                && completion_owners <= allowed_completion_owners
        };
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.cleanup_fence_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if within_allowance() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, &mut notified).await.is_err() {
                return within_allowance();
            }
        }
    }

    /// Wait until only the caller's declared fence owners remain, then under
    /// the same synchronous gate used by new stop/completion admissions run a
    /// no-await action that enqueues `turn.completed` and drops the caller's
    /// fence. No new stop can enter between the final count check and enqueue.
    pub async fn linearize_cleanup_event<F>(
        &self,
        conversation_id: &str,
        allowed_stop_owners: usize,
        allowed_completion_owners: usize,
        timeout: Duration,
        action: F,
    ) -> bool
    where
        F: FnOnce(),
    {
        let deadline = Instant::now() + timeout;
        let mut action = Some(action);
        loop {
            let notified = self.cleanup_fence_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let linearized = if let Ok(_gate) = self.cleanup_linearization.lock() {
                // A missing map entry means there are zero owners. Keep lock
                // poisoning distinct from that normal idle state: collapsing
                // both into `None` would turn every ordinary completion (which
                // has no stop tombstone) into a timeout and leave the UI busy.
                let stop_owners = self
                    .stop_tombstones
                    .lock()
                    .map(|fences| fences.get(conversation_id).copied().unwrap_or(0))
                    .unwrap_or(usize::MAX);
                let completion_owners = self
                    .completion_tombstones
                    .lock()
                    .map(|fences| fences.get(conversation_id).copied().unwrap_or(0))
                    .unwrap_or(usize::MAX);
                if stop_owners <= allowed_stop_owners
                    && completion_owners <= allowed_completion_owners
                {
                    if let Some(action) = action.take() {
                        action();
                    }
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if linearized {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, &mut notified).await.is_err() {
                return false;
            }
        }
    }

    pub fn register_turn_owner_abort_handle(
        &self,
        conversation_id: &str,
        turn_id: u64,
        abort_handle: AbortHandle,
    ) -> bool {
        let abort_immediately = match self.active_turns.lock() {
            Ok(mut turns) => {
                let Some(turn) = turns
                    .get_mut(conversation_id)
                    .filter(|turn| turn.id == turn_id)
                else {
                    return false;
                };
                if turn.release_blocked || turn.cancellation.is_cancelled() {
                    true
                } else {
                    turn.owner_abort_handle = Some(abort_handle.clone());
                    false
                }
            }
            Err(_) => true,
        };
        if abort_immediately {
            abort_handle.abort();
        }
        true
    }

    pub fn arm_cancelled_turn_release(
        self: &Arc<Self>,
        cancellation: AgentTurnCancellation,
    ) -> CancelledTurnReleaseGuard {
        CancelledTurnReleaseGuard {
            cancellation,
            state: Arc::downgrade(self),
            armed: true,
        }
    }

    fn owner_abort_handle(&self, conversation_id: &str, turn_id: u64) -> Option<AbortHandle> {
        self.active_turns.lock().ok().and_then(|turns| {
            turns
                .get(conversation_id)
                .filter(|turn| turn.id == turn_id)
                .and_then(|turn| turn.owner_abort_handle.clone())
        })
    }

    pub fn has_active_turn(&self, conversation_id: &str) -> bool {
        self.active_turns
            .lock()
            .map(|active_turns| active_turns.contains_key(conversation_id))
            .unwrap_or(false)
    }

    pub fn active_turn_allows_cancel(
        &self,
        conversation_id: &str,
        user_id: &str,
        require_public: bool,
    ) -> bool {
        self.active_turns
            .lock()
            .ok()
            .and_then(|turns| {
                turns.get(conversation_id).map(|turn| {
                    turn.owner_user_id.as_deref() == Some(user_id)
                        && (!require_public || turn.public_cancellable)
                })
            })
            .unwrap_or(false)
    }

    /// Linearize a user cancel against the public-build -> active-turn
    /// handoff. If admission won, the caller must run the full exact-turn stop;
    /// if cancellation won, only this requester's public builds are cancelled
    /// and their token is re-checked under the same gate by admission.
    pub fn authorize_in_memory_user_cancel(
        self: &Arc<Self>,
        conversation_id: &str,
        user_id: &str,
    ) -> Result<InMemoryUserCancelAuthorization, AppError> {
        let _linearization = self
            .cleanup_linearization
            .lock()
            .map_err(|_| AppError::Internal("cleanup linearization lock poisoned".into()))?;
        if self
            .active_turns
            .lock()
            .map_err(|_| AppError::Internal("conversation runtime state lock poisoned".into()))?
            .get(conversation_id)
            .is_some_and(|turn| {
                turn.owner_user_id.as_deref() == Some(user_id) && turn.public_cancellable
            })
        {
            return Ok(InMemoryUserCancelAuthorization {
                authority: InMemoryCancelAuthority::ActiveTurn,
                preflight_guard: None,
            });
        }

        let ids = self
            .runtime_builds
            .lock()
            .map_err(|_| AppError::Internal("conversation runtime build lock poisoned".into()))?
            .get(conversation_id)
            .map(|builds| {
                let mut ids = Vec::new();
                for (id, entry) in builds {
                    if entry.public_cancellable
                        && entry.requester_user_id.as_deref() == Some(user_id)
                    {
                        entry.cancellation.cancel();
                        ids.push(*id);
                    }
                }
                ids
            })
            .unwrap_or_default();
        // No active turn won the gate, so retain a requester fence for both
        // outcomes. Even when existing builds were cancelled, this prevents a
        // second same-requester public build from registering in the
        // scan-to-ack window and escaping the acknowledged cancellation.
        let key = (conversation_id.to_owned(), user_id.to_owned());
        let mut preflights = self.user_cancel_preflights.lock().map_err(|_| {
            AppError::Internal("conversation user-cancel preflight lock poisoned".into())
        })?;
        let owners = preflights.entry(key).or_insert(0);
        *owners = owners.saturating_add(1);
        let preflight_guard = Some(UserCancelPreflightGuard {
            conversation_id: conversation_id.to_owned(),
            user_id: user_id.to_owned(),
            state: Arc::downgrade(self),
        });
        if ids.is_empty() {
            Ok(InMemoryUserCancelAuthorization {
                authority: InMemoryCancelAuthority::None,
                preflight_guard,
            })
        } else {
            self.runtime_build_notify.notify_waiters();
            Ok(InMemoryUserCancelAuthorization {
                authority: InMemoryCancelAuthority::PublicBuilds(ids),
                preflight_guard,
            })
        }
    }

    /// Wall-clock time (epoch ms) the live turn for `conversation_id` was
    /// acquired, if one is active. `None` when no turn is in flight.
    pub fn active_turn_started_at(&self, conversation_id: &str) -> Option<i64> {
        self.active_turns
            .lock()
            .ok()
            .and_then(|active_turns| active_turns.get(conversation_id).map(|turn| turn.started_at))
    }

    /// Snapshot the currently active turn without cancelling it yet. Keeping
    /// cancellation separate lets the caller first detach/terminate the exact
    /// external runtime it observed, closing the old-turn/new-turn kill race.
    pub fn active_turn_cancellation(&self, conversation_id: &str) -> Option<AgentTurnCancellation> {
        self.active_turns
            .lock()
            .ok()
            .and_then(|turns| {
                turns.get(conversation_id).map(|turn| AgentTurnCancellation {
                    conversation_id: conversation_id.to_owned(),
                    turn_id: turn.id,
                    wire_turn_id: turn.wire_turn_id.clone(),
                    terminal_msg_id: turn.terminal_msg_id.clone(),
                    wire_context: turn.wire_context.clone(),
                    cancellation: turn.cancellation.clone(),
                    terminal_observed: Arc::clone(&turn.terminal_observed),
                    terminal_notify: Arc::clone(&turn.terminal_notify),
                    owner_quiesced: Arc::clone(&turn.owner_quiesced),
                    owner_quiesced_notify: Arc::clone(&turn.owner_quiesced_notify),
                    owner_abort_handle: turn.owner_abort_handle.clone(),
                })
            })
    }

    /// Capture and close the exact turn's release gate atomically. This is the
    /// only snapshot operation a stop endpoint should use before runtime
    /// teardown; read-only observers may use [`Self::active_turn_cancellation`].
    pub fn begin_turn_cancellation(&self, conversation_id: &str) -> Option<AgentTurnCancellation> {
        self.active_turns.lock().ok().and_then(|mut turns| {
            let turn = turns.get_mut(conversation_id)?;
            turn.release_blocked = true;
            Some(AgentTurnCancellation {
                conversation_id: conversation_id.to_owned(),
                turn_id: turn.id,
                wire_turn_id: turn.wire_turn_id.clone(),
                terminal_msg_id: turn.terminal_msg_id.clone(),
                wire_context: turn.wire_context.clone(),
                cancellation: turn.cancellation.clone(),
                terminal_observed: Arc::clone(&turn.terminal_observed),
                terminal_notify: Arc::clone(&turn.terminal_notify),
                owner_quiesced: Arc::clone(&turn.owner_quiesced),
                owner_quiesced_notify: Arc::clone(&turn.owner_quiesced_notify),
                owner_abort_handle: turn.owner_abort_handle.clone(),
            })
        })
    }

    fn cancellation_is_current(&self, cancellation: &AgentTurnCancellation) -> bool {
        self.active_turns
            .lock()
            .map(|turns| {
                turns
                    .get(cancellation.conversation_id())
                    .is_some_and(|turn| turn.id == cancellation.turn_id())
            })
            .unwrap_or(false)
    }

    /// Wait for the exact captured generation to release. A newer turn for the
    /// same conversation counts as released: the old cancel must never wait on,
    /// signal, or tear down that replacement.
    pub async fn wait_for_turn_release(
        &self,
        cancellation: &AgentTurnCancellation,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            // Register before checking to avoid a release between the check and
            // the await (classic lost-notification race).
            let notified = self.turn_release_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.cancellation_is_current(cancellation) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, &mut notified).await.is_err() {
                return !self.cancellation_is_current(cancellation);
            }
        }
    }

    /// Last-resort, generation-safe ownership release used after a bounded stop
    /// grace period. This can only remove the exact turn captured by the stop
    /// request and therefore cannot unlock or cancel a newer generation.
    pub fn force_release_cancelled_turn(&self, cancellation: &AgentTurnCancellation) -> bool {
        let removed = match self.active_turns.lock() {
            Ok(mut turns) => {
                let owns_current_turn = turns
                    .get(cancellation.conversation_id())
                    .is_some_and(|turn| turn.id == cancellation.turn_id());
                if owns_current_turn {
                    turns.remove(cancellation.conversation_id());
                }
                owns_current_turn
            }
            Err(_) => false,
        };
        if removed {
            info!(
                conversation_id = cancellation.conversation_id(),
                turn_id = cancellation.turn_id(),
                "cancelled conversation turn ownership released"
            );
            self.turn_release_notify.notify_waiters();
        }
        removed
    }

    /// The knowledge-mount signature the live agent for `conversation_id` was
    /// last built with, if recorded. `None` means no build has been observed
    /// (e.g. after a restart, or a conversation never started) — callers treat
    /// that as "no live agent to reconcile against".
    pub fn knowledge_signature(&self, conversation_id: &str) -> Option<String> {
        self.knowledge_signatures
            .lock()
            .ok()
            .and_then(|sigs| sigs.get(conversation_id).cloned())
    }

    /// Record the knowledge-mount signature the agent for `conversation_id` was
    /// (re)built with. Called right after `apply_knowledge_mounts` resolves the
    /// mounts for an upcoming build so the NEXT binding change is detectable.
    pub fn set_knowledge_signature(&self, conversation_id: &str, signature: String) {
        if let Ok(mut sigs) = self.knowledge_signatures.lock() {
            sigs.insert(conversation_id.to_owned(), signature);
        }
    }

    /// Drop a conversation's recorded knowledge signature (on delete) so the
    /// map does not grow unbounded across a long-lived process.
    pub fn clear_knowledge_signature(&self, conversation_id: &str) {
        if let Ok(mut sigs) = self.knowledge_signatures.lock() {
            sigs.remove(conversation_id);
        }
    }

    /// Accumulate `tokens` (one turn's `input + output`) into the conversation's
    /// running total. Called by the stream relay when it sees a `TurnCompleted`
    /// metrics event. A poisoned lock or a non-positive count is silently
    /// ignored (observability must never break a turn). Saturating add so a
    /// pathological provider count can never overflow the total.
    pub fn add_turn_tokens(&self, conversation_id: &str, tokens: i64) {
        if tokens <= 0 {
            return;
        }
        if let Ok(mut totals) = self.turn_tokens.lock() {
            let entry = totals.entry(conversation_id.to_owned()).or_insert(0);
            *entry = entry.saturating_add(tokens);
        }
    }

    /// Read AND remove the conversation's accumulated token total. Returns
    /// `None` when nothing was recorded (no `TurnCompleted` seen — e.g. a
    /// non-nomi engine, a turn that errored before completing, or a relay not
    /// wired with the runtime state). An execution attempt calls this once after
    /// its Agent turn settles to persist token usage; removing the
    /// entry keeps the map bounded and prevents a stale read on conversation-id
    /// reuse (execution attempts use a fresh conversation).
    pub fn take_turn_tokens(&self, conversation_id: &str) -> Option<i64> {
        self.turn_tokens
            .lock()
            .ok()
            .and_then(|mut totals| totals.remove(conversation_id))
    }

    /// Evict a conversation's accumulated token entry WITHOUT reading it. Bounds the
    /// map for the benign leak case: an execution-attempt conversation that
    /// accumulated some `TurnCompleted` usage but errored before the attempt called
    /// [`Self::take_turn_tokens`] would otherwise linger until process restart.
    /// Called on conversation delete (alongside [`Self::clear_knowledge_signature`]),
    /// so a removed conversation never keeps a stale accumulator entry. Idempotent —
    /// a no-op when nothing was recorded (the common chat path never records here).
    pub fn clear_turn_tokens(&self, conversation_id: &str) {
        if let Ok(mut totals) = self.turn_tokens.lock() {
            totals.remove(conversation_id);
        }
    }

    pub fn summary_from_parts(
        &self,
        conversation_id: &str,
        runtime_status: Option<ConversationStatus>,
        has_runtime: bool,
        pending_confirmations: usize,
    ) -> ConversationRuntimeSummary {
        let active_turn_started_at = self.active_turn_started_at(conversation_id);
        let turn_is_active = active_turn_started_at.is_some();
        let stop_in_progress = self
            .stop_tombstones
            .lock()
            .map(|fences| fences.contains_key(conversation_id))
            .unwrap_or(true);
        let completion_in_progress = self
            .completion_tombstones
            .lock()
            .map(|fences| fences.contains_key(conversation_id))
            .unwrap_or(true);
        let deletion_in_progress = self
            .deletion_tombstones
            .lock()
            .map(|fences| fences.contains(conversation_id))
            .unwrap_or(true);
        let runtime_build_in_progress = self
            .runtime_builds
            .lock()
            .map(|builds| {
                builds
                    .get(conversation_id)
                    .is_some_and(|leases| {
                        leases
                            .values()
                            .any(|entry| !entry.cancellation.is_cancelled())
                    })
            })
            .unwrap_or(true);

        let state = if stop_in_progress
            || completion_in_progress
            || deletion_in_progress
            || runtime_build_in_progress
        {
            ConversationRuntimeStateKind::Starting
        } else if pending_confirmations > 0 {
            ConversationRuntimeStateKind::WaitingConfirmation
        } else if turn_is_active && runtime_status != Some(ConversationStatus::Running) {
            ConversationRuntimeStateKind::Starting
        } else if turn_is_active {
            ConversationRuntimeStateKind::Running
        } else {
            ConversationRuntimeStateKind::Idle
        };

        let is_processing = state != ConversationRuntimeStateKind::Idle;

        ConversationRuntimeSummary {
            state,
            can_send_message: !is_processing,
            has_runtime,
            runtime_status,
            is_processing,
            pending_confirmations,
            // Only the authoritative active-turn/fence state drives
            // processing. A manager's stale `Running` status remains visible
            // in `runtime_status` for diagnostics but cannot resurrect busy
            // after the exact turn owner completed.
            processing_started_at: if is_processing { active_turn_started_at } else { None },
        }
    }

    fn release(&self, conversation_id: &str, turn_id: u64) -> bool {
        match self.active_turns.lock() {
            Ok(mut active_turns) => {
                let (owns_current_turn, release_blocked) = active_turns
                    .get(conversation_id)
                    .map(|turn| (turn.id == turn_id, turn.release_blocked))
                    .unwrap_or((false, false));
                if owns_current_turn && release_blocked {
                    if let Some(turn) = active_turns.get(conversation_id)
                        && !turn.owner_quiesced.swap(true, Ordering::AcqRel)
                    {
                        turn.owner_quiesced_notify.notify_waiters();
                    }
                    info!(
                        conversation_id,
                        turn_id,
                        "conversation turn release deferred until cancelled runtime teardown"
                    );
                    return false;
                }
                if owns_current_turn {
                    active_turns.remove(conversation_id);
                    info!(conversation_id, turn_id, "conversation runtime turn handle released");
                } else {
                    warn!(
                        conversation_id,
                        turn_id,
                        "stale conversation runtime turn handle could not release current turn"
                    );
                }
                drop(active_turns);
                if owns_current_turn {
                    self.turn_release_notify.notify_waiters();
                }
                owns_current_turn
            }
            Err(_) => {
                warn!(
                    conversation_id,
                    "conversation runtime state lock poisoned while releasing turn"
                );
                false
            }
        }
    }
}

impl AgentTurnHandle {
    pub fn turn_id(&self) -> u64 {
        self.turn_id
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub fn turn_cancellation(&self) -> AgentTurnCancellation {
        AgentTurnCancellation {
            conversation_id: self.conversation_id.clone(),
            turn_id: self.turn_id,
            wire_turn_id: self.wire_turn_id.clone(),
            terminal_msg_id: self.terminal_msg_id.clone(),
            wire_context: self.wire_context.clone(),
            cancellation: self.cancellation.clone(),
            terminal_observed: Arc::clone(&self.terminal_observed),
            terminal_notify: Arc::clone(&self.terminal_notify),
            owner_quiesced: Arc::clone(&self.owner_quiesced),
            owner_quiesced_notify: Arc::clone(&self.owner_quiesced_notify),
            owner_abort_handle: self
                .state
                .upgrade()
                .and_then(|state| state.owner_abort_handle(&self.conversation_id, self.turn_id)),
        }
    }

    /// Advance the wire-terminal generation for an internal continuation or
    /// failover resend while retaining the same outer turn ownership/token.
    /// A stop closes `release_blocked` under the same active-turn lock, so a
    /// segment can never be installed after cancellation began.
    pub fn begin_wire_segment(
        &mut self,
        wire_turn_id: String,
    ) -> Result<AgentTurnCancellation, AppError> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| AppError::Conflict("conversation turn state no longer exists".into()))?;
        let terminal_observed = Arc::new(AtomicU8::new(0));
        let terminal_notify = Arc::new(Notify::new());
        {
            let mut turns = state
                .active_turns
                .lock()
                .map_err(|_| AppError::Internal("conversation runtime state lock poisoned".into()))?;
            let turn = turns
                .get_mut(&self.conversation_id)
                .filter(|turn| turn.id == self.turn_id)
                .ok_or_else(|| AppError::Conflict("conversation turn ownership was released".into()))?;
            if turn.release_blocked || turn.cancellation.is_cancelled() {
                return Err(AppError::Conflict(
                    "conversation turn is stopping; wire continuation rejected".into(),
                ));
            }
            turn.terminal_msg_id = Some(wire_turn_id.clone());
            turn.terminal_observed = Arc::clone(&terminal_observed);
            turn.terminal_notify = Arc::clone(&terminal_notify);
        }
        self.terminal_msg_id = Some(wire_turn_id.clone());
        self.terminal_observed = Arc::clone(&terminal_observed);
        self.terminal_notify = Arc::clone(&terminal_notify);
        Ok(AgentTurnCancellation {
            conversation_id: self.conversation_id.clone(),
            turn_id: self.turn_id,
            wire_turn_id: self.wire_turn_id.clone(),
            terminal_msg_id: Some(wire_turn_id),
            wire_context: self.wire_context.clone(),
            cancellation: self.cancellation.clone(),
            terminal_observed,
            terminal_notify,
            owner_quiesced: Arc::clone(&self.owner_quiesced),
            owner_quiesced_notify: Arc::clone(&self.owner_quiesced_notify),
            owner_abort_handle: state.owner_abort_handle(&self.conversation_id, self.turn_id),
        })
    }

    pub fn release(&mut self) -> bool {
        self.release_inner()
    }

    fn release_inner(&mut self) -> bool {
        if self.released {
            return false;
        }

        let released = self
            .state
            .upgrade()
            .is_some_and(|state| state.release(&self.conversation_id, self.turn_id));
        self.released = true;
        released
    }
}

impl AgentTurnCancellation {
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub fn turn_id(&self) -> u64 {
        self.turn_id
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn wire_turn_id(&self) -> Option<&str> {
        self.wire_turn_id.as_deref()
    }

    pub fn terminal_msg_id(&self) -> Option<&str> {
        self.terminal_msg_id.as_deref()
    }

    pub fn wire_context(&self) -> &TurnWireContext {
        &self.wire_context
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Hard-stop the exact owner future after cooperative cancellation and
    /// transport teardown have exhausted their grace period. A missing handle
    /// means registration raced with stop; registration observes the closed
    /// release gate under the same lock and aborts immediately itself.
    pub fn abort_owner_task(&self) -> bool {
        if let Some(handle) = self.owner_abort_handle.as_ref() {
            handle.abort();
            true
        } else {
            false
        }
    }

    /// Reserve the single authoritative cancelled terminal publication. Both
    /// the normal relay and the bounded stop fallback use this CAS, preventing
    /// duplicate Finish events when a backend terminal races the fallback.
    pub fn try_claim_terminal_surface(&self) -> bool {
        self.terminal_observed
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn mark_terminal_observed(&self) {
        if self.terminal_observed.swap(2, Ordering::AcqRel) != 2 {
            self.terminal_notify.notify_waiters();
        }
    }

    pub async fn wait_for_terminal_observed(&self, timeout: Duration) -> bool {
        let notified = self.terminal_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.terminal_observed.load(Ordering::Acquire) == 2 {
            return true;
        }
        tokio::time::timeout(timeout, &mut notified).await.is_ok()
            || self.terminal_observed.load(Ordering::Acquire) == 2
    }

    pub async fn wait_for_owner_quiesced(&self, timeout: Duration) -> bool {
        let notified = self.owner_quiesced_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.owner_quiesced.load(Ordering::Acquire) {
            return true;
        }
        tokio::time::timeout(timeout, &mut notified).await.is_ok()
            || self.owner_quiesced.load(Ordering::Acquire)
    }
}

impl Drop for AgentTurnHandle {
    fn drop(&mut self) {
        let _ = self.release_inner();
    }
}

impl ConversationDeletionGuard {
    pub fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for ConversationDeletionGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(state) = self.state.upgrade()
            && let Ok(mut tombstones) = state.deletion_tombstones.lock()
        {
            tombstones.remove(&self.conversation_id);
        }
    }
}

impl Drop for UserCancelPreflightGuard {
    fn drop(&mut self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Ok(mut preflights) = state.user_cancel_preflights.lock() else {
            return;
        };
        let key = (self.conversation_id.clone(), self.user_id.clone());
        if let Some(owners) = preflights.get_mut(&key) {
            *owners = owners.saturating_sub(1);
            if *owners == 0 {
                preflights.remove(&key);
            }
        }
    }
}

impl Drop for ConversationStopGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade()
            && let Ok(mut tombstones) = state.stop_tombstones.lock()
        {
            if let Some(owners) = tombstones.get_mut(&self.conversation_id) {
                *owners = owners.saturating_sub(1);
                if *owners == 0 {
                    tombstones.remove(&self.conversation_id);
                }
            }
            drop(tombstones);
            state.cleanup_fence_notify.notify_waiters();
        }
    }
}

impl ConversationStopGuard {
    pub fn cancelled_build_ids(&self) -> &[u64] {
        &self.cancelled_build_ids
    }
}

impl RuntimeBuildLease {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn expected_cancellation_epoch(&self) -> u64 {
        self.expected_cancellation_epoch
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub fn ensure_active(&self) -> Result<(), AppError> {
        if self.cancellation.is_cancelled() {
            Err(AppError::Conflict(format!(
                "conversation {} runtime preparation was cancelled",
                self.conversation_id
            )))
        } else {
            Ok(())
        }
    }

    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if let Some(state) = self.state.upgrade()
            && let Ok(mut builds) = state.runtime_builds.lock()
        {
            if let Some(active) = builds.get_mut(&self.conversation_id) {
                active.remove(&self.id);
                if active.is_empty() {
                    builds.remove(&self.conversation_id);
                }
            }
            drop(builds);
            state.runtime_build_notify.notify_waiters();
        }
    }
}

impl Drop for RuntimeBuildLease {
    fn drop(&mut self) {
        self.release_inner();
    }
}

impl Drop for ConversationCompletionGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade()
            && let Ok(mut tombstones) = state.completion_tombstones.lock()
        {
            if let Some(owners) = tombstones.get_mut(&self.conversation_id) {
                *owners = owners.saturating_sub(1);
                if *owners == 0 {
                    tombstones.remove(&self.conversation_id);
                }
            }
            drop(tombstones);
            state.cleanup_fence_notify.notify_waiters();
        }
    }
}

impl CancelledTurnReleaseGuard {
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelledTurnReleaseGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cancellation.cancel();
        self.cancellation.abort_owner_task();
        if let Some(state) = self.state.upgrade() {
            state.force_release_cancelled_turn(&self.cancellation);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    #[test]
    fn turn_handle_rejects_second_active_turn() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _turn_handle = state.try_acquire_turn("conv-1").expect("first acquisition should win");

        let err = state
            .try_acquire_turn("conv-1")
            .expect_err("second acquisition should fail");
        assert!(err.to_string().contains("already running"));
    }

    #[test]
    fn turn_handle_releases_on_drop() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        {
            let _turn_handle = state.try_acquire_turn("conv-1").expect("turn handle should be acquired");
            assert!(state.has_active_turn("conv-1"));
        }

        assert!(!state.has_active_turn("conv-1"));
        assert!(state.try_acquire_turn("conv-1").is_ok());
    }

    #[tokio::test]
    async fn cancellation_snapshot_is_generation_scoped_and_waits_for_release() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let mut first = state
            .try_acquire_turn_with_wire_id("conv-1", Some("msg-first".to_owned()))
            .expect("first turn");
        let first_cancel = state.active_turn_cancellation("conv-1").expect("cancel snapshot");

        assert_eq!(first_cancel.wire_turn_id(), Some("msg-first"));
        assert!(!first.cancellation_token().is_cancelled());
        first_cancel.cancel();
        assert!(first.cancellation_token().is_cancelled());
        assert!(
            !state
                .wait_for_turn_release(&first_cancel, Duration::from_millis(5))
                .await,
            "cancelling is distinct from releasing ownership"
        );

        assert!(first.release());
        assert!(
            state
                .wait_for_turn_release(&first_cancel, Duration::from_millis(50))
                .await
        );

        let second = state.try_acquire_turn("conv-1").expect("replacement turn");
        assert!(!second.cancellation_token().is_cancelled());
    }

    #[test]
    fn deletion_tombstone_blocks_admission_and_rolls_back_on_failure() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        {
            let _deleting = state
                .begin_conversation_deletion("conv-delete")
                .expect("deletion tombstone");
            assert!(state.try_acquire_turn("conv-delete").is_err());
        }
        assert!(
            state.try_acquire_turn("conv-delete").is_ok(),
            "an uncommitted delete guard must reopen admission"
        );
    }

    #[test]
    fn committed_deletion_tombstone_permanently_rejects_admission() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let mut deleting = state
            .begin_conversation_deletion("conv-deleted")
            .expect("deletion tombstone");
        deleting.commit();
        drop(deleting);
        assert!(state.try_acquire_turn("conv-deleted").is_err());
    }

    #[test]
    fn concurrent_delete_guard_cannot_remove_another_deletes_tombstone() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let first = state
            .begin_conversation_deletion("conv-delete-race")
            .expect("first delete owns tombstone");
        assert!(
            state
                .begin_conversation_deletion("conv-delete-race")
                .is_err(),
            "a second delete must not get a guard capable of removing the first tombstone"
        );
        assert!(state.try_acquire_turn("conv-delete-race").is_err());
        drop(first);
        assert!(state.try_acquire_turn("conv-delete-race").is_ok());
    }

    #[test]
    fn duplicate_stop_joins_single_flight_without_a_second_fence_owner() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let first = state
            .begin_conversation_stop("conv-stop-race")
            .expect("first stop lease")
            .expect("first caller owns stop");
        let second = state
            .begin_conversation_stop("conv-stop-race")
            .expect("second stop lease");
        assert!(second.is_none(), "duplicate must join the existing stop");
        assert!(state.try_acquire_turn("conv-stop-race").is_err());
        drop(first);
        assert!(state.try_acquire_turn("conv-stop-race").is_ok());
    }

    #[tokio::test]
    async fn stop_cancels_exact_runtime_build_and_rejects_its_old_epoch() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let lease = state.begin_runtime_build("conv-build-stop").expect("build lease");
        let old_epoch = lease.expected_cancellation_epoch();
        let stop = state
            .begin_conversation_stop("conv-build-stop")
            .expect("stop admission")
            .expect("stop leader");
        let captured = stop.cancelled_build_ids().to_vec();

        assert!(lease.is_cancelled());
        assert!(
            !state
                .wait_for_runtime_builds(
                    "conv-build-stop",
                    &captured,
                    Duration::from_millis(5),
                )
                .await,
            "the initiator still owns its cancelled build lease"
        );
        drop(lease);
        assert!(
            state
                .wait_for_runtime_builds(
                    "conv-build-stop",
                    &captured,
                    Duration::from_millis(50),
                )
                .await
        );
        drop(stop);
        assert!(
            state
                .try_acquire_turn_with_wire_id_at_epoch(
                    "conv-build-stop",
                    None,
                    Some(old_epoch),
                )
                .is_err(),
            "a pre-stop initiator cannot admit after the stop fence drops"
        );
    }

    #[test]
    fn stale_build_lease_drop_does_not_remove_a_newer_build() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let stale = state.begin_runtime_build("conv-build-aba").expect("stale lease");
        let first_stop = state
            .begin_conversation_stop("conv-build-aba")
            .expect("stop")
            .expect("leader");
        assert!(stale.is_cancelled());
        drop(first_stop);

        let fresh = state.begin_runtime_build("conv-build-aba").expect("fresh lease");
        drop(stale);
        let second_stop = state
            .begin_conversation_stop("conv-build-aba")
            .expect("second stop")
            .expect("second leader");
        assert!(fresh.is_cancelled(), "fresh lease remains registered by its exact id");
        drop(fresh);
        drop(second_stop);
    }

    #[test]
    fn cancelled_but_lingering_build_lease_cannot_resurrect_busy_after_stop() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let lingering = state.begin_runtime_build("conv-build-linger").expect("lease");
        let stop = state
            .begin_conversation_stop("conv-build-linger")
            .expect("stop")
            .expect("leader");
        assert!(lingering.is_cancelled());
        drop(stop);

        let summary = state.summary_from_parts("conv-build-linger", None, false, 0);
        assert_eq!(summary.state, ConversationRuntimeStateKind::Idle);
        assert!(summary.can_send_message);
        drop(lingering);
    }

    #[test]
    fn public_build_cancel_authority_is_requester_scoped_and_never_cancels_private_work() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let alice = state
            .begin_runtime_build_for_requester(
                "conv-build-authority",
                Some("alice".to_owned()),
                true,
            )
            .expect("alice public lease");
        let bob = state
            .begin_runtime_build_for_requester(
                "conv-build-authority",
                Some("bob".to_owned()),
                true,
            )
            .expect("bob public lease");
        let private = state
            .begin_runtime_build_for_requester(
                "conv-build-authority",
                Some("alice".to_owned()),
                false,
            )
            .expect("private lease");

        let mallory = state
            .authorize_in_memory_user_cancel("conv-build-authority", "mallory")
            .expect("mallory authorization");
        assert_eq!(mallory.authority, InMemoryCancelAuthority::None);
        let alice_authorization = state
            .authorize_in_memory_user_cancel("conv-build-authority", "alice")
            .expect("alice authorization");
        assert_eq!(
            alice_authorization.authority,
            InMemoryCancelAuthority::PublicBuilds(vec![alice.id()]),
        );
        assert!(alice.is_cancelled());
        assert!(!bob.is_cancelled());
        assert!(!private.is_cancelled());
    }

    #[test]
    fn empty_cancel_scan_fences_same_requester_builds_until_all_preflights_drop() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let first = state
            .authorize_in_memory_user_cancel("conv-empty-preflight", "alice")
            .expect("first cancel preflight");
        let second = state
            .authorize_in_memory_user_cancel("conv-empty-preflight", "alice")
            .expect("second concurrent cancel preflight");
        assert_eq!(first.authority, InMemoryCancelAuthority::None);
        assert_eq!(second.authority, InMemoryCancelAuthority::None);

        assert!(
            state
                .begin_runtime_build_for_requester(
                    "conv-empty-preflight",
                    Some("alice".to_owned()),
                    true,
                )
                .is_err(),
            "same requester cannot register after cancellation scanned empty",
        );
        let bob = state
            .begin_runtime_build_for_requester(
                "conv-empty-preflight",
                Some("bob".to_owned()),
                true,
            )
            .expect("another requester's public work is independent");
        let private = state
            .begin_runtime_build_for_requester(
                "conv-empty-preflight",
                Some("alice".to_owned()),
                false,
            )
            .expect("private/durable work is outside public cancel authority");

        drop(first.preflight_guard);
        assert!(
            state
                .begin_runtime_build_for_requester(
                    "conv-empty-preflight",
                    Some("alice".to_owned()),
                    true,
                )
                .is_err(),
            "the second concurrent cancel still owns the requester fence",
        );
        drop(second.preflight_guard);
        let retried = state
            .begin_runtime_build_for_requester(
                "conv-empty-preflight",
                Some("alice".to_owned()),
                true,
            )
            .expect("a later explicit retry is admitted after every preflight ends");
        drop((bob, private, retried));
    }

    #[test]
    fn user_cancel_is_linearized_with_public_build_to_active_turn_handoff() {
        // Cancellation wins the shared linearization gate: admission must
        // observe the preparation token as cancelled while it holds that same
        // gate and cannot resurrect a turn after the stop was acknowledged.
        let cancel_first = Arc::new(ConversationRuntimeStateService::default());
        let cancelled_build = cancel_first
            .begin_runtime_build_for_requester(
                "conv-cancel-first",
                Some("owner".to_owned()),
                true,
            )
            .expect("public build lease");
        let cancelled_token = cancelled_build.cancellation_token();
        let cancel_authorization = cancel_first
            .authorize_in_memory_user_cancel("conv-cancel-first", "owner")
            .expect("cancel authorization");
        assert_eq!(
            cancel_authorization.authority,
            InMemoryCancelAuthority::PublicBuilds(vec![cancelled_build.id()]),
        );
        assert!(cancel_authorization.preflight_guard.is_some());
        assert!(
            cancel_first
                .begin_runtime_build_for_requester(
                    "conv-cancel-first",
                    Some("owner".to_owned()),
                    true,
                )
                .is_err(),
            "an existing-build cancel fences scan-to-ack registration",
        );
        assert!(
            cancel_first
                .try_acquire_turn_with_wire_context_at_epoch_and_owner(
                    "conv-cancel-first",
                    None,
                    TurnWireContext::default(),
                    Some(cancelled_build.expected_cancellation_epoch()),
                    Some("owner".to_owned()),
                    true,
                    Some(&cancelled_token),
                )
                .is_err()
        );
        drop(cancel_authorization.preflight_guard);
        assert!(
            cancel_first
                .begin_runtime_build_for_requester(
                    "conv-cancel-first",
                    Some("owner".to_owned()),
                    true,
                )
                .is_ok(),
            "a later explicit request is legal after cancellation returns",
        );

        // Admission wins the gate: cancellation must discover the public
        // active turn and choose the exact-generation stop path rather than
        // returning after cancelling only the now-finished build phase.
        let admission_first = Arc::new(ConversationRuntimeStateService::default());
        let admitted_build = admission_first
            .begin_runtime_build_for_requester(
                "conv-admission-first",
                Some("owner".to_owned()),
                true,
            )
            .expect("public build lease");
        let admitted_token = admitted_build.cancellation_token();
        let _turn = admission_first
            .try_acquire_turn_with_wire_context_at_epoch_and_owner(
                "conv-admission-first",
                None,
                TurnWireContext::default(),
                Some(admitted_build.expected_cancellation_epoch()),
                Some("owner".to_owned()),
                true,
                Some(&admitted_token),
            )
            .expect("turn admission");
        let admitted_authorization = admission_first
            .authorize_in_memory_user_cancel("conv-admission-first", "owner")
            .expect("cancel authorization");
        assert_eq!(
            admitted_authorization.authority,
            InMemoryCancelAuthority::ActiveTurn,
        );
        assert!(admitted_authorization.preflight_guard.is_none());
        assert!(!admitted_build.is_cancelled());
    }

    #[test]
    fn cached_active_owner_allows_only_the_correct_cancel_origin() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _turn = state
            .try_acquire_turn_with_wire_context_at_epoch_and_owner(
                "conv-owner-cancel",
                None,
                TurnWireContext::default(),
                None,
                Some("owner".to_owned()),
                false,
                None,
            )
            .expect("private turn");

        assert!(!state.active_turn_allows_cancel("conv-owner-cancel", "other", false));
        assert!(!state.active_turn_allows_cancel("conv-owner-cancel", "owner", true));
        assert!(state.active_turn_allows_cancel("conv-owner-cancel", "owner", false));
    }

    #[test]
    fn completion_fence_stays_busy_and_stop_joins_until_idle_event_boundary() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let mut turn = state.try_acquire_turn("conv-completion").expect("turn");
        let completion = state
            .begin_turn_completion("conv-completion", turn.turn_id())
            .expect("completion admission")
            .expect("completion owner");
        assert!(turn.release());

        let summary = state.summary_from_parts("conv-completion", None, false, 0);
        assert!(summary.is_processing);
        assert!(!summary.can_send_message);
        assert!(
            state
                .begin_conversation_stop("conv-completion")
                .expect("stop joins completion")
                .is_none()
        );
        drop(completion);
        let idle = state.summary_from_parts("conv-completion", None, false, 0);
        assert!(!idle.is_processing);
        assert!(idle.can_send_message);
        assert!(state.try_acquire_turn("conv-completion").is_ok());
    }

    #[tokio::test]
    async fn completion_event_linearizes_when_no_stop_fence_exists() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let mut turn = state.try_acquire_turn("conv-completion-event").expect("turn");
        let completion = state
            .begin_turn_completion("conv-completion-event", turn.turn_id())
            .expect("completion admission")
            .expect("completion owner");
        assert!(turn.release());

        let published = Arc::new(AtomicBool::new(false));
        let published_in_action = Arc::clone(&published);
        assert!(
            state
                .linearize_cleanup_event(
                    "conv-completion-event",
                    0,
                    1,
                    Duration::from_millis(100),
                    move || {
                        published_in_action.store(true, Ordering::Release);
                        drop(completion);
                    },
                )
                .await,
            "an absent stop-tombstone entry must count as zero owners"
        );
        assert!(published.load(Ordering::Acquire));
        assert!(
            state
                .wait_for_cleanup_fences("conv-completion-event", Duration::from_millis(100))
                .await
        );
    }

    #[tokio::test]
    async fn blocked_owner_release_reports_quiescence_before_force_release() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let mut handle = state
            .try_acquire_turn_with_wire_id("conv-quiesce", Some("turn-root".to_owned()))
            .expect("turn");
        let cancellation = state
            .begin_turn_cancellation("conv-quiesce")
            .expect("cancel snapshot");
        assert!(!handle.release(), "stop gate owns the actual release");
        assert!(
            cancellation
                .wait_for_owner_quiesced(Duration::from_millis(20))
                .await
        );
        assert!(state.force_release_cancelled_turn(&cancellation));
    }

    #[test]
    fn stale_handle_cannot_release_replacement_generation() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let first = state.try_acquire_turn("conv-1").expect("first turn");
        let first_cancel = state.active_turn_cancellation("conv-1").expect("cancel snapshot");

        assert!(state.force_release_cancelled_turn(&first_cancel));
        let second = state.try_acquire_turn("conv-1").expect("replacement turn");
        drop(first);

        assert!(state.has_active_turn("conv-1"));
        assert!(!second.cancellation_token().is_cancelled());
    }

    #[test]
    fn summary_uses_active_turn_as_starting_state() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _turn_handle = state.try_acquire_turn("conv-1").expect("turn handle should be acquired");

        let summary = state.summary_from_parts("conv-1", None, false, 0);

        assert_eq!(summary.state, ConversationRuntimeStateKind::Starting);
        assert!(summary.is_processing);
        assert!(!summary.can_send_message);
        assert!(
            summary.processing_started_at.is_some(),
            "an active turn must expose its start time"
        );
    }

    #[test]
    fn stale_manager_running_without_an_active_turn_is_authoritatively_idle() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let summary = state.summary_from_parts(
            "conv-stale-runtime",
            Some(ConversationStatus::Running),
            true,
            0,
        );

        assert_eq!(summary.state, ConversationRuntimeStateKind::Idle);
        assert!(!summary.is_processing);
        assert!(summary.can_send_message);
        assert_eq!(summary.runtime_status, Some(ConversationStatus::Running));
    }

    #[test]
    fn summary_exposes_turn_start_time_and_clears_when_idle() {
        let state = Arc::new(ConversationRuntimeStateService::default());

        // Idle: no active turn, no start time.
        let idle = state.summary_from_parts("conv-1", None, false, 0);
        assert_eq!(idle.state, ConversationRuntimeStateKind::Idle);
        assert!(!idle.is_processing);
        assert_eq!(idle.processing_started_at, None);

        // Active: start time matches the recorded acquisition time.
        let turn_handle = state.try_acquire_turn("conv-1").expect("turn handle should be acquired");
        let expected = state.active_turn_started_at("conv-1");
        assert!(expected.is_some());

        let running = state.summary_from_parts("conv-1", None, false, 0);
        assert!(running.is_processing);
        assert_eq!(running.processing_started_at, expected);

        // Released: back to idle, start time gone.
        drop(turn_handle);
        let after = state.summary_from_parts("conv-1", None, false, 0);
        assert!(!after.is_processing);
        assert_eq!(after.processing_started_at, None);
    }

    #[test]
    fn turn_tokens_accumulate_and_take_removes() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        // Nothing recorded → None.
        assert_eq!(state.take_turn_tokens("conv-1"), None);

        // Two turns accumulate additively (continuation / failover resend).
        state.add_turn_tokens("conv-1", 120);
        state.add_turn_tokens("conv-1", 80);
        // take returns the cumulative total AND removes the entry.
        assert_eq!(state.take_turn_tokens("conv-1"), Some(200));
        // Second take is None — the entry was removed (bounded map, no stale read).
        assert_eq!(state.take_turn_tokens("conv-1"), None);
    }

    #[test]
    fn turn_tokens_ignores_non_positive() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        state.add_turn_tokens("conv-1", 0);
        state.add_turn_tokens("conv-1", -5);
        // No positive count ever recorded → still None.
        assert_eq!(state.take_turn_tokens("conv-1"), None);
        // A later positive count is recorded normally.
        state.add_turn_tokens("conv-1", 42);
        assert_eq!(state.take_turn_tokens("conv-1"), Some(42));
    }

    #[test]
    fn turn_tokens_keyed_per_conversation() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        state.add_turn_tokens("conv-a", 10);
        state.add_turn_tokens("conv-b", 99);
        assert_eq!(state.take_turn_tokens("conv-a"), Some(10));
        // conv-b is untouched by conv-a's take.
        assert_eq!(state.take_turn_tokens("conv-b"), Some(99));
    }

    // C item 5: clear_turn_tokens evicts an accumulated entry without reading it
    // (the benign-leak bound for an errored conversation), and is idempotent.
    #[test]
    fn clear_turn_tokens_evicts_entry() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        state.add_turn_tokens("conv-x", 150);
        // Clear (not take): the entry is gone, a later take sees None.
        state.clear_turn_tokens("conv-x");
        assert_eq!(state.take_turn_tokens("conv-x"), None);
        // Idempotent: clearing a never-recorded / already-cleared conv is a no-op.
        state.clear_turn_tokens("conv-x");
        state.clear_turn_tokens("never-seen");
        assert_eq!(state.take_turn_tokens("never-seen"), None);
    }
}
