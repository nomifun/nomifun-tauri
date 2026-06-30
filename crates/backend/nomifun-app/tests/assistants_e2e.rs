//! HTTP integration tests for `/api/assistants/*` plus the source-dispatched
//! `/api/skills/assistant-rule/*` and `/api/skills/assistant-skill/*` trio.
//!
//! Each test exercises the router end-to-end via `tower::ServiceExt::oneshot`
//! against a real `nomifun_app::create_router_with_states` instance backed by
//! an in-memory SQLite database. The assistant module state is re-built with
//! a temp-dir built-in manifest, a temp user-data dir, and (where needed) an
//! extension registry initialized from a fixture manifest that contributes an
//! assistant — giving coverage of the three-source dispatch (builtin / user /
//! extension) without touching `~/.nomifun/`.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use nomifun_app::{AppConfig, AppServices, ModuleStates, build_module_states, create_router_with_states};
use nomifun_assistant::{AssistantRouterState, AssistantService, BuiltinAssistantRegistry};
use nomifun_db::{
    IAssistantOverrideRepository, IAssistantRepository, IAssistantTagRepository, IProviderRepository,
    SqliteAssistantOverrideRepository, SqliteAssistantRepository, SqliteAssistantTagRepository, SqliteProviderRepository,
    init_database_memory,
};
use nomifun_extension::{
    AssistantRuleDispatcher, ExtensionRegistry, ExtensionRouterState, ExtensionSource, ExtensionStateStore,
    ExternalPathsManager, HubIndexManager, HubInstaller, HubRouterState, ScanPath, SkillPaths, SkillRouterState,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use common::{body_json, delete_with_token, get_with_token, json_with_token, setup_and_login};

// ---------------------------------------------------------------------------
// Fixture — router + temp dirs + services
// ---------------------------------------------------------------------------

/// Hold onto the temp dirs for the lifetime of the fixture so on-disk
/// fixtures survive until the test returns.
#[allow(dead_code)]
struct Fixture {
    app: axum::Router,
    services: AppServices,
    token: String,
    csrf: String,
    // user-data root containing assistant-rules / assistant-skills / assistant-avatars
    user_data_dir: std::path::PathBuf,
    // dir holding assistants.json manifest + per-file rule/skill/avatar assets
    builtin_assets_dir: std::path::PathBuf,
    _user_tmp: TempDir,
    _builtin_tmp: TempDir,
    _ext_tmp: TempDir,
}

/// Build the whole app with:
/// - a manifest at `{builtin_tmp}/assets/assistants.json` registering two
///   built-ins (`builtin-office` with rule/skill/avatar files on disk, and
///   `builtin-bare` with nothing referenced)
/// - a temp user-data dir that `AssistantService` uses for user rule/skill/
///   avatar storage
/// - an extension registry initialized from a fixture manifest that
///   contributes an assistant with id `ext-helper`
///
/// Also logs in `admin` and hands back the session + CSRF tokens so tests
/// can issue authenticated mutating requests.
async fn fixture() -> Fixture {
    let user_tmp = TempDir::new().unwrap();
    let builtin_tmp = TempDir::new().unwrap();
    let ext_tmp = TempDir::new().unwrap();

    let user_data_dir = user_tmp.path().to_path_buf();
    let builtin_assets_dir = builtin_tmp.path().join("assets");
    std::fs::create_dir_all(&builtin_assets_dir).unwrap();

    // Builtin manifest: office has rule/skill/avatar on disk, bare has nothing.
    std::fs::create_dir_all(builtin_assets_dir.join("rules")).unwrap();
    std::fs::create_dir_all(builtin_assets_dir.join("skills")).unwrap();
    std::fs::write(builtin_assets_dir.join("rules/office.en-US.md"), "office rule body").unwrap();
    std::fs::write(builtin_assets_dir.join("skills/office.en-US.md"), "office skill body").unwrap();
    // Tiny PNG-ish placeholder — content_type logic only inspects extension.
    std::fs::write(builtin_assets_dir.join("office.png"), b"not-a-real-png").unwrap();

    let manifest = json!({
        "version": "1.0.0",
        "assistants": [
            {
                "id": "builtin-office",
                "name": "Office",
                "preset_agent_type": "gemini",
                "rule_file": "rules/office.{locale}.md",
                "skill_file": "skills/office.{locale}.md",
                "avatar": "office.png",
            },
            {
                "id": "builtin-bare",
                "name": "Bare",
                "preset_agent_type": "gemini",
            }
        ]
    });
    std::fs::write(
        builtin_assets_dir.join("assistants.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    // Extension fixture: a single extension directory containing a manifest
    // with `contributes.assistants = [{ id: "ext-helper", ... }]`.
    let ext_root = ext_tmp.path().join("extensions");
    let ext_dir = ext_root.join("fixture-ext");
    std::fs::create_dir_all(&ext_dir).unwrap();
    let ext_manifest = json!({
        "name": "fixture-ext",
        "version": "1.0.0",
        "display_name": "Fixture Extension",
        "contributes": {
            "assistants": [{
                "id": "ext-helper",
                "name": "Helper",
                "description": "Contributed by fixture-ext",
                "system_prompt": "You are helpful.",
                "context": "Extension context body",
            }]
        }
    });
    std::fs::write(
        ext_dir.join("nomi-extension.json"),
        serde_json::to_vec_pretty(&ext_manifest).unwrap(),
    )
    .unwrap();

    // Bring up in-memory DB + services + default module states.
    let db = init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let (mut states, _): (ModuleStates, _) = build_module_states(&services).await;

    // Replace the extension + hub + skill states with freshly-constructed
    // ones rooted at our temp dirs. The defaults built by
    // `build_module_states` point at `~/.nomifun/` for the state store and
    // external-paths file, which can hold arbitrary contents on a dev box
    // and poison the test. Building from scratch gives us a pristine
    // registry we can initialize with our fixture extension.
    let ext_data_dir = ext_tmp.path().join("ext-data");
    std::fs::create_dir_all(&ext_data_dir).unwrap();
    let state_store = ExtensionStateStore::new(ext_data_dir.join("extension-states.json"));
    let registry = ExtensionRegistry::new(state_store, services.event_bus.clone(), "1.0.0".to_string());
    registry
        .initialize_with_scan_paths(vec![ScanPath {
            path: ext_root.clone(),
            source: ExtensionSource::Env,
        }])
        .await
        .unwrap();
    states.extension = ExtensionRouterState {
        registry: registry.clone(),
    };
    let hub_dir = ext_data_dir.join("extensions");
    let index_manager = HubIndexManager::new(hub_dir, registry.clone());
    let installer = HubInstaller::new(index_manager.clone(), registry.clone());
    states.hub = HubRouterState {
        index_manager,
        installer,
    };
    let ext_paths_mgr = Arc::new(ExternalPathsManager::with_file(ext_data_dir.join("paths.json")).await);
    let skill_paths = SkillPaths {
        data_dir: ext_data_dir.clone(),
        user_skills_dir: ext_data_dir.join("skills"),
        cron_skills_dir: ext_data_dir.join("cron").join("skills"),
        builtin_skills_dir: ext_data_dir.join("builtin-skills"),
        builtin_rules_dir: ext_data_dir.join("builtin-rules"),
        assistant_rules_dir: user_data_dir.join("assistant-rules"),
        assistant_skills_dir: user_data_dir.join("assistant-skills"),
    };
    states.skill = SkillRouterState {
        skill_paths,
        external_paths_manager: ext_paths_mgr,
        assistant_dispatcher: None, // wired below once service is constructed
        skill_tag_repo: std::sync::Arc::new(nomifun_db::SqliteSkillTagRepository::new(
            services.database.pool().clone(),
        )),
        builtin_skill_tags: std::sync::Arc::new(std::collections::HashMap::new()),
    };

    // Rebuild AssistantService pointing at our temp built-in manifest + temp
    // user-data dir. `build_module_states` loads the default built-in
    // registry (pointing at $exe_dir/assets or dev fallback) and uses
    // `~/.nomifun/` for user data — neither is appropriate for tests.
    let pool = services.database.pool().clone();
    let repo: Arc<dyn IAssistantRepository> = Arc::new(SqliteAssistantRepository::new(pool.clone()));
    let override_repo: Arc<dyn IAssistantOverrideRepository> =
        Arc::new(SqliteAssistantOverrideRepository::new(pool.clone()));
    let tag_repo: Arc<dyn IAssistantTagRepository> = Arc::new(SqliteAssistantTagRepository::new(pool.clone()));
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool));
    // Seed an OpenAI-compatible provider so create / import calls without
    // an explicit `preset_agent_type` resolve to `"nomi"` instead of
    // erroring out — mirroring a configured production setup.
    provider_repo
        .create(nomifun_db::CreateProviderParams {
            id: None,
            platform: "openai",
            name: "Test OpenAI",
            base_url: "https://example.invalid",
            api_key_encrypted: "stub",
            models: "[]",
            enabled: true,
            capabilities: "[]",
            context_limit: None,
            model_protocols: None,
            model_descriptions: None,
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
        })
        .await
        .expect("seed provider");
    let builtin = Arc::new(BuiltinAssistantRegistry::load_from_dir(builtin_assets_dir.clone()));
    let service = Arc::new(AssistantService::new(
        repo,
        override_repo,
        tag_repo,
        provider_repo,
        builtin,
        registry,
        user_data_dir.clone(),
    ));
    states.assistant = AssistantRouterState {
        service: service.clone(),
    };
    // Rewire the skill-router dispatcher so assistant-rule / assistant-skill
    // endpoints route through the test-configured service.
    let dispatcher: Arc<dyn AssistantRuleDispatcher> = service;
    states.skill.assistant_dispatcher = Some(dispatcher);

    let mut app = create_router_with_states(&services, states);
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    Fixture {
        app,
        services,
        token,
        csrf,
        user_data_dir,
        builtin_assets_dir,
        _user_tmp: user_tmp,
        _builtin_tmp: builtin_tmp,
        _ext_tmp: ext_tmp,
    }
}

// ===========================================================================
// GET /api/assistants
// ===========================================================================

#[tokio::test]
async fn list_populated_returns_builtins_and_extension() {
    let fx = fixture().await;

    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let list = json["data"].as_array().unwrap();
    // 2 builtins + 1 extension
    assert_eq!(list.len(), 3, "body = {json}");
    let ids: Vec<&str> = list.iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"builtin-office"));
    assert!(ids.contains(&"builtin-bare"));
    assert!(ids.contains(&"ext-helper"));
    let sources: Vec<&str> = list.iter().map(|a| a["source"].as_str().unwrap()).collect();
    assert!(sources.contains(&"builtin"));
    assert!(sources.contains(&"extension"));
}

#[tokio::test]
async fn list_requires_auth() {
    let fx = fixture().await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/assistants")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    // auth_middleware returns 403 Forbidden for any authentication failure
    // (see `nomifun_auth::middleware::auth_middleware`).
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// POST /api/assistants
// ===========================================================================

#[tokio::test]
async fn create_happy_path_returns_201() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({ "id": "u1", "name": "Mine", "description": "hello" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["id"], "u1");
    assert_eq!(json["data"]["source"], "user");
    assert_eq!(json["data"]["name"], "Mine");
    assert_eq!(json["data"]["description"], "hello");
}

#[tokio::test]
async fn create_rejects_empty_name_with_400() {
    let fx = fixture().await;
    let req = json_with_token("POST", "/api/assistants", json!({ "name": "   " }), &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_rejects_builtin_id_collision_with_400() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({ "id": "builtin-office", "name": "spoof" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_rejects_extension_id_collision_with_400() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({ "id": "ext-helper", "name": "spoof" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_rejects_duplicate_user_id_with_409() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({ "id": "u1", "name": "A" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({ "id": "u1", "name": "B" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

// ===========================================================================
// PUT /api/assistants/{id}
// ===========================================================================

#[tokio::test]
async fn update_happy_path_returns_200() {
    let fx = fixture().await;
    create_user(&fx, "u1", "original").await;

    let req = json_with_token(
        "PUT",
        "/api/assistants/u1",
        json!({ "name": "renamed" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "renamed");
}

#[tokio::test]
async fn update_missing_user_returns_404() {
    let fx = fixture().await;
    let req = json_with_token(
        "PUT",
        "/api/assistants/ghost",
        json!({ "name": "renamed" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_builtin_is_forbidden() {
    let fx = fixture().await;
    let req = json_with_token(
        "PUT",
        "/api/assistants/builtin-office",
        json!({ "name": "hijack" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn update_extension_is_forbidden() {
    let fx = fixture().await;
    let req = json_with_token(
        "PUT",
        "/api/assistants/ext-helper",
        json!({ "name": "hijack" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// DELETE /api/assistants/{id}
// ===========================================================================

#[tokio::test]
async fn delete_happy_path_removes_row_and_user_assets() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    // Drop a rule, skill, and avatar on disk so the fs-cleanup branch has
    // something to remove.
    let rules_dir = fx.user_data_dir.join("assistant-rules");
    let skills_dir = fx.user_data_dir.join("assistant-skills");
    let avatars_dir = fx.user_data_dir.join("assistant-avatars");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::create_dir_all(&avatars_dir).unwrap();
    std::fs::write(rules_dir.join("u1.md"), "rule").unwrap();
    std::fs::write(skills_dir.join("u1.md"), "skill").unwrap();
    std::fs::write(avatars_dir.join("u1.png"), b"avatar").unwrap();

    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token("/api/assistants/u1", &fx.token, &fx.csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Row is gone (list no longer contains u1).
    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants", &fx.token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let ids: Vec<&str> = json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&"u1"));

    // Fs cleanup ran.
    assert!(!rules_dir.join("u1.md").exists());
    assert!(!skills_dir.join("u1.md").exists());
    assert!(!avatars_dir.join("u1.png").exists());
}

#[tokio::test]
async fn delete_builtin_is_forbidden() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token("/api/assistants/builtin-office", &fx.token, &fx.csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn delete_extension_is_forbidden() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token("/api/assistants/ext-helper", &fx.token, &fx.csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// PATCH /api/assistants/{id}/state
// ===========================================================================

#[tokio::test]
async fn set_state_inserts_override_for_builtin() {
    let fx = fixture().await;
    let req = json_with_token(
        "PATCH",
        "/api/assistants/builtin-office/state",
        json!({ "enabled": false, "sort_order": 9 }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["enabled"], false);
    assert_eq!(json["data"]["sort_order"], 9);
    assert_eq!(json["data"]["source"], "builtin");
}

#[tokio::test]
async fn set_state_updates_existing_override_for_user() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    // First call inserts.
    let req = json_with_token(
        "PATCH",
        "/api/assistants/u1/state",
        json!({ "enabled": false, "sort_order": 3 }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Second call updates sort_order and preserves enabled when omitted.
    let req = json_with_token(
        "PATCH",
        "/api/assistants/u1/state",
        json!({ "sort_order": 7 }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["enabled"], false);
    assert_eq!(json["data"]["sort_order"], 7);
}

#[tokio::test]
async fn set_state_extension_is_400() {
    let fx = fixture().await;
    let req = json_with_token(
        "PATCH",
        "/api/assistants/ext-helper/state",
        json!({ "enabled": false }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn set_state_unknown_user_returns_404() {
    let fx = fixture().await;
    let req = json_with_token(
        "PATCH",
        "/api/assistants/ghost/state",
        json!({ "enabled": true }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// POST /api/assistants/import
// ===========================================================================

#[tokio::test]
async fn import_happy_path_inserts_new_rows() {
    let fx = fixture().await;
    let body = json!({
        "assistants": [
            { "id": "u1", "name": "A" },
            { "id": "u2", "name": "B" },
        ]
    });
    let req = json_with_token("POST", "/api/assistants/import", body, &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["imported"], 2);
    assert_eq!(json["data"]["skipped"], 0);
    assert_eq!(json["data"]["failed"], 0);
}

#[tokio::test]
async fn import_skips_builtin_collision() {
    let fx = fixture().await;
    let body = json!({
        "assistants": [
            { "id": "builtin-office", "name": "spoof" }
        ]
    });
    let req = json_with_token("POST", "/api/assistants/import", body, &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["imported"], 0);
    assert_eq!(json["data"]["skipped"], 1);
}

#[tokio::test]
async fn import_skips_extension_collision() {
    let fx = fixture().await;
    let body = json!({
        "assistants": [
            { "id": "ext-helper", "name": "spoof" }
        ]
    });
    let req = json_with_token("POST", "/api/assistants/import", body, &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["imported"], 0);
    assert_eq!(json["data"]["skipped"], 1);
}

#[tokio::test]
async fn import_skips_already_imported_user_row() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    let body = json!({
        "assistants": [
            { "id": "u1", "name": "A-updated" }
        ]
    });
    let req = json_with_token("POST", "/api/assistants/import", body, &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["imported"], 0);
    assert_eq!(json["data"]["skipped"], 1);

    // Verify we did NOT overwrite the original name.
    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants", &fx.token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let entry = find_id(&json["data"], "u1").unwrap();
    assert_eq!(entry["name"], "A");
}

#[tokio::test]
async fn import_retry_is_idempotent() {
    let fx = fixture().await;
    let body = json!({
        "assistants": [
            { "id": "u1", "name": "A" }
        ]
    });
    // First attempt — imported.
    let req = json_with_token("POST", "/api/assistants/import", body.clone(), &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    let first = body_json(resp).await;
    assert_eq!(first["data"]["imported"], 1);

    // Second attempt — same payload, now skipped.
    let req = json_with_token("POST", "/api/assistants/import", body, &fx.token, &fx.csrf);
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    let second = body_json(resp).await;
    assert_eq!(second["data"]["imported"], 0);
    assert_eq!(second["data"]["skipped"], 1);
}

// ===========================================================================
// GET /api/assistants/{id}/avatar
// ===========================================================================

#[tokio::test]
async fn avatar_builtin_returns_bytes_with_content_type() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants/builtin-office/avatar", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(&bytes[..], b"not-a-real-png");
}

#[tokio::test]
async fn avatar_user_returns_bytes_after_file_planted() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    let avatars_dir = fx.user_data_dir.join("assistant-avatars");
    std::fs::create_dir_all(&avatars_dir).unwrap();
    std::fs::write(avatars_dir.join("u1.svg"), b"<svg></svg>").unwrap();

    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants/u1/avatar", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("image/svg+xml")
    );
}

#[tokio::test]
async fn avatar_missing_returns_404() {
    let fx = fixture().await;
    // builtin-bare declared no avatar in the manifest; lookup should 404.
    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/assistants/builtin-bare/avatar", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// POST /api/skills/assistant-rule/read
// ===========================================================================

#[tokio::test]
async fn read_rule_builtin_returns_manifest_file_contents() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/read",
        json!({ "assistant_id": "builtin-office", "locale": "en-US" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"], "office rule body");
}

#[tokio::test]
async fn read_rule_extension_returns_empty_string() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/read",
        json!({ "assistant_id": "ext-helper", "locale": "en-US" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"], "");
}

#[tokio::test]
async fn read_rule_user_round_trip_through_write() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;

    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/write",
        json!({ "assistant_id": "u1", "content": "my rule", "locale": "en-US" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/read",
        json!({ "assistant_id": "u1", "locale": "en-US" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"], "my rule");
}

// ===========================================================================
// POST /api/skills/assistant-rule/write
// ===========================================================================

#[tokio::test]
async fn write_rule_user_happy_path() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/write",
        json!({ "assistant_id": "u1", "content": "rule body" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // File was actually written.
    let file = fx.user_data_dir.join("assistant-rules/u1.md");
    assert_eq!(std::fs::read_to_string(file).unwrap(), "rule body");
}

#[tokio::test]
async fn write_rule_builtin_returns_400() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/write",
        json!({ "assistant_id": "builtin-office", "content": "nope" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn write_rule_extension_returns_400() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-rule/write",
        json!({ "assistant_id": "ext-helper", "content": "nope" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// DELETE /api/skills/assistant-rule/{id}
// ===========================================================================

#[tokio::test]
async fn delete_rule_user_removes_file() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    let rules_dir = fx.user_data_dir.join("assistant-rules");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(rules_dir.join("u1.md"), "body").unwrap();

    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token("/api/skills/assistant-rule/u1", &fx.token, &fx.csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!rules_dir.join("u1.md").exists());
}

#[tokio::test]
async fn delete_rule_builtin_returns_400() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token(
            "/api/skills/assistant-rule/builtin-office",
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delete_rule_extension_returns_400() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token(
            "/api/skills/assistant-rule/ext-helper",
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// POST /api/skills/assistant-skill/read
// ===========================================================================

#[tokio::test]
async fn read_skill_builtin_returns_manifest_file_contents() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/read",
        json!({ "assistant_id": "builtin-office", "locale": "en-US" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"], "office skill body");
}

#[tokio::test]
async fn read_skill_extension_returns_empty_string() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/read",
        json!({ "assistant_id": "ext-helper", "locale": "en-US" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"], "");
}

#[tokio::test]
async fn read_skill_user_round_trip_through_write() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;

    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/write",
        json!({ "assistant_id": "u1", "content": "my skill", "locale": "zh-CN" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/read",
        json!({ "assistant_id": "u1", "locale": "zh-CN" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"], "my skill");
}

// ===========================================================================
// POST /api/skills/assistant-skill/write
// ===========================================================================

#[tokio::test]
async fn write_skill_user_happy_path() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/write",
        json!({ "assistant_id": "u1", "content": "skill body" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let file = fx.user_data_dir.join("assistant-skills/u1.md");
    assert_eq!(std::fs::read_to_string(file).unwrap(), "skill body");
}

#[tokio::test]
async fn write_skill_builtin_returns_400() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/write",
        json!({ "assistant_id": "builtin-office", "content": "nope" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn write_skill_extension_returns_400() {
    let fx = fixture().await;
    let req = json_with_token(
        "POST",
        "/api/skills/assistant-skill/write",
        json!({ "assistant_id": "ext-helper", "content": "nope" }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// DELETE /api/skills/assistant-skill/{id}
// ===========================================================================

#[tokio::test]
async fn delete_skill_user_removes_file() {
    let fx = fixture().await;
    create_user(&fx, "u1", "A").await;
    let skills_dir = fx.user_data_dir.join("assistant-skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(skills_dir.join("u1.md"), "body").unwrap();

    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token("/api/skills/assistant-skill/u1", &fx.token, &fx.csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!skills_dir.join("u1.md").exists());
}

#[tokio::test]
async fn delete_skill_builtin_returns_400() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token(
            "/api/skills/assistant-skill/builtin-office",
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delete_skill_extension_returns_400() {
    let fx = fixture().await;
    let resp = fx
        .app
        .clone()
        .oneshot(delete_with_token(
            "/api/skills/assistant-skill/ext-helper",
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Helpers local to this test module
// ===========================================================================

async fn create_user(fx: &Fixture, id: &str, name: &str) {
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({ "id": id, "name": name }),
        &fx.token,
        &fx.csrf,
    );
    let resp = fx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "create {id} failed");
}

fn find_id<'a>(list: &'a Value, id: &str) -> Option<&'a Value> {
    list.as_array()?.iter().find(|a| a["id"].as_str() == Some(id))
}
