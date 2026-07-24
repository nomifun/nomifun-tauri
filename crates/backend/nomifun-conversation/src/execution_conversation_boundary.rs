//! Narrow cross-domain read port for Conversation/AgentExecution relations.
//!
//! Conversation owns message and deletion behavior; it must not receive the
//! complete execution repository merely to project and guard relations.

use std::sync::Arc;

use async_trait::async_trait;
use nomifun_common::{
    AgentExecutionId, AppError, ConversationExecutionRelation, ConversationId,
};
use nomifun_db::{
    AgentExecutionTurnAuthority, ConversationDeliveryReceiptClaim, IAgentExecutionRepository,
    TurnLifecycleTransition,
};
use nomifun_db::models::ConversationDeliveryReceiptRow;

/// Read-model projection exposed on a conversation response.
///
/// Conversation does not own execution state. These identifiers are derived
/// from the authoritative relation table on every read and are never persisted
/// in `conversations.extra` or duplicated in another column.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversationExecutionProjection {
    pub linked_execution_id: Option<String>,
    pub execution_step_id: Option<String>,
    pub execution_attempt_id: Option<String>,
}

#[async_trait]
pub trait ExecutionConversationBoundary: Send + Sync {
    async fn projection(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<ConversationExecutionProjection, AppError>;

    async fn is_active_attempt(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<bool, AppError>;

    async fn is_retained_attempt(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<bool, AppError>;

    async fn claim_attempt_turn_receipt(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _candidate_message_id: &str,
        _kind: &str,
        _request_payload: &str,
        _authority: &AgentExecutionTurnAuthority,
        _expected_admission_epoch: i64,
        _now: i64,
    ) -> Result<ConversationDeliveryReceiptClaim, AppError> {
        Err(AppError::Conflict(
            "Agent Execution turn authority is unavailable in this process".to_owned(),
        ))
    }

    async fn abandon_exact_attempt_turn_admission(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _candidate_message_id: &str,
        _request_payload: &str,
        _authority: &AgentExecutionTurnAuthority,
        _expected_admitted_epoch: i64,
        _reason: &str,
        _completed_at: i64,
    ) -> Result<TurnLifecycleTransition, AppError> {
        Err(AppError::Conflict(
            "Agent Execution turn authority is unavailable in this process".to_owned(),
        ))
    }

    async fn validate_attempt_turn_effect(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _kind: &str,
        _request_payload: &str,
        _authority: &AgentExecutionTurnAuthority,
        _now: i64,
    ) -> Result<ConversationDeliveryReceiptRow, AppError> {
        Err(AppError::Conflict(
            "Agent Execution turn authority is unavailable in this process".to_owned(),
        ))
    }
}

/// Explicit boundary for isolated tests or processes whose database cannot
/// contain Agent Execution relations. Production assembly must use
/// [`RepositoryExecutionConversationBoundary`]; making this value explicit at
/// construction prevents a missing production dependency from silently
/// disabling mutation guards.
#[derive(Debug, Default)]
pub struct NoExecutionConversationBoundary;

#[async_trait]
impl ExecutionConversationBoundary for NoExecutionConversationBoundary {
    async fn projection(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
    ) -> Result<ConversationExecutionProjection, AppError> {
        Ok(ConversationExecutionProjection::default())
    }

    async fn is_active_attempt(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
    ) -> Result<bool, AppError> {
        Ok(false)
    }

    async fn is_retained_attempt(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
    ) -> Result<bool, AppError> {
        Ok(false)
    }

    async fn claim_attempt_turn_receipt(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _candidate_message_id: &str,
        _kind: &str,
        _request_payload: &str,
        _authority: &AgentExecutionTurnAuthority,
        _expected_admission_epoch: i64,
        _now: i64,
    ) -> Result<ConversationDeliveryReceiptClaim, AppError> {
        Err(AppError::Conflict(
            "Agent Execution turn authority is unavailable in this process".to_owned(),
        ))
    }

    async fn abandon_exact_attempt_turn_admission(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _candidate_message_id: &str,
        _request_payload: &str,
        _authority: &AgentExecutionTurnAuthority,
        _expected_admitted_epoch: i64,
        _reason: &str,
        _completed_at: i64,
    ) -> Result<TurnLifecycleTransition, AppError> {
        Err(AppError::Conflict(
            "Agent Execution turn authority is unavailable in this process".to_owned(),
        ))
    }

    async fn validate_attempt_turn_effect(
        &self,
        _owner_id: &str,
        _conversation_id: &str,
        _operation_id: &str,
        _kind: &str,
        _request_payload: &str,
        _authority: &AgentExecutionTurnAuthority,
        _now: i64,
    ) -> Result<ConversationDeliveryReceiptRow, AppError> {
        Err(AppError::Conflict(
            "Agent Execution turn authority is unavailable in this process".to_owned(),
        ))
    }
}

/// SQLite/repository adapter kept outside `ConversationService` so the service
/// depends only on the narrow cross-domain contract above.
pub struct RepositoryExecutionConversationBoundary {
    repository: Arc<dyn IAgentExecutionRepository>,
}

impl RepositoryExecutionConversationBoundary {
    pub fn new(repository: Arc<dyn IAgentExecutionRepository>) -> Self {
        Self { repository }
    }
}

#[async_trait]
impl ExecutionConversationBoundary for RepositoryExecutionConversationBoundary {
    async fn projection(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<ConversationExecutionProjection, AppError> {
        ConversationId::try_from(conversation_id)
            .map_err(|error| AppError::BadRequest(format!("invalid conversation_id: {error}")))?;
        let links = self
            .repository
            .resolve_conversation_link(owner_id, conversation_id)
            .await?;
        for link in &links {
            AgentExecutionId::try_from(link.execution_id.as_str()).map_err(|error| {
                AppError::Internal(format!("invalid persisted execution link execution_id: {error}"))
            })?;
            if link
                .step_id
                .as_deref()
                .is_some_and(|step_id| nomifun_common::validate_uuidv7(step_id).is_err())
            {
                return Err(AppError::Internal(
                    "invalid persisted execution link step_id".to_owned(),
                ));
            }
            if link
                .attempt_id
                .as_deref()
                .is_some_and(|attempt_id| nomifun_common::validate_uuidv7(attempt_id).is_err())
            {
                return Err(AppError::Internal(
                    "invalid persisted execution link attempt_id".to_owned(),
                ));
            }
        }

        let mut active_attempts = links.iter().filter(|link| {
            link.active && link.relation == ConversationExecutionRelation::Attempt.as_str()
        });
        let attempt = active_attempts.next();
        if active_attempts.next().is_some() {
            return Err(AppError::Conflict(
                "conversation has multiple active execution attempts".to_owned(),
            ));
        }

        // Attempt transcripts remain execution-owned after settlement and
        // cleanup acknowledgement. Keeping their historical identifiers in the
        // projection prevents them from leaking back into the ordinary session
        // list while the collaboration detail can still read the transcript.
        // The repository orders newest links first; an active attempt wins,
        // followed by the newest retained attempt, then an active lead.
        let retained_attempt = links.iter().find(|link| {
            link.relation == ConversationExecutionRelation::Attempt.as_str()
                && link.step_id.is_some()
                && link.attempt_id.is_some()
        });
        let attempt = attempt.or(retained_attempt);

        // Exactly one active lead is the Conversation's current collaboration;
        // inactive lead rows remain immutable execution history.
        let lead = links.iter().find(|link| {
            link.active && link.relation == ConversationExecutionRelation::Lead.as_str()
        });

        let linked_execution = attempt.or(lead);
        // A soft-deleted Execution is no longer a navigable resource. Attempt
        // identity remains visible and retained, but must not expose a dead
        // execution route that resolves to 404.
        let linked_execution_id = if let Some(link) = linked_execution {
            self.repository
                .get_execution(owner_id, &link.execution_id)
                .await?
                .map(|_| link.execution_id.clone())
        } else {
            None
        };

        Ok(ConversationExecutionProjection {
            linked_execution_id,
            execution_step_id: attempt.and_then(|link| link.step_id.clone()),
            execution_attempt_id: attempt.and_then(|link| link.attempt_id.clone()),
        })
    }

    async fn is_active_attempt(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<bool, AppError> {
        let links = self
            .repository
            .resolve_conversation_link(owner_id, conversation_id)
            .await?;
        Ok(links.iter().any(|link| {
            link.active
                && link.relation == ConversationExecutionRelation::Attempt.as_str()
                && link.attempt_id.is_some()
        }))
    }

    async fn is_retained_attempt(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<bool, AppError> {
        Ok(self
            .repository
            .has_attempt_conversation_link(owner_id, conversation_id)
            .await?)
    }

    async fn claim_attempt_turn_receipt(
        &self,
        owner_id: &str,
        conversation_id: &str,
        operation_id: &str,
        candidate_message_id: &str,
        kind: &str,
        request_payload: &str,
        authority: &AgentExecutionTurnAuthority,
        expected_admission_epoch: i64,
        now: i64,
    ) -> Result<ConversationDeliveryReceiptClaim, AppError> {
        Ok(self
            .repository
            .claim_attempt_turn_delivery_receipt(
                owner_id,
                conversation_id,
                operation_id,
                candidate_message_id,
                kind,
                request_payload,
                authority,
                expected_admission_epoch,
                now,
            )
            .await?)
    }

    async fn abandon_exact_attempt_turn_admission(
        &self,
        owner_id: &str,
        conversation_id: &str,
        operation_id: &str,
        candidate_message_id: &str,
        request_payload: &str,
        authority: &AgentExecutionTurnAuthority,
        expected_admitted_epoch: i64,
        reason: &str,
        completed_at: i64,
    ) -> Result<TurnLifecycleTransition, AppError> {
        Ok(self
            .repository
            .abandon_exact_attempt_turn_admission(
                owner_id,
                conversation_id,
                operation_id,
                candidate_message_id,
                request_payload,
                authority,
                expected_admitted_epoch,
                reason,
                completed_at,
            )
            .await?)
    }

    async fn validate_attempt_turn_effect(
        &self,
        owner_id: &str,
        conversation_id: &str,
        operation_id: &str,
        kind: &str,
        request_payload: &str,
        authority: &AgentExecutionTurnAuthority,
        now: i64,
    ) -> Result<ConversationDeliveryReceiptRow, AppError> {
        Ok(self
            .repository
            .validate_attempt_turn_effect_authority(
                owner_id,
                conversation_id,
                operation_id,
                kind,
                request_payload,
                authority,
                now,
            )
            .await?)
    }
}
