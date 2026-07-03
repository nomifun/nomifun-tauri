mod common;

use axum::http::StatusCode;
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;

use nomifun_app::{AppConfig, AppServices, build_module_states, create_router_with_states, derive_encryption_key};
use nomifun_common::{decrypt_string, now_ms};
use nomifun_db::{IChannelRepository, SqliteChannelRepository};
use nomifun_extension::{ExtensionSource, ScanPath};

use common::{body_json, build_app, build_app_with_skill_paths, get_with_token, json_with_token, setup_and_login};

fn write_legacy_extension_fixture(tmp: &TempDir) -> std::path::PathBuf {
    let ext_root = tmp.path().join("extensions");
    let ext_dir = ext_root.join("legacy-suite");
    std::fs::create_dir_all(ext_dir.join("assets")).unwrap();
    std::fs::create_dir_all(ext_dir.join("assistants")).unwrap();
    std::fs::create_dir_all(ext_dir.join("agents")).unwrap();
    std::fs::create_dir_all(ext_dir.join("skills")).unwrap();
    std::fs::create_dir_all(ext_dir.join("themes")).unwrap();

    std::fs::write(ext_dir.join("assets/adapter.png"), "adapter").unwrap();
    std::fs::write(ext_dir.join("assets/assistant.png"), "assistant").unwrap();
    std::fs::write(ext_dir.join("assets/agent.png"), "agent").unwrap();
    std::fs::write(ext_dir.join("assets/theme-cover.png"), "cover").unwrap();
    std::fs::write(ext_dir.join("assets/channel.png"), "channel").unwrap();
    std::fs::write(ext_dir.join("assistants/context.md"), "Assistant context from file.").unwrap();
    std::fs::write(ext_dir.join("agents/context.md"), "Agent context from file.").unwrap();
    std::fs::write(ext_dir.join("skills/review.md"), "# review skill").unwrap();
    std::fs::write(ext_dir.join("themes/dark.css"), ":root { --legacy-bg: #111; }").unwrap();

    std::fs::write(
        ext_dir.join("nomi-extension.json"),
        serde_json::to_vec_pretty(&json!({
            "name": "legacy-suite",
            "displayName": "Legacy Suite",
            "version": "1.0.0",
            "engine": {
                "nomifun": "^1.0.0"
            },
            "contributes": {
                "acpAdapters": [
                    {
                        "id": "legacy-acp",
                        "name": "Legacy ACP",
                        "connectionType": "cli",
                        "cliCommand": "legacy-cli",
                        "acpArgs": ["--acp"],
                        "icon": "assets/adapter.png",
                        "apiKeyFields": [
                            {
                                "key": "LEGACY_API_KEY",
                                "label": "API Key",
                                "type": "password",
                                "required": true
                            }
                        ],
                        "yoloMode": {
                            "type": "session"
                        }
                    }
                ],
                "skills": [
                    {
                        "name": "review-skill",
                        "description": "Review code",
                        "file": "skills/review.md"
                    }
                ],
                "channelPlugins": [
                    {
                        "id": "legacy-channel",
                        "name": "Legacy Channel",
                        "description": "Legacy channel plugin",
                        "platform": "legacy-chat",
                        "entryPoint": "plugins/legacy-channel.js",
                        "icon": "assets/channel.png",
                        "credentialFields": [
                            {
                                "key": "legacyToken",
                                "label": "Legacy Token",
                                "type": "password",
                                "required": true
                            }
                        ],
                        "configFields": [
                            {
                                "key": "pollingInterval",
                                "label": "Polling Interval",
                                "type": "number",
                                "default": 30
                            }
                        ]
                    }
                ],
                "assistants": [
                    {
                        "id": "legacy-assistant",
                        "name": "Legacy Assistant",
                        "avatar": "assets/assistant.png",
                        "presetAgentType": "gemini",
                        "contextFile": "assistants/context.md",
                        "models": ["gemini-2.0-flash"],
                        "enabledSkills": ["review-skill"],
                        "prompts": ["Review the diff"]
                    }
                ],
                "agents": [
                    {
                        "id": "legacy-agent",
                        "name": "Legacy Agent",
                        "avatar": "assets/agent.png",
                        "presetAgentType": "codex",
                        "contextFile": "agents/context.md",
                        "models": ["codex-mini"],
                        "enabledSkills": ["review-skill"],
                        "prompts": ["Ship it"]
                    }
                ],
                "mcpServers": [
                    {
                        "name": "legacy-mcp",
                        "description": "Legacy MCP",
                        "enabled": false,
                        "transport": {
                            "type": "stdio",
                            "command": "npx",
                            "args": ["-y", "legacy-mcp"]
                        }
                    }
                ],
                "themes": [
                    {
                        "id": "legacy-dark",
                        "name": "Legacy Dark",
                        "file": "themes/dark.css",
                        "cover": "assets/theme-cover.png"
                    }
                ]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    ext_root
}

async fn build_app_with_extension_root(ext_root: &std::path::Path) -> (axum::Router, AppServices) {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let data_dir = ext_root.join("..").join("data");
    let config = AppConfig {
        data_dir: data_dir.clone(),
        work_dir: data_dir,
        app_version: "1.0.0".to_string(),
        ..Default::default()
    };
    let services = AppServices::from_config(db, &config).await.unwrap();
    let (states, _) = build_module_states(&services).await;
    states
        .extension
        .registry
        .initialize_with_scan_paths(vec![ScanPath {
            path: ext_root.to_path_buf(),
            source: ExtensionSource::Local,
        }])
        .await
        .unwrap();
    let router = create_router_with_states(&services, states);
    (router, services)
}

// ---------------------------------------------------------------------------
// EQ — Extension query (unauthenticated → rejected)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn eq_unauthenticated_access_rejected() {
    let (app, _) = build_app().await;
    let resp = app.oneshot(common::get_request("/api/extensions")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// EQ — Extension query (authenticated)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn eq1_get_loaded_extensions_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app.oneshot(get_with_token("/api/extensions", &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["data"].is_array());
}

#[tokio::test]
async fn eq3_get_themes_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/extensions/themes", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
}

#[tokio::test]
async fn eq4_get_assistants_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/extensions/assistants", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
}

#[tokio::test]
async fn eq5_get_acp_adapters_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/extensions/acp-adapters", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn eq6_get_agents_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/extensions/agents", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn eq7_get_mcp_servers_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/extensions/mcp-servers", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn eq8_get_skills_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/extensions/skills", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn eq8b_get_channel_plugins_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/extensions/channel-plugins", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"], json!([]));
}

#[tokio::test]
async fn eq9_get_settings_tabs_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/extensions/settings-tabs", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn eq10_get_webui_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/extensions/webui", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn eq11_get_agent_activity() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/extensions/agent-activity", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
}

// ---------------------------------------------------------------------------
// EQ-12: i18n
// ---------------------------------------------------------------------------

#[tokio::test]
async fn eq12_get_i18n_for_locale() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/extensions/i18n",
            json!({"locale": "zh-CN"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    // With no extensions loaded, i18n data should be an empty object
    assert!(json["data"].is_object());
}

// ---------------------------------------------------------------------------
// EQ-13, EQ-14: Permissions / risk level for nonexistent → 404
// ---------------------------------------------------------------------------

#[tokio::test]
async fn eq13_permissions_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/extensions/permissions",
            json!({"name": "nonexistent-ext"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn eq14_risk_level_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/extensions/risk-level",
            json!({"name": "nonexistent-ext"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn eq15_legacy_acp_skill_and_mcp_endpoints_preserve_contract() {
    let tmp = TempDir::new().unwrap();
    let ext_root = write_legacy_extension_fixture(&tmp);
    let (mut app, services) = build_app_with_extension_root(&ext_root).await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let skills_resp = app
        .clone()
        .oneshot(get_with_token("/api/extensions/skills", &token))
        .await
        .unwrap();
    assert_eq!(skills_resp.status(), StatusCode::OK);
    let skills_json = body_json(skills_resp).await;
    let skills = skills_json["data"].as_array().unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["name"], "review-skill");
    assert_eq!(skills[0]["description"], "Review code");
    assert!(skills[0]["location"].as_str().unwrap().ends_with("skills/review.md"));
    assert!(skills[0].get("path").is_none());

    let acp_resp = app
        .clone()
        .oneshot(get_with_token("/api/extensions/acp-adapters", &token))
        .await
        .unwrap();
    assert_eq!(acp_resp.status(), StatusCode::OK);
    let acp_json = body_json(acp_resp).await;
    let adapters = acp_json["data"].as_array().unwrap();
    assert_eq!(adapters.len(), 1);
    assert_eq!(adapters[0]["id"], "legacy-acp");
    assert_eq!(adapters[0]["cliCommand"], "legacy-cli");
    assert_eq!(adapters[0]["defaultCliPath"], "legacy-cli");
    assert_eq!(adapters[0]["connectionType"], "cli");
    assert_eq!(adapters[0]["supportsStreaming"], false);
    assert_eq!(adapters[0]["yoloMode"]["type"], "session");
    assert_eq!(
        adapters[0]["avatar"],
        "/api/extensions/legacy-suite/assets/assets/adapter.png"
    );
    assert_eq!(adapters[0]["_extensionName"], "legacy-suite");

    let mcp_resp = app
        .oneshot(get_with_token("/api/extensions/mcp-servers", &token))
        .await
        .unwrap();
    assert_eq!(mcp_resp.status(), StatusCode::OK);
    let mcp_json = body_json(mcp_resp).await;
    let servers = mcp_json["data"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["name"], "legacy-mcp");
    assert_eq!(servers[0]["enabled"], false);
    assert_eq!(servers[0]["transport"]["type"], "stdio");
    assert!(servers[0]["original_json"].as_str().unwrap().contains("legacy-mcp"));
}

#[tokio::test]
async fn eq16_legacy_assistant_agent_and_theme_endpoints_preserve_contract() {
    let tmp = TempDir::new().unwrap();
    let ext_root = write_legacy_extension_fixture(&tmp);
    let (mut app, services) = build_app_with_extension_root(&ext_root).await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let assistant_resp = app
        .clone()
        .oneshot(get_with_token("/api/extensions/assistants", &token))
        .await
        .unwrap();
    assert_eq!(assistant_resp.status(), StatusCode::OK);
    let assistant_json = body_json(assistant_resp).await;
    let assistants = assistant_json["data"].as_array().unwrap();
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0]["id"], "ext-legacy-assistant");
    assert_eq!(assistants[0]["presetAgentType"], "gemini");
    assert_eq!(assistants[0]["enabledSkills"][0], "review-skill");
    assert_eq!(assistants[0]["prompts"][0], "Review the diff");
    assert_eq!(assistants[0]["models"][0], "gemini-2.0-flash");
    assert_eq!(assistants[0]["_kind"], "assistant");
    assert_eq!(assistants[0]["context"], "Assistant context from file.");
    assert_eq!(
        assistants[0]["avatar"],
        "/api/extensions/legacy-suite/assets/assets/assistant.png"
    );

    let agent_resp = app
        .clone()
        .oneshot(get_with_token("/api/extensions/agents", &token))
        .await
        .unwrap();
    assert_eq!(agent_resp.status(), StatusCode::OK);
    let agent_json = body_json(agent_resp).await;
    let agents = agent_json["data"].as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["id"], "ext-legacy-agent");
    assert_eq!(agents[0]["presetAgentType"], "codex");
    assert_eq!(agents[0]["enabledSkills"][0], "review-skill");
    assert_eq!(agents[0]["prompts"][0], "Ship it");
    assert_eq!(agents[0]["models"][0], "codex-mini");
    assert_eq!(agents[0]["_kind"], "agent");
    assert_eq!(agents[0]["context"], "Agent context from file.");
    assert_eq!(
        agents[0]["avatar"],
        "/api/extensions/legacy-suite/assets/assets/agent.png"
    );

    let theme_resp = app
        .oneshot(get_with_token("/api/extensions/themes", &token))
        .await
        .unwrap();
    assert_eq!(theme_resp.status(), StatusCode::OK);
    let theme_json = body_json(theme_resp).await;
    let themes = theme_json["data"].as_array().unwrap();
    assert_eq!(themes.len(), 1);
    assert_eq!(themes[0]["id"], "ext-legacy-suite-legacy-dark");
    assert_eq!(themes[0]["is_preset"], true);
    assert!(themes[0]["css"].as_str().unwrap().contains("--legacy-bg"));
    assert_eq!(
        themes[0]["cover"],
        "/api/extensions/legacy-suite/assets/assets/theme-cover.png"
    );
}

#[tokio::test]
async fn eq17_legacy_channel_plugin_endpoint_preserves_contract() {
    let tmp = TempDir::new().unwrap();
    let ext_root = write_legacy_extension_fixture(&tmp);
    let (mut app, services) = build_app_with_extension_root(&ext_root).await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/extensions/channel-plugins", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let plugins = json["data"].as_array().unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0]["id"], "legacy-channel");
    assert_eq!(plugins[0]["type"], "legacy-channel");
    assert_eq!(plugins[0]["name"], "Legacy Channel");
    assert_eq!(plugins[0]["platform"], "legacy-chat");
    assert!(
        plugins[0]["entryPoint"]
            .as_str()
            .unwrap()
            .ends_with("plugins/legacy-channel.js")
    );
    assert_eq!(plugins[0]["enabled"], true);
    assert_eq!(plugins[0]["connected"], false);
    assert_eq!(plugins[0]["is_extension"], true);
    assert_eq!(plugins[0]["has_token"], false);
    assert_eq!(plugins[0]["active_users"], 0);
    assert_eq!(plugins[0]["extension_meta"]["description"], "Legacy channel plugin");
    assert_eq!(plugins[0]["extension_meta"]["extensionName"], "legacy-suite");
    assert_eq!(
        plugins[0]["extension_meta"]["icon"],
        "/api/extensions/legacy-suite/assets/assets/channel.png"
    );
    assert_eq!(
        plugins[0]["extension_meta"]["credentialFields"][0]["key"],
        "legacyToken"
    );
    assert_eq!(
        plugins[0]["extension_meta"]["configFields"][0]["key"],
        "pollingInterval"
    );
    assert_eq!(plugins[0]["extension_meta"]["configFields"][0]["default"], 30);
}

#[tokio::test]
async fn eq18_channel_status_lists_builtin_and_extension_placeholders() {
    let tmp = TempDir::new().unwrap();
    let ext_root = write_legacy_extension_fixture(&tmp);
    let (mut app, services) = build_app_with_extension_root(&ext_root).await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/channel/plugins", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let plugins = json["data"].as_array().unwrap();

    let telegram = plugins.iter().find(|plugin| plugin["type"] == "telegram").unwrap();
    assert_eq!(telegram["enabled"], false);
    assert_eq!(telegram["connected"], false);
    assert_eq!(telegram["is_extension"], false);

    let legacy = plugins
        .iter()
        .find(|plugin| plugin["type"] == "legacy-channel")
        .unwrap();
    assert_eq!(legacy["enabled"], false);
    assert_eq!(legacy["connected"], false);
    assert_eq!(legacy["status"], "stopped");
    assert_eq!(legacy["is_extension"], true);
    assert_eq!(legacy["extension_meta"]["extensionName"], "legacy-suite");
    assert_eq!(legacy["extension_meta"]["credentialFields"][0]["key"], "legacyToken");
    assert_eq!(legacy["extension_meta"]["configFields"][0]["key"], "pollingInterval");
}

#[tokio::test]
async fn eq19_channel_status_merges_extension_meta_for_persisted_row() {
    let tmp = TempDir::new().unwrap();
    let ext_root = write_legacy_extension_fixture(&tmp);
    let (mut app, services) = build_app_with_extension_root(&ext_root).await;
    let repo = SqliteChannelRepository::new(services.database.pool().clone());
    let now = now_ms();
    repo.upsert_plugin(&nomifun_db::models::ChannelPluginRow {
        id: "legacy-channel".to_string(),
        r#type: "legacy-channel".to_string(),
        name: "Legacy Channel Persisted".to_string(),
        enabled: true,
        config: "{\"token\":\"secret\"}".to_string(),
        status: Some("running".to_string()),
        last_connected: Some(now),
        companion_id: None,
        public_agent_id: None,
        bot_key: None,
        created_at: now,
        updated_at: now,
    })
    .await
    .unwrap();

    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;
    let resp = app
        .oneshot(get_with_token("/api/channel/plugins", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let plugins = json["data"].as_array().unwrap();
    let legacy = plugins
        .iter()
        .find(|plugin| plugin["type"] == "legacy-channel")
        .unwrap();
    assert_eq!(legacy["name"], "Legacy Channel Persisted");
    assert_eq!(legacy["enabled"], true);
    assert_eq!(legacy["status"], "running");
    assert_eq!(legacy["has_token"], true);
    assert_eq!(legacy["is_extension"], true);
    assert_eq!(legacy["extension_meta"]["description"], "Legacy channel plugin");
    assert_eq!(
        legacy["extension_meta"]["icon"],
        "/api/extensions/legacy-suite/assets/assets/channel.png"
    );
}

#[tokio::test]
async fn eq20_enable_extension_channel_persists_config_and_exposes_status() {
    let tmp = TempDir::new().unwrap();
    let ext_root = write_legacy_extension_fixture(&tmp);
    let (mut app, services) = build_app_with_extension_root(&ext_root).await;
    let repo = SqliteChannelRepository::new(services.database.pool().clone());
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let enable_resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/channel/plugins/enable",
            json!({
                "plugin_id": "legacy-channel",
                "config": {
                    "legacyToken": "secret-token",
                    "pollingInterval": 42
                }
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(enable_resp.status(), StatusCode::OK);
    let enable_json = body_json(enable_resp).await;
    assert_eq!(enable_json["data"]["success"], true);

    let row = repo.get_plugin("legacy-channel").await.unwrap().unwrap();
    assert!(row.enabled);
    assert_eq!(row.r#type, "legacy-channel");
    assert_eq!(row.status.as_deref(), Some("stopped"));

    let encryption_key = derive_encryption_key(&services.jwt_secret_raw);
    let decrypted = decrypt_string(&row.config, &encryption_key).unwrap();
    let config_json: serde_json::Value = serde_json::from_str(&decrypted).unwrap();
    assert_eq!(config_json["credentials"]["legacyToken"], "secret-token");
    assert_eq!(config_json["config"]["pollingInterval"], 42);

    let status_resp = app
        .oneshot(get_with_token("/api/channel/plugins", &token))
        .await
        .unwrap();
    assert_eq!(status_resp.status(), StatusCode::OK);
    let status_json = body_json(status_resp).await;
    let plugins = status_json["data"].as_array().unwrap();
    let legacy = plugins
        .iter()
        .find(|plugin| plugin["type"] == "legacy-channel")
        .unwrap();
    assert_eq!(legacy["enabled"], true);
    assert_eq!(legacy["status"], "stopped");
    assert_eq!(legacy["has_token"], true);
    assert_eq!(legacy["is_extension"], true);
}

#[tokio::test]
async fn eq21_disable_extension_channel_updates_status() {
    let tmp = TempDir::new().unwrap();
    let ext_root = write_legacy_extension_fixture(&tmp);
    let (mut app, services) = build_app_with_extension_root(&ext_root).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let _ = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/channel/plugins/enable",
            json!({
                "plugin_id": "legacy-channel",
                "config": {
                    "legacyToken": "secret-token"
                }
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();

    let disable_resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/channel/plugins/disable",
            json!({
                "plugin_id": "legacy-channel"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(disable_resp.status(), StatusCode::OK);
    let disable_json = body_json(disable_resp).await;
    assert_eq!(disable_json["data"]["success"], true);

    let status_resp = app
        .oneshot(get_with_token("/api/channel/plugins", &token))
        .await
        .unwrap();
    assert_eq!(status_resp.status(), StatusCode::OK);
    let status_json = body_json(status_resp).await;
    let plugins = status_json["data"].as_array().unwrap();
    let legacy = plugins
        .iter()
        .find(|plugin| plugin["type"] == "legacy-channel")
        .unwrap();
    assert_eq!(legacy["enabled"], false);
    assert_eq!(legacy["status"], "stopped");
    assert_eq!(legacy["is_extension"], true);
}

// ---------------------------------------------------------------------------
// EM — Extension management
// ---------------------------------------------------------------------------

#[tokio::test]
async fn em3_enable_nonexistent_returns_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/extensions/enable",
            json!({"name": "nonexistent"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn em4_disable_nonexistent_returns_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/extensions/disable",
            json!({"name": "nonexistent"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// HM — Hub marketplace
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hm1_get_hub_extensions() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/hub/extensions", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    // Empty index → empty array
    assert!(json["data"].is_array());
}

#[tokio::test]
async fn hm3_install_nonexistent_returns_error() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/hub/install",
            json!({"name": "nonexistent-ext"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let inner = &json["data"];
    assert_eq!(inner["success"], false);
    assert!(inner["msg"].as_str().is_some());
}

#[tokio::test]
async fn hm5_check_updates_empty() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/hub/check-updates",
            json!({}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["data"].is_array());
}

// ---------------------------------------------------------------------------
// SM — Skill management
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sm11_get_skill_paths() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app.oneshot(get_with_token("/api/skills/paths", &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let data = &json["data"];
    assert!(data["user_skills_dir"].is_string());
    assert!(data["builtin_skills_dir"].is_string());
}

#[tokio::test]
async fn sm9_detect_paths() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/skills/detect-paths", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["data"].is_array());
}

// ---------------------------------------------------------------------------
// CP — Custom external paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cp1_get_external_paths_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/skills/external-paths", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["data"].is_array());
    assert_eq!(json["data"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// AUTH — Auth protection on hub and skill routes too
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_hub_unauthenticated() {
    let (app, _) = build_app().await;
    let resp = app.oneshot(common::get_request("/api/hub/extensions")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn auth_skills_unauthenticated() {
    let (app, _) = build_app().await;
    let resp = app.oneshot(common::get_request("/api/skills")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// RM — Built-in rule reading
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rm1_read_builtin_rule_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/skills/builtin-rule",
            json!({"file_name": "nonexistent-rule.md"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    // File not found → returns empty string (graceful degradation)
    assert_eq!(json["data"], "");
}

#[tokio::test]
async fn rm2_read_builtin_rule_happy_path_returns_file_content() {
    let tmp = TempDir::new().unwrap();
    let (mut app, services, paths) = build_app_with_skill_paths(tmp.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    std::fs::write(
        paths.builtin_rules_dir.join("code-review.md"),
        "# Code Review Rules\n\nBe kind.\n",
    )
    .unwrap();

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/skills/builtin-rule",
            json!({"file_name": "code-review.md"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"], "# Code Review Rules\n\nBe kind.\n");
}

#[tokio::test]
async fn rm3_read_builtin_rule_rejects_path_traversal() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/skills/builtin-rule",
            json!({"file_name": "../etc/passwd"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
}

// ---------------------------------------------------------------------------
// SK — Built-in skill file reading (E4 / `POST /api/skills/builtin-skill`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sk1_read_builtin_skill_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/skills/builtin-skill",
            json!({"file_name": "nonexistent.md"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"], "");
}

#[tokio::test]
async fn sk2_read_builtin_skill_happy_path_returns_file_content() {
    let tmp = TempDir::new().unwrap();
    let (mut app, services, paths) = build_app_with_skill_paths(tmp.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    std::fs::write(
        paths.builtin_skills_dir.join("cowork-skills.md"),
        "## Cowork skills\n\n- git\n- bash\n",
    )
    .unwrap();

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/skills/builtin-skill",
            json!({"file_name": "cowork-skills.md"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"], "## Cowork skills\n\n- git\n- bash\n");
}

#[tokio::test]
async fn sk3_read_builtin_skill_rejects_path_traversal() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    // Path traversal / absolute-path attempts must be rejected. Relative
    // paths with `/` are now legitimate (e.g. `auto-inject/cron/SKILL.md`)
    // and handled by the valid-path code path further below.
    for bad in ["../escape.md", "/etc/passwd", "foo/../etc/passwd", ""] {
        let resp = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/skills/builtin-skill",
                json!({"file_name": bad}),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "file_name={bad:?} should be rejected",
        );
    }
}

// ---------------------------------------------------------------------------
// SI — Skill info (E5 / `POST /api/skills/info`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn si1_read_skill_info_from_directory_path() {
    let tmp = TempDir::new().unwrap();
    let (mut app, services, _paths) = build_app_with_skill_paths(tmp.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let skill_dir = tmp.path().join("my-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: my-skill\ndescription: Handy little thing\n---\nBody",
    )
    .unwrap();

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/skills/info",
            json!({ "skill_path": skill_dir.to_str().unwrap() }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["name"], "my-skill");
    assert_eq!(json["data"]["description"], "Handy little thing");
}

#[tokio::test]
async fn si2_read_skill_info_falls_back_to_directory_name_when_name_empty() {
    let tmp = TempDir::new().unwrap();
    let (mut app, services, _paths) = build_app_with_skill_paths(tmp.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let skill_dir = tmp.path().join("fallback-dir");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: \ndescription: Empty-name skill\n---\nBody",
    )
    .unwrap();

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/skills/info",
            json!({ "skill_path": skill_dir.to_str().unwrap() }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["name"], "fallback-dir");
    assert_eq!(json["data"]["description"], "Empty-name skill");
}

#[tokio::test]
async fn si3_read_skill_info_returns_not_found_for_missing_path() {
    let tmp = TempDir::new().unwrap();
    let (mut app, services, _paths) = build_app_with_skill_paths(tmp.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let missing = tmp.path().join("no-such-skill");

    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/skills/info",
            json!({ "skill_path": missing.to_str().unwrap() }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
}

// ---------------------------------------------------------------------------
// SL — Skill listing (E1 / `GET /api/skills`)
// ---------------------------------------------------------------------------

fn write_skill(dir: &std::path::Path, name: &str, description: &str) {
    let skill = dir.join(name);
    std::fs::create_dir_all(&skill).unwrap();
    let frontmatter = format!("---\nname: {name}\ndescription: {description}\n---\nBody");
    std::fs::write(skill.join("SKILL.md"), frontmatter).unwrap();
}

#[tokio::test]
async fn sl1_list_skills_tags_builtin_and_custom_with_source_field() {
    let tmp = TempDir::new().unwrap();
    let (mut app, services, paths) = build_app_with_skill_paths(tmp.path()).await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let builtin_dir = paths.builtin_skills_dir.clone();
    write_skill(&builtin_dir, "review", "Built-in review skill");
    write_skill(&paths.user_skills_dir, "my-skill", "A user-imported skill");

    let resp = app.oneshot(get_with_token("/api/skills", &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let arr = json["data"].as_array().unwrap();
    assert_eq!(arr.len(), 2);

    let by_name: std::collections::HashMap<_, _> = arr
        .iter()
        .map(|v| (v["name"].as_str().unwrap().to_owned(), v.clone()))
        .collect();

    let review = &by_name["review"];
    assert_eq!(review["source"], "builtin");
    assert_eq!(review["is_custom"], false);
    assert!(
        review["location"].as_str().unwrap().contains("review"),
        "location should point at the skill dir",
    );

    let my_skill = &by_name["my-skill"];
    assert_eq!(my_skill["source"], "custom");
    assert_eq!(my_skill["is_custom"], true);
}

#[tokio::test]
async fn sl2_list_skills_user_custom_overrides_builtin() {
    let tmp = TempDir::new().unwrap();
    let (mut app, services, paths) = build_app_with_skill_paths(tmp.path()).await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let builtin_dir = paths.builtin_skills_dir.clone();
    write_skill(&builtin_dir, "review", "Built-in review");
    write_skill(&paths.user_skills_dir, "review", "Custom review override");

    let resp = app.oneshot(get_with_token("/api/skills", &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let arr = json["data"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["description"], "Custom review override");
    assert_eq!(arr[0]["source"], "custom");
}

#[tokio::test]
async fn sl3_list_skills_returns_empty_array_when_no_skills() {
    let tmp = TempDir::new().unwrap();
    let (mut app, services, _paths) = build_app_with_skill_paths(tmp.path()).await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app.oneshot(get_with_token("/api/skills", &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// BA — Built-in auto skills (E2 / `GET /api/skills/builtin-auto`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ba1_auto_skills_lists_underscore_builtin_entries() {
    let tmp = TempDir::new().unwrap();
    let (mut app, services, paths) = build_app_with_skill_paths(tmp.path()).await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let builtin_dir = paths.builtin_skills_dir.clone();
    let auto_dir = builtin_dir.join("auto-inject");
    write_skill(&auto_dir, "cron", "Schedule recurring tasks");
    write_skill(&auto_dir, "skill-creator", "Scaffold a new skill");
    // A top-level builtin that must NOT appear in the auto list.
    write_skill(&builtin_dir, "review", "Top-level");

    let resp = app
        .oneshot(get_with_token("/api/skills/builtin-auto", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let arr = json["data"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    let names: std::collections::HashSet<_> = arr.iter().map(|v| v["name"].as_str().unwrap()).collect();
    assert!(names.contains("cron"));
    assert!(names.contains("skill-creator"));
    assert!(!names.contains("review"));
    // Must be `{ name, description, location }` — no path / is_custom leak.
    for item in arr {
        assert!(item.get("path").is_none());
        assert!(item.get("is_custom").is_none());
        assert!(item.get("is_custom").is_none());
        assert!(item["description"].is_string());
    }
}

#[tokio::test]
async fn ba2_auto_skills_returns_empty_array_when_subdir_missing() {
    let tmp = TempDir::new().unwrap();
    let (mut app, services, _paths) = build_app_with_skill_paths(tmp.path()).await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let resp = app
        .oneshot(get_with_token("/api/skills/builtin-auto", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn ba3_auto_skills_unauthenticated_rejected() {
    let (app, _) = build_app().await;
    let resp = app
        .oneshot(common::get_request("/api/skills/builtin-auto"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// DE — `GET /api/skills/detect-external` (source slug contract)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn de1_detect_external_populates_custom_source_slug() {
    // The renderer uses `source` as a React key / `data-testid` suffix
    // (`external-source-tab-${source}` in `SkillsHubSettings.tsx`). Custom
    // paths MUST produce slugs prefixed with `custom-` per the e2e contract
    // in `tests/e2e/features/settings/skills/edge-cases.e2e.ts`.
    let tmp = TempDir::new().unwrap();
    let (mut app, services, _paths) = build_app_with_skill_paths(tmp.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let ext_dir = tmp.path().join("external-skills");
    let skill_dir = ext_dir.join("my-ext-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: my-ext-skill\ndescription: External skill\n---\nBody",
    )
    .unwrap();
    let ext_path_str = ext_dir.to_string_lossy().into_owned();

    // Register the custom path through the HTTP surface so the state the
    // handler reads is the same as production.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/external-paths",
            json!({"name": "E2E Custom", "path": ext_path_str}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .oneshot(get_with_token("/api/skills/detect-external", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let arr = json["data"].as_array().expect("data should be an array");
    let custom = arr
        .iter()
        .find(|s| s["name"] == "E2E Custom")
        .expect("custom source should be returned");
    assert_eq!(custom["source"], format!("custom-{ext_path_str}"));
    assert!(
        custom["source"].as_str().unwrap().starts_with("custom-"),
        "custom source must start with `custom-` for e2e testid contract",
    );
    assert_eq!(custom["skill_count"], 1);
}

#[tokio::test]
async fn de2_detect_external_source_slugs_are_unique() {
    let tmp = TempDir::new().unwrap();
    let (mut app, services, _paths) = build_app_with_skill_paths(tmp.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass1").await;

    let mk = |p: &std::path::Path, skill: &str| {
        let dir = p.join(skill);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {skill}\ndescription: d\n---\nBody"),
        )
        .unwrap();
    };
    let dir_a = tmp.path().join("src-a");
    let dir_b = tmp.path().join("src-b");
    mk(&dir_a, "skill-a");
    mk(&dir_b, "skill-b");
    let path_a = dir_a.to_string_lossy().into_owned();
    let path_b = dir_b.to_string_lossy().into_owned();

    for (name, p) in [("Alpha", &path_a), ("Beta", &path_b)] {
        let resp = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/skills/external-paths",
                json!({"name": name, "path": p}),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let resp = app
        .oneshot(get_with_token("/api/skills/detect-external", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let arr = json["data"].as_array().unwrap();
    let slugs: Vec<&str> = arr
        .iter()
        .filter(|s| s["name"] == "Alpha" || s["name"] == "Beta")
        .map(|s| s["source"].as_str().unwrap())
        .collect();
    assert_eq!(slugs.len(), 2);
    assert_ne!(slugs[0], slugs[1], "distinct custom paths → distinct slugs");
}
