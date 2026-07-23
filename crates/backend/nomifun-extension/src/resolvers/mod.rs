//! Contribution resolvers — transform raw manifest declarations into
//! runtime-ready structures.
//!
//! Each sub-module handles one contribution type. The top-level
//! [`resolve_all_contributions`] coordinates resolution across all
//! enabled extensions.

pub mod acp_adapter;
pub mod agent;
pub mod preset;
pub mod channel_plugin;
pub mod i18n;
pub mod mcp_server;
pub mod model_provider;
pub mod settings_tab;
pub mod skill;
pub mod theme;
pub mod webui;

use std::path::Path;

use crate::types::{LoadedExtension, ResolvedContributions};

/// Build one unambiguous global identity for an extension catalog item.
///
/// Product entity IDs remain bare UUIDv7 values. Extension manifest identities
/// are natural keys and live exclusively in `source_key`.
pub(super) fn extension_source_key(
    extension_name: &str,
    local_key: &str,
) -> Result<String, crate::ExtensionError> {
    let valid_part = |value: &str| {
        !value.is_empty()
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'_' | b'-' | b'.')
            })
    };
    let prefixed_uuidv7 = local_key
        .rsplit_once('_')
        .is_some_and(|(_, suffix)| nomifun_common::validate_uuidv7(suffix).is_ok());
    let bare_uuidv7 = nomifun_common::validate_uuidv7(local_key).is_ok();
    let source_key = format!("{extension_name}:{local_key}");
    if !valid_part(extension_name)
        || !valid_part(local_key)
        || source_key.len() > 255
        || bare_uuidv7
        || prefixed_uuidv7
    {
        return Err(crate::ExtensionError::ResolutionFailed {
            extension_name: extension_name.to_owned(),
            reason: format!(
                "catalog source_key must use '<extension-name>:<local-key>' with lowercase ASCII natural-key parts and must not use UUIDv7-derived product identity; got '{source_key}'"
            ),
        });
    }
    Ok(source_key)
}

/// Resolve all contributions from a single extension.
///
/// Failures in individual contribution types are logged and skipped —
/// one broken theme does not block ACP adapter resolution.
pub fn resolve_extension_contributions(ext: &LoadedExtension) -> ResolvedContributions {
    let ext_name = &ext.manifest.name;
    let ext_dir = Path::new(&ext.directory);

    let contributes = match &ext.manifest.contributes {
        Some(c) => c,
        None => return ResolvedContributions::default(),
    };

    ResolvedContributions {
        acp_adapters: acp_adapter::resolve_acp_adapters(&contributes.acp_adapters, ext_name, ext_dir),
        mcp_servers: mcp_server::resolve_mcp_servers(&contributes.mcp_servers, ext_name),
        presets: preset::resolve_presets(&contributes.presets, ext_name, ext_dir),
        agents: agent::resolve_agents(&contributes.agents, ext_name, ext_dir),
        skills: skill::resolve_skills(&contributes.skills, ext_name, ext_dir),
        themes: theme::resolve_themes(&contributes.themes, ext_name, ext_dir),
        channel_plugins: channel_plugin::resolve_channel_plugins(&contributes.channel_plugins, ext_name, ext_dir),
        webui: webui::resolve_webui_contributions(&contributes.webui, ext_name, ext_dir),
        settings_tabs: settings_tab::resolve_settings_tabs(&contributes.settings_tabs, ext_name, ext_dir),
        model_providers: model_provider::resolve_model_providers(&contributes.model_providers, ext_name),
        // i18n is resolved separately via resolve_i18n_for_locale()
        // because it requires a locale parameter at query time.
        i18n: std::collections::HashMap::new(),
    }
}

/// Resolve contributions from all enabled extensions.
///
/// Extensions that are disabled (`state.enabled == false`) are skipped.
pub fn resolve_all_contributions(extensions: &[LoadedExtension]) -> ResolvedContributions {
    let mut merged = ResolvedContributions::default();

    for ext in extensions {
        if !ext.state.enabled {
            tracing::debug!(extension = ext.manifest.name, "Skipping disabled extension");
            continue;
        }

        let resolved = resolve_extension_contributions(ext);
        merge_contributions(&mut merged, resolved, &ext.manifest.name);
    }

    merged
        .settings_tabs
        .sort_by(|left, right| left.order.cmp(&right.order).then_with(|| left.label.cmp(&right.label)));

    merged
}

/// Merge `source` contributions into `target`.
fn merge_contributions(target: &mut ResolvedContributions, source: ResolvedContributions, extension_name: &str) {
    if !source.acp_adapters.is_empty() {
        tracing::debug!(
            extension = extension_name,
            count = source.acp_adapters.len(),
            "Merged ACP adapters"
        );
    }

    target.acp_adapters.extend(source.acp_adapters);
    target.mcp_servers.extend(source.mcp_servers);
    target.presets.extend(source.presets);
    target.agents.extend(source.agents);
    target.skills.extend(source.skills);
    target.themes.extend(source.themes);
    target.channel_plugins.extend(source.channel_plugins);
    target.webui.extend(source.webui);
    target.settings_tabs.extend(source.settings_tabs);
    target.model_providers.extend(source.model_providers);
    target.i18n.extend(source.i18n);
}

/// Convenience: resolve i18n data for a given locale across all enabled extensions.
pub fn resolve_i18n_for_all(
    extensions: &[LoadedExtension],
    locale: &str,
) -> std::collections::HashMap<String, std::collections::HashMap<String, String>> {
    let ext_data: Vec<(String, Option<crate::types::I18nConfig>, String)> = extensions
        .iter()
        .filter(|ext| ext.state.enabled)
        .map(|ext| {
            (
                ext.manifest.name.clone(),
                ext.manifest.i18n.clone(),
                ext.directory.clone(),
            )
        })
        .collect();

    i18n::resolve_i18n_for_locale(&ext_data, locale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use std::collections::HashMap;

    #[test]
    fn extension_source_key_rejects_uuidv7_derived_product_ids() {
        for local_key in [
            "0190f5fe-7c00-7a00-8000-000000000001",
            "preset_0190f5fe-7c00-7a00-8000-000000000001",
        ] {
            assert!(extension_source_key("my-ext", local_key).is_err());
        }
    }

    fn make_extension(name: &str, enabled: bool, contributes: Option<ExtContributes>) -> LoadedExtension {
        LoadedExtension {
            manifest: ExtensionManifest {
                name: name.to_owned(),
                version: "1.0.0".to_owned(),
                display_name: None,
                description: None,
                author: None,
                license: None,
                homepage: None,
                icon: None,
                engine: None,
                api_version: None,
                dependencies: HashMap::new(),
                entry_point: None,
                permissions: None,
                contributes,
                lifecycle: None,
                i18n: None,
            },
            directory: "/tmp/ext".to_owned(),
            source: ExtensionSource::Local,
            state: ExtensionState {
                name: name.to_owned(),
                version: "1.0.0".to_owned(),
                enabled,
                installed_at: None,
                last_activated_at: None,
            },
        }
    }

    #[test]
    fn test_resolve_extension_no_contributes() {
        let ext = make_extension("empty-ext", true, None);
        let result = resolve_extension_contributions(&ext);
        assert!(result.acp_adapters.is_empty());
        assert!(result.mcp_servers.is_empty());
        assert!(result.presets.is_empty());
    }

    #[test]
    fn test_resolve_extension_with_model_providers() {
        let contributes = ExtContributes {
            model_providers: vec![ExtModelProvider {
                id: "mp-1".into(),
                name: "Test Provider".into(),
                description: None,
                protocol: None,
                base_url: None,
                models: vec![],
            }],
            ..Default::default()
        };

        let ext = make_extension("provider-ext", true, Some(contributes));
        let result = resolve_extension_contributions(&ext);
        assert_eq!(result.model_providers.len(), 1);
        assert_eq!(result.model_providers[0].extension_name, "provider-ext");
    }

    #[test]
    fn test_resolve_all_skips_disabled() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("skills")).unwrap();
        std::fs::write(dir.path().join("skills/my-skill.md"), "# skill").unwrap();

        let enabled = make_extension(
            "enabled-ext",
            true,
            Some(ExtContributes {
                skills: vec![ExtSkill {
                    name: "my-skill".into(),
                    description: None,
                    path: Some("skills/my-skill.md".into()),
                }],
                ..Default::default()
            }),
        );
        let enabled = LoadedExtension {
            directory: dir.path().to_string_lossy().into_owned(),
            ..enabled
        };
        let disabled = make_extension(
            "disabled-ext",
            false,
            Some(ExtContributes {
                skills: vec![ExtSkill {
                    name: "hidden-skill".into(),
                    description: None,
                    path: Some("skills/hidden-skill.md".into()),
                }],
                ..Default::default()
            }),
        );
        let disabled = LoadedExtension {
            directory: dir.path().to_string_lossy().into_owned(),
            ..disabled
        };

        let result = resolve_all_contributions(&[enabled, disabled]);
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "my-skill");
    }

    #[test]
    fn test_resolve_all_merges_multiple_extensions() {
        let ext_a = make_extension(
            "ext-a",
            true,
            Some(ExtContributes {
                mcp_servers: vec![ExtMcpServer {
                    source_key: "mcp-a".into(),
                    name: "MCP A".into(),
                    description: None,
                    config: serde_json::json!({}),
                }],
                ..Default::default()
            }),
        );
        let ext_b = make_extension(
            "ext-b",
            true,
            Some(ExtContributes {
                mcp_servers: vec![ExtMcpServer {
                    source_key: "mcp-b".into(),
                    name: "MCP B".into(),
                    description: None,
                    config: serde_json::json!({}),
                }],
                ..Default::default()
            }),
        );

        let result = resolve_all_contributions(&[ext_a, ext_b]);
        assert_eq!(result.mcp_servers.len(), 2);
    }

    #[test]
    fn test_resolve_all_empty_extensions() {
        let result = resolve_all_contributions(&[]);
        assert!(result.acp_adapters.is_empty());
        assert!(result.i18n.is_empty());
    }

    #[test]
    fn test_resolve_all_sorts_settings_tabs_globally() {
        let ext_a = make_extension(
            "ext-a",
            true,
            Some(ExtContributes {
                settings_tabs: vec![ExtSettingsTab {
                    id: "zeta".into(),
                    label: "Zeta".into(),
                    icon: None,
                    url: "settings/zeta.html".into(),
                    position: None,
                    order: 100,
                }],
                ..Default::default()
            }),
        );
        let ext_b = make_extension(
            "ext-b",
            true,
            Some(ExtContributes {
                settings_tabs: vec![
                    ExtSettingsTab {
                        id: "alpha".into(),
                        label: "Alpha".into(),
                        icon: None,
                        url: "settings/alpha.html".into(),
                        position: None,
                        order: 50,
                    },
                    ExtSettingsTab {
                        id: "beta".into(),
                        label: "Beta".into(),
                        icon: None,
                        url: "settings/beta.html".into(),
                        position: None,
                        order: 100,
                    },
                ],
                ..Default::default()
            }),
        );

        let result = resolve_all_contributions(&[ext_a, ext_b]);
        let ids: Vec<&str> = result.settings_tabs.iter().map(|tab| tab.id.as_str()).collect();
        assert_eq!(ids, vec!["ext-ext-b-alpha", "ext-ext-b-beta", "ext-ext-a-zeta"]);
    }
}
