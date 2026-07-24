use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::{BoxFuture, FutureExt, Shared};
use nomifun_common::AppError;
use tokio::sync::watch;

use crate::runtime_state::AgentRuntimeTurn;

type SharedTeardownFuture = Shared<BoxFuture<'static, Result<(), Arc<str>>>>;

/// The gateway identity of one admitted turn.
///
/// `turn_generation` fences reused gateway run IDs, while `runtime_turn`
/// fences delivery into the in-process event stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GatewayRunTurn {
    pub(crate) run_id: String,
    pub(crate) turn_generation: u64,
    pub(crate) runtime_turn: AgentRuntimeTurn,
}

/// The exact admitted turn that teardown must stop. `run_id` is optional only
/// during the narrow window before `chat.send` acknowledges it; the generation
/// and runtime turn still fence any terminal event subsequently bound to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GatewayTeardownTarget {
    pub(crate) session_key: String,
    pub(crate) run_id: Option<String>,
    pub(crate) turn_generation: u64,
    pub(crate) runtime_turn: AgentRuntimeTurn,
}

impl GatewayTeardownTarget {
    pub(crate) fn matches_terminal(&self, terminal: &GatewayRunTurn) -> bool {
        self.turn_generation == terminal.turn_generation
            && self.runtime_turn == terminal.runtime_turn
            && self
                .run_id
                .as_deref()
                .is_none_or(|run_id| run_id == terminal.run_id)
    }
}

#[derive(Clone)]
pub(crate) struct TeardownAttempt {
    id: u64,
    future: SharedTeardownFuture,
}

#[derive(Default)]
struct TeardownCoordinatorState {
    next_id: u64,
    current: Option<TeardownAttempt>,
}

/// Coordinates the synchronous `kill()` entry point and result-bearing
/// `kill_and_wait()` entry point so they execute exactly one ordered teardown
/// attempt instead of racing two aborts/closes against one another.
#[derive(Default)]
pub(crate) struct TeardownCoordinator {
    state: Mutex<TeardownCoordinatorState>,
}

impl TeardownCoordinator {
    pub(crate) fn start_or_join<F>(&self, future: F) -> Result<TeardownAttempt, AppError>
    where
        F: Future<Output = Result<(), AppError>> + Send + 'static,
    {
        let (attempt, is_new) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| AppError::Internal("OpenClaw teardown coordinator lock was poisoned".into()))?;
            if let Some(attempt) = state.current.as_ref() {
                (attempt.clone(), false)
            } else {
                state.next_id = state.next_id.wrapping_add(1);
                let id = state.next_id;
                let future = async move {
                    future
                        .await
                        .map_err(|error| Arc::<str>::from(error.to_string()))
                }
                .boxed()
                .shared();
                let attempt = TeardownAttempt { id, future };
                state.current = Some(attempt.clone());
                (attempt, true)
            }
        };

        // A synchronous kill must keep making progress even when its caller
        // never asks for the result. Shared polling means kill_and_wait can
        // still join this exact attempt without issuing a second abort.
        if is_new {
            let background = attempt.future.clone();
            tokio::spawn(async move {
                let _ = background.await;
            });
        }

        Ok(attempt)
    }

    pub(crate) async fn wait(
        &self,
        attempt: TeardownAttempt,
        context: &'static str,
    ) -> Result<(), AppError> {
        match attempt.future.await {
            Ok(()) => Ok(()),
            Err(message) => {
                // Keep a failed attempt joinable until one result-bearing
                // waiter has observed it. The next registry retry can then
                // perform one fresh bounded attempt.
                if let Ok(mut state) = self.state.lock()
                    && state.current.as_ref().is_some_and(|current| current.id == attempt.id)
                {
                    state.current = None;
                }
                Err(AppError::Internal(format!("{context}: {message}")))
            }
        }
    }
}

pub(crate) async fn request_abort_bounded<F>(
    request: F,
    timeout: Duration,
    context: &'static str,
) -> Result<(), AppError>
where
    F: Future<Output = Result<(), AppError>>,
{
    match tokio::time::timeout(timeout, request).await {
        Ok(result) => result,
        Err(_) => Err(AppError::Timeout(format!(
            "{context} chat.abort RPC did not complete within {} ms",
            timeout.as_millis()
        ))),
    }
}

pub(crate) async fn wait_for_terminal_proof(
    target: &GatewayTeardownTarget,
    mut terminal_rx: watch::Receiver<Option<GatewayRunTurn>>,
    timeout: Duration,
    context: &'static str,
) -> Result<(), AppError> {
    let wait = async {
        loop {
            if terminal_rx
                .borrow()
                .as_ref()
                .is_some_and(|terminal| target.matches_terminal(terminal))
            {
                return Ok(());
            }
            terminal_rx.changed().await.map_err(|_| {
                AppError::Internal(format!(
                    "{context} terminal-proof channel closed before the active run stopped"
                ))
            })?;
        }
    };

    match tokio::time::timeout(timeout, wait).await {
        Ok(result) => result,
        Err(_) => Err(AppError::Timeout(format!(
            "{context} did not receive the exact run terminal within {} ms",
            timeout.as_millis()
        ))),
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum TestAbortBehavior {
    Reject,
    AcknowledgeOnly,
    AcknowledgeAndTerminate,
}

#[cfg(test)]
pub(crate) async fn spawn_test_gateway(
    behavior: TestAbortBehavior,
) -> (
    String,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use std::sync::atomic::Ordering;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let abort_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_abort_count = Arc::clone(&abort_count);
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (mut sink, mut stream) = websocket.split();
        let challenge = json!({
            "type": "event",
            "event": "connect.challenge",
            "payload": { "nonce": "teardown-test" }
        });
        sink.send(Message::Text(challenge.to_string().into()))
            .await
            .unwrap();

        let connect = loop {
            let Some(Ok(Message::Text(text))) = stream.next().await else {
                return;
            };
            let frame: Value = serde_json::from_str(&text).unwrap();
            if frame["method"] == "connect" {
                break frame;
            }
        };
        let hello = json!({
            "type": "res",
            "id": connect["id"],
            "ok": true,
            "payload": {
                "type": "hello-ok",
                "protocol": 4,
                "server": { "version": "test", "connId": "teardown" },
                "features": { "methods": [], "events": [] },
                "auth": { "role": "operator", "scopes": ["operator.admin"] },
                "policy": { "maxPayload": 26214400, "tickIntervalMs": 30000 }
            }
        });
        sink.send(Message::Text(hello.to_string().into()))
            .await
            .unwrap();

        while let Some(message) = stream.next().await {
            let Ok(Message::Text(text)) = message else {
                break;
            };
            let frame: Value = serde_json::from_str(&text).unwrap();
            if frame["method"] != "chat.abort" {
                continue;
            }
            server_abort_count.fetch_add(1, Ordering::SeqCst);
            assert_eq!(frame["params"]["sessionKey"], "session-1");
            assert_eq!(frame["params"]["runId"], "run-1");
            match behavior {
                TestAbortBehavior::Reject => {
                    let response = json!({
                        "type": "res",
                        "id": frame["id"],
                        "ok": false,
                        "error": {
                            "code": "ABORT_REJECTED",
                            "message": "abort rejected"
                        }
                    });
                    sink.send(Message::Text(response.to_string().into()))
                        .await
                        .unwrap();
                }
                TestAbortBehavior::AcknowledgeOnly
                | TestAbortBehavior::AcknowledgeAndTerminate => {
                    let response = json!({
                        "type": "res",
                        "id": frame["id"],
                        "ok": true,
                        "payload": { "aborted": true }
                    });
                    sink.send(Message::Text(response.to_string().into()))
                        .await
                        .unwrap();
                    if matches!(behavior, TestAbortBehavior::AcknowledgeAndTerminate) {
                        let terminal = json!({
                            "type": "event",
                            "event": "chat",
                            "payload": {
                                "runId": "run-1",
                                "sessionKey": "session-1",
                                "state": "aborted"
                            }
                        });
                        sink.send(Message::Text(terminal.to_string().into()))
                            .await
                        .unwrap();
                    }
                }
            }
        }
    });
    (url, abort_count, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::ConversationStatus;
    use tokio::sync::watch;

    fn test_turn() -> AgentRuntimeTurn {
        crate::runtime_state::AgentRuntimeState::new("teardown-test", "/workspace", 4)
            .reset_for_new_turn(ConversationStatus::Running)
    }

    fn target(runtime_turn: AgentRuntimeTurn) -> GatewayTeardownTarget {
        GatewayTeardownTarget {
            session_key: "session-1".into(),
            run_id: Some("run-1".into()),
            turn_generation: 7,
            runtime_turn,
        }
    }

    #[tokio::test]
    async fn abort_rpc_failure_is_not_swallowed() {
        let result = request_abort_bounded(
            async { Err(AppError::BadGateway("abort rejected".into())) },
            Duration::from_millis(50),
            "test gateway",
        )
        .await;

        assert!(matches!(result, Err(AppError::BadGateway(message)) if message == "abort rejected"));
    }

    #[tokio::test]
    async fn missing_terminal_proof_fails_closed() {
        let runtime_turn = test_turn();
        let (_tx, rx) = watch::channel(None);
        let result = wait_for_terminal_proof(
            &target(runtime_turn),
            rx,
            Duration::from_millis(20),
            "test gateway",
        )
        .await;

        assert!(matches!(result, Err(AppError::Timeout(message)) if message.contains("exact run terminal")));
    }

    #[tokio::test]
    async fn only_the_exact_generation_terminal_is_accepted() {
        let runtime_turn = test_turn();
        let (tx, rx) = watch::channel(None);
        tx.send_replace(Some(GatewayRunTurn {
            run_id: "run-1".into(),
            turn_generation: 6,
            runtime_turn,
        }));
        let exact_target = target(runtime_turn);
        let waiter = tokio::spawn(async move {
            wait_for_terminal_proof(
                &exact_target,
                rx,
                Duration::from_millis(100),
                "test gateway",
            )
            .await
        });
        tokio::task::yield_now().await;
        tx.send_replace(Some(GatewayRunTurn {
            run_id: "run-1".into(),
            turn_generation: 7,
            runtime_turn,
        }));

        waiter.await.unwrap().unwrap();
    }

}
