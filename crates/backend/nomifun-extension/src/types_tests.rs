use super::*;
use serde_json::json;

// -- Permissions & Risk --

#[test]
fn test_risk_level_serde() {
    assert_eq!(serde_json::to_string(&RiskLevel::Safe).unwrap(), r#""safe""#);
    assert_eq!(serde_json::to_string(&RiskLevel::Moderate).unwrap(), r#""moderate""#);
    assert_eq!(serde_json::to_string(&RiskLevel::Dangerous).unwrap(), r#""dangerous""#);
}

#[test]
fn test_network_permission_unrestricted() {
    let perm = NetworkPermission::Unrestricted(true);
    let json = serde_json::to_value(&perm).unwrap();
    assert_eq!(json, json!(true));
}

#[test]
fn test_network_permission_scoped() {
    let perm = NetworkPermission::Scoped {
        allowed_domains: vec!["api.example.com".into()],
        reasoning: "needed for API calls".into(),
    };
    let json = serde_json::to_value(&perm).unwrap();
    assert_eq!(json["allowed_domains"], json!(["api.example.com"]));
    assert_eq!(json["reasoning"], "needed for API calls");
}

#[test]
fn test_network_permission_scoped_deserialize() {
    let raw = json!({"allowed_domains": ["a.com"], "reasoning": "test"});
    let perm: NetworkPermission = serde_json::from_value(raw).unwrap();
    assert!(matches!(perm, NetworkPermission::Scoped { .. }));
}

#[test]
fn test_network_permission_rejects_legacy_allowed_domains_alias() {
    for raw in [
        json!({"allowedDomains": ["a.com"], "reasoning": "test"}),
        json!({
            "allowed_domains": ["a.com"],
            "allowedDomains": ["legacy.example.com"],
            "reasoning": "test"
        }),
    ] {
        assert!(serde_json::from_value::<NetworkPermission>(raw).is_err());
    }
}

#[test]
fn test_filesystem_scope_serde() {
    assert_eq!(
        serde_json::to_string(&FilesystemScope::ExtensionOnly).unwrap(),
        r#""extension-only""#
    );
    assert_eq!(
        serde_json::to_string(&FilesystemScope::Workspace).unwrap(),
        r#""workspace""#
    );
    assert_eq!(serde_json::to_string(&FilesystemScope::Full).unwrap(), r#""full""#);
}

#[test]
fn test_ext_permissions_empty() {
    let perms = ExtPermissions::default();
    let json = serde_json::to_value(&perms).unwrap();
    assert_eq!(json, json!({}));
}

#[test]
fn test_ext_permissions_roundtrip() {
    let perms = ExtPermissions {
        storage: Some(true),
        network: Some(NetworkPermission::Unrestricted(true)),
        shell: Some(true),
        filesystem: Some(FilesystemScope::Full),
        clipboard: None,
        active_user: None,
        events: Some(true),
    };
    let json_str = serde_json::to_string(&perms).unwrap();
    let parsed: ExtPermissions = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed, perms);
}

#[test]
fn test_permission_level_serde() {
    let cases = [
        (PermissionLevel::None, r#""none""#),
        (PermissionLevel::Limited, r#""limited""#),
        (PermissionLevel::Full, r#""full""#),
    ];
    for (variant, expected) in cases {
        assert_eq!(serde_json::to_string(&variant).unwrap(), expected);
    }
}

// -- Contributions --

#[test]
fn test_ext_contributes_empty() {
    let c = ExtContributes::default();
    let json = serde_json::to_value(&c).unwrap();
    assert_eq!(json, json!({}));
}

#[test]
fn preset_and_agent_catalog_entries_require_source_key_and_reject_generic_id() {
    assert!(
        serde_json::from_value::<ExtPreset>(json!({
            "id": "legacy-preset",
            "name": "Legacy Preset"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ExtAgent>(json!({
            "id": "legacy-agent",
            "name": "Legacy Agent"
        }))
        .is_err()
    );

    let preset: ExtPreset = serde_json::from_value(json!({
        "source_key": "review",
        "name": "Review"
    }))
    .unwrap();
    let agent: ExtAgent = serde_json::from_value(json!({
        "source_key": "coder",
        "name": "Coder"
    }))
    .unwrap();
    assert_eq!(preset.source_key, "review");
    assert_eq!(agent.source_key, "coder");
}

#[test]
fn test_ext_contributes_with_skills() {
    let c = ExtContributes {
        skills: vec![ExtSkill {
            name: "my-skill".into(),
            description: Some("A test skill".into()),
            path: Some("skills/my-skill".into()),
        }],
        ..Default::default()
    };
    let json = serde_json::to_value(&c).unwrap();
    assert_eq!(json["skills"][0]["name"], "my-skill");
}

#[test]
fn test_ext_acp_adapter_minimal() {
    let adapter = ExtAcpAdapter {
        id: "claude-adapter".into(),
        name: "Claude".into(),
        description: None,
        cli_command: Some("claude".into()),
        default_cli_path: None,
        acp_args: vec![],
        env: HashMap::new(),
        avatar: None,
        auth_required: None,
        supports_streaming: Some(true),
        connection_type: None,
        endpoint: None,
        models: vec![],
        yolo_mode: None,
        health_check: None,
        api_key_fields: vec![],
    };
    let json = serde_json::to_value(&adapter).unwrap();
    assert_eq!(json["id"], "claude-adapter");
    assert_eq!(json["cli_command"], "claude");
    assert_eq!(json["supports_streaming"], true);
    // Empty vecs should be omitted
    assert!(json.get("acp_args").is_none());
}

#[test]
fn test_ext_theme_serde() {
    let theme = ExtTheme {
        id: "dark".into(),
        name: "Dark Mode".into(),
        description: Some("A dark theme".into()),
        css_file: "themes/dark.css".into(),
        cover_image: Some("images/dark-preview.png".into()),
    };
    let json = serde_json::to_value(&theme).unwrap();
    assert_eq!(json["css_file"], "themes/dark.css");
    assert_eq!(json["cover_image"], "images/dark-preview.png");
}

#[test]
fn test_ext_webui_with_routes() {
    let webui = ExtWebui {
        id: "my-panel".into(),
        directory: "webui/dist".into(),
        routes: vec![ExtWebuiRoute {
            path: "/my-ext/api/data".into(),
            method: "GET".into(),
            handler: "handlers/data.js".into(),
        }],
    };
    let json = serde_json::to_value(&webui).unwrap();
    assert_eq!(json["routes"][0]["path"], "/my-ext/api/data");
    assert_eq!(json["routes"][0]["method"], "GET");
}

#[test]
fn test_ext_settings_tab_with_position() {
    let tab = ExtSettingsTab {
        id: "ext-settings".into(),
        label: "Extension Settings".into(),
        icon: None,
        url: "settings/index.html".into(),
        position: Some(SettingsTabPosition {
            relative_to: "general".into(),
            placement: "after".into(),
        }),
        order: 80,
    };
    let json = serde_json::to_value(&tab).unwrap();
    assert_eq!(json["position"]["relative_to"], "general");
    assert_eq!(json["position"]["placement"], "after");
    assert_eq!(json["order"], 80);
}

#[test]
fn test_ext_settings_tab_rejects_legacy_field_aliases() {
    let raw = json!({
        "id": "legacy-settings",
        "name": "Legacy Settings",
        "entryPoint": "settings/legacy.html",
        "position": {
            "anchor": "general",
            "placement": "after"
        }
    });

    assert!(serde_json::from_value::<ExtSettingsTab>(raw).is_err());
}

// -- ExtMcpServer flatten roundtrip (M-50) --

#[test]
fn test_ext_mcp_server_roundtrip_with_extra_config() {
    let raw = json!({
        "source_key": "my-mcp",
        "name": "My MCP Server",
        "description": "A test MCP server",
        "command": "npx",
        "args": ["-y", "@modelcontextprotocol/server-filesystem"],
        "transport": "stdio"
    });
    let server: ExtMcpServer = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(server.source_key, "my-mcp");
    assert_eq!(server.name, "My MCP Server");
    assert_eq!(server.description.as_deref(), Some("A test MCP server"));

    // Flattened config should contain the extra fields
    let re_serialized = serde_json::to_value(&server).unwrap();
    assert_eq!(re_serialized["command"], "npx");
    assert_eq!(re_serialized["transport"], "stdio");
    assert_eq!(re_serialized["source_key"], "my-mcp");
    assert_eq!(re_serialized["name"], "My MCP Server");
}

#[test]
fn test_ext_mcp_server_minimal() {
    let raw = json!({"source_key": "s1", "name": "S1"});
    let server: ExtMcpServer = serde_json::from_value(raw).unwrap();
    assert_eq!(server.source_key, "s1");
    let re_serialized = serde_json::to_value(&server).unwrap();
    assert_eq!(re_serialized["source_key"], "s1");
    assert_eq!(re_serialized["name"], "S1");
}

#[test]
fn test_ext_mcp_server_rejects_legacy_identity_fields() {
    for legacy_field in ["id", "sourceKey", "contributionKey", "contribution_key"] {
        let mut legacy_only = serde_json::Map::from_iter([
            ("name".to_owned(), json!("Legacy MCP")),
            (legacy_field.to_owned(), json!("legacy-mcp")),
        ]);
        assert!(
            serde_json::from_value::<ExtMcpServer>(serde_json::Value::Object(legacy_only.clone())).is_err(),
            "legacy identity field unexpectedly accepted: {legacy_field}"
        );

        legacy_only.insert("source_key".to_owned(), json!("canonical-mcp"));
        assert!(
            serde_json::from_value::<ExtMcpServer>(serde_json::Value::Object(legacy_only)).is_err(),
            "mixed canonical and legacy identity fields unexpectedly accepted: {legacy_field}"
        );
    }
}

// -- Manifest --

#[test]
fn test_manifest_minimal_deserialize() {
    let raw = json!({
        "name": "my-ext",
        "version": "1.0.0"
    });
    let manifest: ExtensionManifest = serde_json::from_value(raw).unwrap();
    assert_eq!(manifest.name, "my-ext");
    assert_eq!(manifest.version, "1.0.0");
    assert!(manifest.contributes.is_none());
    assert!(manifest.permissions.is_none());
    assert!(manifest.dependencies.is_empty());
}

#[test]
fn test_manifest_full_roundtrip() {
    let manifest = ExtensionManifest {
        name: "test-ext".into(),
        version: "2.1.0".into(),
        display_name: Some("Test Extension".into()),
        description: Some("A test extension".into()),
        author: Some("Test Author".into()),
        license: Some("MIT".into()),
        homepage: Some("https://example.com".into()),
        icon: Some("icon.png".into()),
        engine: Some(EngineConfig {
            nomifun: Some("^1.0.0".into()),
        }),
        api_version: Some("1.0.0".into()),
        dependencies: HashMap::from([("dep-ext".into(), "^1.0.0".into())]),
        entry_point: Some("main.js".into()),
        permissions: Some(ExtPermissions {
            storage: Some(true),
            events: Some(true),
            ..Default::default()
        }),
        contributes: Some(ExtContributes::default()),
        lifecycle: Some(LifecycleHooks {
            on_install: Some("scripts/install.sh".into()),
            on_activate: Some("scripts/activate.sh".into()),
            on_deactivate: None,
            on_uninstall: None,
        }),
        i18n: Some(I18nConfig {
            locales: vec!["en".into(), "zh-CN".into()],
            directory: "i18n".into(),
        }),
    };
    let json_str = serde_json::to_string(&manifest).unwrap();
    let parsed: ExtensionManifest = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed, manifest);
}

#[test]
fn test_manifest_snake_case_keys() {
    let manifest = ExtensionManifest {
        name: "x".into(),
        version: "1.0.0".into(),
        display_name: Some("X".into()),
        api_version: Some("1.0.0".into()),
        entry_point: Some("main.js".into()),
        description: None,
        author: None,
        license: None,
        homepage: None,
        icon: None,
        engine: None,
        dependencies: HashMap::new(),
        permissions: None,
        contributes: None,
        lifecycle: None,
        i18n: None,
    };
    let json = serde_json::to_value(&manifest).unwrap();
    assert!(json.get("display_name").is_some());
    assert!(json.get("api_version").is_some());
    assert!(json.get("entry_point").is_some());
    // camelCase keys should not exist
    assert!(json.get("displayName").is_none());
    assert!(json.get("apiVersion").is_none());
}

// -- Extension state & source --

#[test]
fn test_extension_source_serde() {
    let cases = [
        (ExtensionSource::Local, r#""local""#),
        (ExtensionSource::Appdata, r#""appdata""#),
        (ExtensionSource::Env, r#""env""#),
    ];
    for (variant, expected) in cases {
        let json = serde_json::to_string(&variant).unwrap();
        assert_eq!(json, expected);
        let parsed: ExtensionSource = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, variant);
    }
}

#[test]
fn test_extension_state_roundtrip() {
    let state = ExtensionState {
        name: "my-ext".into(),
        version: "1.0.0".into(),
        enabled: true,
        installed_at: Some(1700000000000),
        last_activated_at: Some(1700001000000),
    };
    let json_str = serde_json::to_string(&state).unwrap();
    let parsed: ExtensionState = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed, state);
}

#[test]
fn test_extension_state_optional_timestamps() {
    let raw = json!({
        "name": "x",
        "version": "1.0.0",
        "enabled": false
    });
    let state: ExtensionState = serde_json::from_value(raw).unwrap();
    assert!(!state.enabled);
    assert!(state.installed_at.is_none());
    assert!(state.last_activated_at.is_none());
}

// -- Events --

#[test]
fn test_extension_system_event_serde() {
    let cases = [
        (ExtensionSystemEvent::ExtensionActivated, r#""EXTENSION_ACTIVATED""#),
        (ExtensionSystemEvent::ExtensionDeactivated, r#""EXTENSION_DEACTIVATED""#),
        (ExtensionSystemEvent::ExtensionInstalled, r#""EXTENSION_INSTALLED""#),
        (ExtensionSystemEvent::ExtensionUninstalled, r#""EXTENSION_UNINSTALLED""#),
        (ExtensionSystemEvent::RegistryReloaded, r#""REGISTRY_RELOADED""#),
        (ExtensionSystemEvent::StatesPersisted, r#""STATES_PERSISTED""#),
    ];
    for (variant, expected) in cases {
        let json = serde_json::to_string(&variant).unwrap();
        assert_eq!(json, expected);
        let parsed: ExtensionSystemEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, variant);
    }
}

#[test]
fn test_lifecycle_payload_roundtrip() {
    let payload = ExtensionLifecyclePayload {
        extension_name: "my-ext".into(),
        event: ExtensionSystemEvent::ExtensionActivated,
        timestamp: 1700000000000,
        data: Some(json!({"reason": "user action"})),
    };
    let json_str = serde_json::to_string(&payload).unwrap();
    let parsed: ExtensionLifecyclePayload = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed, payload);
}

#[test]
fn test_lifecycle_payload_without_data() {
    let payload = ExtensionLifecyclePayload {
        extension_name: "test".into(),
        event: ExtensionSystemEvent::RegistryReloaded,
        timestamp: 1700000000000,
        data: None,
    };
    let json = serde_json::to_value(&payload).unwrap();
    assert!(json.get("data").is_none());
}

#[test]
fn test_resolved_settings_tab_serializes_backend_contract_keys() {
    let tab = ResolvedSettingsTab {
        extension_name: "hello".into(),
        id: "ext-hello-settings".into(),
        label: "Hello Settings".into(),
        icon: Some("/api/extensions/hello/assets/icons/gear.svg".into()),
        url: "/api/extensions/hello/assets/settings/index.html".into(),
        position: Some(SettingsTabPosition {
            relative_to: "general".into(),
            placement: "after".into(),
        }),
        order: 80,
    };

    let json = serde_json::to_value(&tab).unwrap();
    assert_eq!(json["extension_name"], "hello");
    assert_eq!(json["position"]["relative_to"], "general");
    assert_eq!(json["order"], 80);
}

// -- Hub --

#[test]
fn test_hub_extension_status_serde() {
    let cases = [
        (HubExtensionStatus::NotInstalled, r#""not_installed""#),
        (HubExtensionStatus::Installed, r#""installed""#),
        (HubExtensionStatus::UpdateAvailable, r#""update_available""#),
        (HubExtensionStatus::Installing, r#""installing""#),
        (HubExtensionStatus::InstallFailed, r#""install_failed""#),
    ];
    for (variant, expected) in cases {
        let json = serde_json::to_string(&variant).unwrap();
        assert_eq!(json, expected);
        let parsed: HubExtensionStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, variant);
    }
}

#[test]
fn test_hub_extension_with_status_roundtrip() {
    let ext = HubExtensionWithStatus {
        name: "cool-ext".into(),
        version: "1.2.3".into(),
        display_name: Some("Cool Extension".into()),
        description: Some("Does cool things".into()),
        author: Some("Author".into()),
        icon: None,
        tags: vec!["productivity".into()],
        bundled: false,
        status: HubExtensionStatus::Installed,
    };
    let json_str = serde_json::to_string(&ext).unwrap();
    let parsed: HubExtensionWithStatus = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed, ext);
}

#[test]
fn test_hub_extension_bundled_status() {
    let ext = HubExtensionWithStatus {
        name: "builtin-ext".into(),
        version: "1.0.0".into(),
        display_name: None,
        description: None,
        author: None,
        icon: None,
        tags: vec![],
        bundled: true,
        status: HubExtensionStatus::Installed,
    };
    let json = serde_json::to_value(&ext).unwrap();
    assert_eq!(json["bundled"], true);
    assert_eq!(json["status"], "installed");
}

// -- Loaded extension --

#[test]
fn test_loaded_extension_roundtrip() {
    let loaded = LoadedExtension {
        manifest: ExtensionManifest {
            name: "test".into(),
            version: "1.0.0".into(),
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
            contributes: None,
            lifecycle: None,
            i18n: None,
        },
        directory: "/path/to/ext".into(),
        source: ExtensionSource::Env,
        state: ExtensionState {
            name: "test".into(),
            version: "1.0.0".into(),
            enabled: true,
            installed_at: None,
            last_activated_at: None,
        },
    };
    let json_str = serde_json::to_string(&loaded).unwrap();
    let parsed: LoadedExtension = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed, loaded);
}

// -- I18n config --

#[test]
fn test_i18n_config_default_directory() {
    let raw = json!({"locales": ["en"]});
    let config: I18nConfig = serde_json::from_value(raw).unwrap();
    assert_eq!(config.directory, "i18n");
}

#[test]
fn test_i18n_config_custom_directory() {
    let raw = json!({"locales": ["en", "zh-CN"], "directory": "lang"});
    let config: I18nConfig = serde_json::from_value(raw).unwrap();
    assert_eq!(config.directory, "lang");
}

// -- Lifecycle hooks --

#[test]
fn test_lifecycle_hooks_empty() {
    let hooks = LifecycleHooks::default();
    let json = serde_json::to_value(&hooks).unwrap();
    assert_eq!(json, json!({}));
}

#[test]
fn test_lifecycle_hooks_partial() {
    let raw = json!({"on_install": "scripts/install.sh"});
    let hooks: LifecycleHooks = serde_json::from_value(raw).unwrap();
    assert_eq!(hooks.on_install.as_deref(), Some("scripts/install.sh"));
    assert!(hooks.on_activate.is_none());
}
