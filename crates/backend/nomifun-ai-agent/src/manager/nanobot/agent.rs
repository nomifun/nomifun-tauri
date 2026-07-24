use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nomifun_common::{AgentKillReason, AgentType, AppError, Confirmation, ConversationStatus, ErrorChain, TimestampMs};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, broadcast};
use tracing::{debug, error, info, warn};

use nomifun_common::CommandSpec;

use crate::runtime_state::{AgentRuntimeState, AgentRuntimeTurn};
use crate::capability::cli_process::CliAgentProcess;
use crate::manager::process_registry::register_session_process;
use crate::protocol::events::{AgentStreamEvent, ErrorEventData, TurnStopReason};
use crate::protocol::send_error::AgentSendError;
use crate::types::{SendMessageData, inject_runtime_preset_context};
use std::path::PathBuf;

/// Grace period before force-killing a Nanobot process (ms).
const NANOBOT_KILL_GRACE_MS: u64 = 500;

/// Internal mutable state for the Nanobot agent.
struct NanobotState {
    has_messages: bool,
    runtime_turn: Option<AgentRuntimeTurn>,
}

/// Nanobot's raw event protocol has no run/message identity. Once a turn is
/// cancelled or reaches any terminal frame, this exact process may never own a
/// successor: a delayed Start/content/tool frame would otherwise be
/// indistinguishable from successor output.
fn quarantine_cancelled_turn(
    state: &mut NanobotState,
    turn_boundary_recycle_required: &AtomicBool,
) -> Option<AgentRuntimeTurn> {
    turn_boundary_recycle_required.store(true, Ordering::Release);
    let turn = state.runtime_turn.take();
    turn
}

/// Relay one parsed CLI frame under the exact turn token currently owned by
/// this process. Terminal frames atomically detach that token and permanently
/// close the process' admission authority. Non-terminal frames are accepted
/// only while the token is still the runtime's active Running turn.
fn relay_event_for_exact_turn(
    runtime: &AgentRuntimeState,
    state: &mut NanobotState,
    turn_boundary_recycle_required: &AtomicBool,
    event: AgentStreamEvent,
) -> bool {
    match event {
        AgentStreamEvent::Finish(data) => {
            turn_boundary_recycle_required.store(true, Ordering::Release);
            let Some(runtime_turn) = state.runtime_turn.take() else {
                return false;
            };
            runtime.emit_finish_for_turn(runtime_turn, data.session_id, data.stop_reason)
        }
        AgentStreamEvent::Error(data) => {
            turn_boundary_recycle_required.store(true, Ordering::Release);
            let Some(runtime_turn) = state.runtime_turn.take() else {
                return false;
            };
            runtime.emit_error_data_for_turn(runtime_turn, data)
        }
        event => {
            let Some(runtime_turn) = state.runtime_turn else {
                return false;
            };
            runtime.emit_for_turn(runtime_turn, event)
        }
    }
}

fn relay_broken_stream_error_for_exact_turn(
    runtime: &AgentRuntimeState,
    state: &mut NanobotState,
    data: ErrorEventData,
) -> bool {
    let Some(runtime_turn) = state.runtime_turn.take() else {
        return false;
    };
    runtime.emit_error_data_for_turn(runtime_turn, data)
}

fn admit_turn(
    runtime: &AgentRuntimeState,
    state: &mut NanobotState,
    turn_boundary_recycle_required: &AtomicBool,
) -> Result<AgentRuntimeTurn, AgentSendError> {
    if turn_boundary_recycle_required.load(Ordering::Acquire) {
        return Err(AgentSendError::stream_broken(
            "Nanobot's completed JSON-lines process must be recycled before another turn",
        ));
    }
    if !runtime.is_transport_healthy() {
        return Err(AgentSendError::stream_broken(
            "Nanobot's process/event relay is no longer running",
        ));
    }
    if state.runtime_turn.is_some() {
        return Err(AgentSendError::from_app_error(AppError::Conflict(
            "Nanobot already owns an active exact runtime turn".into(),
        )));
    }

    let runtime_turn = runtime.reset_for_new_turn(ConversationStatus::Running);
    state.has_messages = true;
    state.runtime_turn = Some(runtime_turn);
    Ok(runtime_turn)
}

/// Manages a Nanobot CLI agent subprocess.
///
/// Nanobot is the simplest agent type:
/// - CLI blocking mode (fire-and-forget)
/// - No YOLO mode support
/// - No confirmation system
/// - Single response stream only
pub struct NanobotAgentManager {
    runtime: AgentRuntimeState,
    process: Arc<CliAgentProcess>,
    state: RwLock<NanobotState>,
    raw_rx: Mutex<Option<broadcast::Receiver<Value>>>,
    /// Immutable preset contract for the first prompt handled by this
    /// single-process Nanobot runtime.
    preset_context: Option<String>,
    /// Set at the first terminal boundary. Unlike `transport_healthy`, this is
    /// not a crash signal: the registry performs a deliberate exact teardown
    /// before admitting the next turn.
    turn_boundary_recycle_required: AtomicBool,
}

impl NanobotAgentManager {
    /// Create a new Nanobot agent by spawning the CLI subprocess.
    pub async fn new(
        conversation_id: String,
        workspace: String,
        cli_path: PathBuf,
        data_dir: PathBuf,
        preset_context: Option<String>,
    ) -> Result<Self, AppError> {
        let spawn_config = Self::build_spawn_config(cli_path, &workspace);
        let command_preview = spawn_config.command.display().to_string();
        let process = Arc::new(CliAgentProcess::spawn(spawn_config).await?);
        register_session_process(
            &data_dir,
            Arc::clone(&process),
            conversation_id.clone(),
            AgentType::Nanobot,
            None,
            Some(command_preview),
        )?;

        let raw_rx = process
            .take_initial_receiver()
            .expect("Initial receiver should be available immediately after spawn");
        let runtime = AgentRuntimeState::new(conversation_id, workspace, 256);

        Ok(Self {
            runtime,
            process,
            state: RwLock::new(NanobotState {
                has_messages: false,
                runtime_turn: None,
            }),
            raw_rx: Mutex::new(Some(raw_rx)),
            preset_context,
            turn_boundary_recycle_required: AtomicBool::new(false),
        })
    }

    fn build_spawn_config(cli_path: PathBuf, workspace: &str) -> CommandSpec {
        CommandSpec {
            command: cli_path,
            args: vec![],
            env: vec![],
            cwd: Some(workspace.to_owned()),
        }
    }

    pub(crate) fn requires_turn_boundary_recycle(&self) -> bool {
        self.turn_boundary_recycle_required.load(Ordering::Acquire)
    }

    async fn terminate_active_turn_for_broken_stream(&self, message: String) {
        self.runtime.mark_transport_broken();
        self.turn_boundary_recycle_required
            .store(false, Ordering::Release);
        let data = AgentSendError::stream_broken(message).stream_error().clone();
        {
            let mut state = self.state.write().await;
            let _ = relay_broken_stream_error_for_exact_turn(
                &self.runtime,
                &mut state,
                data,
            );
        }
    }

    /// Start the event relay (call after wrapping in Arc).
    pub fn start_relay(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.run_event_relay().await;
        });
    }

    async fn run_event_relay(self: Arc<Self>) {
        let mut raw_rx = {
            let mut guard = self.raw_rx.lock().await;
            match guard.take() {
                Some(rx) => rx,
                None => {
                    warn!(
                        conversation_id = %self.runtime.conversation_id(),
                        "Nanobot event relay already started"
                    );
                    return;
                }
            }
        };

        loop {
            let raw_event = tokio::select! {
                event = raw_rx.recv() => Some(event),
                status = self.process.wait_for_exit() => {
                    if self.runtime.status() == Some(ConversationStatus::Running) {
                        let detail = status
                            .map(|status| format!(" ({status})"))
                            .unwrap_or_default();
                        self.terminate_active_turn_for_broken_stream(format!(
                            "Nanobot process exited before the turn completed{detail}"
                        ))
                        .await;
                    } else {
                        self.runtime.mark_transport_broken();
                        self.turn_boundary_recycle_required
                            .store(true, Ordering::Release);
                    }
                    None
                }
            };
            let Some(raw_event) = raw_event else {
                break;
            };
            match raw_event {
                Ok(raw_json) => {
                    if self.handle_raw_event(raw_json).await {
                        self.runtime.bump_activity();
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        conversation_id = %self.runtime.conversation_id(),
                        lagged = n,
                        "Nanobot event relay lagged"
                    );
                    self.terminate_active_turn_for_broken_stream(format!(
                        "Nanobot event relay lost {n} buffered event(s)"
                    ))
                    .await;
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!(
                        conversation_id = %self.runtime.conversation_id(),
                        "Nanobot CLI event channel closed"
                    );
                    if self.runtime.status() == Some(ConversationStatus::Running) {
                        self.terminate_active_turn_for_broken_stream(
                            "Nanobot event channel closed before the turn completed".into(),
                        )
                        .await;
                    } else {
                        self.runtime.mark_transport_broken();
                        self.turn_boundary_recycle_required
                            .store(true, Ordering::Release);
                    }
                    break;
                }
            }
        }

    }

    async fn handle_raw_event(&self, raw: Value) -> bool {
        let stream_event = match serde_json::from_value::<AgentStreamEvent>(raw.clone()) {
            Ok(event) => event,
            Err(_) => {
                debug!(
                    conversation_id = %self.runtime.conversation_id(),
                    "Unrecognized Nanobot event, skipping"
                );
                return false;
            }
        };

        let emitted = {
            let mut state = self.state.write().await;
            relay_event_for_exact_turn(
                &self.runtime,
                &mut state,
                &self.turn_boundary_recycle_required,
                stream_event,
            )
        };
        if !emitted {
            debug!(
                conversation_id = %self.runtime.conversation_id(),
                "Discarding stale or unowned Nanobot CLI frame"
            );
        }
        emitted
    }
}

#[async_trait::async_trait]
impl crate::runtime_handle::AgentRuntimeControl for NanobotAgentManager {
    fn agent_type(&self) -> AgentType {
        AgentType::Nanobot
    }

    fn conversation_id(&self) -> &str {
        self.runtime.conversation_id()
    }

    fn workspace(&self) -> &str {
        self.runtime.workspace()
    }

    fn status(&self) -> Option<ConversationStatus> {
        self.runtime.status()
    }

    fn is_transport_healthy(&self) -> bool {
        self.runtime.is_transport_healthy()
    }

    fn last_activity_at(&self) -> TimestampMs {
        self.runtime.last_activity_at()
    }

    fn touch_activity(&self) {
        self.runtime.bump_activity();
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.runtime.subscribe()
    }

    async fn send_message(&self, mut data: SendMessageData) -> Result<(), AgentSendError> {
        let (runtime_turn, is_first) = {
            let mut state = self.state.write().await;
            let is_first = !state.has_messages;
            let runtime_turn = admit_turn(
                &self.runtime,
                &mut state,
                &self.turn_boundary_recycle_required,
            )?;
            (runtime_turn, is_first)
        };
        self.runtime.bump_activity();

        let transport_broken = !self.runtime.is_transport_healthy();
        let terminal_boundary_closed = self
            .turn_boundary_recycle_required
            .load(Ordering::Acquire);
        if transport_broken || terminal_boundary_closed {
            let error = AgentSendError::stream_broken(
                "Nanobot's process/event relay closed during exact turn admission",
            );
            let mut state = self.state.write().await;
            if state.runtime_turn == Some(runtime_turn) {
                if transport_broken {
                    self.turn_boundary_recycle_required
                        .store(false, Ordering::Release);
                    relay_broken_stream_error_for_exact_turn(
                        &self.runtime,
                        &mut state,
                        error.stream_error().clone(),
                    );
                } else {
                    relay_event_for_exact_turn(
                        &self.runtime,
                        &mut state,
                        &self.turn_boundary_recycle_required,
                        AgentStreamEvent::Error(error.stream_error().clone()),
                    );
                }
            }
            return Err(error);
        }

        data.content = inject_runtime_preset_context(
            data.content,
            self.preset_context.as_deref(),
            is_first,
        );

        // Nanobot uses fire-and-forget: send the message, CLI blocks until complete
        let payload = json!({
            "type": "send.message",
            "data": {
                "content": data.content,
                "msgId": data.msg_id,
            }
        });

        match self.process.send(&payload).await {
            Ok(()) => Ok(()),
            Err(err) => {
                error!(
                    conversation_id = %self.runtime.conversation_id(),
                    error = %ErrorChain(&err),
                    "Nanobot send_message failed, emitting Error"
                );
                let send_error = AgentSendError::from_app_error(err);
                self.runtime.mark_transport_broken();
                self.turn_boundary_recycle_required
                    .store(false, Ordering::Release);
                let mut state = self.state.write().await;
                if state.runtime_turn == Some(runtime_turn) {
                    relay_broken_stream_error_for_exact_turn(
                        &self.runtime,
                        &mut state,
                        send_error.stream_error().clone(),
                    );
                }
                Err(send_error)
            }
        }
    }

    async fn cancel(&self) -> Result<(), AppError> {
        let runtime_turn = {
            let mut state = self.state.write().await;
            quarantine_cancelled_turn(
                &mut state,
                &self.turn_boundary_recycle_required,
            )
        };
        let payload = json!({ "type": "stop.stream", "data": {} });
        let stop_result = self.process.send(&payload).await;
        if let Some(runtime_turn) = runtime_turn {
            self.runtime
                .emit_finish_for_turn(runtime_turn, None, Some(TurnStopReason::Cancelled));
        }
        stop_result
    }

    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AppError> {
        info!(
            conversation_id = %self.runtime.conversation_id(),
            ?reason,
            "Killing Nanobot agent"
        );
        self.turn_boundary_recycle_required
            .store(true, Ordering::Release);

        let process = Arc::clone(&self.process);
        let grace = Duration::from_millis(NANOBOT_KILL_GRACE_MS);
        tokio::spawn(async move {
            if let Err(e) = process.kill(grace).await {
                error!(error = %ErrorChain(&e), "Failed to kill Nanobot process");
            }
        });

        if let Ok(mut state) = self.state.try_write()
            && let Some(runtime_turn) = state.runtime_turn.take()
        {
            if reason == Some(AgentKillReason::UserCancelled) {
                self.runtime
                    .emit_finish_for_turn(runtime_turn, None, Some(TurnStopReason::Cancelled));
            } else {
                self.runtime.emit_error_data_for_turn(
                    runtime_turn,
                    ErrorEventData::legacy(
                        format!("Nanobot agent was terminated ({reason:?})"),
                        None,
                    ),
                );
            }
        }

        Ok(())
    }
}

impl NanobotAgentManager {
    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AppError>> + Send>> {
        let kill_result = crate::runtime_handle::AgentRuntimeControl::kill(self, reason);
        let process = Arc::clone(&self.process);
        let grace = Duration::from_millis(NANOBOT_KILL_GRACE_MS);
        Box::pin(async move {
            kill_result?;
            process.kill(grace).await
        })
    }
}

/// Nanobot-specific operations reached through `AgentRuntimeHandle::Nanobot(..)`.
/// Nanobot does not track tool confirmations or approval memory, so these
/// are trivial stubs matching the semantics of the removed `IAgentManager`
/// default impls.
impl NanobotAgentManager {
    pub fn confirm(&self, _msg_id: &str, _call_id: &str, _data: Value, _always_allow: bool) -> Result<(), AppError> {
        Err(AppError::BadRequest("Nanobot does not support confirmations".into()))
    }

    pub fn get_confirmations(&self) -> Vec<Confirmation> {
        Vec::new()
    }

    pub fn check_approval(&self, _action: &str, _command_type: Option<&str>) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::events::{
        FinishEventData, StartEventData, TextEventData, ToolCallEventData,
        ToolCallStatus,
    };

    #[test]
    fn build_spawn_config_basic() {
        let config = NanobotAgentManager::build_spawn_config(PathBuf::from("/usr/bin/nanobot"), "/project");
        assert_eq!(config.command.to_str().unwrap(), "/usr/bin/nanobot");
        assert_eq!(config.cwd, Some("/project".into()));
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
    }

    #[test]
    fn cancelled_turn_is_detached_and_requires_deliberate_recycle() {
        let runtime = AgentRuntimeState::new("nanobot-cancel", "/workspace", 8);
        let turn = runtime.reset_for_new_turn(ConversationStatus::Running);
        let recycle_required = AtomicBool::new(false);
        let mut state = NanobotState {
            has_messages: true,
            runtime_turn: Some(turn),
        };

        assert_eq!(
            quarantine_cancelled_turn(&mut state, &recycle_required),
            Some(turn)
        );
        assert_eq!(state.runtime_turn, None);
        assert!(runtime.is_transport_healthy());
        assert!(recycle_required.load(Ordering::Acquire));

        // A delayed uncorrelated CLI terminal now has no token to consume;
        // the registry will deliberately replace this process before another
        // exact turn instead of misclassifying the boundary as a crash.
        assert_eq!(state.runtime_turn.take(), None);
    }

    #[tokio::test]
    async fn late_start_content_and_tool_frames_are_absorbed_after_terminal() {
        let runtime = AgentRuntimeState::new("nanobot-late-terminal", "/workspace", 16);
        let turn = runtime.reset_for_new_turn(ConversationStatus::Running);
        let recycle_required = AtomicBool::new(false);
        let mut state = NanobotState {
            has_messages: true,
            runtime_turn: Some(turn),
        };
        let mut events = runtime.subscribe();

        assert!(relay_event_for_exact_turn(
            &runtime,
            &mut state,
            &recycle_required,
            AgentStreamEvent::Start(StartEventData::default()),
        ));
        assert!(matches!(
            events.recv().await.unwrap(),
            AgentStreamEvent::Start(_)
        ));

        assert!(relay_event_for_exact_turn(
            &runtime,
            &mut state,
            &recycle_required,
            AgentStreamEvent::Finish(FinishEventData {
                session_id: Some("completed-session".into()),
                stop_reason: Some(TurnStopReason::EndTurn),
            }),
        ));
        assert!(matches!(
            events.recv().await.unwrap(),
            AgentStreamEvent::Finish(_)
        ));
        assert_eq!(runtime.status(), Some(ConversationStatus::Finished));
        assert!(recycle_required.load(Ordering::Acquire));
        assert_eq!(state.runtime_turn, None);

        let late_frames = [
            AgentStreamEvent::Start(StartEventData::default()),
            AgentStreamEvent::Text(TextEventData {
                content: "late completed-turn content".into(),
            }),
            AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: "late-tool".into(),
                name: "late_tool".into(),
                args: Value::Null,
                status: ToolCallStatus::Running,
                input: None,
                output: None,
                description: None,
                retry: None,
                artifacts: Vec::new(),
            }),
        ];
        for frame in late_frames {
            assert!(
                !relay_event_for_exact_turn(
                    &runtime,
                    &mut state,
                    &recycle_required,
                    frame,
                ),
                "a frame after terminal must have no emission authority"
            );
        }
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(
            runtime.status(),
            Some(ConversationStatus::Finished),
            "late Start must never resurrect a completed runtime"
        );
    }

    #[tokio::test]
    async fn completed_process_cannot_admit_successor_or_pollute_replacement() {
        let old_runtime = AgentRuntimeState::new("nanobot-old-process", "/workspace", 16);
        let first_turn = old_runtime.reset_for_new_turn(ConversationStatus::Running);
        let old_recycle_required = AtomicBool::new(false);
        let mut old_state = NanobotState {
            has_messages: true,
            runtime_turn: Some(first_turn),
        };
        assert!(relay_event_for_exact_turn(
            &old_runtime,
            &mut old_state,
            &old_recycle_required,
            AgentStreamEvent::Finish(FinishEventData::default()),
        ));

        assert!(
            admit_turn(
                &old_runtime,
                &mut old_state,
                &old_recycle_required,
            )
            .is_err(),
            "a terminal Nanobot process must reject successor admission"
        );

        // The registry supplies a fresh manager/runtime after exact teardown.
        // Frames from the old manager retain only the old event bus and no
        // runtime token, so they cannot enter the replacement subscription.
        let replacement_runtime =
            AgentRuntimeState::new("nanobot-replacement", "/workspace", 16);
        let replacement_recycle_required = AtomicBool::new(false);
        let mut replacement_state = NanobotState {
            has_messages: false,
            runtime_turn: None,
        };
        let _successor = admit_turn(
            &replacement_runtime,
            &mut replacement_state,
            &replacement_recycle_required,
        )
        .expect("fresh process admits the successor turn");
        let mut replacement_events = replacement_runtime.subscribe();

        assert!(!relay_event_for_exact_turn(
            &old_runtime,
            &mut old_state,
            &old_recycle_required,
            AgentStreamEvent::Start(StartEventData::default()),
        ));
        assert!(!relay_event_for_exact_turn(
            &old_runtime,
            &mut old_state,
            &old_recycle_required,
            AgentStreamEvent::Text(TextEventData {
                content: "old process tail".into(),
            }),
        ));
        assert!(matches!(
            replacement_events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(
            replacement_runtime.status(),
            Some(ConversationStatus::Running)
        );
    }
}
