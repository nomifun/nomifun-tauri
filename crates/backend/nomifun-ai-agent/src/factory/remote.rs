use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use nomifun_api_types::RemoteBuildExtra;
use nomifun_common::{AgentKillReason, AppError, RemoteAgentProtocol};
use tracing::warn;

use crate::runtime_handle::AgentRuntimeHandle;
use crate::factory::AgentFactoryDeps;
use crate::factory::context::FactoryContext;
use crate::manager::openclaw::device_identity::identity_from_secret_bytes;
use crate::manager::remote::{RemoteAgentConfig, RemoteAgentManager};
use crate::types::AgentRuntimeBuildOptions;

struct RemoteConstructionGuard {
    agent: Option<Arc<RemoteAgentManager>>,
}

impl RemoteConstructionGuard {
    fn new(agent: Arc<RemoteAgentManager>) -> Self {
        Self { agent: Some(agent) }
    }

    fn disarm(&mut self) {
        self.agent = None;
    }
}

impl Drop for RemoteConstructionGuard {
    fn drop(&mut self) {
        if let Some(agent) = self.agent.take() {
            let _ = crate::AgentRuntimeControl::kill(
                agent.as_ref(),
                Some(AgentKillReason::UserCancelled),
            );
        }
    }
}

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
        remote_agent_id: row.id.clone(),
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
    let mut construction_guard = RemoteConstructionGuard::new(Arc::clone(&agent));
    if let Some(device_token) = issued_device_token {
        let encrypted = nomifun_common::encrypt_string(&device_token, &deps.encryption_key)?;
        deps.remote_agent_repo
            .update_device_token(&row.id, Some(&encrypted))
            .await
            .map_err(|e| AppError::Internal(format!("Failed to persist remote device token: {e}")))?;
    }
    // Start the self-retaining relay only after every fallible factory await.
    // If token persistence is cancelled/fails, the armed guard closes the
    // already-connected WebSocket instead of leaving an unregistered manager.
    agent.start_event_relay().await;
    construction_guard.disarm();
    Ok(AgentRuntimeHandle::Remote(agent))
}
