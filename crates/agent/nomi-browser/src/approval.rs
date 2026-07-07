//! **Phase D: browser approval gate** — the facade-level seam through which an
//! out-of-band human approval is requested for a security-sensitive browser event,
//! awaited, and decided. ONE trait serves BOTH paths that need a live, bounded,
//! user-present decision:
//!
//! 1. **Human takeover** — an *irreversible* action (submit / pay / delete / send) in
//!    a bypass (yolo/companion) session, which the redline gate would otherwise
//!    hard-deny. With a gate wired, the user is asked to approve it once.
//! 2. **Cross-origin POST egress (SD-5)** — the engine's `Fetch.requestPaused` firewall
//!    suspends a gated cross-origin POST and awaits a verdict ([`GateEgressApprover`]
//!    adapts this trait to the engine's [`nomi_browser_engine::firewall::EgressApprover`]).
//!
//! # Injection pattern (mirrors ExtractModel / VisualLocator / SiteMemorySink)
//!
//! `Option<Arc<dyn BrowserApprovalGate>>` on [`crate::BrowserTool`], default `None`.
//! **`None` → fail-closed** (takeover unavailable → irreversible action stays Blocked;
//! egress approver absent → gated egress fails closed). This preserves the exact
//! pre-wiring behavior (zero regression). The real impls live in the layer that has a
//! user channel: the desktop bootstrap (event + `ToolApprovalManager` oneshot) and the
//! gateway (the GW2 `nomi_browser_confirm` pending channel).
//!
//! # Security keystone
//!
//! The gate impl MUST **fail-closed**: a timeout, a dropped channel, a missing UI, or
//! any ambiguity returns [`ApprovalDecision::Deny`]. ONLY an explicit user approval
//! returns [`ApprovalDecision::Approve`]. The preview for an egress ask carries host +
//! field NAMES only — **never field values** (the engine builds it that way).

use std::sync::Arc;

use async_trait::async_trait;
use nomi_browser_engine::firewall::{EgressApprover, EgressVerdict, PostPreview};

/// What the user is being asked to approve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalKind {
    /// An irreversible browser action under a bypass session (human takeover).
    IrreversibleAction {
        /// The facade action name (e.g. `click`, `press_key`, `navigate`).
        action: String,
        /// A human-readable description (e.g. the target element's accessible name).
        /// **Never a resolved secret** — the facade resolves `secret:NAME` itself.
        description: String,
    },
    /// A gated cross-origin POST egress (SD-5). Carries the safe preview: target host,
    /// body size, and form field NAMES — **never field values**.
    CrossOriginPost {
        /// Target host the POST would be sent to.
        host: String,
        /// Body size in bytes.
        size: usize,
        /// Form field names (names only, no values).
        field_names: Vec<String>,
    },
}

/// A request for out-of-band human approval of a browser event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalAsk {
    /// What is being approved (+ its safe, value-free preview).
    pub kind: ApprovalKind,
    /// Optional current-page preview as a `data:image/png;base64,...` URL. The facade
    /// attaches it (best-effort) for a human-takeover ask so a **silent (headless)**
    /// session can still show the user the page they are approving an irreversible action
    /// on — without needing a visible window. `None` for egress asks and whenever the
    /// screenshot capture failed (the text ask is still surfaced; screenshot never gates).
    /// The image is the engine's redaction-aware screenshot (same call `do_screenshot`
    /// uses), so known secrets are blacked out before it leaves the engine.
    pub screenshot: Option<String>,
}

impl ApprovalAsk {
    /// A short, human-readable title for the approval prompt.
    pub fn title(&self) -> String {
        match &self.kind {
            ApprovalKind::IrreversibleAction { action, .. } => {
                format!("Approve irreversible browser action: {action}")
            }
            ApprovalKind::CrossOriginPost { host, .. } => {
                format!("Approve cross-origin data egress to {host}")
            }
        }
    }

    /// A human-readable description for the prompt (never leaks secret values).
    pub fn description(&self) -> String {
        match &self.kind {
            ApprovalKind::IrreversibleAction { action, description } => format!(
                "The agent wants to run `{action}` ({description}) — this is irreversible \
                 (may submit / pay / delete / send) and requires Browser approval."
            ),
            ApprovalKind::CrossOriginPost { host, size, field_names } => {
                let fields = if field_names.is_empty() {
                    "non-form body".to_string()
                } else {
                    format!("fields: {}", field_names.join(", "))
                };
                format!(
                    "The page wants to POST {size} bytes to a different origin ({host}); {fields}. \
                     Cross-origin data egress is held for your approval (values are not shown)."
                )
            }
        }
    }
}

/// The user's decision. Binary by design: the gate impl maps every non-approval
/// outcome (deny / timeout / channel-drop / no-UI) to [`Self::Deny`] (fail-closed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// The user explicitly approved.
    Approve,
    /// Denied — explicitly, or by fail-closed default (timeout / unavailable).
    Deny,
}

impl ApprovalDecision {
    /// `true` only for [`Self::Approve`] (the redline keystone equivalent).
    pub fn is_approved(self) -> bool {
        matches!(self, ApprovalDecision::Approve)
    }
}

/// The facade seam: surface an [`ApprovalAsk`] to the user and await their decision.
///
/// The implementation owns the notify + await + timeout + fail-closed logic. It MUST
/// return [`ApprovalDecision::Deny`] on any non-approval outcome.
#[async_trait]
pub trait BrowserApprovalGate: Send + Sync {
    /// Request out-of-band human approval. Returns the decision (fail-closed to `Deny`).
    async fn request_approval(&self, ask: ApprovalAsk) -> ApprovalDecision;
}

/// Optional injection point on [`crate::BrowserTool`] (mirrors `ExtractModelRef`).
/// `None` → fail-closed (no takeover, no egress approval).
pub type BrowserApprovalGateRef = Option<Arc<dyn BrowserApprovalGate>>;

/// Adapts a [`BrowserApprovalGate`] to the engine's [`EgressApprover`] trait (SD-5).
///
/// The engine's `Fetch.requestPaused` firewall loop suspends a gated cross-origin POST
/// and `await`s `approve_egress(preview)`; this adapter forwards the (value-free)
/// preview to the gate and maps the human decision to an [`EgressVerdict`]:
/// approve → [`EgressVerdict::Continue`] (release once), deny → [`EgressVerdict::Fail`]
/// (fail-closed — the leak window stays shut).
pub struct GateEgressApprover {
    gate: Arc<dyn BrowserApprovalGate>,
}

impl GateEgressApprover {
    /// Wrap a gate as an engine egress approver.
    pub fn new(gate: Arc<dyn BrowserApprovalGate>) -> Self {
        Self { gate }
    }
}

#[async_trait]
impl EgressApprover for GateEgressApprover {
    async fn approve_egress(&self, preview: &PostPreview) -> EgressVerdict {
        let ask = ApprovalAsk {
            kind: ApprovalKind::CrossOriginPost {
                host: preview.host.clone(),
                size: preview.size,
                field_names: preview.field_names.clone(),
            },
            // Egress asks never carry a screenshot (the page image is irrelevant to a
            // value-free host/field-names egress decision, and could leak the very content
            // the firewall is gating).
            screenshot: None,
        };
        match self.gate.request_approval(ask).await {
            // Approve once. We deliberately do NOT map to ContinueAndRemember — "remember
            // this domain" is a separate, more dangerous decision the binary gate doesn't
            // grant (a future richer decision could add it).
            ApprovalDecision::Approve => EgressVerdict::Continue,
            // Deny / timeout / unavailable → fail-closed (engine fails the request).
            ApprovalDecision::Deny => EgressVerdict::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A fake gate that returns a predetermined decision and records the asks it saw.
    struct FakeGate {
        decision: ApprovalDecision,
        seen: Mutex<Vec<ApprovalAsk>>,
    }

    impl FakeGate {
        fn new(decision: ApprovalDecision) -> Self {
            Self { decision, seen: Mutex::new(Vec::new()) }
        }
    }

    #[async_trait]
    impl BrowserApprovalGate for FakeGate {
        async fn request_approval(&self, ask: ApprovalAsk) -> ApprovalDecision {
            self.seen.lock().unwrap().push(ask);
            self.decision
        }
    }

    #[tokio::test]
    async fn egress_approver_approve_maps_to_continue() {
        let gate = Arc::new(FakeGate::new(ApprovalDecision::Approve));
        let approver = GateEgressApprover::new(gate.clone());
        let preview = PostPreview {
            host: "evil.example.com".into(),
            size: 42,
            field_names: vec!["username".into(), "card".into()],
        };
        let verdict = approver.approve_egress(&preview).await;
        assert_eq!(verdict, EgressVerdict::Continue, "approve → Continue (release once)");
        assert!(!verdict.remembers_domain(), "binary approve must NOT remember the domain");
        // The gate saw the value-free preview (host + field NAMES, never values).
        let seen = gate.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        match &seen[0].kind {
            ApprovalKind::CrossOriginPost { host, size, field_names } => {
                assert_eq!(host, "evil.example.com");
                assert_eq!(*size, 42);
                assert_eq!(field_names, &vec!["username".to_string(), "card".to_string()]);
            }
            _ => panic!("expected CrossOriginPost ask"),
        }
    }

    #[tokio::test]
    async fn egress_approver_deny_maps_to_fail_closed() {
        let gate = Arc::new(FakeGate::new(ApprovalDecision::Deny));
        let approver = GateEgressApprover::new(gate);
        let preview = PostPreview { host: "x.test".into(), size: 1, field_names: vec![] };
        let verdict = approver.approve_egress(&preview).await;
        assert_eq!(verdict, EgressVerdict::Fail, "deny → Fail (fail-closed)");
        assert!(!verdict.is_continue());
    }

    #[test]
    fn ask_description_never_leaks_values_only_names() {
        let ask = ApprovalAsk {
            kind: ApprovalKind::CrossOriginPost {
                host: "shop.test".into(),
                size: 100,
                field_names: vec!["card_number".into()],
            },
            screenshot: None,
        };
        let desc = ask.description();
        // Field NAME appears; the prompt explicitly notes values are not shown.
        assert!(desc.contains("card_number"));
        assert!(desc.contains("shop.test"));
        assert!(desc.contains("values are not shown"));
    }

    #[test]
    fn decision_is_approved_only_for_approve() {
        assert!(ApprovalDecision::Approve.is_approved());
        assert!(!ApprovalDecision::Deny.is_approved());
    }
}
