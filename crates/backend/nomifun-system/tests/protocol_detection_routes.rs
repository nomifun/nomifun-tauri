//! Black-box integration tests for protocol detection endpoint.
//!
//! Uses `wiremock` to mock remote API responses and tests the full
//! HTTP flow: request -> handler -> remote API probe -> response.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use nomifun_db::{
    SqliteClientPreferenceRepository, SqliteProviderRepository, SqliteSettingsRepository, init_database_memory,
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
    let http_client = reqwest::Client::builder().no_proxy().build().unwrap();
    SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(db.pool().clone()))),
        client_pref_service: ClientPrefService::new(Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()))),
        provider_service: ProviderService::new(provider_repo.clone(), TEST_KEY),
        model_fetch_service: ModelFetchService::new(provider_repo, TEST_KEY, http_client.clone()),
        model_profile_service: nomifun_system::ModelProfileService::new(std::sync::Arc::new(
            nomifun_db::SqliteModelProfileRepository::new(db.pool().clone()),
        )),
        managed_model_service: None,
        local_model_service: None,
        protocol_detection_service: ProtocolDetectionService::new(http_client.clone()),
        version_check_service: VersionCheckService::new(http_client, "0.1.0".to_owned()),
        data_dir: std::env::temp_dir(),
    }
}

async fn setup() -> axum::Router {
    let db = init_database_memory().await.unwrap();
    let state = build_state(&db);
    system_routes(state)
}

async fn detect(router: &axum::Router, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/providers/detect-protocol")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detect_protocol_missing_base_url() {
    let router = setup().await;
    let (status, json) = detect(&router, json!({"api_key": "sk-xxx"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!json["success"].as_bool().unwrap());
}

#[tokio::test]
async fn detect_protocol_missing_api_key() {
    let router = setup().await;
    let (status, json) = detect(&router, json!({"base_url": "https://example.com"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!json["success"].as_bool().unwrap());
}

#[tokio::test]
async fn detect_protocol_empty_base_url() {
    let router = setup().await;
    let (status, _) = detect(&router, json!({"base_url": "  ", "api_key": "sk-test"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn detect_protocol_empty_api_key() {
    let router = setup().await;
    let (status, _) = detect(&router, json!({"base_url": "https://example.com", "api_key": "  "})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// OpenAI detection with mock server
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detect_openai_protocol_success() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("Authorization", "Bearer sk-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "gpt-4"},
                {"id": "gpt-3.5-turbo"}
            ]
        })))
        .mount(&mock_server)
        .await;

    let router = setup().await;
    let (status, json) = detect(
        &router,
        json!({
            "base_url": mock_server.uri(),
            "api_key": "sk-test-key"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(json["success"].as_bool().unwrap());

    let data = &json["data"];
    assert_eq!(data["protocol"], "openai");
    assert!(data["confidence"].as_u64().unwrap() > 0);
    let models = data["models"].as_array().unwrap();
    assert!(models.contains(&json!("gpt-4")));
    assert!(models.contains(&json!("gpt-3.5-turbo")));
    assert_eq!(data["suggestion"]["type"], "none");
}

// ---------------------------------------------------------------------------
// Anthropic detection with mock server
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detect_anthropic_protocol_success() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "sk-ant-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "claude-sonnet-4-20250514"},
                {"id": "claude-opus-4-20250514"}
            ]
        })))
        .mount(&mock_server)
        .await;

    let router = setup().await;
    let (status, json) = detect(
        &router,
        json!({
            "base_url": mock_server.uri(),
            "api_key": "sk-ant-test-key",
            "preferred_protocol": "anthropic"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let data = &json["data"];
    assert_eq!(data["protocol"], "anthropic");
    assert!(data["confidence"].as_u64().unwrap() >= 90);
    let models = data["models"].as_array().unwrap();
    assert!(models.contains(&json!("claude-sonnet-4-20250514")));
}

// ---------------------------------------------------------------------------
// Gemini detection with mock server
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detect_gemini_protocol_success() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .and(query_param("key", "AIzaSyBtest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {"name": "models/gemini-2.5-pro"},
                {"name": "models/gemini-2.5-flash"}
            ]
        })))
        .mount(&mock_server)
        .await;

    let router = setup().await;
    let (status, json) = detect(
        &router,
        json!({
            "base_url": mock_server.uri(),
            "api_key": "AIzaSyBtest",
            "preferred_protocol": "gemini"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let data = &json["data"];
    assert_eq!(data["protocol"], "gemini");
    assert!(data["confidence"].as_u64().unwrap() >= 80);
    let models = data["models"].as_array().unwrap();
    // Prefix stripped
    assert!(models.contains(&json!("gemini-2.5-pro")));
    assert!(models.contains(&json!("gemini-2.5-flash")));
}

// ---------------------------------------------------------------------------
// All protocols fail → unknown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detect_protocol_all_fail_returns_unknown() {
    let mock_server = MockServer::start().await;

    // All endpoints return 404
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock_server)
        .await;

    let router = setup().await;
    let (status, json) = detect(
        &router,
        json!({
            "base_url": mock_server.uri(),
            "api_key": "sk-unknown-key"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let data = &json["data"];
    assert_eq!(data["protocol"], "unknown");
    assert_eq!(data["confidence"], 0);
    assert_eq!(data["suggestion"]["type"], "check_key");
}

// ---------------------------------------------------------------------------
// Auth failure detection (401)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detect_protocol_auth_failure_returns_check_key() {
    let mock_server = MockServer::start().await;

    // OpenAI endpoint returns 401
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"message": "Invalid API key"}
        })))
        .mount(&mock_server)
        .await;

    // /v1/models also returns 401
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&mock_server)
        .await;

    let router = setup().await;
    let (status, json) = detect(
        &router,
        json!({
            "base_url": mock_server.uri(),
            "api_key": "invalid-key"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let data = &json["data"];
    // Should detect a protocol (OpenAI likely) with check_key suggestion
    assert!(data["confidence"].as_u64().unwrap() > 0);
    assert_eq!(data["suggestion"]["type"], "check_key");
}

// ---------------------------------------------------------------------------
// URL fix variant detection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detect_openai_via_v1_variant() {
    let mock_server = MockServer::start().await;

    // /models returns 404
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock_server)
        .await;

    // /v1/models returns success
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-4"}]
        })))
        .mount(&mock_server)
        .await;

    let router = setup().await;
    let (status, json) = detect(
        &router,
        json!({
            "base_url": mock_server.uri(),
            "api_key": "sk-test"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let data = &json["data"];
    assert_eq!(data["protocol"], "openai");
    // fixed_base_url should be set when using /v1 variant
    assert!(data["fixed_base_url"].is_string());
    assert!(data["fixed_base_url"].as_str().unwrap().ends_with("/v1"));
}

// ---------------------------------------------------------------------------
// Multi-key testing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detect_with_multi_key_test() {
    let mock_server = MockServer::start().await;

    // /models returns success for any key
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-4"}]
        })))
        .mount(&mock_server)
        .await;

    let router = setup().await;
    let (status, json) = detect(
        &router,
        json!({
            "base_url": mock_server.uri(),
            "api_key": "key1,key2,key3",
            "test_all_keys": true
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let data = &json["data"];
    assert_eq!(data["protocol"], "openai");

    let mkr = &data["multi_key_result"];
    assert_eq!(mkr["total"], 3);
    assert_eq!(mkr["details"].as_array().unwrap().len(), 3);
    // All keys should be valid (mock returns 200 for any key)
    assert_eq!(mkr["valid"], 3);
    assert_eq!(mkr["invalid"], 0);
}

// ---------------------------------------------------------------------------
// Multi-key partial validity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detect_multi_key_partial_validity() {
    let mock_server = MockServer::start().await;

    // /models returns success only for "good-key"
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("Authorization", "Bearer good-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-4"}]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("Authorization", "Bearer bad-key"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&mock_server)
        .await;

    let router = setup().await;
    let (status, json) = detect(
        &router,
        json!({
            "base_url": mock_server.uri(),
            "api_key": "good-key,bad-key",
            "test_all_keys": true
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let data = &json["data"];
    let mkr = &data["multi_key_result"];
    assert_eq!(mkr["total"], 2);
    assert_eq!(mkr["valid"], 1);
    assert_eq!(mkr["invalid"], 1);

    // Verify details are sorted by index
    let details = mkr["details"].as_array().unwrap();
    assert_eq!(details[0]["index"], 0);
    assert!(details[0]["valid"].as_bool().unwrap());
    assert_eq!(details[1]["index"], 1);
    assert!(!details[1]["valid"].as_bool().unwrap());
}

// ---------------------------------------------------------------------------
// Preferred protocol takes priority
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preferred_protocol_tested_first() {
    let mock_server = MockServer::start().await;

    // Only Anthropic endpoint works
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "claude-3"}]
        })))
        .mount(&mock_server)
        .await;

    let router = setup().await;
    let (status, json) = detect(
        &router,
        json!({
            "base_url": mock_server.uri(),
            "api_key": "test-key",
            "preferred_protocol": "anthropic"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let data = &json["data"];
    assert_eq!(data["protocol"], "anthropic");
}

// ---------------------------------------------------------------------------
// Single key → no multiKeyResult
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_key_no_multi_key_result() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-4"}]
        })))
        .mount(&mock_server)
        .await;

    let router = setup().await;
    let (status, json) = detect(
        &router,
        json!({
            "base_url": mock_server.uri(),
            "api_key": "sk-single-key",
            "test_all_keys": true
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let data = &json["data"];
    // Single key → multi_key_result should be null
    assert!(data["multi_key_result"].is_null());
}

// ---------------------------------------------------------------------------
// Timeout configuration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detect_protocol_with_custom_timeout() {
    let mock_server = MockServer::start().await;

    // Return success after a small delay (well within timeout)
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "gpt-4"}]}))
                .set_delay(std::time::Duration::from_millis(50)),
        )
        .mount(&mock_server)
        .await;

    let router = setup().await;
    let (status, json) = detect(
        &router,
        json!({
            "base_url": mock_server.uri(),
            "api_key": "sk-test",
            "timeout": 5000
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"]["protocol"], "openai");
}

// ---------------------------------------------------------------------------
// Multi-protocol detection (detectedProtocols)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detect_multiple_protocols_reports_all_successes() {
    let mock_server = MockServer::start().await;

    // OpenAI probe: GET /models with Authorization header
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("Authorization", "Bearer multi-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "gpt-4"},
                {"id": "gpt-3.5-turbo"}
            ]
        })))
        .mount(&mock_server)
        .await;

    // Anthropic probe: GET /v1/models with x-api-key header
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "multi-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "claude-sonnet-4-20250514"},
                {"id": "claude-opus-4-20250514"}
            ]
        })))
        .mount(&mock_server)
        .await;

    let router = setup().await;
    let (status, json) = detect(
        &router,
        json!({
            "base_url": mock_server.uri(),
            "api_key": "multi-test-key"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(json["success"].as_bool().unwrap());

    let data = &json["data"];
    // Primary result is still the first successful probe in priority order (OpenAI)
    assert_eq!(data["protocol"], "openai");
    assert_eq!(data["success"], true);

    // detectedProtocols contains both OpenAI and Anthropic
    let detected = data["detectedProtocols"].as_array().unwrap();
    assert!(
        detected.len() >= 2,
        "Expected at least 2 detected protocols, got {}",
        detected.len()
    );

    let protocols: Vec<&str> = detected.iter().map(|d| d["protocol"].as_str().unwrap()).collect();
    assert!(protocols.contains(&"openai"), "Expected openai in detectedProtocols");
    assert!(
        protocols.contains(&"anthropic"),
        "Expected anthropic in detectedProtocols"
    );

    // Each entry should have confidence > 0
    for entry in detected {
        assert!(entry["confidence"].as_u64().unwrap() > 0);
    }
}
