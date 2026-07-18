use std::sync::Arc;

use nomifun_api_types::AgentErrorCode;
use nomifun_common::{AgentKillReason, AgentType, ConversationSource, now_ms};
use nomifun_db::{ConversationRowUpdate, SaveRuntimeStateParams};
use tracing::{info, warn};

use crate::convert::string_to_enum;
use crate::service::ConversationService;
use crate::stream_relay::RelayOutcome;
use nomifun_ai_agent::AgentRuntimeRegistry;
use tokio_util::sync::CancellationToken;

impl ConversationService {
    async fn clear_conversation_model_seed_after_model_not_found(
        &self,
        conversation_id: &str,
        error_code: Option<AgentErrorCode>,
    ) {
        if error_code != Some(AgentErrorCode::UserLlmProviderModelNotFound) {
            return;
        }

        let conv_id = conversation_id;

        let row = match self.conversation_repo().get(conv_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                warn!(
                    conversation_id,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "Conversation ACP model seed clear skipped because conversation row is missing"
                );
                return;
            }
            Err(err) => {
                warn!(
                    conversation_id,
                    error = %err,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "Failed to load conversation before clearing ACP model seed"
                );
                return;
            }
        };

        let mut extra: serde_json::Value = match serde_json::from_str(&row.extra) {
            Ok(extra) => extra,
            Err(err) => {
                warn!(
                    conversation_id,
                    error = %err,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "Conversation ACP model seed clear skipped because extra JSON is invalid"
                );
                return;
            }
        };

        let Some(extra_obj) = extra.as_object_mut() else {
            warn!(
                conversation_id,
                error_code = ?error_code,
                reason = ?AgentKillReason::AgentErrorRecovery,
                "Conversation ACP model seed clear skipped because extra is not an object"
            );
            return;
        };
        let Some(previous_model_value) = extra_obj.remove("current_model_id") else {
            return;
        };
        let previous_model_id = previous_model_value.as_str().map(ToOwned::to_owned);
        if previous_model_id.is_none() {
            warn!(
                conversation_id,
                error_code = ?error_code,
                reason = ?AgentKillReason::AgentErrorRecovery,
                "Conversation ACP model seed was malformed and will be cleared"
            );
        }

        let extra_json = match serde_json::to_string(&extra) {
            Ok(json) => json,
            Err(err) => {
                warn!(
                    conversation_id,
                    ?previous_model_id,
                    error = %err,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "Failed to serialize conversation extra after clearing ACP model seed"
                );
                return;
            }
        };
        let update = ConversationRowUpdate {
            extra: Some(extra_json),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        if let Err(err) = self.conversation_repo().update(conv_id, &update).await {
            warn!(
                conversation_id,
                ?previous_model_id,
                error = %err,
                error_code = ?error_code,
                reason = ?AgentKillReason::AgentErrorRecovery,
                "Failed to clear conversation ACP model seed after model_not_found"
            );
            return;
        }

        let source = row
            .source
            .as_deref()
            .and_then(|value| string_to_enum::<ConversationSource>(value).ok());
        self.broadcast_list_changed(&row.user_id, conversation_id, "updated", source.as_ref());
        info!(
            conversation_id,
            ?previous_model_id,
            error_code = ?error_code,
            reason = ?AgentKillReason::AgentErrorRecovery,
            "Conversation ACP model seed cleared after model_not_found"
        );
    }

    async fn clear_persisted_acp_model_after_model_not_found(
        &self,
        conversation_id: &str,
        error_code: Option<AgentErrorCode>,
    ) {
        if error_code != Some(AgentErrorCode::UserLlmProviderModelNotFound) {
            return;
        }

        let conv_id = conversation_id;

        let previous_model_id = match self.acp_session_repo().load_runtime_state(conv_id).await {
            Ok(Some(state)) => state.current_model_id,
            Ok(None) => None,
            Err(err) => {
                warn!(
                    conversation_id,
                    error = %err,
                    "Failed to load ACP persisted model before clearing after model_not_found"
                );
                None
            }
        };

        let params = SaveRuntimeStateParams {
            current_model_id: Some(None),
            ..Default::default()
        };
        match self
            .acp_session_repo()
            .save_runtime_state(conv_id, &params)
            .await
        {
            Ok(true) => {
                info!(
                    conversation_id,
                    ?previous_model_id,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "ACP persisted model cleared after model_not_found"
                );
            }
            Ok(false) => {
                warn!(
                    conversation_id,
                    ?previous_model_id,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "ACP persisted model clear skipped because session row is missing"
                );
            }
            Err(err) => {
                warn!(
                    conversation_id,
                    ?previous_model_id,
                    error = %err,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "Failed to clear ACP persisted model after model_not_found"
                );
            }
        }
    }

    pub(crate) async fn evict_acp_task_after_terminal_error(
        &self,
        conversation_id: &str,
        agent_type: AgentType,
        outcome: &RelayOutcome,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        turn_generation: u64,
        cancellation: &CancellationToken,
    ) -> bool {
        let _ = turn_generation;
        if agent_type != AgentType::Acp || !outcome.terminal.is_error() {
            return false;
        }
        if cancellation.is_cancelled() {
            return true;
        }

        let started_at = now_ms();
        let error_code = outcome.terminal.code();
        let retryable = outcome.terminal.retryable();
        info!(
            conversation_id,
            ?agent_type,
            error_code = ?error_code,
            retryable = ?retryable,
            reason = ?AgentKillReason::AgentErrorRecovery,
            "ACP task marked unhealthy after terminal error; evicting task"
        );
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return true,
            _ = runtime_registry.terminate_and_wait(
                conversation_id,
                Some(AgentKillReason::AgentErrorRecovery),
            ) => {}
        }
        if cancellation.is_cancelled() {
            return true;
        }
        self.clear_persisted_acp_model_after_model_not_found(conversation_id, error_code)
            .await;
        if cancellation.is_cancelled() {
            return true;
        }
        self.clear_conversation_model_seed_after_model_not_found(conversation_id, error_code)
            .await;
        info!(
            conversation_id,
            ?agent_type,
            error_code = ?error_code,
            retryable = ?retryable,
            elapsed_ms = now_ms().saturating_sub(started_at),
            reason = ?AgentKillReason::AgentErrorRecovery,
            "ACP task eviction completed after terminal error"
        );
        true
    }
}
