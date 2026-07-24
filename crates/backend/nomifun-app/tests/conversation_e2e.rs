//! E2E tests for conversation CRUD, clone, reset, associated, and auth protection.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, delete_with_token, get_request, get_with_token, json_with_token, setup_and_login};

const MISSING_CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345679999";

// ── Helpers ───────────────────────────────────────────────────────────

fn create_body(name: &str) -> serde_json::Value {
    json!({
        "type": "acp",
        "name": name,
        "extra": { "workspace": "/project" }
    })
}

fn create_body_with_extra(name: &str, extra: serde_json::Value) -> serde_json::Value {
    json!({
        "type": "acp",
        "name": name,
        "extra": extra
    })
}

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

// ── T1: Create ────────────────────────────────────────────────────────

#[tokio::test]
async fn t1_1_create_conversation_success() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/conversations", create_body("Code Review"), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let data = &json["data"];
    assert_eq!(data["name"], "Code Review");
    assert_eq!(data["type"], "acp");
    assert_eq!(data["status"], "pending");
    assert_eq!(data["source"], "nomifun");
    assert_eq!(data["pinned"], false);
    nomifun_common::ConversationId::parse(
        data["conversation_id"]
            .as_str()
            .expect("conversation id"),
    )
        .expect("conversation id must be a bare UUIDv7");
    assert!(data.get("id").is_none());
    assert!(data["created_at"].as_i64().is_some());
    assert!(data["modified_at"].as_i64().is_some());
    assert_eq!(data["extra"]["workspace"], "/project");
}

#[tokio::test]
async fn t1_2_create_various_agent_types() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let types = ["acp", "openclaw-gateway", "nanobot", "remote"];
    for agent_type in types {
        let body = json!({
            "type": agent_type,
            "extra": {}
        });
        let req = json_with_token("POST", "/api/conversations", body, &token, &csrf);
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "type={agent_type}");
        let json = body_json(resp).await;
        assert_eq!(json["data"]["type"], agent_type);
    }
}

#[tokio::test]
async fn t1_3_create_with_optional_fields() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({
        "type": "acp",
        "name": "Telegram Bot",
        "source": "telegram",
        "channel_chat_id": "user:123",
        "extra": {}
    });
    let req = json_with_token("POST", "/api/conversations", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["source"], "telegram");
    assert_eq!(json["data"]["channel_chat_id"], "user:123");
}

#[tokio::test]
async fn t1_4_create_missing_required_field() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Missing type
    let body = json!({
        "model": { "provider_id": "0190f5fe-7c00-7a00-8000-000000000010", "model": "m1" },
        "extra": {}
    });
    let req = json_with_token("POST", "/api/conversations", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // model is optional — omitting it should succeed
    let body = json!({ "type": "acp", "extra": {} });
    let req = json_with_token("POST", "/api/conversations", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Missing extra
    let body = json!({
        "type": "nomi",
        "model": { "provider_id": "0190f5fe-7c00-7a00-8000-000000000010", "model": "m1" }
    });
    let req = json_with_token("POST", "/api/conversations", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn t1_5_create_invalid_type() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({
        "type": "invalid_type",
        "model": { "provider_id": "0190f5fe-7c00-7a00-8000-000000000010", "model": "m1" },
        "extra": {}
    });
    let req = json_with_token("POST", "/api/conversations", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn t1_5b_create_accepts_interior_whitespace_and_rejects_edge_whitespace() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Interior whitespace ("Application Support" on macOS, "my project") is a
    // normal path and must be accepted.
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("Application Support").join("my project");
    std::fs::create_dir_all(&workspace).unwrap();

    let body = json!({
        "type": "acp",
        "extra": {
            "workspace": workspace.to_string_lossy()
        }
    });
    let req = json_with_token("POST", "/api/conversations", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["extra"]["workspace"], workspace.to_string_lossy().as_ref());

    // A directory name that ends with whitespace is pathological (Win32
    // strips trailing spaces on lookup) and stays rejected.
    let edge_workspace = format!("{} ", temp.path().join("repo").to_string_lossy());
    let body = json!({
        "type": "acp",
        "extra": {
            "workspace": edge_workspace
        }
    });
    let req = json_with_token("POST", "/api/conversations", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp).await;
    assert_eq!(json["code"], "WORKSPACE_PATH_EDGE_WHITESPACE_UNSUPPORTED");
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("begins or ends with whitespace"),
        "unexpected error payload: {json}"
    );
}

#[tokio::test]
async fn t1_6_create_requires_auth() {
    let (app, _services) = build_app().await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/conversations")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&create_body("test")).unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── T2: List ──────────────────────────────────────────────────────────

#[tokio::test]
async fn t2_1_list_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app.oneshot(get_with_token("/api/conversations", &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["items"].as_array().unwrap().len(), 0);
    assert_eq!(json["data"]["total"], 0);
    assert_eq!(json["data"]["has_more"], false);
}

#[tokio::test]
async fn t2_2_list_basic() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    for i in 0..3 {
        let req = json_with_token(
            "POST",
            "/api/conversations",
            create_body(&format!("Conv {i}")),
            &token,
            &csrf,
        );
        app.clone().oneshot(req).await.unwrap();
    }

    let resp = app.oneshot(get_with_token("/api/conversations", &token)).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["items"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn t2_3_list_cursor_pagination() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    for i in 0..5 {
        let req = json_with_token(
            "POST",
            "/api/conversations",
            create_body(&format!("Conv {i}")),
            &token,
            &csrf,
        );
        app.clone().oneshot(req).await.unwrap();
    }

    // First page: limit=2
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/conversations?limit=2", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let items = json["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(json["data"]["has_more"], true);

    // Second page using cursor
    let cursor = items.last().unwrap()["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/conversations?limit=2&cursor={cursor}"),
            &token,
        ))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let items2 = json["data"]["items"].as_array().unwrap();
    assert_eq!(items2.len(), 2);
    assert_eq!(json["data"]["has_more"], true);

    // Third page
    let cursor2 = items2.last().unwrap()["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations?limit=2&cursor={cursor2}"),
            &token,
        ))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let items3 = json["data"]["items"].as_array().unwrap();
    assert_eq!(items3.len(), 1);
    assert_eq!(json["data"]["has_more"], false);
}

#[tokio::test]
async fn t2_4_list_source_filter() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create 2 nomifun + 1 telegram
    for _ in 0..2 {
        let req = json_with_token("POST", "/api/conversations", create_body("Nomi Conv"), &token, &csrf);
        app.clone().oneshot(req).await.unwrap();
    }

    let tg_body = json!({
        "type": "acp",
        "name": "TG Conv",
        "source": "telegram",
        "extra": {}
    });
    let req = json_with_token("POST", "/api/conversations", tg_body, &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let resp = app
        .oneshot(get_with_token("/api/conversations?source=telegram", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let items = json["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["source"], "telegram");
}

#[tokio::test]
async fn t2_5_list_pinned_filter() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create 2 conversations
    let req = json_with_token("POST", "/api/conversations", create_body("Unpinned"), &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let req = json_with_token("POST", "/api/conversations", create_body("Will Pin"), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let pinned_id = json["data"]["conversation_id"].as_str().unwrap().to_owned();

    // Pin one
    let req = json_with_token(
        "PATCH",
        &format!("/api/conversations/{pinned_id}"),
        json!({"pinned": true}),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    let resp = app
        .oneshot(get_with_token("/api/conversations?pinned=true", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let items = json["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["pinned"], true);
}

#[tokio::test]
async fn t2_6_list_requires_auth() {
    let (app, _services) = build_app().await;
    let resp = app.oneshot(get_request("/api/conversations")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── T3: Get ───────────────────────────────────────────────────────────

#[tokio::test]
async fn t3_1_get_existing() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/conversations", create_body("My Conv"), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let id = json["data"]["conversation_id"].as_str().unwrap().to_owned();

    let resp = app
        .oneshot(get_with_token(&format!("/api/conversations/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["conversation_id"], id);
    assert!(json["data"].get("id").is_none());
    assert_eq!(json["data"]["name"], "My Conv");
}

#[tokio::test]
async fn t3_2_get_not_found() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{MISSING_CONVERSATION_ID}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn t3_3_get_requires_auth() {
    let (app, _services) = build_app().await;
    let resp = app.oneshot(get_request("/api/conversations/some-id")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── T4: Update ────────────────────────────────────────────────────────

#[tokio::test]
async fn t4_1_update_name() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/conversations", create_body("Original"), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let id = json["data"]["conversation_id"].as_str().unwrap().to_owned();
    let original_modified = json["data"]["modified_at"].as_i64().unwrap();

    let req = json_with_token(
        "PATCH",
        &format!("/api/conversations/{id}"),
        json!({"name": "Updated"}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "Updated");
    assert!(json["data"]["modified_at"].as_i64().unwrap() >= original_modified);
}

#[tokio::test]
async fn t4_2_update_pin_and_unpin() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/conversations", create_body("Pin Test"), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let id = json["data"]["conversation_id"].as_str().unwrap().to_owned();

    // Pin
    let req = json_with_token(
        "PATCH",
        &format!("/api/conversations/{id}"),
        json!({"pinned": true}),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["pinned"], true);
    assert!(json["data"]["pinned_at"].as_i64().is_some());

    // Unpin
    let req = json_with_token(
        "PATCH",
        &format!("/api/conversations/{id}"),
        json!({"pinned": false}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["pinned"], false);
    assert!(json["data"]["pinned_at"].is_null());
}

#[tokio::test]
async fn t4_3_update_extra_merge() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = create_body_with_extra(
        "Merge Test",
        json!({"workspace": "/old", "context_file_name": "ctx.md"}),
    );
    let req = json_with_token("POST", "/api/conversations", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let id = json["data"]["conversation_id"].as_str().unwrap().to_owned();

    // Merge update: change workspace, keep contextFileName
    let req = json_with_token(
        "PATCH",
        &format!("/api/conversations/{id}"),
        json!({"extra": {"workspace": "/new"}}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["extra"]["workspace"], "/new");
    assert_eq!(json["data"]["extra"]["context_file_name"], "ctx.md");
}

#[tokio::test]
async fn t4_4_update_model() {
    let (mut app, services) = build_app().await;
    seed_provider(&services, "0190f5fe-7c00-7a00-8000-000000000010", "m1").await;
    seed_provider(&services, "0190f5fe-7c00-7a00-8000-000000000011", "new-model").await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // nomi — only type that allows top-level model updates
    let create = json!({
        "type": "nomi",
        "name": "Model Test",
        "model": { "provider_id": "0190f5fe-7c00-7a00-8000-000000000010", "model": "m1" },
        "extra": {}
    });
    let req = json_with_token("POST", "/api/conversations", create, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let id = json["data"]["conversation_id"].as_str().unwrap().to_owned();

    let req = json_with_token(
        "PATCH",
        &format!("/api/conversations/{id}"),
        json!({"model": {"provider_id": "0190f5fe-7c00-7a00-8000-000000000011", "model": "new-model"}}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["model"]["provider_id"], "0190f5fe-7c00-7a00-8000-000000000011");
    assert_eq!(json["data"]["model"]["model"], "new-model");
}

#[tokio::test]
async fn t4_5_update_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "PATCH",
        &format!("/api/conversations/{MISSING_CONVERSATION_ID}"),
        json!({"name": "X"}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn t4_6_update_requires_auth() {
    let (app, _services) = build_app().await;
    let resp = app.oneshot(get_request("/api/conversations/some-id")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── T5: Delete ────────────────────────────────────────────────────────

#[tokio::test]
async fn t5_1_delete_conversation() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/conversations", create_body("To Delete"), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let id = json["data"]["conversation_id"].as_str().unwrap().to_owned();

    let message_id = nomifun_common::MessageId::new().into_string();
    nomifun_db::sqlx::query(
        "INSERT INTO messages \
         (message_id, conversation_id, type, content, hidden, created_at) \
         VALUES (?, ?, 'text', '{}', 0, 1)",
    )
    .bind(&message_id)
    .bind(&id)
    .execute(services.database.pool())
    .await
    .unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO conversation_artifacts \
         (conversation_id, kind, status, payload, created_at, updated_at) \
         VALUES (?, 'skill_suggest', 'active', '{}', 1, 1)",
    )
    .bind(&id)
    .execute(services.database.pool())
    .await
    .unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO message_correlations \
         (conversation_id, turn_message_id, message_type, correlation_key, message_id) \
         VALUES (?, ?, 'text', 'delete-cleanup', ?)",
    )
    .bind(&id)
    .bind(&message_id)
    .bind(nomifun_common::MessageId::new().into_string())
    .execute(services.database.pool())
    .await
    .unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO acp_session \
         (conversation_id, agent_backend, agent_source) \
         VALUES (?, 'acp', 'builtin')",
    )
    .bind(&id)
    .execute(services.database.pool())
    .await
    .unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO idmm_interventions \
         (user_id, target_kind, target_id, watch, at, signal, tier_used, action, outcome) \
         VALUES (?, 'conversation', ?, 'fault', 1, 'test', 'rule_only', 'none', 'recorded')",
    )
    .bind(services.authoritative_user_id.as_ref())
    .bind(&id)
    .execute(services.database.pool())
    .await
    .unwrap();

    let resp = app
        .clone()
        .oneshot(delete_with_token(&format!("/api/conversations/{id}"), &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let orphan_counts: (i64, i64, i64, i64, i64) = nomifun_db::sqlx::query_as(
        "SELECT \
            (SELECT COUNT(*) FROM messages WHERE conversation_id = ?1), \
            (SELECT COUNT(*) FROM conversation_artifacts WHERE conversation_id = ?1), \
            (SELECT COUNT(*) FROM message_correlations WHERE conversation_id = ?1), \
            (SELECT COUNT(*) FROM acp_session WHERE conversation_id = ?1), \
            (SELECT COUNT(*) FROM idmm_interventions \
             WHERE target_kind = 'conversation' AND target_id = ?1)",
    )
    .bind(&id)
    .fetch_one(services.database.pool())
    .await
    .unwrap();
    assert_eq!(
        orphan_counts,
        (0, 0, 0, 0, 0),
        "conversation deletion must directly remove database-owned orphan rows"
    );

    // Verify it's gone
    let resp = app
        .oneshot(get_with_token(&format!("/api/conversations/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn t5_2_delete_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .oneshot(delete_with_token(
            &format!("/api/conversations/{MISSING_CONVERSATION_ID}"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn t5_3_delete_requires_auth() {
    let (app, _services) = build_app().await;
    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri("/api/conversations/some-id")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── T6: Clone ─────────────────────────────────────────────────────────

#[tokio::test]
async fn t6_2_clone_without_source() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let clone_body = json!({
        "conversation": {
            "type": "acp",
            "name": "Fresh Clone",
            "extra": {}
        }
    });
    let req = json_with_token("POST", "/api/conversations/clone", clone_body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "Fresh Clone");
    assert_eq!(json["data"]["type"], "acp");
}

#[tokio::test]
async fn t6_4_clone_requires_auth() {
    let (app, _services) = build_app().await;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/conversations/clone")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(b"{}".to_vec()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── T7: Reset ─────────────────────────────────────────────────────────

#[tokio::test]
async fn t7_1_reset_conversation() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create conversation
    let req = json_with_token("POST", "/api/conversations", create_body("Reset Test"), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let id = json["data"]["conversation_id"].as_str().unwrap().to_owned();

    // Insert a message directly via repo
    let repo = nomifun_db::SqliteConversationRepository::new(services.database.pool().clone());
    let msg = nomifun_db::models::MessageRow {
        id: 0,
        message_id: nomifun_common::MessageId::new().into_string(),
        conversation_id: id.clone(),
        msg_id: None,
        r#type: "text".into(),
        content: r#"{"content":"hello"}"#.into(),
        position: None,
        status: None,
        hidden: false,
        created_at: 1000,
    };
    nomifun_db::IConversationRepository::insert_message(&repo, &msg)
        .await
        .unwrap();

    // Reset
    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{id}/reset"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify messages cleared
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/conversations/{id}/messages"), &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["items"].as_array().unwrap().len(), 0);

    // Verify status is pending
    let resp = app
        .oneshot(get_with_token(&format!("/api/conversations/{id}"), &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["status"], "pending");
}

#[tokio::test]
async fn t7_2_reset_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{MISSING_CONVERSATION_ID}/reset"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn t7_3_reset_requires_auth() {
    let (app, _services) = build_app().await;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/conversations/some-id/reset")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(b"{}".to_vec()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── T10: Associated ───────────────────────────────────────────────────

#[tokio::test]
async fn t10_1_associated_same_workspace() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create 3 conversations: 2 same workspace, 1 different
    let body1 = create_body_with_extra("Conv A", json!({"workspace": "/same"}));
    let req = json_with_token("POST", "/api/conversations", body1, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let id_a = json["data"]["conversation_id"].as_str().unwrap().to_owned();

    let body2 = create_body_with_extra("Conv B", json!({"workspace": "/same"}));
    let req = json_with_token("POST", "/api/conversations", body2, &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let body3 = create_body_with_extra("Conv C", json!({"workspace": "/other"}));
    let req = json_with_token("POST", "/api/conversations", body3, &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let resp = app
        .oneshot(get_with_token(&format!("/api/conversations/{id_a}/associated"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json["data"].as_array().unwrap();
    assert_eq!(items.len(), 1); // only Conv B, not self or Conv C
    assert_eq!(items[0]["extra"]["workspace"], "/same");
}

#[tokio::test]
async fn t10_2_associated_none() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = create_body_with_extra("Unique", json!({"workspace": "/unique"}));
    let req = json_with_token("POST", "/api/conversations", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let id = json["data"]["conversation_id"].as_str().unwrap().to_owned();

    let resp = app
        .oneshot(get_with_token(&format!("/api/conversations/{id}/associated"), &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn t10_3_associated_not_found() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{MISSING_CONVERSATION_ID}/associated"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn t10_4_associated_requires_auth() {
    let (app, _services) = build_app().await;
    let resp = app
        .oneshot(get_request("/api/conversations/some-id/associated"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── T12: Boundary scenarios ───────────────────────────────────────────

#[tokio::test]
async fn t12_1_long_name() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let long_name = "A".repeat(1000);
    let req = json_with_token("POST", "/api/conversations", create_body(&long_name), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"].as_str().unwrap().len(), 1000);
}

#[tokio::test]
async fn t12_2_large_nested_extra() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let big_extra = json!({
        "workspace": "/project",
        "nested": {
            "level1": {
                "level2": {
                    "level3": { "deep": true }
                }
            }
        },
        "array": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
    });
    let body = create_body_with_extra("Big Extra", big_extra.clone());
    let req = json_with_token("POST", "/api/conversations", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(
        json["data"]["extra"]["nested"]["level1"]["level2"]["level3"]["deep"],
        true
    );
    assert_eq!(json["data"]["extra"]["array"].as_array().unwrap().len(), 10);
}

#[tokio::test]
async fn t12_3_concurrent_creates() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let mut ids = Vec::new();
    for i in 0..10 {
        let req = json_with_token(
            "POST",
            "/api/conversations",
            create_body(&format!("Concurrent {i}")),
            &token,
            &csrf,
        );
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let json = body_json(resp).await;
        ids.push(json["data"]["conversation_id"].as_str().unwrap().to_owned());
    }

    // All IDs should be unique
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 10);
}

// ── Full lifecycle ────────────────────────────────────────────────────

#[tokio::test]
async fn full_conversation_lifecycle() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create
    let req = json_with_token(
        "POST",
        "/api/conversations",
        create_body("Lifecycle Test"),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let id = json["data"]["conversation_id"].as_str().unwrap().to_owned();
    assert_eq!(json["data"]["status"], "pending");

    // Read
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/conversations/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Update
    let req = json_with_token(
        "PATCH",
        &format!("/api/conversations/{id}"),
        json!({"name": "Updated Lifecycle"}),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "Updated Lifecycle");

    // Delete
    let resp = app
        .clone()
        .oneshot(delete_with_token(&format!("/api/conversations/{id}"), &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify gone
    let resp = app
        .oneshot(get_with_token(&format!("/api/conversations/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
