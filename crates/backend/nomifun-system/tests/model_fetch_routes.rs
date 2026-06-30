//! Black-box integration tests for model fetch endpoint.
//!
//! Uses `wiremock` to mock remote API responses and tests the full
//! HTTP flow: request -> handler -> remote API call -> response.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use nomifun_common::encrypt_string;
use nomifun_db::{
    CreateProviderParams, IProviderRepository, SqliteClientPreferenceRepository, SqliteProviderRepository,
    SqliteSettingsRepository, init_database_memory,
};
use nomifun_system::{
    ClientPrefService, ModelFetchService, ProtocolDetectionService, ProviderService, SettingsService,
    SystemRouterState, VersionCheckService, system_routes,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const TEST_KEY: [u8; 32] = [0x42; 32];

fn build_state(db: &nomifun_db::Database) -> SystemRouterState {
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let http_client = reqwest::Client::new();
    SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(db.pool().clone()))),
        client_pref_service: ClientPrefService::new(Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()))),
        provider_service: ProviderService::new(provider_repo.clone(), TEST_KEY),
        model_fetch_service: ModelFetchService::new(provider_repo, TEST_KEY, http_client.clone()),
        protocol_detection_service: ProtocolDetectionService::new(http_client.clone()),
        version_check_service: VersionCheckService::new(http_client, "0.1.0".to_owned()),
        data_dir: std::env::temp_dir(),
    }
}

async fn setup() -> (axum::Router, nomifun_db::Database) {
    let db = init_database_memory().await.unwrap();
    let state = build_state(&db);
    (system_routes(state), db)
}

async fn create_provider(db: &nomifun_db::Database, platform: &str, base_url: &str, api_key: &str) -> String {
    let repo = SqliteProviderRepository::new(db.pool().clone());
    let encrypted = encrypt_string(api_key, &TEST_KEY).unwrap();
    let row = repo
        .create(CreateProviderParams {
            id: None,
            platform,
            name: "Test Provider",
            base_url,
            api_key_encrypted: &encrypted,
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
        .unwrap();
    row.id
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn post_request(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests: basic flow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_models_nonexistent_provider() {
    let (router, _db) = setup().await;
    let req = post_request("/api/providers/nonexistent/models", json!({"try_fix": false}));
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn fetch_models_vertex_ai_hardcoded() {
    let (router, db) = setup().await;
    let id = create_provider(&db, "vertex-ai", "https://unused", "fake-key").await;
    let req = post_request(&format!("/api/providers/{id}/models"), json!({"try_fix": false}));
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let models = json["data"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0], "gemini-2.5-pro");
    assert_eq!(models[1], "gemini-2.5-flash");
    assert!(json["data"].get("fixed_base_url").is_none());
}

#[tokio::test]
async fn fetch_models_minimax_hardcoded() {
    let (router, db) = setup().await;
    let id = create_provider(&db, "minimax", "https://unused", "fake-key").await;
    let req = post_request(&format!("/api/providers/{id}/models"), json!({}));
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["models"].as_array().unwrap().len(), 3);
}

// ---------------------------------------------------------------------------
// Tests: OpenAI-compatible with mock
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_models_openai_compatible_success() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("Authorization", "Bearer test-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "gpt-4o", "object": "model"},
                {"id": "gpt-4o-mini", "object": "model"}
            ]
        })))
        .mount(&mock_server)
        .await;

    let (router, db) = setup().await;
    let id = create_provider(&db, "openai", &mock_server.uri(), "test-api-key").await;

    let req = post_request(&format!("/api/providers/{id}/models"), json!({"try_fix": false}));
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let models = json["data"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0], "gpt-4o");
    assert_eq!(models[1], "gpt-4o-mini");
}

#[tokio::test]
async fn fetch_models_openai_remote_error() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock_server)
        .await;

    let (router, db) = setup().await;
    let id = create_provider(&db, "openai", &mock_server.uri(), "test-key").await;

    let req = post_request(&format!("/api/providers/{id}/models"), json!({"try_fix": false}));
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

// ---------------------------------------------------------------------------
// Tests: Anthropic with mock
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_models_anthropic_success() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "sk-ant-test"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "claude-sonnet-4-20250514", "type": "model"},
                {"id": "claude-opus-4-20250514", "type": "model"}
            ],
            "has_more": false
        })))
        .mount(&mock_server)
        .await;

    let (router, db) = setup().await;
    let id = create_provider(&db, "anthropic", &mock_server.uri(), "sk-ant-test").await;

    let req = post_request(&format!("/api/providers/{id}/models"), json!({}));
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let models = json["data"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0], "claude-sonnet-4-20250514");
}

#[tokio::test]
async fn fetch_models_anthropic_fallback_on_error() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&mock_server)
        .await;

    let (router, db) = setup().await;
    let id = create_provider(&db, "anthropic", &mock_server.uri(), "bad-key").await;

    let req = post_request(&format!("/api/providers/{id}/models"), json!({}));
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let models = json["data"]["models"].as_array().unwrap();
    // Should return fallback models
    assert!(!models.is_empty());
}

// ---------------------------------------------------------------------------
// Tests: Gemini with mock
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_models_gemini_success() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {"name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro"},
                {"name": "models/gemini-2.5-flash", "displayName": "Gemini 2.5 Flash"}
            ]
        })))
        .mount(&mock_server)
        .await;

    let (router, db) = setup().await;
    let id = create_provider(&db, "gemini", &mock_server.uri(), "gemini-key").await;

    let req = post_request(&format!("/api/providers/{id}/models"), json!({}));
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let models = json["data"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    // models/ prefix should be stripped
    assert_eq!(models[0], "gemini-2.5-pro");
    assert_eq!(models[1], "gemini-2.5-flash");
}

#[tokio::test]
async fn fetch_models_gemini_fallback_on_error() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&mock_server)
        .await;

    let (router, db) = setup().await;
    let id = create_provider(&db, "gemini", &mock_server.uri(), "bad-key").await;

    let req = post_request(&format!("/api/providers/{id}/models"), json!({}));
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let models = json["data"]["models"].as_array().unwrap();
    assert!(!models.is_empty());
}

// ---------------------------------------------------------------------------
// Tests: new-api (OpenAI with /v1 enforcement)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_models_new_api_adds_v1() {
    let mock_server = MockServer::start().await;
    // new-api should ensure /v1 is in the path
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "model-a"}]
        })))
        .mount(&mock_server)
        .await;

    let (router, db) = setup().await;
    // base_url without /v1
    let id = create_provider(&db, "new-api", &mock_server.uri(), "test-key").await;

    let req = post_request(&format!("/api/providers/{id}/models"), json!({}));
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let models = json["data"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0], "model-a");
}

// ---------------------------------------------------------------------------
// Tests: URL auto-fix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_models_url_auto_fix_success() {
    let mock_server = MockServer::start().await;
    // Original /models should fail
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock_server)
        .await;
    // /v1/models should succeed
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "fixed-model"}]
        })))
        .mount(&mock_server)
        .await;

    let (router, db) = setup().await;
    let id = create_provider(&db, "openai", &mock_server.uri(), "test-key").await;

    let req = post_request(&format!("/api/providers/{id}/models"), json!({"try_fix": true}));
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let models = json["data"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0], "fixed-model");
    // fixedBaseUrl should be present
    assert!(json["data"]["fixed_base_url"].as_str().unwrap().contains("/v1"));
}

#[tokio::test]
async fn fetch_models_url_auto_fix_not_triggered_when_success() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "original-model"}]
        })))
        .mount(&mock_server)
        .await;

    let (router, db) = setup().await;
    let id = create_provider(&db, "openai", &mock_server.uri(), "test-key").await;

    let req = post_request(&format!("/api/providers/{id}/models"), json!({"try_fix": true}));
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let models = json["data"]["models"].as_array().unwrap();
    assert_eq!(models[0], "original-model");
    // fixedBaseUrl should NOT be present since original URL worked
    assert!(json["data"].get("fixed_base_url").is_none());
}

#[tokio::test]
async fn fetch_models_url_auto_fix_not_for_anthropic() {
    let mock_server = MockServer::start().await;
    // Anthropic API fails
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&mock_server)
        .await;

    let (router, db) = setup().await;
    let id = create_provider(&db, "anthropic", &mock_server.uri(), "bad-key").await;

    // Even with tryFix=true, Anthropic should use fallback, not URL fix
    let req = post_request(&format!("/api/providers/{id}/models"), json!({"try_fix": true}));
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    // Should be fallback models, no fixedBaseUrl
    assert!(json["data"].get("fixed_base_url").is_none());
}

// ---------------------------------------------------------------------------
// Tests: anonymous fetch-models (T1b)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_models_anonymous_returns_models_for_valid_input() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("Authorization", "Bearer sk-anon"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-4o"}, {"id": "gpt-4o-mini"}]
        })))
        .mount(&mock_server)
        .await;

    let (router, _db) = setup().await;
    let req = post_request(
        "/api/providers/fetch-models",
        json!({
            "platform": "openai",
            "base_url": mock_server.uri(),
            "api_key": "sk-anon"
        }),
    );
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let models = json["data"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0], "gpt-4o");
}

#[tokio::test]
async fn fetch_models_anonymous_rejects_empty_api_key() {
    let (router, _db) = setup().await;
    let req = post_request(
        "/api/providers/fetch-models",
        json!({
            "platform": "openai",
            "base_url": "https://api.openai.com",
            "api_key": "   "
        }),
    );
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn fetch_models_anonymous_minimax_hardcoded() {
    // Hardcoded-list platforms work without hitting any remote endpoint.
    let (router, _db) = setup().await;
    let req = post_request(
        "/api/providers/fetch-models",
        json!({
            "platform": "minimax",
            "base_url": "https://unused",
            "api_key": "fake"
        }),
    );
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["models"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn fetch_models_route_literal_segment_beats_id_shadowing() {
    // Regression guard for axum route ordering: POST /api/providers/fetch-models
    // must NOT be matched as /api/providers/{id}/models with id="fetch-models".
    // If shadowing occurred we'd either hit the by-id handler (→ 404 provider
    // not found) or get a routing error. Hitting the anonymous handler returns
    // 400 for missing required fields, which is the right signature.
    let (router, _db) = setup().await;
    let req = post_request("/api/providers/fetch-models", json!({}));
    let resp = router.oneshot(req).await.unwrap();
    // Missing "platform" / "base_url" / "api_key" — anonymous handler
    // rejects with 400 via JSON deserialization failure, not 404 from the
    // by-id handler.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
