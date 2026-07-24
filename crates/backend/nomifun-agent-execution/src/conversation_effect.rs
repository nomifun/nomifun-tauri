//! Durable internal effects for one Agent attempt conversation.
//!
//! The attempt's `runtime_state` is the write-ahead intent.  External
//! Conversation delivery happens only after this value and its audit event
//! commit; successful settlement clears the state atomically with the attempt.

use nomifun_common::AppError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PendingConversationEffect {
    StopTurn {
        operation_id: String,
    },
    DecisionInput {
        operation_id: String,
        content: String,
    },
    Steer {
        operation_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttemptConversationEffects {
    pub(crate) pending_conversation_effects: Vec<PendingConversationEffect>,
    /// Fail-closed recovery marker written by the repository when a formerly
    /// Running invocation cannot prove that its external effect was untouched.
    /// There is intentionally no automatic resolution path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) review_blocked: Option<RecoveryReviewBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryReviewBlock {
    pub(crate) kind: String,
    pub(crate) operation_id: String,
    pub(crate) receipt_state: String,
    pub(crate) reason: String,
}

impl AttemptConversationEffects {
    pub(crate) fn push_stop_turn(&mut self, operation_id: String) -> Result<(), AppError> {
        if !self.pending_conversation_effects.is_empty() {
            return Err(AppError::Conflict(
                "a conversation effect is already pending for this attempt".to_owned(),
            ));
        }
        self.pending_conversation_effects
            .push(PendingConversationEffect::StopTurn { operation_id });
        Ok(())
    }

    pub(crate) fn push_steer(&mut self, operation_id: String, content: String) -> Result<(), AppError> {
        if !self.pending_conversation_effects.is_empty() {
            return Err(AppError::Conflict(
                "a conversation effect is already pending for this attempt".to_owned(),
            ));
        }
        self.pending_conversation_effects
            .push(PendingConversationEffect::Steer {
                operation_id,
                content,
            });
        Ok(())
    }

    pub(crate) fn push_decision(
        &mut self,
        operation_id: String,
        content: String,
    ) -> Result<(), AppError> {
        if self.review_blocked.is_some() {
            return Err(AppError::Conflict(
                "this interrupted Agent turn is blocked for manual review and cannot be resumed automatically"
                    .to_owned(),
            ));
        }
        if self
            .pending_conversation_effects
            .iter()
            .any(|effect| matches!(effect, PendingConversationEffect::DecisionInput { .. }))
        {
            return Err(AppError::Conflict(
                "a decision continuation is already pending for this attempt".to_owned(),
            ));
        }
        self.pending_conversation_effects
            .push(PendingConversationEffect::DecisionInput {
                operation_id,
                content,
            });
        Ok(())
    }

    pub(crate) fn decode(raw: Option<&str>) -> Result<Self, AppError> {
        match raw {
            Some(raw) => serde_json::from_str(raw).map_err(|error| {
                AppError::Internal(format!(
                    "invalid persisted attempt conversation effects: {error}"
                ))
            }),
            None => Ok(Self::default()),
        }
    }

    pub(crate) fn encode(&self) -> Result<String, AppError> {
        serde_json::to_string(self).map_err(|error| {
            AppError::Internal(format!("encode attempt conversation effects: {error}"))
        })
    }
}
