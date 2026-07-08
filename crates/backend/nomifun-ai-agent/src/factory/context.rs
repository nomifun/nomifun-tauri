//! Workspace resolution + per-agent metadata shared across factory
//! builders. Produced by `FactoryContext::resolve` at the top of
//! `build_agent`, then passed into the per-agent `build(..)` functions.

use nomifun_common::{AgentType, AppError};

use crate::factory::AgentFactoryDeps;
use crate::types::BuildTaskOptions;

const TEMP_WORKSPACE_ID_EXTRA_KEY: &str = "temp_workspace_id";

pub(super) struct FactoryContext {
    pub conversation_id: String,
    pub workspace: String,
    pub is_custom_workspace: bool,
}

impl FactoryContext {
    pub async fn resolve(deps: &AgentFactoryDeps, options: &BuildTaskOptions) -> Result<Self, AppError> {
        let conversation_id = options.conversation_id.clone();

        // `is_custom_workspace` is the authoritative signal for "user
        // chose this path" — determined here and plumbed down to the
        // managers that care (currently AcpAgentManager, for first-message
        // injection). Do NOT re-derive it from the workspace string later:
        // user paths may incidentally contain "conversations" or "-temp-".
        let (workspace, is_custom_workspace) = if options.workspace.is_empty() {
            // Fallback workspace path: kept in sync with
            // ConversationService::create, which places auto-provisioned
            // workspaces under `{work_dir}/conversations/{label}-temp-{token}/`.
            // Reaching this branch means the caller did not supply an
            // `extra.workspace`; construct the same `{label}-temp-{token}`
            // layout so DB id reuse cannot land a new conversation in an old
            // temp workspace.
            let label = workspace_label(&options.agent_type, options.extra.get("backend"));
            let temp_workspace_id = temp_workspace_id_for_options(options);
            let dir = deps
                .work_dir
                .join("conversations")
                .join(format!("{label}-temp-{temp_workspace_id}"));
            std::fs::create_dir_all(&dir)
                .map_err(|e| AppError::Internal(format!("Failed to create temp workspace: {e}")))?;
            (dir.to_string_lossy().into_owned(), false)
        } else {
            (options.workspace.clone(), true)
        };

        Ok(Self {
            conversation_id,
            workspace,
            is_custom_workspace,
        })
    }
}

/// Label used in auto-provisioned temp workspace directory names.
///
/// For ACP conversations the label is the vendor string from
/// `extra.backend` (e.g. `"claude"`); otherwise the agent type's serde
/// name (e.g. `"nomi"`). Must stay in sync with
/// `ConversationService::create`'s `conversation_label`.
fn workspace_label(agent_type: &AgentType, backend: Option<&serde_json::Value>) -> String {
    if *agent_type == AgentType::Acp
        && let Some(serde_json::Value::String(s)) = backend
        && !s.is_empty()
    {
        return s.clone();
    }
    agent_type.serde_name().to_owned()
}

fn temp_workspace_id_for_options(options: &BuildTaskOptions) -> String {
    options
        .extra
        .get(TEMP_WORKSPACE_ID_EXTRA_KEY)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| match options.conversation_created_at {
            Some(created_at) => format!("legacy-{}-{created_at}", options.conversation_id),
            None => format!("legacy-{}", options.conversation_id),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::ProviderWithModel;
    use serde_json::json;

    fn options(extra: serde_json::Value, created_at: Option<i64>) -> BuildTaskOptions {
        BuildTaskOptions {
            agent_type: AgentType::Acp,
            workspace: String::new(),
            model: ProviderWithModel {
                provider_id: "p".into(),
                model: "m".into(),
                use_model: None,
            },
            conversation_id: "1".into(),
            extra,
            conversation_created_at: created_at,
        }
    }

    #[test]
    fn temp_workspace_id_prefers_backend_minted_token() {
        let opts = options(json!({ "temp_workspace_id": "ws_abc", "backend": "claude" }), Some(10));
        assert_eq!(temp_workspace_id_for_options(&opts), "ws_abc");
    }

    #[test]
    fn legacy_temp_workspace_id_includes_created_at_to_avoid_id_only_reuse() {
        let first = options(json!({ "backend": "claude" }), Some(10));
        let second = options(json!({ "backend": "claude" }), Some(20));

        assert_ne!(
            temp_workspace_id_for_options(&first),
            temp_workspace_id_for_options(&second),
            "legacy fallback must not be derived solely from reusable conversation_id"
        );
    }
}
