use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use nomifun_api_types::RemoteBuildExtra;
use nomifun_common::{AgentKillReason, AppError, RemoteAgentProtocol};
use tracing::warn;

use crate::runtime_handle::AgentRuntimeHandle;
use crate::factory::AgentFactoryDeps;
use crate::factory::construction_guard::ConstructionGuard;
use crate::factory::context::FactoryContext;
use crate::manager::openclaw::device_identity::identity_from_secret_bytes;
use crate::manager::remote::{RemoteAgentConfig, RemoteAgentManager};
use crate::types::AgentRuntimeBuildOptions;

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    options: AgentRuntimeBuildOptions,
    ctx: FactoryContext,
) -> Result<AgentRuntimeHandle, AppError> {
    let extra: RemoteBuildExtra = serde_json::from_value(options.extra)
        .map_err(|e| AppError::BadRequest(format!("Invalid Remote build options: {e}")))?;
    let resume_session_key = extra.session_key.clone();
    let row = deps
        .remote_agent_repo
        .find_by_id(&extra.remote_agent_id)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to load remote agent config: {e}")))?
        .ok_or_else(|| AppError::NotFound(format!("Remote agent '{}' not found", extra.remote_agent_id)))?;
    let auth_token = row
        .auth_token
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(|encrypted| nomifun_common::decrypt_string(encrypted, &deps.encryption_key))
        .transpose()
        .inspect_err(|e| {
            warn!(error = %e, "Failed to decrypt remote agent auth_token");
        })?;
    let device_token = row
        .device_token
        .as_deref()
        .map(|encrypted| nomifun_common::decrypt_string(encrypted, &deps.encryption_key))
        .transpose()?;
    let device_identity = match (row.device_id.as_deref(), row.device_private_key.as_deref()) {
        (None, None) => {
            return Err(AppError::Internal(
                "Remote agent has no dedicated OpenClaw device identity; delete and re-create the remote agent configuration".into(),
            ));
        }
        (Some(device_id), Some(encrypted)) => {
            let private_b64 = nomifun_common::decrypt_string(encrypted, &deps.encryption_key)?;
            let private_bytes = BASE64
                .decode(private_b64)
                .map_err(|e| AppError::Internal(format!("Invalid remote device private key: {e}")))?;
            Some(identity_from_secret_bytes(device_id.to_owned(), &private_bytes)?)
        }
        _ => {
            return Err(AppError::Internal(
                "Remote agent device identity is incomplete; re-create the remote agent configuration".into(),
            ));
        }
    };
    let config = RemoteAgentConfig {
        // The runtime keeps the canonical entity ID as its stable identity.
        remote_agent_id: extra.remote_agent_id.clone(),
        protocol: serde_json::from_value(serde_json::Value::String(row.protocol.clone()))
            .unwrap_or(RemoteAgentProtocol::Acp),
        url: row.url.clone(),
        auth_type: row.auth_type.clone(),
        auth_token,
        device_token,
        allow_insecure: row.allow_insecure,
        resume_session_key,
        device_identity,
    };
    let (agent, issued_device_token) =
        RemoteAgentManager::connect(ctx.conversation_id, ctx.workspace, config).await?;
    let mut construction_guard = ConstructionGuard::new(
        Arc::clone(&agent),
        "Remote OpenClaw runtime",
        |agent| {
            crate::AgentRuntimeControl::kill(
                agent,
                Some(AgentKillReason::UserCancelled),
            )
        },
    );
    if let Some(device_token) = issued_device_token {
        let encrypted = match nomifun_common::encrypt_string(&device_token, &deps.encryption_key) {
            Ok(encrypted) => encrypted,
            Err(error) => {
                return Err(
                    construction_guard
                        .teardown_before_error(error, |agent| async move {
                            agent
                                .kill_and_wait(Some(AgentKillReason::UserCancelled))
                                .await
                        })
                        .await,
                );
            }
        };
        if let Err(error) = deps
            .remote_agent_repo
            .update_device_token(&extra.remote_agent_id, Some(&encrypted))
            .await
            .map_err(|e| AppError::Internal(format!("Failed to persist remote device token: {e}")))
        {
            return Err(
                construction_guard
                    .teardown_before_error(error, |agent| async move {
                        agent
                            .kill_and_wait(Some(AgentKillReason::UserCancelled))
                            .await
                    })
                    .await,
            );
        }
    }
    // Start the self-retaining relay only after every fallible factory await.
    // If token persistence fails, the ordinary path awaits ordered connection
    // teardown before returning; Drop remains only the abruptly-cancelled
    // future / panic backstop.
    agent.start_event_relay().await;
    construction_guard.disarm();
    Ok(AgentRuntimeHandle::Remote(agent))
}
