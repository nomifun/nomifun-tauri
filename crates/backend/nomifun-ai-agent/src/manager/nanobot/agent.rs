use std::sync::Arc;
use std::time::Duration;

use nomifun_common::{AgentKillReason, AgentType, AppError, Confirmation, ConversationStatus, ErrorChain, TimestampMs};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, broadcast};
use tracing::{debug, error, info, warn};

use nomifun_common::CommandSpec;

use crate::runtime_state::AgentRuntimeState;
use crate::capability::cli_process::CliAgentProcess;
use crate::manager::process_registry::register_session_process;
use crate::protocol::events::{AgentStreamEvent, TurnStopReason};
use crate::protocol::send_error::AgentSendError;
use crate::types::SendMessageData;
use std::path::PathBuf;

/// Grace period before force-killing a Nanobot process (ms).
const NANOBOT_KILL_GRACE_MS: u64 = 500;

/// Internal mutable state for the Nanobot agent.
struct NanobotState {
    has_messages: bool,
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
}

impl NanobotAgentManager {
    /// Create a new Nanobot agent by spawning the CLI subprocess.
    pub async fn new(
        conversation_id: String,
        workspace: String,
        cli_path: PathBuf,
        data_dir: PathBuf,
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
            state: RwLock::new(NanobotState { has_messages: false }),
            raw_rx: Mutex::new(Some(raw_rx)),
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
                        self.runtime.emit_stream_broken(format!(
                            "Nanobot process exited before the turn completed{detail}"
                        ));
                    } else {
                        self.runtime.mark_transport_broken();
                    }
                    None
                }
            };
            let Some(raw_event) = raw_event else {
                break;
            };
            match raw_event {
                Ok(raw_json) => {
                    self.runtime.bump_activity();
                    self.handle_raw_event(raw_json).await;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        conversation_id = %self.runtime.conversation_id(),
                        lagged = n,
                        "Nanobot event relay lagged"
                    );
                    self.runtime.emit_stream_broken(format!(
                        "Nanobot event relay lost {n} buffered event(s)"
                    ));
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!(
                        conversation_id = %self.runtime.conversation_id(),
                        "Nanobot CLI event channel closed"
                    );
                    if self.runtime.status() == Some(ConversationStatus::Running) {
                        self.runtime.emit_stream_broken(
                            "Nanobot event channel closed before the turn completed",
                        );
                    } else {
                        self.runtime.mark_transport_broken();
                    }
                    break;
                }
            }
        }

    }

    async fn handle_raw_event(&self, raw: Value) {
        let stream_event = match serde_json::from_value::<AgentStreamEvent>(raw.clone()) {
            Ok(event) => event,
            Err(_) => {
                debug!(
                    conversation_id = %self.runtime.conversation_id(),
                    "Unrecognized Nanobot event, skipping"
                );
                return;
            }
        };

        match stream_event {
            event @ AgentStreamEvent::Start(_) => {
                self.runtime.transition_to(ConversationStatus::Running);
                self.runtime.emit(event);
            }
            AgentStreamEvent::Finish(data) => {
                self.runtime.emit_finish_with_reason(data.session_id, data.stop_reason);
            }
            AgentStreamEvent::Error(data) => self.runtime.emit_error_data(data),
            event => self.runtime.emit(event),
        }
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

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.runtime.subscribe()
    }

    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        self.runtime.bump_activity();
        if !self.runtime.is_transport_healthy() {
            return Err(AgentSendError::stream_broken(
                "Nanobot's process/event relay is no longer running",
            ));
        }

        {
            let mut state = self.state.write().await;
            state.has_messages = true;
        }
        self.runtime.reset_for_new_turn(ConversationStatus::Running);
        if !self.runtime.is_transport_healthy() {
            let error = AgentSendError::stream_broken(
                "Nanobot's process/event relay stopped during turn admission",
            );
            self.runtime.emit_error_data(error.stream_error().clone());
            return Err(error);
        }

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
                self.runtime.emit_error_data(send_error.stream_error().clone());
                Err(send_error)
            }
        }
    }

    async fn cancel(&self) -> Result<(), AppError> {
        let payload = json!({ "type": "stop.stream", "data": {} });
        self.process.send(&payload).await?;
        self.runtime
            .emit_finish_with_reason(None, Some(TurnStopReason::Cancelled));
        Ok(())
    }

    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AppError> {
        info!(
            conversation_id = %self.runtime.conversation_id(),
            ?reason,
            "Killing Nanobot agent"
        );

        let process = Arc::clone(&self.process);
        let grace = Duration::from_millis(NANOBOT_KILL_GRACE_MS);
        tokio::spawn(async move {
            if let Err(e) = process.kill(grace).await {
                error!(error = %ErrorChain(&e), "Failed to kill Nanobot process");
            }
        });

        if reason == Some(AgentKillReason::UserCancelled) {
            self.runtime
                .emit_finish_with_reason(None, Some(TurnStopReason::Cancelled));
        } else if self.runtime.status() == Some(ConversationStatus::Running) {
            self.runtime.emit_error(format!("Nanobot agent was terminated ({reason:?})"));
        }

        Ok(())
    }
}

impl NanobotAgentManager {
    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = crate::runtime_handle::AgentRuntimeControl::kill(self, reason);
        let process = Arc::clone(&self.process);
        let grace = Duration::from_millis(NANOBOT_KILL_GRACE_MS);
        Box::pin(async move {
            let _ = process.kill(grace).await;
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

    #[test]
    fn build_spawn_config_basic() {
        let config = NanobotAgentManager::build_spawn_config(PathBuf::from("/usr/bin/nanobot"), "/project");
        assert_eq!(config.command.to_str().unwrap(), "/usr/bin/nanobot");
        assert_eq!(config.cwd, Some("/project".into()));
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
    }
}
