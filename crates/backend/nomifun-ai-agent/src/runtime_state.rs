//! Shared mutable runtime state for all Agent managers.
//!
//! Each `*AgentManager` composes a single `AgentRuntimeState` to hold its
//! identity (`conversation_id`, `workspace`), status, last-activity
//! timestamp, and the event broadcast channel. This collapses five
//! fields that were repeated across every manager into one value
//! object, and makes the invariant `emit_finish` = (status ← Finished
//! AND broadcast Finish) enforceable in a single place.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::{Notify, broadcast};

use nomifun_api_types::{AgentErrorCode, AgentStreamErrorData};
use nomifun_common::{ConversationStatus, TimestampMs, now_ms};

use crate::protocol::events::{AgentStreamEvent, ErrorEventData, FinishEventData, TurnStopReason};

#[derive(Clone)]
pub struct AgentRuntimeState {
    conversation_id: Arc<str>,
    workspace: Arc<str>,
    status: Arc<RwLock<Option<ConversationStatus>>>,
    last_activity: Arc<AtomicI64>,
    event_tx: broadcast::Sender<AgentStreamEvent>,
    finished_notify: Arc<Notify>,
    /// False once a manager's permanent process/connection relay exits or
    /// loses buffered events. Unlike turn status this is not resettable: a
    /// cached runtime without its sole relay must be evicted and rebuilt.
    transport_healthy: Arc<AtomicBool>,
}

impl AgentRuntimeState {
    /// Construct a new runtime.
    ///
    /// `channel_capacity` is the broadcast buffer size for `event_tx`.
    pub fn new(conversation_id: impl Into<String>, workspace: impl Into<String>, channel_capacity: usize) -> Self {
        let (event_tx, _) = broadcast::channel(channel_capacity);
        Self {
            conversation_id: Arc::from(conversation_id.into()),
            workspace: Arc::from(workspace.into()),
            status: Arc::new(RwLock::new(None)),
            last_activity: Arc::new(AtomicI64::new(now_ms())),
            event_tx,
            finished_notify: Arc::new(Notify::new()),
            transport_healthy: Arc::new(AtomicBool::new(true)),
        }
    }

    // ── Read ────────────────────────────────────────────────────────────

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    pub fn status(&self) -> Option<ConversationStatus> {
        *self.status.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn last_activity_at(&self) -> TimestampMs {
        self.last_activity.load(Ordering::Relaxed)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }

    pub fn is_transport_healthy(&self) -> bool {
        self.transport_healthy.load(Ordering::Acquire)
    }

    pub fn mark_transport_broken(&self) {
        self.transport_healthy.store(false, Ordering::Release);
    }

    /// Wait until a running turn has completed, bounded by `timeout`.
    ///
    /// The notification future is registered before checking status, avoiding
    /// the classic check-then-sleep race when a turn finishes concurrently.
    /// A runtime with no active turn returns immediately.
    pub async fn wait_until_finished(&self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.finished_notify.notified();
            if !matches!(self.status(), Some(ConversationStatus::Running)) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
                return !matches!(self.status(), Some(ConversationStatus::Running));
            }
        }
    }

    /// Crate-private accessor for the broadcast sender, exposed so
    /// managers can clone it where a `broadcast::Sender<..>` clone is
    /// needed directly (e.g. passing into an SDK builder). Prefer
    /// `emit` / `emit_finish` / `emit_error` for event emission.
    #[allow(dead_code)]
    pub(crate) fn event_sender(&self) -> broadcast::Sender<AgentStreamEvent> {
        self.event_tx.clone()
    }

    // State transitions and event emission are centralized here.

    pub fn bump_activity(&self) {
        self.last_activity.store(now_ms(), Ordering::Relaxed);
    }

    /// Transition to `status`. Finished is absorbing — subsequent
    /// transitions from Finished to anything else are no-ops (including
    /// Finished → Finished, which is idempotent).
    pub fn transition_to(&self, status: ConversationStatus) {
        let mut guard = self.status.write().unwrap_or_else(|e| e.into_inner());
        if matches!(*guard, Some(ConversationStatus::Finished)) {
            // Finished is the absorbing state; ignore further writes.
            return;
        }
        *guard = Some(status);
    }

    /// Force-reset the status so a new turn can emit Finish again.
    /// Only intended for multi-turn agents (e.g. nomi) where the same
    /// runtime instance handles successive user messages.
    pub fn reset_for_new_turn(&self, status: ConversationStatus) {
        let mut guard = self.status.write().unwrap_or_else(|e| e.into_inner());
        *guard = Some(status);
    }

    pub fn emit(&self, event: AgentStreamEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Atomic: set status ← Finished AND broadcast `Finish(session_id)`.
    /// Idempotent in the Finished absorbing state (no-op).
    pub fn emit_finish(&self, session_id: Option<String>) {
        self.emit_finish_with_reason(session_id, None);
    }

    /// Like [`Self::emit_finish`] but carries an explicit `stop_reason` so
    /// downstream consumers (StreamRelay, AutoWork, IDMM) can tell a clean
    /// `EndTurn` apart from a refusal / truncation / user cancel. Idempotent
    /// in the Finished absorbing state (no-op) — which also means a LATE
    /// terminal event (e.g. the ACP protocol's delayed `Finish(Cancelled)`
    /// after `cancel()` already finished the turn) is absorbed instead of
    /// leaking into the next turn's subscription.
    pub fn emit_finish_with_reason(&self, session_id: Option<String>, stop_reason: Option<TurnStopReason>) {
        let already_finished = {
            let mut guard = self.status.write().unwrap_or_else(|e| e.into_inner());
            let was_finished = matches!(*guard, Some(ConversationStatus::Finished));
            if !was_finished {
                *guard = Some(ConversationStatus::Finished);
            }
            was_finished
        };
        if already_finished {
            return;
        }
        let _ = self
            .event_tx
            .send(AgentStreamEvent::Finish(FinishEventData {
                session_id,
                stop_reason,
            }));
        self.finished_notify.notify_waiters();
    }

    /// Atomic: set status ← Finished AND broadcast `Error { message }`.
    /// Idempotent in the Finished absorbing state (no-op).
    pub fn emit_error(&self, message: impl Into<String>) {
        self.emit_error_data(ErrorEventData::legacy(message, None));
    }

    /// Publish an unrecoverable event-transport integrity failure. Managers
    /// use this only when their permanent relay exits or loses buffered events,
    /// never for provider/API errors that may be retried on the same runtime.
    /// The conversation service recognizes this exact code and evicts the dead
    /// runtime before releasing turn admission.
    pub fn emit_stream_broken(&self, message: impl Into<String>) {
        self.mark_transport_broken();
        self.emit_error_data(ErrorEventData::legacy(
            message,
            Some(AgentErrorCode::NomifunStreamBroken),
        ));
    }

    /// Atomic: set status ← Finished AND broadcast the structured error payload.
    /// Idempotent in the Finished absorbing state (no-op).
    pub fn emit_error_data(&self, data: AgentStreamErrorData) {
        let already_finished = {
            let mut guard = self.status.write().unwrap_or_else(|e| e.into_inner());
            let was_finished = matches!(*guard, Some(ConversationStatus::Finished));
            if !was_finished {
                *guard = Some(ConversationStatus::Finished);
            }
            was_finished
        };
        if already_finished {
            return;
        }
        let _ = self.event_tx.send(AgentStreamEvent::Error(data));
        self.finished_notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> AgentRuntimeState {
        AgentRuntimeState::new("conv-1", "/tmp/workspace", 8)
    }

    #[tokio::test]
    async fn new_has_no_status_and_current_last_activity() {
        let rt = runtime();
        assert_eq!(rt.conversation_id(), "conv-1");
        assert_eq!(rt.workspace(), "/tmp/workspace");
        assert_eq!(rt.status(), None);
        // last_activity_at should be close to `now_ms()` (within a second).
        let diff = now_ms() - rt.last_activity_at();
        assert!(diff.abs() < 1000);
    }

    #[tokio::test]
    async fn bump_activity_monotonic() {
        let rt = runtime();
        let before = rt.last_activity_at();
        std::thread::sleep(std::time::Duration::from_millis(2));
        rt.bump_activity();
        let after = rt.last_activity_at();
        assert!(after >= before);
    }

    #[tokio::test]
    async fn emit_finish_transitions_and_broadcasts() {
        let rt = runtime();
        let mut rx = rt.subscribe();
        rt.emit_finish(Some("sess-1".into()));
        assert_eq!(rt.status(), Some(ConversationStatus::Finished));
        let ev = rx.recv().await.expect("finish event");
        match ev {
            AgentStreamEvent::Finish(data) => {
                assert_eq!(data.session_id.as_deref(), Some("sess-1"));
            }
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_error_transitions_and_broadcasts() {
        let rt = runtime();
        let mut rx = rt.subscribe();
        rt.emit_error("boom");
        assert_eq!(rt.status(), Some(ConversationStatus::Finished));
        let ev = rx.recv().await.expect("error event");
        match ev {
            AgentStreamEvent::Error(data) => {
                assert_eq!(data.message, "boom");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_broken_error_is_structured_for_runtime_eviction() {
        let rt = runtime();
        let mut rx = rt.subscribe();
        rt.emit_stream_broken("event relay lost its terminal boundary");

        let event = rx.recv().await.expect("stream-broken error event");
        let AgentStreamEvent::Error(data) = event else {
            panic!("expected structured stream-broken error");
        };
        assert_eq!(data.code, Some(AgentErrorCode::NomifunStreamBroken));
        assert_eq!(rt.status(), Some(ConversationStatus::Finished));
    }

    #[tokio::test]
    async fn emit_finish_is_idempotent_in_finished_state() {
        let rt = runtime();
        let mut rx = rt.subscribe();

        rt.emit_finish(None);
        // Drain the first event.
        let _ = rx.recv().await.unwrap();

        // Second call should be a no-op: status stays Finished, no new
        // broadcast.
        rt.emit_finish(Some("ignored".into()));
        assert_eq!(rt.status(), Some(ConversationStatus::Finished));

        // Nothing else should have landed on the receiver.
        let res = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(res.is_err(), "expected no additional broadcast, got {res:?}");
    }

    #[tokio::test]
    async fn emit_finish_with_reason_carries_reason_and_absorbs_late_duplicates() {
        let rt = runtime();
        let mut rx = rt.subscribe();

        rt.emit_finish_with_reason(None, Some(TurnStopReason::Cancelled));
        assert_eq!(rt.status(), Some(ConversationStatus::Finished));
        match rx.recv().await.unwrap() {
            AgentStreamEvent::Finish(d) => assert_eq!(d.stop_reason, Some(TurnStopReason::Cancelled)),
            other => panic!("expected Finish, got {other:?}"),
        }

        // Absorbing: a late terminal event for the same turn (e.g. the ACP
        // protocol's delayed Finish after cancel) must NOT broadcast — it
        // would leak into the next turn's subscription.
        rt.emit_finish_with_reason(None, Some(TurnStopReason::EndTurn));
        let res = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(res.is_err(), "expected no additional broadcast, got {res:?}");
    }

    #[tokio::test]
    async fn emit_error_after_finish_is_noop() {
        let rt = runtime();
        let mut rx = rt.subscribe();

        rt.emit_finish(None);
        let _ = rx.recv().await.unwrap();

        rt.emit_error("late error — should be ignored");
        assert_eq!(rt.status(), Some(ConversationStatus::Finished));

        let res = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn reset_for_new_turn_overrides_finished() {
        let rt = runtime();
        rt.emit_finish(None);
        assert_eq!(rt.status(), Some(ConversationStatus::Finished));

        rt.reset_for_new_turn(ConversationStatus::Running);
        assert_eq!(rt.status(), Some(ConversationStatus::Running));
    }

    #[tokio::test]
    async fn reset_for_new_turn_then_emit_finish_sends_event() {
        let rt = runtime();
        rt.emit_finish(None);
        assert_eq!(rt.status(), Some(ConversationStatus::Finished));

        rt.reset_for_new_turn(ConversationStatus::Running);
        let mut rx = rt.subscribe();
        rt.emit_finish(Some("sess-2".into()));
        assert_eq!(rt.status(), Some(ConversationStatus::Finished));

        let ev = rx.recv().await.expect("finish event after reset");
        match ev {
            AgentStreamEvent::Finish(data) => {
                assert_eq!(data.session_id.as_deref(), Some("sess-2"));
            }
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wait_until_finished_blocks_running_turn_until_terminal_event() {
        let rt = runtime();
        rt.reset_for_new_turn(ConversationStatus::Running);

        let waiter_runtime = rt.clone();
        let waiter = tokio::spawn(async move {
            waiter_runtime
                .wait_until_finished(std::time::Duration::from_secs(1))
                .await
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "running turn must keep teardown barrier closed");

        rt.emit_finish_with_reason(None, Some(TurnStopReason::Cancelled));
        assert!(waiter.await.unwrap(), "terminal event should open teardown barrier");
    }
}
