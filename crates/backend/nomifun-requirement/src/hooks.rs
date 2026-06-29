//! One-directional integration seam between AutoWork and IDMM.
//!
//! AutoWork (this crate) drives turn execution; IDMM (the `nomifun-idmm` crate)
//! supervises a session for stalls. To let AutoWork ensure a session is being
//! supervised while a turn runs — WITHOUT this crate depending on `nomifun-idmm`
//! (which would be a cycle, since idmm conceptually sits above requirement) —
//! AutoWork defines this trait and holds an optional handle. `nomifun-idmm`
//! implements it; `nomifun-app` injects the implementation at assembly time.

use nomifun_api_types::AutoWorkTargetKind;

/// Implemented by `nomifun-idmm::IdmmManager`. AutoWork calls
/// `ensure_supervising` at the top of each loop iteration so that, if the user
/// enabled IDMM for this target, supervision is (idempotently) running while the
/// turn executes. The implementation resolves the session owner and config
/// internally; this call is cheap and a no-op when IDMM is disabled or already
/// supervising the target.
#[async_trait::async_trait]
pub trait IdmmHandle: Send + Sync {
    fn ensure_supervising(&self, kind: AutoWorkTargetKind, target_id: &str);

    /// Whether a supervisor is currently live for `(kind, target_id)`. AutoWork
    /// uses this to decide whether to WAIT THROUGH a retryable error (IDMM owns
    /// in-turn recovery and will retry) instead of immediately failing the turn
    /// and racing a fresh requirement into the same session. Returns false when
    /// IDMM is disabled / not supervising — then AutoWork keeps the legacy
    /// "first error fails the turn" behavior.
    ///
    /// `kind` is part of the identity: a conversation and a terminal can share a
    /// numeric `target_id`, so supervision state is keyed by `(kind, target_id)`
    /// (spec §2.2 C3).
    fn is_supervising(&self, kind: AutoWorkTargetKind, target_id: &str) -> bool;

    /// Whether the supervised session currently has a PENDING DECISION the agent
    /// is waiting on — it ended its turn on a 选择题/开放式提问 (or is blocked on a
    /// tool-permission) that IDMM's DECISION watch will answer. Reuses IDMM's own
    /// on-arm detection (`SessionProbe::pending_signal`), so AutoWork's decision
    /// behaves exactly like what IDMM will act on.
    ///
    /// AutoWork uses this to YIELD on a decision-ending turn: rather than treating
    /// the question as the requirement's terminal state (parking it `needs_review`
    /// + burning an attempt) and racing a fresh requirement into the session, it
    /// waits for IDMM to answer and the work to continue. Returns false when IDMM
    /// is disabled / not supervising / the decision watch is off / nothing pending
    /// — then AutoWork keeps its legacy "a clean finish ends the turn" behavior.
    async fn has_pending_decision(&self, kind: AutoWorkTargetKind, target_id: &str) -> bool;
}
