//! Provider CRUD, model fetch, and protocol detection tests with auth.

mod common;

use axum::http::StatusCode;
use nomifun_common::ProviderId;
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{header as match_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{body_json, build_app, delete_with_token, get_with_token, json_with_token, setup_and_login};

// ===========================================================================
// Provider CRUD
// ===========================================================================

#[tokio::test]
async fn provider_full_crud_with_auth() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // 1. List — empty
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/providers", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let providers = json["data"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    ProviderId::parse(providers[0]["provider_id"].as_str().unwrap()).unwrap();
    assert_eq!(providers[0]["platform"], "nomifun-free-model");

    // 2. Create
    let req = json_with_token(
        "POST",
        "/api/providers",
        json!({
            "platform": "anthropic",
            "name": "Anthropic",
            "base_url": "https://api.anthropic.com",
            "api_key": "sk-ant-api03-test1234"
        }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let id = json["data"]["provider_id"].as_str().unwrap().to_string();
    assert_eq!(json["data"]["platform"], "anthropic");
    assert_eq!(json["data"]["name"], "Anthropic");
    let api_key = json["data"]["api_key"].as_str().unwrap();
    assert_eq!(
        api_key, "sk-ant-api03-test1234",
        "API key should be plaintext on the wire (pre-launch)"
    );

    // 3. List — should contain one
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/providers", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let providers = json["data"].as_array().unwrap();
    assert_eq!(providers.len(), 2);
    assert!(
        providers
            .iter()
            .any(|provider| provider["platform"] == "nomifun-free-model")
    );
    assert!(
        providers
            .iter()
            .any(|provider| provider["provider_id"].as_str() == Some(id.as_str()))
    );

    // 4. Update
    let req = json_with_token(
        "PUT",
        &format!("/api/providers/{id}"),
        json!({"name": "Updated Name", "enabled": false}),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "Updated Name");
    assert!(!json["data"]["enabled"].as_bool().unwrap());

    // 5. Delete
    let resp = app
        .clone()
        .oneshot(delete_with_token(&format!("/api/providers/{id}"), &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 6. Verify deleted
    let resp = app.oneshot(get_with_token("/api/providers", &token)).await.unwrap();
    let json = body_json(resp).await;
    let providers = json["data"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    ProviderId::parse(providers[0]["provider_id"].as_str().unwrap()).unwrap();
    assert_eq!(providers[0]["platform"], "nomifun-free-model");
}

#[tokio::test]
async fn provider_create_validation_with_auth() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Missing platform
    let req = json_with_token(
        "POST",
        "/api/providers",
        json!({"name": "Test", "base_url": "https://api.example.com", "api_key": "sk-test"}),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Invalid URL
    let req = json_with_token(
        "POST",
        "/api/providers",
        json!({"platform": "openai", "name": "Test", "base_url": "not-a-url", "api_key": "sk-test"}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn provider_update_nonexistent_with_auth() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let provider_id = ProviderId::new().into_string();
    let req = json_with_token(
        "PUT",
        &format!("/api/providers/{provider_id}"),
        json!({"name": "X"}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn provider_delete_nonexistent_with_auth() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let provider_id = ProviderId::new().into_string();
    let resp = app
        .oneshot(delete_with_token(
            &format!("/api/providers/{provider_id}"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// Model fetch
// ===========================================================================

#[tokio::test]
async fn model_fetch_openai_with_auth() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-4o"}, {"id": "gpt-4o-mini"}]
        })))
        .mount(&mock_server)
        .await;

    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/providers",
        json!({
            "platform": "openai",
            "name": "OpenAI Mock",
            "base_url": mock_server.uri(),
            "api_key": "test-api-key"
        }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let id = json["data"]["provider_id"].as_str().unwrap().to_string();

    let req = json_with_token(
        "POST",
        &format!("/api/providers/{id}/models"),
        json!({"try_fix": false}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let models = json["data"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0], "gpt-4o");
}

#[tokio::test]
async fn model_fetch_nonexistent_provider_with_auth() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let provider_id = ProviderId::new().into_string();
    let req = json_with_token(
        "POST",
        &format!("/api/providers/{provider_id}/models"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// Protocol detection
// ===========================================================================

#[tokio::test]
async fn protocol_detect_openai_with_auth() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(match_header("Authorization", "Bearer sk-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-4"}, {"id": "gpt-3.5-turbo"}]
        })))
        .mount(&mock_server)
        .await;

    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/providers/detect-protocol",
        json!({
            "base_url": mock_server.uri(),
            "api_key": "sk-test-key"
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["protocol"], "openai");
    assert!(json["data"]["confidence"].as_u64().unwrap() > 0);
    let models = json["data"]["models"].as_array().unwrap();
    assert!(!models.is_empty());
}

#[tokio::test]
async fn protocol_detect_all_fail_with_auth() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock_server)
        .await;

    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/providers/detect-protocol",
        json!({
            "base_url": mock_server.uri(),
            "api_key": "sk-unknown"
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["protocol"], "unknown");
    assert_eq!(json["data"]["confidence"], 0);
}

#[tokio::test]
async fn protocol_detect_validation_with_auth() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Missing baseUrl
    let req = json_with_token(
        "POST",
        "/api/providers/detect-protocol",
        json!({"api_key": "sk-test"}),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Missing apiKey
    let req = json_with_token(
        "POST",
        "/api/providers/detect-protocol",
        json!({"base_url": "https://api.example.com"}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn protocol_detect_switch_platform_suggestion_with_auth() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-4"}]
        })))
        .mount(&mock_server)
        .await;

    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/providers/detect-protocol",
        json!({
            "base_url": mock_server.uri(),
            "api_key": "sk-test",
            "preferred_protocol": "anthropic"
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["protocol"], "openai");
    assert_eq!(json["data"]["suggestion"]["type"], "switch_platform");
}
