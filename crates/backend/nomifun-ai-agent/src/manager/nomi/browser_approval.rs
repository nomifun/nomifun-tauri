//! **Phase D: desktop browser approval gate.**
//!
//! Implements the facade [`nomi_browser::BrowserApprovalGate`] for the desktop/backend
//! host. When an irreversible browser action (in a bypass session) or a gated cross-origin
//! POST (SD-5) needs human approval, this raises a generic [`Confirmation`] into the
//! session's confirmation store + broadcasts it on the agent event stream (so the desktop
//! `MessagePermission` UI renders it — **the same path tool-approvals use, no new event,
//! no frontend change**), then awaits the shared [`ToolApprovalManager`] keyed by the same
//! `call_id` the frontend resolves through [`super::agent::NomiAgentManager::confirm`].
//!
//! **Fail-closed keystone**: a timeout, a dropped channel, or an explicit deny all map to
//! [`ApprovalDecision::Deny`] — only an explicit user approval returns `Approve`.

#![cfg(feature = "browser-use")]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use nomi_browser::{ApprovalAsk, ApprovalDecision, BrowserApprovalGate};
use nomi_protocol::events::ToolCategory;
use nomi_protocol::{ToolApprovalManager, ToolApprovalResult};
use nomifun_common::{Confirmation, ConfirmationOption, generate_id};
use serde_json::json;
use tokio::sync::broadcast;

use crate::protocol::events::{AcpPermissionEventData, AgentStreamEvent};

/// Default time the user has to approve a browser action before it fail-closes.
/// Matches the engine's egress approval timeout (`EGRESS_APPROVAL_TIMEOUT` = 120s) so a
/// suspended cross-origin POST and its approval prompt expire together.
const BROWSER_APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Desktop impl of the facade [`BrowserApprovalGate`]. Construct it BEFORE the session's
/// bootstrap (it shares the same `confirmations` store + `approval_manager` the manager
/// installs) and pass it via `AgentBootstrap::approval_gate`.
pub struct DesktopApprovalGate {
    event_tx: broadcast::Sender<AgentStreamEvent>,
    confirmations: Arc<RwLock<Vec<Confirmation>>>,
    approval_manager: Arc<ToolApprovalManager>,
    timeout: Duration,
    unrestricted_approval: bool,
}

impl DesktopApprovalGate {
    /// Create a gate sharing the session's event stream, confirmation store, and approval
    /// manager (the same instances the `BackendProtocolSink` + engine use, so the existing
    /// `confirm` resolve path drives it).
    pub fn new(
        event_tx: broadcast::Sender<AgentStreamEvent>,
        confirmations: Arc<RwLock<Vec<Confirmation>>>,
        approval_manager: Arc<ToolApprovalManager>,
        unrestricted_approval: bool,
    ) -> Self {
        Self {
            event_tx,
            confirmations,
            approval_manager,
            timeout: BROWSER_APPROVAL_TIMEOUT,
            unrestricted_approval,
        }
    }

    fn build_confirmation(call_id: &str, ask: &ApprovalAsk) -> Confirmation {
        Confirmation {
            id: generate_id(),
            call_id: call_id.to_string(),
            title: Some(ask.title()),
            action: Some("browser_approval".to_string()),
            description: ask.description(),
            command_type: Some("browser".to_string()),
            options: vec![
                ConfirmationOption {
                    label: "messages.confirmation.yesAllowOnce".to_string(),
                    value: json!("proceed_once"),
                    params: None,
                },
                ConfirmationOption {
                    label: "messages.confirmation.yesAllowAlways".to_string(),
                    value: json!("proceed_always"),
                    params: None,
                },
                ConfirmationOption {
                    label: "messages.confirmation.no".to_string(),
                    value: json!("cancel"),
                    params: None,
                },
            ],
            // Phase 3: carry the facade's current-page preview (data:image/png;base64 URL)
            // so the MessagePermission card can show it — the silent-mode "approve without a
            // visible window" fix. `None` for egress asks / when capture failed.
            screenshot: ask.screenshot.clone(),
        }
    }
}

#[async_trait::async_trait]
impl BrowserApprovalGate for DesktopApprovalGate {
    async fn request_approval(&self, ask: ApprovalAsk) -> ApprovalDecision {
        let category = ToolCategory::Irreversible;
        let category_key = category.to_string();
        // Browser egress approval runs below the normal tool-approval layer, so it must
        // explicitly honor the same session auto-approval policy here.
        if self.unrestricted_approval || self.approval_manager.is_auto_approved(&category_key) {
            return ApprovalDecision::Approve;
        }

        let call_id = generate_id();

        // Register the oneshot FIRST — before the confirmation can possibly be seen and
        // resolved by the user — so a fast resolve can never race ahead of registration
        // (a resolve for an unregistered call_id is a no-op → we'd then fail-closed on
        // timeout, which is safe, but registering first avoids that entirely).
        let rx = self
            .approval_manager
            .request_approval(&call_id, &category);

        // Raise the confirmation: push into the shared store + broadcast on the event
        // stream. The desktop `MessagePermission` UI renders it; the user resolves it via
        // `NomiAgentManager::confirm`, which fires our oneshot through `approval_manager`.
        let conf = Self::build_confirmation(&call_id, &ask);
        if let Ok(mut confs) = self.confirmations.write() {
            confs.push(conf.clone());
        }
        let _ = self
            .event_tx
            .send(AgentStreamEvent::AcpPermission(AcpPermissionEventData::Confirmation(conf)));

        let decision = match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(ToolApprovalResult::Approved)) => ApprovalDecision::Approve,
            // Explicit deny, dropped channel (client gone), or timeout → fail-closed.
            _ => ApprovalDecision::Deny,
        };

        // Cleanup: on a timeout the frontend never called `confirm` (which removes the
        // confirmation), so drop the stale entry here. On a real resolve `confirm` already
        // removed it → this retain is a harmless no-op.
        if let Ok(mut confs) = self.confirmations.write() {
            confs.retain(|c| c.call_id != call_id);
        }

        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomi_browser::ApprovalKind;
    use nomi_protocol::commands::{ApprovalScope, SessionMode};

    fn ask() -> ApprovalAsk {
        ApprovalAsk {
            kind: ApprovalKind::IrreversibleAction {
                action: "click".into(),
                description: "Pay now".into(),
            },
            screenshot: None,
        }
    }

    /// Drive the full round-trip: the gate raises a `Confirmation` into the store +
    /// registers a oneshot; resolving that call_id through `approval_manager` (exactly what
    /// `NomiAgentManager::confirm` does) returns the decision and clears the confirmation.
    async fn drive(resolve_approved: bool) -> (ApprovalDecision, bool) {
        let (tx, _rx) = broadcast::channel(16);
        let confirmations = Arc::new(RwLock::new(Vec::new()));
        let mgr = Arc::new(ToolApprovalManager::new());
        let gate = DesktopApprovalGate::new(tx, confirmations.clone(), mgr.clone(), false);

        let confs_for_task = confirmations.clone();
        let handle = tokio::spawn(async move { gate.request_approval(ask()).await });

        // Wait for the gate to publish its confirmation, then grab the generated call_id.
        let call_id = loop {
            if let Some(c) = confs_for_task.read().unwrap().first() {
                break c.call_id.clone();
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        };

        if resolve_approved {
            mgr.approve(&call_id, ApprovalScope::Once);
        } else {
            mgr.resolve(
                &call_id,
                ToolApprovalResult::Denied { reason: "user denied".into() },
            );
        }

        let decision = handle.await.unwrap();
        let cleaned = confirmations.read().unwrap().is_empty();
        (decision, cleaned)
    }

    #[tokio::test]
    async fn approve_round_trip_returns_approve_and_clears_confirmation() {
        let (decision, cleaned) = drive(true).await;
        assert_eq!(decision, ApprovalDecision::Approve);
        assert!(cleaned, "the confirmation must be removed after resolve");
    }

    #[tokio::test]
    async fn deny_round_trip_returns_deny() {
        let (decision, cleaned) = drive(false).await;
        assert_eq!(decision, ApprovalDecision::Deny, "explicit deny → fail-closed Deny");
        assert!(cleaned);
    }

    #[tokio::test]
    async fn timeout_fails_closed_to_deny() {
        // Never resolve → the bounded await must fail-closed to Deny.
        let (tx, _rx) = broadcast::channel(16);
        let confirmations = Arc::new(RwLock::new(Vec::new()));
        let mgr = Arc::new(ToolApprovalManager::new());
        // Short timeout for the test (don't advance a full 120s).
        let gate = DesktopApprovalGate {
            event_tx: tx,
            confirmations: confirmations.clone(),
            approval_manager: mgr,
            timeout: Duration::from_millis(50),
            unrestricted_approval: false,
        };
        let handle = tokio::spawn(async move { gate.request_approval(ask()).await });
        tokio::time::sleep(Duration::from_millis(60)).await;
        let decision = handle.await.unwrap();
        assert_eq!(decision, ApprovalDecision::Deny, "timeout must fail-closed to Deny");
        assert!(confirmations.read().unwrap().is_empty(), "stale confirmation cleaned up on timeout");
    }

    #[test]
    fn browser_approval_confirmation_has_allow_always() {
        let conf = DesktopApprovalGate::build_confirmation("call-browser", &ask());
        let values: Vec<_> = conf.options.iter().map(|o| o.value.clone()).collect();
        assert!(values.contains(&json!("proceed_once")));
        assert!(values.contains(&json!("proceed_always")));
        assert!(values.contains(&json!("cancel")));
    }

    #[tokio::test]
    async fn unrestricted_gate_approves_without_confirmation() {
        let (tx, _rx) = broadcast::channel(16);
        let confirmations = Arc::new(RwLock::new(Vec::new()));
        let mgr = Arc::new(ToolApprovalManager::new());
        let gate = DesktopApprovalGate::new(tx, confirmations.clone(), mgr, true);

        let decision = gate.request_approval(ask()).await;

        assert_eq!(decision, ApprovalDecision::Approve);
        assert!(
            confirmations.read().unwrap().is_empty(),
            "unrestricted Browser approval should not publish a confirmation"
        );
    }

    #[tokio::test]
    async fn yolo_session_mode_approves_without_confirmation() {
        let (tx, _rx) = broadcast::channel(16);
        let confirmations = Arc::new(RwLock::new(Vec::new()));
        let mgr = Arc::new(ToolApprovalManager::new());
        mgr.set_mode(SessionMode::Yolo);
        let gate = DesktopApprovalGate {
            event_tx: tx,
            confirmations: confirmations.clone(),
            approval_manager: mgr,
            timeout: Duration::from_millis(50),
            unrestricted_approval: false,
        };

        let decision = gate.request_approval(ask()).await;

        assert_eq!(decision, ApprovalDecision::Approve);
        assert!(
            confirmations.read().unwrap().is_empty(),
            "yolo/full-auto sessions should not publish Browser approval confirmations"
        );
    }

    #[tokio::test]
    async fn always_approved_irreversible_category_skips_prompt() {
        let (tx, _rx) = broadcast::channel(16);
        let confirmations = Arc::new(RwLock::new(Vec::new()));
        let mgr = Arc::new(ToolApprovalManager::new());
        mgr.add_auto_approve(&ToolCategory::Irreversible.to_string());
        let gate = DesktopApprovalGate::new(tx, confirmations.clone(), mgr, false);

        let decision = gate.request_approval(ask()).await;

        assert_eq!(decision, ApprovalDecision::Approve);
        assert!(
            confirmations.read().unwrap().is_empty(),
            "session-level always approval should bypass future Browser prompts"
        );
    }
}
