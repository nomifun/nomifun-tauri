//! Integration tests for contribution resolution (test-plan CR-1..CR-10).
//!
//! These are black-box tests that exercise the public resolver APIs with
//! realistic extension manifests and file system fixtures.

use std::collections::HashMap;

use nomifun_extension::types::*;
use nomifun_extension::{resolve_all_contributions, resolve_extension_contributions, resolve_i18n_for_all};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_loaded_extension(name: &str, dir: &str, contributes: ExtContributes) -> LoadedExtension {
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
            contributes: Some(contributes),
            lifecycle: None,
            i18n: None,
        },
        directory: dir.to_owned(),
        source: ExtensionSource::Local,
        state: ExtensionState {
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
            enabled: true,
            installed_at: None,
            last_activated_at: None,
        },
    }
}

fn make_loaded_extension_with_i18n(name: &str, dir: &str, i18n: I18nConfig) -> LoadedExtension {
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
            contributes: None,
            lifecycle: None,
            i18n: Some(i18n),
        },
        directory: dir.to_owned(),
        source: ExtensionSource::Local,
        state: ExtensionState {
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
            enabled: true,
            installed_at: None,
            last_activated_at: None,
        },
    }
}

// ---------------------------------------------------------------------------
// CR-1: ACP Adapter resolution
// ---------------------------------------------------------------------------

#[test]
fn cr1_acp_adapter_resolved_with_env_and_avatar() {
    unsafe { std::env::set_var("_CR1_API_KEY", "test-key-123") };

    let mut env = HashMap::new();
    env.insert("API_KEY".into(), "${_CR1_API_KEY}".into());

    let contributes = ExtContributes {
        acp_adapters: vec![ExtAcpAdapter {
            id: "claude-adapter".into(),
            name: "Claude Adapter".into(),
            description: Some("Claude via ACP".into()),
            cli_command: Some("claude".into()),
            default_cli_path: None,
            acp_args: vec!["--dangerously-skip-permissions".into()],
            env,
            avatar: Some("icons/claude.png".into()),
            auth_required: Some(true),
            supports_streaming: Some(true),
            connection_type: Some("stdio".into()),
            endpoint: None,
            models: vec!["claude-sonnet-4-20250514".into()],
            yolo_mode: Some(serde_json::json!({
                "type": "session"
            })),
            health_check: None,
            api_key_fields: vec![],
        }],
        ..Default::default()
    };

    let ext = make_loaded_extension("claude-ext", "/ext/claude-ext", contributes);
    let result = resolve_extension_contributions(&ext);

    assert_eq!(result.acp_adapters.len(), 1);
    let adapter = &result.acp_adapters[0];
    assert_eq!(adapter.extension_name, "claude-ext");
    assert_eq!(adapter.id, "claude-adapter");
    assert_eq!(adapter.cli_command.as_deref(), Some("claude"));
    assert_eq!(adapter.env["API_KEY"], "test-key-123");
    assert!(adapter.avatar.as_ref().unwrap().contains("icons/claude.png"));

    unsafe { std::env::remove_var("_CR1_API_KEY") };
}

// ---------------------------------------------------------------------------
// CR-2: MCP Server resolution
// ---------------------------------------------------------------------------

#[test]
fn cr2_mcp_server_resolved_as_opaque_config() {
    let contributes = ExtContributes {
        mcp_servers: vec![ExtMcpServer {
            source_key: "sqlite-mcp".into(),
            name: "SQLite MCP".into(),
            description: Some("SQLite via MCP".into()),
            config: serde_json::json!({
                "command": "npx",
                "args": ["-y", "@anthropic/mcp-server-sqlite"],
                "transport": "stdio"
            }),
        }],
        ..Default::default()
    };

    let ext = make_loaded_extension("sqlite-ext", "/ext/sqlite-ext", contributes);
    let result = resolve_extension_contributions(&ext);

    assert_eq!(result.mcp_servers.len(), 1);
    let server = &result.mcp_servers[0];
    assert_eq!(server.extension_name, "sqlite-ext");
    assert_eq!(server.source_key, "sqlite-ext:sqlite-mcp");
    assert_eq!(server.config["command"], "npx");
    assert_eq!(server.config["transport"], "stdio");
}

// ---------------------------------------------------------------------------
// CR-3: Preset resolution with @file: reference
// ---------------------------------------------------------------------------

#[test]
fn cr3_preset_file_reference_resolved() {
    let dir = std::env::temp_dir().join("cr3_preset_resolve");
    let prompts = dir.join("prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(prompts.join("system.md"), "You are a helpful coding preset.").unwrap();

    let contributes = ExtContributes {
        presets: vec![ExtPreset {
            source_key: "code-helper".into(),
            name: "Code Helper".into(),
            description: Some("AI coding preset".into()),
            system_prompt: Some("@file:prompts/system.md".into()),
            icon: Some("icons/code.png".into()),
            context: None,
            preferred_agent_id: Some("0190f5fe-7c00-7a00-8000-000000000001".into()),
            enabled_skills: vec!["code-review".into()],
            prompts: vec!["Review this patch".into()],
            models: vec!["gemini-2.0-flash".into()],
        }],
        ..Default::default()
    };

    let ext = make_loaded_extension("helper-ext", &dir.to_string_lossy(), contributes);
    let result = resolve_extension_contributions(&ext);

    assert_eq!(result.presets.len(), 1);
    let preset = &result.presets[0];
    assert_eq!(preset.extension_name, "helper-ext");
    assert_eq!(
        preset.system_prompt.as_deref(),
        Some("You are a helpful coding preset.")
    );
    assert_eq!(
        preset.preferred_agent_id.as_deref(),
        Some("0190f5fe-7c00-7a00-8000-000000000001")
    );
    assert_eq!(preset.enabled_skills, vec!["code-review"]);
    assert_eq!(preset.prompts, vec!["Review this patch"]);
    assert_eq!(preset.models, vec!["gemini-2.0-flash"]);

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// CR-4: Agent resolution with @file: reference
// ---------------------------------------------------------------------------

#[test]
fn cr4_agent_file_reference_resolved() {
    let dir = std::env::temp_dir().join("cr4_agent_resolve");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("agent_ctx.md"), "Agent context loaded from file.").unwrap();

    let contributes = ExtContributes {
        agents: vec![ExtAgent {
            source_key: "auto-agent".into(),
            name: "Auto Agent".into(),
            description: Some("Autonomous coding agent".into()),
            agent_type: Some("claude".into()),
            context: Some("@file:agent_ctx.md".into()),
            icon: None,
            enabled_skills: vec!["ship-it".into()],
            prompts: vec!["Fix the build".into()],
            models: vec!["claude-sonnet-4".into()],
        }],
        ..Default::default()
    };

    let ext = make_loaded_extension("agent-ext", &dir.to_string_lossy(), contributes);
    let result = resolve_extension_contributions(&ext);

    assert_eq!(result.agents.len(), 1);
    let agent = &result.agents[0];
    assert_eq!(agent.extension_name, "agent-ext");
    assert_eq!(agent.source_key, "agent-ext:auto-agent");
    assert_eq!(agent.agent_type.as_deref(), Some("claude"));
    assert_eq!(agent.context.as_deref(), Some("Agent context loaded from file."));
    assert_eq!(agent.enabled_skills, vec!["ship-it"]);
    assert_eq!(agent.prompts, vec!["Fix the build"]);
    assert_eq!(agent.models, vec!["claude-sonnet-4"]);

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// CR-5: Skill resolution
// ---------------------------------------------------------------------------

#[test]
fn cr5_skill_resolved_with_path() {
    let dir = std::env::temp_dir().join("cr5_skill_resolved_with_path");
    std::fs::create_dir_all(dir.join("skills")).unwrap();
    std::fs::write(dir.join("skills/code-review.md"), "# review").unwrap();

    let contributes = ExtContributes {
        skills: vec![ExtSkill {
            name: "code-review".into(),
            description: Some("Review code for quality".into()),
            path: Some("skills/code-review.md".into()),
        }],
        ..Default::default()
    };

    let ext = make_loaded_extension("skill-ext", &dir.to_string_lossy(), contributes);
    let result = resolve_extension_contributions(&ext);

    assert_eq!(result.skills.len(), 1);
    let skill = &result.skills[0];
    assert_eq!(skill.extension_name, "skill-ext");
    assert_eq!(skill.name, "code-review");
    assert!(skill.path.as_ref().unwrap().contains("skills/code-review"));

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// CR-6: Theme resolution (CSS content loaded)
// ---------------------------------------------------------------------------

#[test]
fn cr6_theme_css_content_loaded() {
    let dir = std::env::temp_dir().join("cr6_theme_resolve");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("dark.css"), ":root { --bg: #1a1a2e; --text: #eaeaea; }").unwrap();

    let contributes = ExtContributes {
        themes: vec![ExtTheme {
            id: "dark-theme".into(),
            name: "Dark Theme".into(),
            description: Some("A dark color scheme".into()),
            css_file: "dark.css".into(),
            cover_image: Some("images/dark-preview.png".into()),
        }],
        ..Default::default()
    };

    let ext = make_loaded_extension("theme-ext", &dir.to_string_lossy(), contributes);
    let result = resolve_extension_contributions(&ext);

    assert_eq!(result.themes.len(), 1);
    let theme = &result.themes[0];
    assert_eq!(theme.extension_name, "theme-ext");
    assert_eq!(theme.css_content, ":root { --bg: #1a1a2e; --text: #eaeaea; }");
    assert!(theme.cover_image.as_ref().unwrap().contains("images/dark-preview.png"));

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// CR-7: WebUI route namespace validation
// ---------------------------------------------------------------------------

#[test]
fn cr7_webui_valid_namespace_resolves() {
    let contributes = ExtContributes {
        webui: vec![ExtWebui {
            id: "dashboard".into(),
            directory: "dist".into(),
            routes: vec![ExtWebuiRoute {
                path: "/my-ext/dashboard".into(),
                method: "GET".into(),
                handler: "handler.js".into(),
            }],
        }],
        ..Default::default()
    };

    let ext = make_loaded_extension("my-ext", "/ext/my-ext", contributes);
    let result = resolve_extension_contributions(&ext);

    assert_eq!(result.webui.len(), 1);
    assert_eq!(result.webui[0].extension_name, "my-ext");
}

#[test]
fn cr7_webui_wrong_namespace_rejected() {
    let contributes = ExtContributes {
        webui: vec![ExtWebui {
            id: "bad-route".into(),
            directory: "dist".into(),
            routes: vec![ExtWebuiRoute {
                path: "/other-ext/api".into(),
                method: "GET".into(),
                handler: "handler.js".into(),
            }],
        }],
        ..Default::default()
    };

    let ext = make_loaded_extension("my-ext", "/ext/my-ext", contributes);
    let result = resolve_extension_contributions(&ext);

    // Invalid route should be filtered out
    assert!(result.webui.is_empty());
}

#[test]
fn cr7_webui_reserved_prefix_rejected() {
    let contributes = ExtContributes {
        webui: vec![ExtWebui {
            id: "reserved".into(),
            directory: "dist".into(),
            routes: vec![ExtWebuiRoute {
                path: "/api/data".into(),
                method: "GET".into(),
                handler: "handler.js".into(),
            }],
        }],
        ..Default::default()
    };

    // Extension name "api" would match namespace but /api/ is reserved
    let ext = make_loaded_extension("api", "/ext/api", contributes);
    let result = resolve_extension_contributions(&ext);
    assert!(result.webui.is_empty());
}

// ---------------------------------------------------------------------------
// CR-8: Settings tab with position
// ---------------------------------------------------------------------------

#[test]
fn cr8_settings_tab_position_preserved() {
    let contributes = ExtContributes {
        settings_tabs: vec![ExtSettingsTab {
            id: "ext-settings".into(),
            label: "My Extension".into(),
            icon: Some("icons/gear.svg".into()),
            url: "settings/index.html".into(),
            position: Some(SettingsTabPosition {
                relative_to: "general".into(),
                placement: "after".into(),
            }),
            order: 80,
        }],
        ..Default::default()
    };

    let ext = make_loaded_extension("my-ext", "/ext/my-ext", contributes);
    let result = resolve_extension_contributions(&ext);

    assert_eq!(result.settings_tabs.len(), 1);
    let tab = &result.settings_tabs[0];
    assert_eq!(tab.extension_name, "my-ext");
    assert_eq!(tab.id, "ext-my-ext-ext-settings");
    assert_eq!(tab.url, "/api/extensions/my-ext/assets/settings/index.html");
    assert_eq!(
        tab.icon.as_deref(),
        Some("/api/extensions/my-ext/assets/icons/gear.svg")
    );
    assert_eq!(tab.order, 80);
    let pos = tab.position.as_ref().unwrap();
    assert_eq!(pos.relative_to, "general");
    assert_eq!(pos.placement, "after");
}

// ---------------------------------------------------------------------------
// CR-9: Model provider resolution
// ---------------------------------------------------------------------------

#[test]
fn cr9_model_provider_resolved() {
    let contributes = ExtContributes {
        model_providers: vec![ExtModelProvider {
            id: "custom-provider".into(),
            name: "Custom LLM".into(),
            description: Some("Custom model provider".into()),
            protocol: Some("openai".into()),
            base_url: Some("https://api.custom.com/v1".into()),
            models: vec!["custom-model-1".into(), "custom-model-2".into()],
        }],
        ..Default::default()
    };

    let ext = make_loaded_extension("provider-ext", "/ext/provider-ext", contributes);
    let result = resolve_extension_contributions(&ext);

    assert_eq!(result.model_providers.len(), 1);
    let provider = &result.model_providers[0];
    assert_eq!(provider.extension_name, "provider-ext");
    assert_eq!(provider.id, "custom-provider");
    assert_eq!(provider.protocol.as_deref(), Some("openai"));
    assert_eq!(provider.models.len(), 2);
}

// ---------------------------------------------------------------------------
// CR-10: i18n data loading
// ---------------------------------------------------------------------------

#[test]
fn cr10_i18n_data_loaded_for_supported_locale() {
    let dir = std::env::temp_dir().join("cr10_i18n_resolve");
    let i18n_dir = dir.join("i18n");
    std::fs::create_dir_all(&i18n_dir).unwrap();
    std::fs::write(
        i18n_dir.join("zh-CN.json"),
        r#"{"greeting": "你好", "settings.title": "设置"}"#,
    )
    .unwrap();

    let ext = make_loaded_extension_with_i18n(
        "i18n-ext",
        &dir.to_string_lossy(),
        I18nConfig {
            locales: vec!["en".into(), "zh-CN".into()],
            directory: "i18n".into(),
        },
    );

    let result = resolve_i18n_for_all(&[ext], "zh-CN");
    assert_eq!(result.len(), 1);
    let messages = &result["i18n-ext"];
    assert_eq!(messages["greeting"], "你好");
    assert_eq!(messages["settings.title"], "设置");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cr10_i18n_unsupported_locale_returns_empty() {
    let dir = std::env::temp_dir().join("cr10_i18n_unsupported");
    let i18n_dir = dir.join("i18n");
    std::fs::create_dir_all(&i18n_dir).unwrap();
    std::fs::write(i18n_dir.join("en.json"), r#"{"key": "value"}"#).unwrap();

    let ext = make_loaded_extension_with_i18n(
        "en-only-ext",
        &dir.to_string_lossy(),
        I18nConfig {
            locales: vec!["en".into()],
            directory: "i18n".into(),
        },
    );

    let result = resolve_i18n_for_all(&[ext], "fr");
    assert!(result.is_empty());

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Cross-cutting: resolve_all_contributions merges and filters
// ---------------------------------------------------------------------------

#[test]
fn resolve_all_merges_contributions_from_multiple_extensions() {
    let dir = std::env::temp_dir().join("resolve_all_merges_contributions_from_multiple_extensions");
    std::fs::create_dir_all(dir.join("a/skills")).unwrap();
    std::fs::create_dir_all(dir.join("b/skills")).unwrap();
    std::fs::write(dir.join("a/skills/skill-a.md"), "# a").unwrap();
    std::fs::write(dir.join("b/skills/skill-b.md"), "# b").unwrap();

    let ext_a = make_loaded_extension(
        "ext-a",
        &dir.join("a").to_string_lossy(),
        ExtContributes {
            skills: vec![ExtSkill {
                name: "skill-a".into(),
                description: None,
                path: Some("skills/skill-a.md".into()),
            }],
            model_providers: vec![ExtModelProvider {
                id: "mp-a".into(),
                name: "Provider A".into(),
                description: None,
                protocol: None,
                base_url: None,
                models: vec![],
            }],
            ..Default::default()
        },
    );
    let ext_b = make_loaded_extension(
        "ext-b",
        &dir.join("b").to_string_lossy(),
        ExtContributes {
            skills: vec![ExtSkill {
                name: "skill-b".into(),
                description: None,
                path: Some("skills/skill-b.md".into()),
            }],
            ..Default::default()
        },
    );

    let result = resolve_all_contributions(&[ext_a, ext_b]);
    assert_eq!(result.skills.len(), 2);
    assert_eq!(result.model_providers.len(), 1);

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn resolve_all_skips_disabled_extensions() {
    let mut ext = make_loaded_extension(
        "disabled-ext",
        "/ext/disabled",
        ExtContributes {
            skills: vec![ExtSkill {
                name: "hidden".into(),
                description: None,
                path: None,
            }],
            ..Default::default()
        },
    );
    ext.state.enabled = false;

    let result = resolve_all_contributions(&[ext]);
    assert!(result.skills.is_empty());
}

#[test]
fn channel_plugin_resolved_with_entry_point() {
    let contributes = ExtContributes {
        channel_plugins: vec![ExtChannelPlugin {
            id: "slack".into(),
            name: "Slack".into(),
            description: Some("Slack channel plugin".into()),
            platform: Some("slack".into()),
            entry_point: Some("plugins/slack.js".into()),
            icon: Some("icons/slack.png".into()),
            credential_fields: vec![serde_json::json!({ "key": "token" })],
            config_fields: vec![serde_json::json!({ "key": "channel" })],
        }],
        ..Default::default()
    };

    let ext = make_loaded_extension("channel-ext", "/ext/channel-ext", contributes);
    let result = resolve_extension_contributions(&ext);

    assert_eq!(result.channel_plugins.len(), 1);
    assert_eq!(result.channel_plugins[0].extension_name, "channel-ext");
    assert!(
        result.channel_plugins[0]
            .entry_point
            .as_ref()
            .unwrap()
            .contains("plugins/slack.js")
    );
    assert_eq!(result.channel_plugins[0].credential_fields.len(), 1);
    assert_eq!(result.channel_plugins[0].config_fields.len(), 1);
}
