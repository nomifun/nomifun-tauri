//! Trusted Conversation effects used by the Agent Execution infrastructure.
//!
//! Public HTTP and gateway handlers receive [`ConversationService`] and can
//! only call its ordinary user-facing methods. Stable operation identities are
//! accepted exclusively through this explicitly constructed infrastructure
//! port, so a client request can never opt itself into the durable delivery
//! path by adding a JSON field.

use std::sync::Arc;

use nomifun_ai_agent::AgentRuntimeRegistry;
use nomifun_api_types::SendMessageRequest;
use nomifun_common::AppError;
use nomifun_db::AgentExecutionTurnAuthority;

use crate::{ConversationService, IdempotentMessageDelivery};

#[derive(Clone)]
pub struct AgentExecutionConversationPort {
    service: ConversationService,
    runtime_registry: Arc<dyn AgentRuntimeRegistry>,
}

impl AgentExecutionConversationPort {
    pub(crate) fn new(
        service: ConversationService,
        runtime_registry: Arc<dyn AgentRuntimeRegistry>,
    ) -> Self {
        Self {
            service,
            runtime_registry,
        }
    }

    /// Deliver an initial or decision-continuation turn under a stable,
    /// execution-owned operation identity.
    pub async fn deliver_turn(
        &self,
        owner_id: &str,
        conversation_id: &str,
        operation_id: &str,
        authority: AgentExecutionTurnAuthority,
        request: SendMessageRequest,
    ) -> Result<IdempotentMessageDelivery, AppError> {
        self.service
            .send_agent_execution_message_idempotent(
                owner_id,
                conversation_id,
                operation_id,
                authority,
                request,
                &self.runtime_registry,
            )
            .await
    }

    pub async fn delivery_result(
        &self,
        owner_id: &str,
        conversation_id: &str,
        operation_id: &str,
    ) -> Result<Option<IdempotentMessageDelivery>, AppError> {
        self.service
            .idempotent_delivery_result(owner_id, conversation_id, operation_id)
            .await
    }

    /// Deliver a durable mid-turn control effect without falling back to a new
    /// ordinary Conversation turn.
    pub async fn steer_turn(
        &self,
        owner_id: &str,
        conversation_id: &str,
        operation_id: &str,
        request: SendMessageRequest,
    ) -> Result<String, AppError> {
        self.service
            .steer_message_idempotent(
                owner_id,
                conversation_id,
                operation_id,
                request,
                &self.runtime_registry,
            )
            .await
    }
}

impl ConversationService {
    /// Construct the capability passed only to Agent Execution assembly.
    /// Durable operation identities are intentionally absent from every
    /// public request DTO and ordinary Conversation method.
    pub fn agent_execution_port(
        &self,
        runtime_registry: Arc<dyn AgentRuntimeRegistry>,
    ) -> AgentExecutionConversationPort {
        AgentExecutionConversationPort::new(self.clone(), runtime_registry)
    }
}
