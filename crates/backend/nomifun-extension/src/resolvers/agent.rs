use std::path::Path;

use tracing::warn;

use crate::asset_paths::resolve_extension_asset_url;
use crate::error::ExtensionError;
use crate::resolvers::extension_source_key;
use crate::template::resolve_file_reference;
use crate::types::{ExtAgent, ResolvedAgent};

/// Resolve a single agent contribution.
///
/// The `context` field supports `@file:` references.
pub fn resolve_agent(agent: &ExtAgent, extension_name: &str, ext_dir: &Path) -> Result<ResolvedAgent, ExtensionError> {
    let source_key = extension_source_key(extension_name, &agent.source_key)?;
    let context = agent
        .context
        .as_deref()
        .map(|v| resolve_file_reference(v, ext_dir))
        .transpose()?;

    let icon = agent
        .icon
        .as_deref()
        .and_then(|value| resolve_extension_asset_url(extension_name, value));

    Ok(ResolvedAgent {
        extension_name: extension_name.to_owned(),
        source_key,
        name: agent.name.clone(),
        description: agent.description.clone(),
        agent_type: agent.agent_type.clone(),
        context,
        icon,
        enabled_skills: agent.enabled_skills.clone(),
        prompts: agent.prompts.clone(),
        models: agent.models.clone(),
    })
}

/// Resolve all agent contributions from an extension.
pub fn resolve_agents(agents: &[ExtAgent], extension_name: &str, ext_dir: &Path) -> Vec<ResolvedAgent> {
    agents
        .iter()
        .filter_map(|a| {
            resolve_agent(a, extension_name, ext_dir)
                .map_err(|e| {
                    warn!(
                        extension = extension_name,
                        agent_source_key = a.source_key,
                        "Failed to resolve agent: {e}"
                    );
                    e
                })
                .ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_agent_plain_text() {
        let agent = ExtAgent {
            source_key: "agent-1".into(),
            name: "My Agent".into(),
            description: Some("Autonomous agent".into()),
            agent_type: Some("claude".into()),
            context: Some("You are an agent.".into()),
            icon: None,
            enabled_skills: vec![],
            prompts: vec![],
            models: vec![],
        };

        let result = resolve_agent(&agent, "my-ext", Path::new("/ext/my-ext")).unwrap();

        assert_eq!(result.extension_name, "my-ext");
        assert_eq!(result.source_key, "my-ext:agent-1");
        assert_eq!(result.agent_type.as_deref(), Some("claude"));
        assert_eq!(result.context.as_deref(), Some("You are an agent."));
    }

    #[test]
    fn test_resolve_agent_file_reference() {
        let dir = std::env::temp_dir().join("ext_test_resolve_agent");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent_ctx.md"), "Agent context from file").unwrap();

        let agent = ExtAgent {
            source_key: "agent-2".into(),
            name: "File Agent".into(),
            description: None,
            agent_type: None,
            context: Some("@file:agent_ctx.md".into()),
            icon: None,
            enabled_skills: vec![],
            prompts: vec![],
            models: vec![],
        };

        let result = resolve_agent(&agent, "my-ext", &dir).unwrap();
        assert_eq!(result.context.as_deref(), Some("Agent context from file"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_resolve_agent_missing_file_error() {
        let agent = ExtAgent {
            source_key: "agent-3".into(),
            name: "Bad Agent".into(),
            description: None,
            agent_type: None,
            context: Some("@file:missing.md".into()),
            icon: None,
            enabled_skills: vec![],
            prompts: vec![],
            models: vec![],
        };

        let err = resolve_agent(&agent, "my-ext", Path::new("/tmp/no_such_ext_dir")).unwrap_err();
        assert!(matches!(err, ExtensionError::FileReferenceNotFound(_)));
    }

    #[test]
    fn test_resolve_agent_no_context() {
        let agent = ExtAgent {
            source_key: "agent-4".into(),
            name: "No Context".into(),
            description: None,
            agent_type: None,
            context: None,
            icon: None,
            enabled_skills: vec![],
            prompts: vec![],
            models: vec![],
        };

        let result = resolve_agent(&agent, "my-ext", Path::new("/ext/my-ext")).unwrap();
        assert!(result.context.is_none());
    }
}
