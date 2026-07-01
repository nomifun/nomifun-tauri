use crate::agent_runtime::AgentRuntime;
use crate::protocol::acp::{PermissionDecision, PermissionRequest};
use crate::protocol::events::{AgentStreamEvent, permission_request_to_event_data};
use nomifun_common::Confirmation;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::debug;

struct PendingPermission {
    responder: oneshot::Sender<PermissionDecision>,
    confirmation: Confirmation,
}

/// Routes ACP permission requests from the protocol layer to the user
/// (via `event_tx`) and back (via `confirm`). Owns the receiver channel
/// for incoming permission requests, the pending responder map, and the
/// `closing` flag that prevents new requests from being routed after a
/// graceful shutdown has started.
pub struct PermissionRouter {
    /// Receiver for permission requests from the protocol layer.
    permission_rx: Mutex<mpsc::Receiver<PermissionRequest>>,
    /// Pending ACP permission responders and recovery data keyed by tool call ID.
    pending_permissions: StdMutex<HashMap<String, PendingPermission>>,
    /// Whether a graceful shutdown is in progress.
    closing: AtomicBool,
}

impl PermissionRouter {
    /// Create a new permission router.
    pub fn new(permission_rx: mpsc::Receiver<PermissionRequest>) -> Self {
        Self {
            permission_rx: Mutex::new(permission_rx),
            pending_permissions: StdMutex::new(HashMap::new()),
            closing: AtomicBool::new(false),
        }
    }

    /// Start the permission handler loop.
    ///
    /// This background task receives permission requests from the protocol
    /// layer, converts them to `Permission` events, and waits for user
    /// responses routed through the `confirm()` method.
    ///
    /// `runtime` is shared with the parent manager so permission
    /// arrivals count as activity (preventing idle timeouts) via
    /// `runtime.bump_activity()`.
    pub fn start(self: &Arc<Self>, runtime: AgentRuntime) {
        let this = Arc::clone(self);

        tokio::spawn(async move {
            let mut rx = this.permission_rx.lock().await;

            while let Some(perm_req) = rx.recv().await {
                runtime.bump_activity();

                let call_id = perm_req.request.tool_call.tool_call_id.to_string();

                let permission_event = permission_request_to_event_data(&perm_req.request);
                let confirmation = permission_event
                    .as_confirmation()
                    .expect("ACP permission events must be recoverable as confirmations");

                let mut pending = this.pending_permissions.lock().unwrap();
                if let Some(previous) = pending.insert(
                    call_id.clone(),
                    PendingPermission {
                        responder: perm_req.response_tx,
                        confirmation,
                    },
                ) {
                    let _ = previous.responder.send(PermissionDecision::Cancelled);
                }
                drop(pending);
                debug!(
                    conversation_id = %runtime.conversation_id(),
                    call_id,
                    "ACP permission pending confirmation registered"
                );

                if runtime
                    .event_sender()
                    .send(AgentStreamEvent::AcpPermission(permission_event))
                    .is_err()
                    && let Some(pending) = this.pending_permissions.lock().unwrap().remove(&call_id)
                {
                    let _ = pending.responder.send(PermissionDecision::Cancelled);
                }
            }
        });
    }

    /// Pending permission items recoverable by conversation confirmation APIs.
    pub fn get_confirmations(&self) -> Vec<Confirmation> {
        self.pending_permissions
            .lock()
            .unwrap()
            .values()
            .map(|pending| pending.confirmation.clone())
            .collect()
    }

    /// Resolve a pending permission request with the user's selected option.
    pub fn confirm(
        &self,
        call_id: &str,
        option_id: String,
        conversation_id: &str,
    ) -> Result<(), nomifun_common::AppError> {
        let pending = self
            .pending_permissions
            .lock()
            .unwrap()
            .remove(call_id)
            .ok_or_else(|| {
                nomifun_common::AppError::BadRequest(format!(
                    "Pending ACP permission not found: {call_id}"
                ))
            })?;

        pending
            .responder
            .send(PermissionDecision::Selected { option_id })
            .map_err(|_| {
                nomifun_common::AppError::BadRequest(format!(
                    "Pending ACP permission expired: {call_id}"
                ))
            })?;

        debug!(conversation_id = %conversation_id, call_id, "ACP permission response forwarded");
        Ok(())
    }

    /// Cancel all pending permission requests. Called during `stop()` and `kill()`.
    pub fn cancel_all(&self) {
        for (_, pending) in self.pending_permissions.lock().unwrap().drain() {
            let _ = pending.responder.send(PermissionDecision::Cancelled);
        }
    }

    /// Whether a graceful shutdown is in progress.
    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// Mark the router as closing (graceful shutdown in progress).
    pub fn set_closing(&self) {
        self.closing.store(true, Ordering::Release);
    }

    #[cfg(test)]
    fn insert_pending_for_test(
        &self,
        call_id: String,
        responder: oneshot::Sender<PermissionDecision>,
        confirmation: Confirmation,
    ) {
        self.pending_permissions.lock().unwrap().insert(
            call_id,
            PendingPermission {
                responder,
                confirmation,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::events::AgentStreamEvent;
    use agent_client_protocol::schema::{
        PermissionOption, PermissionOptionKind as SdkPermissionOptionKind,
        RequestPermissionRequest, ToolCallUpdate as SdkToolCallUpdate, ToolCallUpdateFields,
        ToolKind as SdkToolKind,
    };
    use nomifun_common::Confirmation;
    use serde_json::json;
    use std::time::Duration;

    fn sample_confirmation(call_id: &str) -> Confirmation {
        Confirmation {
            id: call_id.to_owned(),
            call_id: call_id.to_owned(),
            title: Some("Write file".to_owned()),
            action: None,
            description: "Write /tmp/current_time.txt".to_owned(),
            command_type: Some("edit".to_owned()),
            options: vec![nomifun_common::ConfirmationOption {
                label: "Allow".to_owned(),
                value: json!("allow_once"),
                params: None,
            }],
            screenshot: None,
        }
    }

    #[test]
    fn get_confirmations_returns_pending_acp_permission() {
        let (_tx, rx) = mpsc::channel(1);
        let router = PermissionRouter::new(rx);
        let (response_tx, _response_rx) = oneshot::channel();

        router.insert_pending_for_test(
            "tool-1".to_owned(),
            response_tx,
            sample_confirmation("tool-1"),
        );

        let confirmations = router.get_confirmations();
        assert_eq!(confirmations.len(), 1);
        assert_eq!(confirmations[0].id, "tool-1");
        assert_eq!(confirmations[0].call_id, "tool-1");
        assert_eq!(confirmations[0].description, "Write /tmp/current_time.txt");
    }

    #[test]
    fn confirm_removes_pending_confirmation_and_forwards_option() {
        let (_tx, rx) = mpsc::channel(1);
        let router = PermissionRouter::new(rx);
        let (response_tx, mut response_rx) = oneshot::channel();
        router.insert_pending_for_test(
            "tool-1".to_owned(),
            response_tx,
            sample_confirmation("tool-1"),
        );

        router
            .confirm("tool-1", "allow_once".to_owned(), "conv-1")
            .expect("confirm should succeed");

        assert!(router.get_confirmations().is_empty());
        assert!(matches!(
            response_rx.try_recv(),
            Ok(PermissionDecision::Selected { option_id }) if option_id == "allow_once"
        ));
    }

    #[test]
    fn confirm_missing_permission_returns_specific_error() {
        let (_tx, rx) = mpsc::channel(1);
        let router = PermissionRouter::new(rx);

        let error = router
            .confirm("missing-tool", "allow_once".to_owned(), "conv-1")
            .expect_err("missing permission should fail");

        assert!(
            error
                .to_string()
                .contains("Pending ACP permission not found: missing-tool")
        );
    }

    #[test]
    fn cancel_all_removes_pending_confirmations() {
        let (_tx, rx) = mpsc::channel(1);
        let router = PermissionRouter::new(rx);
        let (response_tx, _response_rx) = oneshot::channel();
        router.insert_pending_for_test(
            "tool-1".to_owned(),
            response_tx,
            sample_confirmation("tool-1"),
        );

        router.cancel_all();

        assert!(router.get_confirmations().is_empty());
    }

    #[tokio::test]
    async fn start_routes_permission_request_and_exposes_recoverable_confirmation() {
        let (permission_tx, permission_rx) = mpsc::channel(1);
        let router = Arc::new(PermissionRouter::new(permission_rx));
        let runtime = AgentRuntime::new("conv-1", "/tmp/workspace", 8);
        let mut event_rx = runtime.subscribe();
        router.start(runtime);

        let request = RequestPermissionRequest::new(
            "session-1",
            SdkToolCallUpdate::new(
                "tool-1",
                ToolCallUpdateFields::new()
                    .title("Write file")
                    .kind(SdkToolKind::Edit)
                    .raw_input(json!({ "description": "Write /tmp/current_time.txt" })),
            ),
            vec![PermissionOption::new(
                "allow_once",
                "Allow",
                SdkPermissionOptionKind::AllowOnce,
            )],
        );
        let (response_tx, mut response_rx) = oneshot::channel();

        permission_tx
            .send(PermissionRequest {
                request,
                response_tx,
            })
            .await
            .expect("permission request should be accepted");

        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("permission event should be emitted")
            .expect("permission event channel should stay open");
        assert!(matches!(event, AgentStreamEvent::AcpPermission(_)));

        let confirmations = router.get_confirmations();
        assert_eq!(confirmations.len(), 1);
        assert_eq!(confirmations[0].id, "tool-1");
        assert_eq!(confirmations[0].call_id, "tool-1");
        assert_eq!(confirmations[0].command_type.as_deref(), Some("edit"));

        router
            .confirm("tool-1", "allow_once".to_owned(), "conv-1")
            .expect("confirm should resolve routed request");

        assert!(router.get_confirmations().is_empty());
        assert!(matches!(
            response_rx.try_recv(),
            Ok(PermissionDecision::Selected { option_id }) if option_id == "allow_once"
        ));
    }

    #[tokio::test]
    async fn start_routes_team_prefixed_mcp_permission_request_to_user_confirmation() {
        let (permission_tx, permission_rx) = mpsc::channel(1);
        let router = Arc::new(PermissionRouter::new(permission_rx));
        let runtime = AgentRuntime::new("conv-1", "/tmp/workspace", 8);
        let mut event_rx = runtime.subscribe();
        router.start(runtime);

        let request = RequestPermissionRequest::new(
            "session-1",
            SdkToolCallUpdate::new(
                "tool-1",
                ToolCallUpdateFields::new()
                    .title("mcp__nomifun-team__team_send_message")
                    .kind(SdkToolKind::Other)
                    .raw_input(json!({ "to": "slot-1", "message": "hello" })),
            ),
            vec![PermissionOption::new(
                "allow_once",
                "Allow",
                SdkPermissionOptionKind::AllowOnce,
            )],
        );
        let (response_tx, mut response_rx) = oneshot::channel();

        permission_tx
            .send(PermissionRequest {
                request,
                response_tx,
            })
            .await
            .expect("permission request should be accepted");

        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("team-prefixed MCP permission should be emitted")
            .expect("permission event channel should stay open");
        assert!(matches!(event, AgentStreamEvent::AcpPermission(_)));
        assert!(matches!(
            response_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(router.get_confirmations().len(), 1);
    }
}
