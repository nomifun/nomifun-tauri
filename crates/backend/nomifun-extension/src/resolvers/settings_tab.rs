use std::path::Path;

use tracing::warn;

use crate::asset_paths::{is_remote_asset_url, normalized_asset_url_path};
use crate::types::{ExtSettingsTab, ResolvedSettingsTab};

fn resolve_asset_url(extension_name: &str, raw: &str) -> Option<String> {
    if is_remote_asset_url(raw) {
        return Some(raw.to_owned());
    }

    let relative = normalized_asset_url_path(raw)?;
    Some(format!("/api/extensions/{extension_name}/assets/{relative}"))
}

/// Resolve a single settings tab contribution.
///
/// Position information (`relative_to`, `placement`) is preserved for the
/// frontend to handle insertion ordering.
pub fn resolve_settings_tab(
    tab: &ExtSettingsTab,
    extension_name: &str,
    _ext_dir: &Path,
) -> Option<ResolvedSettingsTab> {
    let url = resolve_asset_url(extension_name, &tab.url).or_else(|| {
        warn!(
            extension = extension_name,
            tab_id = tab.id,
            url = tab.url,
            "Skipping settings tab with invalid asset path"
        );
        None
    })?;

    let icon = tab.icon.as_ref().and_then(|icon| {
        resolve_asset_url(extension_name, icon).or_else(|| {
            warn!(
                extension = extension_name,
                tab_id = tab.id,
                icon,
                "Dropping settings tab icon with invalid asset path"
            );
            None
        })
    });

    Some(ResolvedSettingsTab {
        extension_name: extension_name.to_owned(),
        id: format!("ext-{extension_name}-{}", tab.id),
        label: tab.label.clone(),
        icon,
        url,
        position: tab.position.clone(),
        order: tab.order,
    })
}

/// Resolve all settings tab contributions from an extension.
pub fn resolve_settings_tabs(
    tabs: &[ExtSettingsTab],
    extension_name: &str,
    ext_dir: &Path,
) -> Vec<ResolvedSettingsTab> {
    let mut resolved: Vec<_> = tabs
        .iter()
        .filter_map(|tab| resolve_settings_tab(tab, extension_name, ext_dir))
        .collect();

    resolved.sort_by(|left, right| left.order.cmp(&right.order).then_with(|| left.label.cmp(&right.label)));

    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SettingsTabPosition;

    #[test]
    fn test_resolve_settings_tab_with_local_assets_and_position() {
        let tab = ExtSettingsTab {
            id: "my-settings".into(),
            label: "My Settings".into(),
            icon: Some("icons/gear.svg".into()),
            url: "settings/index.html".into(),
            position: Some(SettingsTabPosition {
                relative_to: "general".into(),
                placement: "after".into(),
            }),
            order: 80,
        };

        let result = resolve_settings_tab(&tab, "my-ext", Path::new("/tmp/my-ext")).unwrap();

        assert_eq!(result.extension_name, "my-ext");
        assert_eq!(result.id, "ext-my-ext-my-settings");
        assert_eq!(result.url, "/api/extensions/my-ext/assets/settings/index.html");
        assert_eq!(
            result.icon.as_deref(),
            Some("/api/extensions/my-ext/assets/icons/gear.svg")
        );
        assert_eq!(result.order, 80);
        let pos = result.position.unwrap();
        assert_eq!(pos.relative_to, "general");
        assert_eq!(pos.placement, "after");
    }

    #[test]
    fn test_resolve_settings_tab_keeps_remote_urls() {
        let tab = ExtSettingsTab {
            id: "plain-tab".into(),
            label: "Plain".into(),
            icon: Some("https://example.com/icon.svg".into()),
            url: "https://example.com/settings".into(),
            position: None,
            order: 100,
        };

        let result = resolve_settings_tab(&tab, "my-ext", Path::new("/tmp/my-ext")).unwrap();
        assert!(result.position.is_none());
        assert_eq!(result.url, "https://example.com/settings");
        assert_eq!(result.icon.as_deref(), Some("https://example.com/icon.svg"));
    }

    #[test]
    fn test_resolve_settings_tab_rejects_traversal_url() {
        let tab = ExtSettingsTab {
            id: "bad".into(),
            label: "Bad".into(),
            icon: None,
            url: "../settings.html".into(),
            position: None,
            order: 100,
        };

        assert!(resolve_settings_tab(&tab, "my-ext", Path::new("/tmp/my-ext")).is_none());
    }

    #[test]
    fn test_resolve_settings_tabs_sorts_by_order_then_label() {
        let tabs = vec![
            ExtSettingsTab {
                id: "z".into(),
                label: "Zulu".into(),
                icon: None,
                url: "z.html".into(),
                position: None,
                order: 100,
            },
            ExtSettingsTab {
                id: "a".into(),
                label: "Alpha".into(),
                icon: None,
                url: "a.html".into(),
                position: Some(SettingsTabPosition {
                    relative_to: "general".into(),
                    placement: "before".into(),
                }),
                order: 50,
            },
            ExtSettingsTab {
                id: "b".into(),
                label: "Beta".into(),
                icon: None,
                url: "b.html".into(),
                position: None,
                order: 100,
            },
        ];

        let result = resolve_settings_tabs(&tabs, "my-ext", Path::new("/tmp/my-ext"));
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].id, "ext-my-ext-a");
        assert_eq!(result[1].id, "ext-my-ext-b");
        assert_eq!(result[2].id, "ext-my-ext-z");
    }
}
