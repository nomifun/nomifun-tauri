//! Custom Agent business logic.
//!
//! Extends `AgentService` with CRUD for `agent_source = 'custom'` rows
//! in the `agent_metadata` catalog. Mirrors the frontend PRD
//! F-CAGENT-04 / -05 / -12 / -13 / -14 (create, edit, save, delete,
//! toggle enable).
//!
//! Test-on-save: create / update run `try_connect_custom_agent`
//! before hitting the DB. Failures become `AppError::BadRequest` with
//! a prefixed marker (`cli_not_found:` / `acp_init_failed:`) that the
//! frontend maps back to the same three Alert states it shows for the
//! manual "Test connection" button.

use std::collections::HashMap;
use std::path::Path;

use nomifun_api_types::{
    AgentMetadata, CustomAgentUpsertRequest, TryConnectCustomAgentRequest, TryConnectCustomAgentResponse,
};
use nomifun_common::{AgentId, AppError};
use nomifun_db::UpsertAgentMetadataParams;
use tracing::warn;

use super::AgentService;
use crate::protocol::custom_agent_probe::try_connect_custom_agent as probe;

const CUSTOM_SORT_ORDER_DEFAULT: i64 = 1500;

impl AgentService {
    /// Public accessor for the probe — powers both
    /// `POST /api/agents/custom/try-connect` and the test-on-save path
    /// below.
    pub async fn try_connect_custom_agent(
        &self,
        req: TryConnectCustomAgentRequest,
    ) -> Result<TryConnectCustomAgentResponse, AppError> {
        if req.command.trim().is_empty() {
            return Err(AppError::BadRequest("command must not be empty".into()));
        }
        Ok(probe(&req.command, &req.acp_args, &req.env, self.data_dir()).await)
    }

    pub async fn create_custom_agent(&self, req: CustomAgentUpsertRequest) -> Result<AgentMetadata, AppError> {
        validate_upsert(&req)?;
        probe_or_reject(&req, self.data_dir()).await?;

        let agent_id = AgentId::new();
        self.upsert_custom_row(agent_id.as_str(), &req, /* keep_enabled = */ true)
            .await
    }

    pub async fn update_custom_agent(
        &self,
        agent_id: &str,
        req: CustomAgentUpsertRequest,
    ) -> Result<AgentMetadata, AppError> {
        validate_upsert(&req)?;
        let existing = self
            .registry()
            .repo_handle()
            .get(agent_id)
            .await
            .map_err(|e| AppError::Internal(format!("repo.get: {e}")))?
            .ok_or_else(|| AppError::NotFound(format!("Agent '{agent_id}' not found")))?;
        if existing.agent_source != "custom" {
            return Err(AppError::Forbidden(
                "Only custom agents can be edited via this endpoint".into(),
            ));
        }
        probe_or_reject(&req, self.data_dir()).await?;

        let keep_enabled = existing.enabled;
        self.upsert_custom_row(agent_id, &req, keep_enabled).await
    }

    pub async fn delete_custom_agent(&self, agent_id: &str) -> Result<(), AppError> {
        let existing = self
            .registry()
            .repo_handle()
            .get(agent_id)
            .await
            .map_err(|e| AppError::Internal(format!("repo.get: {e}")))?
            .ok_or_else(|| AppError::NotFound(format!("Agent '{agent_id}' not found")))?;
        if existing.agent_source != "custom" {
            return Err(AppError::Forbidden(
                "Only custom agents can be deleted via this endpoint".into(),
            ));
        }
        let removed = self
            .registry()
            .repo_handle()
            .delete(agent_id)
            .await
            .map_err(|e| AppError::Internal(format!("repo.delete: {e}")))?;
        if !removed {
            return Err(AppError::NotFound(format!("Agent '{agent_id}' not found")));
        }
        if let Err(err) = self.registry().invalidate_and_rehydrate().await {
            warn!(agent_id = %agent_id, error = %err, "registry rehydrate failed after delete_custom_agent");
        }
        Ok(())
    }

    pub async fn set_agent_enabled(&self, agent_id: &str, enabled: bool) -> Result<AgentMetadata, AppError> {
        let updated = self
            .registry()
            .repo_handle()
            .set_enabled(agent_id, enabled)
            .await
            .map_err(|e| AppError::Internal(format!("repo.set_enabled: {e}")))?;
        if !updated {
            return Err(AppError::NotFound(format!("Agent '{agent_id}' not found")));
        }
        if let Err(err) = self.registry().invalidate_and_rehydrate().await {
            warn!(agent_id = %agent_id, error = %err, "registry rehydrate failed after set_agent_enabled");
        }
        self.registry()
            .get(agent_id)
            .await
            .ok_or_else(|| AppError::Internal(format!("Agent '{agent_id}' not visible after enable toggle")))
    }

    async fn upsert_custom_row(
        &self,
        id: &str,
        req: &CustomAgentUpsertRequest,
        enabled: bool,
    ) -> Result<AgentMetadata, AppError> {
        let advanced = req.advanced.clone().unwrap_or_default();

        let args_json =
            serde_json::to_string(&req.args).map_err(|e| AppError::Internal(format!("encode args: {e}")))?;
        let env_json = serde_json::to_string(&req.env).map_err(|e| AppError::Internal(format!("encode env: {e}")))?;
        let native_skills_dirs_json = advanced
            .native_skills_dirs
            .as_ref()
            .map(|v| {
                serde_json::to_string(v).map_err(|e| AppError::Internal(format!("encode native_skills_dirs: {e}")))
            })
            .transpose()?;
        let behavior_policy_json = advanced
            .behavior_policy
            .as_ref()
            .map(|v| serde_json::to_string(v).map_err(|e| AppError::Internal(format!("encode behavior_policy: {e}"))))
            .transpose()?;

        let source_info = serde_json::json!({
            "binary_name": first_token(&req.command),
        });
        let source_info_json = source_info.to_string();

        let params = UpsertAgentMetadataParams {
            agent_id: id,
            icon: req.icon.as_deref(),
            name: req.name.trim(),
            name_i18n: None,
            description: advanced.description.as_deref(),
            description_i18n: None,
            backend: None,
            agent_type: "acp",
            agent_source: "custom",
            agent_source_info: Some(&source_info_json),
            enabled,
            command: Some(req.command.trim()),
            args: Some(&args_json),
            env: Some(&env_json),
            native_skills_dirs: native_skills_dirs_json.as_deref(),
            behavior_policy: behavior_policy_json.as_deref(),
            yolo_id: advanced.yolo_id.as_deref(),
            agent_capabilities: None,
            auth_methods: None,
            config_options: None,
            available_modes: None,
            available_models: None,
            available_commands: None,
            sort_order: CUSTOM_SORT_ORDER_DEFAULT,
        };

        self.registry()
            .repo_handle()
            .upsert(&params)
            .await
            .map_err(|e| AppError::Internal(format!("repo.upsert: {e}")))?;

        self.registry()
            .invalidate_and_rehydrate()
            .await
            .map_err(|e| AppError::Internal(format!("registry rehydrate: {e}")))?;

        self.registry()
            .get(id)
            .await
            .ok_or_else(|| AppError::Internal(format!("Agent '{id}' not visible after upsert")))
    }
}

fn validate_upsert(req: &CustomAgentUpsertRequest) -> Result<(), AppError> {
    if req.name.trim().is_empty() {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }
    if req.command.trim().is_empty() {
        return Err(AppError::BadRequest("command must not be empty".into()));
    }
    Ok(())
}

async fn probe_or_reject(req: &CustomAgentUpsertRequest, data_dir: &Path) -> Result<(), AppError> {
    // Test-only bypass — real probe spawns a child process and relies
    // on a working ACP CLI on PATH, which is not present in CI.
    // Gated behind cfg(test) / the `test-support` feature so production
    // builds cannot be tricked into skipping the probe via env var.
    #[cfg(any(test, feature = "test-support"))]
    if std::env::var("NOMIFUN_BYPASS_PROBE").is_ok() {
        tracing::warn!("NOMIFUN_BYPASS_PROBE set — skipping custom agent probe. Test-only.");
        return Ok(());
    }

    let env_map: HashMap<String, String> = req.env.iter().map(|e| (e.name.clone(), e.value.clone())).collect();
    match probe(&req.command, &req.args, &env_map, data_dir).await {
        TryConnectCustomAgentResponse::Success => Ok(()),
        TryConnectCustomAgentResponse::FailCli { error } => {
            Err(AppError::BadRequest(format!("cli_not_found: {error}")))
        }
        TryConnectCustomAgentResponse::FailAcp { error } => {
            Err(AppError::BadRequest(format!("acp_init_failed: {error}")))
        }
    }
}

fn first_token(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or(s)
}
