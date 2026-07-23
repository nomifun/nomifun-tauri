//! E2E tests for IDMM (Intelligent Decision-Making Mode) HTTP endpoints.
//!
//! Phase 2: the per-session config is two independently-toggled watches
//! (`fault_watch` / `decision_watch`), each with a flattened [`WatchBase`]
//! (enabled / tier / bypass_model / …). The `RulePlusModel` tier requires a
//! resolvable bypass model; `RuleOnly` does not. The GET status surfaces
//! `enabled` (= any watch on), `fault_enabled`, `decision_enabled`, and the
//! persisted `config` blob for form rehydration.

mod common;

use axum::http::StatusCode;
use nomifun_common::{IdmmInterventionId, ProviderId};
use serde_json::json;
use tower::ServiceExt;

use common::{
    body_json, build_app, delete_with_token, get_request, get_with_token, json_with_token,
    setup_and_login,
};

async fn create_conversation(app: &mut axum::Router, token: &str, csrf: &str) -> String {
    let body = json!({ "type": "nomi", "name": "idmm-e2e", "extra": { "workspace": "/project" } });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/conversations", body, token, csrf))
        .await
        .unwrap();
    let json = body_json(resp).await;
    json["data"]["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn create_terminal(app: &mut axum::Router, token: &str, csrf: &str) -> String {
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/terminals",
            json!({
                "name": "idmm-owner-e2e",
                "cwd": std::env::temp_dir().to_string_lossy(),
                "command": "cat",
                "defer_spawn": true
            }),
            token,
            csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    body_json(response).await["data"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// IDMM's global backup provider is a hard executable binding. Seed the
/// provider row explicitly so settings tests exercise a valid production
/// configuration instead of relying on a dangling provider reference.
async fn seed_provider(services: &nomifun_app::AppServices, provider_id: &str, model: &str) {
    nomifun_db::sqlx::query(
        "INSERT INTO providers \
         (provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
          capabilities, created_at, updated_at) \
         VALUES (?, 'openai', ?, 'https://example.invalid', 'encrypted', ?, 1, '[]', 1, 1)",
    )
    .bind(provider_id)
    .bind(format!("Provider {provider_id}"))
    .bind(serde_json::json!([model]).to_string())
    .execute(services.database.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn unauthenticated_get_is_rejected() {
    let (app, _services) = build_app().await;
    let resp = app
        .oneshot(get_request("/api/idmm/conversation/whatever"))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "expected 401/403, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn conversation_and_terminal_idmm_are_forbidden_to_another_user() {
    let (mut app, services) = build_app().await;
    let (owner_token, owner_csrf) =
        setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conversation_id = create_conversation(&mut app, &owner_token, &owner_csrf).await;
    let terminal_id = create_terminal(&mut app, &owner_token, &owner_csrf).await;
    let (other_token, _other_csrf) =
        setup_and_login(&mut app, &services, "idmm-other", "StrongP@ss2").await;

    let response = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/idmm/conversation/{conversation_id}"),
            &other_token,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = app
        .oneshot(get_with_token(
            &format!("/api/idmm/terminal/{terminal_id}"),
            &other_token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rule_only_config_roundtrip_on_conversation() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv = create_conversation(&mut app, &token, &csrf).await;

    // Enable the decision watch at the RuleOnly tier (no bypass model required).
    let body = json!({
        "kind": "conversation",
        "target_id": conv,
        "decision_watch": { "enabled": true, "tier": "rule_only", "scan_interval_secs": 30, "max_retries": 3 }
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/idmm", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "RuleOnly enable should succeed");
    let j = body_json(resp).await;
    assert_eq!(j["data"]["enabled"], true);
    assert_eq!(j["data"]["decision_enabled"], true);
    assert_eq!(j["data"]["run_state"], "armed");

    // Read it back.
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/idmm/conversation/{conv}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["data"]["enabled"], true);
    assert_eq!(j["data"]["config"]["decision_watch"]["tier"], "rule_only");
}

#[tokio::test]
async fn allow_unmarked_pick_flag_round_trips_on_conversation() {
    // The decision strategy's option-decision auto-pick flag must survive
    // serialize → DB → deserialize so the saved config rehydrates the form (and
    // the supervisor reads it). Set it explicitly and read it back.
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv = create_conversation(&mut app, &token, &csrf).await;

    let body = json!({
        "kind": "conversation",
        "target_id": conv,
        "decision_watch": {
            "enabled": true,
            "tier": "rule_only",
            "strategy": { "categories": { "option_decision": { "allow_unmarked_pick": true } } }
        }
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/idmm", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "enabling RuleOnly auto-pick should succeed");

    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/idmm/conversation/{conv}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(
        j["data"]["config"]["decision_watch"]["strategy"]["categories"]["option_decision"]["allow_unmarked_pick"],
        true,
        "allow_unmarked_pick must persist and rehydrate; got {j:?}"
    );
}

#[tokio::test]
async fn model_tier_without_freeform_policy_is_allowed() {
    // The strategy's freeform policy is OPTIONAL for the RulePlusModel tier (a
    // conservative built-in policy is used when empty). With a resolvable global
    // backup provider, enabling the model tier with no freeform must SUCCEED.
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let provider_id = ProviderId::new().into_string();
    seed_provider(&services, &provider_id, "m1").await;
    let conv = create_conversation(&mut app, &token, &csrf).await;

    let settings = json!({ "backup_provider_id": provider_id, "default_steering_prompt": "" });
    let resp = app
        .clone()
        .oneshot(json_with_token("PUT", "/api/idmm/settings", settings, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = json!({
        "kind": "conversation",
        "target_id": conv,
        "decision_watch": { "enabled": true, "tier": "rule_plus_model" }
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/idmm", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "empty freeform must be allowed once a backup model resolves"
    );
    let j = body_json(resp).await;
    assert_eq!(j["data"]["enabled"], true);
    assert_eq!(j["data"]["config"]["decision_watch"]["tier"], "rule_plus_model");
}

#[tokio::test]
async fn model_tier_without_backup_provider_is_rejected() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv = create_conversation(&mut app, &token, &csrf).await;

    // No global backup provider, no per-watch bypass model, and the e2e
    // conversation carries no model of its own → nothing resolves → must 400.
    let body = json!({
        "kind": "conversation",
        "target_id": conv,
        "decision_watch": { "enabled": true, "tier": "rule_plus_model" }
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/idmm", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let j = body_json(resp).await;
    assert!(
        j["error"].as_str().unwrap_or("").contains("backup"),
        "error should mention the missing backup model, got {j:?}"
    );
}

#[tokio::test]
async fn fault_watch_model_tier_without_backup_is_rejected() {
    // The fault watch on the model tier carries the same backup requirement.
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv = create_conversation(&mut app, &token, &csrf).await;

    let body = json!({
        "kind": "conversation",
        "target_id": conv,
        "fault_watch": { "enabled": true, "tier": "rule_plus_model" }
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/idmm", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn model_tier_with_global_backup_succeeds() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let provider_id = ProviderId::new().into_string();
    seed_provider(&services, &provider_id, "m1").await;
    let conv = create_conversation(&mut app, &token, &csrf).await;

    // Configure a global backup provider.
    let settings = json!({ "backup_provider_id": provider_id, "backup_model": "m1", "default_steering_prompt": "" });
    let resp = app
        .clone()
        .oneshot(json_with_token("PUT", "/api/idmm/settings", settings, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = json!({
        "kind": "conversation",
        "target_id": conv,
        "decision_watch": {
            "enabled": true,
            "tier": "rule_plus_model",
            "strategy": { "freeform_policy": "prefer the recommended option; never delete data" }
        }
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/idmm", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "model tier enable with a global backup should succeed"
    );
    let j = body_json(resp).await;
    assert_eq!(j["data"]["config"]["decision_watch"]["tier"], "rule_plus_model");
    assert_eq!(j["data"]["sidecar_provider_resolved"], true);
}

#[tokio::test]
async fn settings_roundtrip() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let provider_id = ProviderId::new().into_string();
    seed_provider(&services, &provider_id, "model-xyz").await;

    let settings = json!({
        "backup_provider_id": provider_id,
        "backup_model": "model-xyz",
        "default_steering_prompt": "be conservative"
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("PUT", "/api/idmm/settings", settings, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(get_with_token("/api/idmm/settings", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["data"]["backup_provider_id"], provider_id);
    assert_eq!(j["data"]["backup_model"], "model-xyz");
    assert_eq!(j["data"]["default_steering_prompt"], "be conservative");
}

#[tokio::test]
async fn settings_update_clears_optional_backup_fields_when_absent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let provider_id = ProviderId::new().into_string();
    seed_provider(&services, &provider_id, "model-old").await;

    let initial = json!({
        "backup_provider_id": provider_id,
        "backup_model": "model-old",
        "default_steering_prompt": "old policy"
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("PUT", "/api/idmm/settings", initial, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let cleared = json!({ "default_steering_prompt": "" });
    let resp = app
        .clone()
        .oneshot(json_with_token("PUT", "/api/idmm/settings", cleared, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(get_with_token("/api/idmm/settings", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert!(
        j["data"].get("backup_provider_id").is_none() || j["data"]["backup_provider_id"].is_null(),
        "clearing the global backup provider must remove the stored preference; got {j:?}"
    );
    assert!(
        j["data"].get("backup_model").is_none() || j["data"]["backup_model"].is_null(),
        "clearing the global backup model must remove the stored preference; got {j:?}"
    );
    assert_eq!(j["data"]["default_steering_prompt"], "");
}

#[tokio::test]
async fn terminal_target_unknown_kind_is_rejected() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .clone()
        .oneshot(get_with_token("/api/idmm/bogus/some-id", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn terminal_target_not_found_is_rejected() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // A terminal that does not exist → ownership verification yields 404.
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/idmm/terminal/nonexistent-term", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn disabled_state_reports_off() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conversation_id = create_conversation(&mut app, &token, &csrf).await;
    // An owned conversation that was never configured → default disabled state.
    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/idmm/conversation/{conversation_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["data"]["enabled"], false);
    assert_eq!(j["data"]["run_state"], "off");
}

// ── must always be able to disable + persisted config round-trips ──

#[tokio::test]
async fn disable_model_watch_without_backup_succeeds() {
    // The user filled the model-tier form, then toggled off. The disable POST
    // sends the model tier but `enabled: false`. Pre-fix this returned 400 and
    // the user could not disable; post-fix the disable must always succeed (a
    // disabled watch carries no operational requirements).
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv = create_conversation(&mut app, &token, &csrf).await;

    let body = json!({
        "kind": "conversation",
        "target_id": conv,
        "decision_watch": { "enabled": false, "tier": "rule_plus_model" }
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/idmm", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "disabling must always succeed regardless of model-tier prerequisites"
    );
    let j = body_json(resp).await;
    assert_eq!(j["data"]["enabled"], false);
    assert_eq!(j["data"]["run_state"], "off");
}

#[tokio::test]
async fn enabled_to_disabled_transition_succeeds_without_validation() {
    // The user enables the model tier with a global backup, later turns it off.
    // The disable POST must succeed even without the backup still resolving.
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let provider_id = ProviderId::new().into_string();
    seed_provider(&services, &provider_id, "m1").await;
    let conv = create_conversation(&mut app, &token, &csrf).await;
    let settings = json!({ "backup_provider_id": provider_id, "backup_model": "m1", "default_steering_prompt": "" });
    let resp = app
        .clone()
        .oneshot(json_with_token("PUT", "/api/idmm/settings", settings, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Enable.
    let body = json!({
        "kind": "conversation",
        "target_id": conv,
        "decision_watch": { "enabled": true, "tier": "rule_plus_model" }
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/idmm", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Disable → must still succeed.
    let body = json!({
        "kind": "conversation",
        "target_id": conv,
        "decision_watch": { "enabled": false, "tier": "rule_plus_model" }
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/idmm", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["data"]["enabled"], false);
}

#[tokio::test]
async fn get_status_round_trips_persisted_config() {
    // After saving a config, GET must return the persisted blob (both watches,
    // tiers, bypass model, strategy) so the frontend can rehydrate its form.
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let provider_id = ProviderId::new().into_string();
    let watch_provider_id = ProviderId::new().into_string();
    seed_provider(&services, &provider_id, "m1").await;
    seed_provider(&services, &watch_provider_id, "m-watch").await;
    let conv = create_conversation(&mut app, &token, &csrf).await;

    let settings = json!({ "backup_provider_id": provider_id, "backup_model": "m1", "default_steering_prompt": "" });
    let resp = app
        .clone()
        .oneshot(json_with_token("PUT", "/api/idmm/settings", settings, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = json!({
        "kind": "conversation",
        "target_id": conv,
        "fault_watch": { "enabled": true, "tier": "rule_only", "scan_interval_secs": 45, "max_retries": 7 },
        "decision_watch": {
            "enabled": true,
            "tier": "rule_plus_model",
            "answer_open_questions": true,
            "bypass_model": { "provider_id": watch_provider_id, "model": "m-watch" },
            "strategy": { "freeform_policy": "round-trip me", "tendency": "aggressive" }
        }
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/idmm", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/idmm/conversation/{conv}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let cfg = j["data"]["config"].clone();
    assert!(
        cfg.is_object(),
        "GET must include the persisted IdmmConfig under .data.config; got {j:?}"
    );
    assert_eq!(cfg["fault_watch"]["enabled"], true);
    assert_eq!(cfg["fault_watch"]["tier"], "rule_only");
    assert_eq!(cfg["fault_watch"]["scan_interval_secs"], 45);
    assert_eq!(cfg["fault_watch"]["max_retries"], 7);
    assert_eq!(cfg["decision_watch"]["enabled"], true);
    assert_eq!(cfg["decision_watch"]["tier"], "rule_plus_model");
    assert_eq!(cfg["decision_watch"]["answer_open_questions"], true);
    assert_eq!(
        cfg["decision_watch"]["bypass_model"]["provider_id"],
        watch_provider_id
    );
    assert_eq!(cfg["decision_watch"]["bypass_model"]["model"], "m-watch");
    assert_eq!(cfg["decision_watch"]["strategy"]["freeform_policy"], "round-trip me");
    assert_eq!(cfg["decision_watch"]["strategy"]["tendency"], "aggressive");
}

#[tokio::test]
async fn legacy_phase1_blob_disables_gracefully() {
    // D3 back-compat at the HTTP layer: a Phase-1-shaped body (enabled/tier/
    // rule/sidecar/steering_prompt) must not error — serde ignores the unknown
    // fields and both watches deserialize to the default (disabled). The POST
    // succeeds and the target reports `enabled: false`.
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv = create_conversation(&mut app, &token, &csrf).await;

    let body = json!({
        "kind": "conversation",
        "target_id": conv,
        "enabled": true,
        "tier": "rule_plus_sidecar",
        "steering_prompt": "prefer recommended",
        "rule": { "idle_threshold_secs": 30, "max_retries": 3, "auto_pick_unmarked": true },
        "sidecar": { "provider_id": "prov", "model": "m" }
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/idmm", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a legacy Phase-1 blob must deserialize to default (disabled), not error"
    );
    let j = body_json(resp).await;
    assert_eq!(j["data"]["enabled"], false, "legacy blob → both watches default-disabled");
}

#[tokio::test]
async fn get_status_omits_config_when_never_configured() {
    // Existing owned targets whose IDMM config was never saved must not carry
    // a `config` field so the frontend can seed from global defaults.
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conversation_id = create_conversation(&mut app, &token, &csrf).await;
    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/idmm/conversation/{conversation_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert!(
        j["data"].get("config").is_none() || j["data"]["config"].is_null(),
        "unconfigured targets must not carry a persisted config; got {j:?}"
    );
}

#[tokio::test]
async fn deleting_conversation_cascades_idmm_records() {
    // IDMM records are disposable and carry no FK (polymorphic target_id), so the
    // app layer must clear them when the owning conversation is deleted. Insert a
    // record for a conversation target, delete the conversation via the HTTP route
    // (which fires the OnConversationDelete cascade hook), then assert the record
    // is gone.
    use nomifun_db::models::NewIdmmInterventionRow;
    use nomifun_db::{IIdmmInterventionRepository, SqliteIdmmInterventionRepository};

    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv = create_conversation(&mut app, &token, &csrf).await;
    let owner_user_id = services
        .user_repo
        .find_by_username("admin")
        .await
        .unwrap()
        .unwrap()
        .user_id
        .to_string();

    let records: SqliteIdmmInterventionRepository =
        SqliteIdmmInterventionRepository::new(services.database.pool().clone());

    let intervention_id = IdmmInterventionId::new();
    let row = NewIdmmInterventionRow {
        intervention_id: intervention_id.as_str().to_owned(),
        user_id: owner_user_id.clone(),
        target_kind: "conversation".into(),
        target_id: conv.clone(),
        watch: "decision".into(),
        at: 1,
        signal: "decision".into(),
        tier_used: "rule".into(),
        category: Some("option".into()),
        action: "answer_choice".into(),
        detail: Some("option 2".into()),
        reason: Some("rule-tier auto-pick".into()),
        confidence: None,
        bypass_model: None,
        outcome: "applied".into(),
    };
    records.insert(&row).await.unwrap();

    let before = records
        .list_for_target(&owner_user_id, "conversation", &conv, 30)
        .await
        .unwrap();
    assert_eq!(before.len(), 1, "record must exist before delete");
    assert_eq!(
        before[0].intervention_id,
        intervention_id.as_str(),
        "repository must preserve the business UUIDv7"
    );
    assert!(
        IdmmInterventionId::parse(before[0].intervention_id.clone()).is_ok(),
        "stored intervention_id must remain a bare lowercase UUIDv7"
    );

    // Delete the conversation — the cascade hook runs inside the delete path.
    let resp = app
        .clone()
        .oneshot(delete_with_token(&format!("/api/conversations/{conv}"), &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "conversation delete should succeed");

    let after = records
        .list_for_target(&owner_user_id, "conversation", &conv, 30)
        .await
        .unwrap();
    assert!(
        after.is_empty(),
        "IDMM records must be cascade-cleared when the conversation is deleted; got {after:?}"
    );
}

#[tokio::test]
async fn cross_session_activity_feed_round_trips() {
    // IDMM is an installation-owner control plane. Its activity feed reads the
    // owner's targets most-recent-first, rejects secondary users at the HTTP
    // boundary, and bulk clear removes only the owner's rows. Insert records
    // for two users directly so repository deletion isolation remains covered
    // without granting the secondary user access to IDMM.
    use nomifun_db::models::NewIdmmInterventionRow;
    use nomifun_db::{IIdmmInterventionRepository, SqliteIdmmInterventionRepository};

    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let (other_token, other_csrf) =
        setup_and_login(&mut app, &services, "idmm-activity-other", "StrongP@ss2").await;
    let owner_user_id = services
        .user_repo
        .find_by_username("admin")
        .await
        .unwrap()
        .unwrap()
        .user_id
        .to_string();
    let other_user_id = services
        .user_repo
        .find_by_username("idmm-activity-other")
        .await
        .unwrap()
        .unwrap()
        .user_id
        .to_string();
    let owner_conversation_a = create_conversation(&mut app, &token, &csrf).await;
    let owner_conversation_b = create_conversation(&mut app, &token, &csrf).await;
    let owner_terminal = create_terminal(&mut app, &token, &csrf).await;
    let other_conversation = create_conversation(&mut app, &other_token, &other_csrf).await;

    let records: SqliteIdmmInterventionRepository =
        SqliteIdmmInterventionRepository::new(services.database.pool().clone());

    let make_row =
        |intervention_id: &IdmmInterventionId,
         user_id: &str,
         target_kind: &str,
         target_id: &str,
         at: i64| NewIdmmInterventionRow {
            intervention_id: intervention_id.as_str().to_owned(),
            user_id: user_id.into(),
            target_kind: target_kind.into(),
            target_id: target_id.into(),
            watch: "decision".into(),
            at,
            signal: "decision".into(),
            tier_used: "rule".into(),
            category: Some("option".into()),
            action: "answer_choice".into(),
            detail: Some("option 2".into()),
            reason: Some("rule-tier auto-pick".into()),
            confidence: None,
            bypass_model: None,
            outcome: "applied".into(),
        };

    // Two distinct targets, at interleaved → most-recent-first must mix them.
    let activity_a = IdmmInterventionId::new();
    let activity_b = IdmmInterventionId::new();
    let activity_c = IdmmInterventionId::new();
    let other_activity = IdmmInterventionId::new();

    records
        .insert(&make_row(
            &activity_a,
            &owner_user_id,
            "conversation",
            &owner_conversation_a,
            10,
        ))
        .await
        .unwrap();
    records
        .insert(&make_row(
            &activity_b,
            &owner_user_id,
            "terminal",
            &owner_terminal,
            30,
        ))
        .await
        .unwrap();
    records
        .insert(&make_row(
            &activity_c,
            &owner_user_id,
            "conversation",
            &owner_conversation_b,
            20,
        ))
        .await
        .unwrap();
    records
        .insert(&make_row(
            &other_activity,
            &other_user_id,
            "conversation",
            &other_conversation,
            40,
        ))
        .await
        .unwrap();

    // GET the feed → most-recent-first across ALL targets (30 -> 20 -> 10).
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/idmm/activity", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let items = j["data"].as_array().expect("activity feed is an array");
    let intervention_ids: Vec<IdmmInterventionId> = items
        .iter()
        .map(|record| {
            assert!(
                record.get("id").is_none(),
                "legacy generic id must not be emitted: {record:?}"
            );
            assert!(
                record.get("intervention_row_id").is_none(),
                "technical row id must not cross the HTTP boundary: {record:?}"
            );
            let value = record
                .get("intervention_id")
                .expect("intervention_id must be emitted");
            assert!(
                value.is_string(),
                "intervention_id must not be an integer: {record:?}"
            );
            let value = value.as_str().unwrap();
            assert!(
                !value.bytes().all(|byte| byte.is_ascii_digit()),
                "numeric-string intervention_id must not be emitted: {record:?}"
            );
            assert!(
                !value.starts_with("idmm_"),
                "prefixed intervention_id must not be emitted: {record:?}"
            );
            IdmmInterventionId::parse(value.to_owned())
                .expect("intervention_id must be a bare lowercase UUIDv7")
        })
        .collect();
    assert_eq!(intervention_ids, vec![activity_b, activity_c, activity_a]);
    // Spans both targets.
    assert_eq!(items[0]["target_kind"], "terminal");
    assert_eq!(items[1]["target_kind"], "conversation");

    // Secondary users cannot access the installation-wide IDMM control plane.
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/idmm/activity", &other_token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // DELETE the feed → clears this owner's records across every target.
    let resp = app
        .clone()
        .oneshot(delete_with_token("/api/idmm/activity", &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["data"], 3, "bulk clear must report the removed count");

    // The feed is now empty.
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/idmm/activity", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert!(
        j["data"].as_array().unwrap().is_empty(),
        "activity feed must be empty after bulk clear; got {j:?}"
    );

    // Clearing the owner's feed must not delete a row belonging to another
    // user, even though that user cannot access IDMM over HTTP.
    let other_rows = records.list_recent(&other_user_id, 100).await.unwrap();
    assert_eq!(other_rows.len(), 1);
    assert_eq!(
        other_rows[0].intervention_id,
        other_activity.as_str(),
        "repository isolation must be checked with the business UUIDv7, not the technical row id"
    );

    // The owner-only boundary remains enforced after the owner's mutation.
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/idmm/activity", &other_token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = app
        .oneshot(delete_with_token(
            "/api/idmm/activity",
            &other_token,
            &other_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
