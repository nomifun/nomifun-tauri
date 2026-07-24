use std::path::Path;

use nomifun_common::validate_uuidv7;
use tracing::warn;

use crate::asset_paths::resolve_extension_asset_url;
use crate::error::ExtensionError;
use crate::resolvers::extension_source_key;
use crate::template::resolve_file_reference;
use crate::types::{ExtPreset, ResolvedPreset};

/// Resolve a single preset contribution.
///
/// Long-text fields (`system_prompt`, `context`) support `@file:` references
/// that are replaced with the referenced file's content.
pub fn resolve_preset(
    preset: &ExtPreset,
    extension_name: &str,
    ext_dir: &Path,
) -> Result<ResolvedPreset, ExtensionError> {
    let source_key = extension_source_key(extension_name, &preset.source_key)?;
    if let Some(preferred_agent_id) = preset.preferred_agent_id.as_deref()
        && validate_uuidv7(preferred_agent_id).is_err()
    {
        return Err(ExtensionError::ResolutionFailed {
            extension_name: extension_name.to_owned(),
            reason: format!(
                "preset '{}' preferred_agent_id must be a canonical lowercase bare UUIDv7, got '{preferred_agent_id}'",
                preset.source_key
            ),
        });
    }

    let system_prompt = preset
        .system_prompt
        .as_deref()
        .map(|v| resolve_file_reference(v, ext_dir))
        .transpose()?;

    let context = preset
        .context
        .as_deref()
        .map(|v| resolve_file_reference(v, ext_dir))
        .transpose()?;

    let icon = preset
        .icon
        .as_deref()
        .and_then(|value| resolve_extension_asset_url(extension_name, value));

    Ok(ResolvedPreset {
        extension_name: extension_name.to_owned(),
        source_key,
        name: preset.name.clone(),
        description: preset.description.clone(),
        system_prompt,
        icon,
        context,
        preferred_agent_id: preset.preferred_agent_id.clone(),
        enabled_skills: preset.enabled_skills.clone(),
        prompts: preset.prompts.clone(),
        models: preset.models.clone(),
    })
}

/// Resolve all preset contributions from an extension.
pub fn resolve_presets(presets: &[ExtPreset], extension_name: &str, ext_dir: &Path) -> Vec<ResolvedPreset> {
    presets
        .iter()
        .filter_map(|a| {
            resolve_preset(a, extension_name, ext_dir)
                .map_err(|e| {
                    warn!(
                        extension = extension_name,
                        preset_source_key = a.source_key,
                        "Failed to resolve preset: {e}"
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

    const AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    #[test]
    fn test_resolve_preset_plain_text() {
        let preset = ExtPreset {
            source_key: "asst-1".into(),
            name: "Helper".into(),
            description: Some("A helpful preset".into()),
            system_prompt: Some("You are helpful.".into()),
            icon: None,
            context: None,
            preferred_agent_id: Some(AGENT_ID.into()),
            enabled_skills: vec![],
            prompts: vec![],
            models: vec![],
        };

        let result = resolve_preset(&preset, "my-ext", Path::new("/ext/my-ext")).unwrap();

        assert_eq!(result.extension_name, "my-ext");
        assert_eq!(result.source_key, "my-ext:asst-1");
        assert_eq!(result.system_prompt.as_deref(), Some("You are helpful."));
        assert_eq!(result.preferred_agent_id.as_deref(), Some(AGENT_ID));
    }

    #[test]
    fn test_resolve_preset_rejects_non_uuid_v7_preferred_agent_id() {
        for invalid_id in [
            "claude",
            "helper-ext:agent",
            "agent_0190f5fe-7c00-7a00-8000-000000000001",
            "550e8400-e29b-41d4-a716-446655440000",
            "0190F5FE-7C00-7A00-8000-000000000001",
        ] {
            let preset = ExtPreset {
                source_key: "invalid-agent-preference".into(),
                name: "Invalid Agent Preference".into(),
                description: None,
                system_prompt: None,
                icon: None,
                context: None,
                preferred_agent_id: Some(invalid_id.into()),
                enabled_skills: vec![],
                prompts: vec![],
                models: vec![],
            };

            let err = resolve_preset(&preset, "my-ext", Path::new("/ext/my-ext")).unwrap_err();
            assert!(
                matches!(err, ExtensionError::ResolutionFailed { .. }),
                "expected ResolutionFailed for {invalid_id}, got {err:?}"
            );
        }
    }

    #[test]
    fn test_resolve_preset_file_reference() {
        let dir = std::env::temp_dir().join("ext_test_resolve_preset");
        let prompts = dir.join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("system.md"), "Loaded from file").unwrap();

        let preset = ExtPreset {
            source_key: "asst-2".into(),
            name: "File Ref".into(),
            description: None,
            system_prompt: Some("@file:prompts/system.md".into()),
            icon: None,
            context: None,
            preferred_agent_id: None,
            enabled_skills: vec![],
            prompts: vec![],
            models: vec![],
        };

        let result = resolve_preset(&preset, "my-ext", &dir).unwrap();
        assert_eq!(result.system_prompt.as_deref(), Some("Loaded from file"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_resolve_preset_file_not_found_error() {
        let preset = ExtPreset {
            source_key: "asst-3".into(),
            name: "Bad Ref".into(),
            description: None,
            system_prompt: Some("@file:missing.md".into()),
            icon: None,
            context: None,
            preferred_agent_id: None,
            enabled_skills: vec![],
            prompts: vec![],
            models: vec![],
        };

        let err = resolve_preset(&preset, "my-ext", Path::new("/tmp/no_such_ext_dir")).unwrap_err();
        assert!(matches!(err, ExtensionError::FileReferenceNotFound(_)));
    }

    #[test]
    fn test_resolve_preset_context_file_reference() {
        let dir = std::env::temp_dir().join("ext_test_resolve_preset_ctx");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("context.md"), "Context content").unwrap();

        let preset = ExtPreset {
            source_key: "asst-4".into(),
            name: "Ctx Ref".into(),
            description: None,
            system_prompt: None,
            icon: None,
            context: Some("@file:context.md".into()),
            preferred_agent_id: None,
            enabled_skills: vec![],
            prompts: vec![],
            models: vec![],
        };

        let result = resolve_preset(&preset, "my-ext", &dir).unwrap();
        assert_eq!(result.context.as_deref(), Some("Context content"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_resolve_presets_skips_bad_refs() {
        let presets = vec![
            ExtPreset {
                source_key: "good".into(),
                name: "Good".into(),
                description: None,
                system_prompt: Some("plain text".into()),
                icon: None,
                context: None,
                preferred_agent_id: None,
                enabled_skills: vec![],
                prompts: vec![],
                models: vec![],
            },
            ExtPreset {
                source_key: "bad".into(),
                name: "Bad".into(),
                description: None,
                system_prompt: Some("@file:missing.md".into()),
                icon: None,
                context: None,
                preferred_agent_id: None,
                enabled_skills: vec![],
                prompts: vec![],
                models: vec![],
            },
        ];

        let result = resolve_presets(&presets, "my-ext", Path::new("/tmp/no_such_ext"));
        // Only the good one should be resolved
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source_key, "my-ext:good");
    }
}
