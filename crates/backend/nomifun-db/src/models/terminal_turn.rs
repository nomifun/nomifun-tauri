use serde::{Deserialize, Serialize};
use std::fmt;

/// Permanent delivery receipt for one automatic Requirement turn admitted to
/// one exact live PTY generation.
///
/// `turn_token` is the capability carried by every later phase transition. A
/// lifecycle hook does not carry this token today, so a hook event alone cannot
/// settle this row.
#[derive(Clone, Serialize, Deserialize, sqlx::FromRow, PartialEq, Eq)]
pub struct TerminalTurnAdmissionRow {
    pub id: i64,
    #[serde(skip_serializing, default)]
    pub turn_token: String,
    pub terminal_id: String,
    pub pty_epoch: i64,
    pub requirement_id: String,
    pub claim_generation: i64,
    /// Same opaque Requirement claim capability used at every PTY boundary.
    #[serde(skip_serializing, default)]
    pub claim_token: Option<String>,
    /// `admitted` | `body_written` | `effects_started` | `settled`.
    pub phase: String,
    /// `done` | `failed` | `needs_review` | `cancelled` once settled.
    pub outcome: Option<String>,
    pub detail: Option<String>,
    pub admitted_at: i64,
    pub effects_started_at: Option<i64>,
    pub settled_at: Option<i64>,
}

impl fmt::Debug for TerminalTurnAdmissionRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalTurnAdmissionRow")
            .field("id", &self.id)
            .field("turn_token", &"<redacted>")
            .field("terminal_id", &self.terminal_id)
            .field("pty_epoch", &self.pty_epoch)
            .field("requirement_id", &self.requirement_id)
            .field("claim_generation", &self.claim_generation)
            .field(
                "claim_token",
                &self.claim_token.as_ref().map(|_| "<redacted>"),
            )
            .field("phase", &self.phase)
            .field("outcome", &self.outcome)
            .field("detail", &self.detail)
            .field("admitted_at", &self.admitted_at)
            .field("effects_started_at", &self.effects_started_at)
            .field("settled_at", &self.settled_at)
            .finish()
    }
}
