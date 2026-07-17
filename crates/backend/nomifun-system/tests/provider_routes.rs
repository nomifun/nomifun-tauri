//! Black-box integration tests for provider CRUD routes.
//!
//! Tests exercise the HTTP layer (request -> handler -> response) via
//! `tower::ServiceExt::oneshot`, without authentication middleware.
//! Auth protection is verified at the app-level E2E tests (task 3.9).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use nomifun_common::ProviderId;
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

const TEST_ENCRYPTION_KEY: [u8; 32] = [0x42; 32];

fn build_state(db: &nomifun_db::Database) -> SystemRouterState {
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let http_client = reqwest::Client::new();
    SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(db.pool().clone()))),
        client_pref_service: ClientPrefService::new(Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()))),
        provider_service: ProviderService::new(provider_repo.clone(), TEST_ENCRYPTION_KEY),
        model_fetch_service: ModelFetchService::new(provider_repo, TEST_ENCRYPTION_KEY, http_client.clone()),
        model_profile_service: nomifun_system::ModelProfileService::new(std::sync::Arc::new(
            nomifun_db::SqliteModelProfileRepository::new(db.pool().clone()),
        )),
        managed_model_service: None,
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

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn get_request(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn delete_request(uri: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn sample_create_body() -> serde_json::Value {
    json!({
        "platform": "anthropic",
        "name": "Anthropic",
        "base_url": "https://api.anthropic.com",
        "api_key": "sk-ant-api03-test1234"
    })
}

/// Create a provider and return (response_json, provider_id, fresh_router).
async fn create_one(db: &nomifun_db::Database) -> (serde_json::Value, String) {
    let app = system_routes(build_state(db));
    let resp = app
        .oneshot(json_request("POST", "/api/providers", sample_create_body()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let id = json["data"]["id"].as_str().unwrap().to_string();
    (json, id)
}

// ===========================================================================
// GET /api/providers — list
// ===========================================================================

#[tokio::test]
async fn list_providers_empty() {
    let (app, _db) = setup().await;
    let resp = app.oneshot(get_request("/api/providers")).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"], json!([]));
}

#[tokio::test]
async fn list_providers_returns_plaintext_api_key() {
    let (_app, db) = setup().await;
    create_one(&db).await;

    let app2 = system_routes(build_state(&db));
    let resp = app2.oneshot(get_request("/api/providers")).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let providers = json["data"].as_array().unwrap();
    assert_eq!(providers.len(), 1);

    let api_key = providers[0]["api_key"].as_str().unwrap();
    // Pre-launch: api_key is returned plaintext on the wire (encrypted at rest).
    assert_eq!(api_key, "sk-ant-api03-test1234");
    assert!(!api_key.contains("***"));
}

#[tokio::test]
async fn provider_sort_order_round_trips_create_update_and_list() {
    let (_app, db) = setup().await;

    let first_body = json!({
        "platform": "openai",
        "name": "Low Priority",
        "base_url": "https://api.openai.com",
        "api_key": "sk-low",
        "sort_order": 10
    });
    let second_body = json!({
        "platform": "anthropic",
        "name": "High Priority",
        "base_url": "https://api.anthropic.com",
        "api_key": "sk-high",
        "sort_order": 0
    });

    let app1 = system_routes(build_state(&db));
    let first_resp = app1
        .oneshot(json_request("POST", "/api/providers", first_body))
        .await
        .unwrap();
    assert_eq!(first_resp.status(), StatusCode::CREATED);
    let first_json = body_json(first_resp).await;
    let first_id = first_json["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(first_json["data"]["sort_order"], 10);

    let app2 = system_routes(build_state(&db));
    let second_resp = app2
        .oneshot(json_request("POST", "/api/providers", second_body))
        .await
        .unwrap();
    assert_eq!(second_resp.status(), StatusCode::CREATED);
    let second_json = body_json(second_resp).await;
    let second_id = second_json["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(second_json["data"]["sort_order"], 0);

    let app3 = system_routes(build_state(&db));
    let list_resp = app3.oneshot(get_request("/api/providers")).await.unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_json = body_json(list_resp).await;
    let providers = list_json["data"].as_array().unwrap();
    assert_eq!(providers[0]["id"], second_id);
    assert_eq!(providers[0]["sort_order"], 0);
    assert_eq!(providers[1]["id"], first_id);
    assert_eq!(providers[1]["sort_order"], 10);

    let app4 = system_routes(build_state(&db));
    let update_resp = app4
        .oneshot(json_request("PUT", &format!("/api/providers/{second_id}"), json!({"sort_order": 20})))
        .await
        .unwrap();
    assert_eq!(update_resp.status(), StatusCode::OK);
    let update_json = body_json(update_resp).await;
    assert_eq!(update_json["data"]["sort_order"], 20);

    let app5 = system_routes(build_state(&db));
    let relist_resp = app5.oneshot(get_request("/api/providers")).await.unwrap();
    let relist_json = body_json(relist_resp).await;
    let providers = relist_json["data"].as_array().unwrap();
    assert_eq!(providers[0]["id"], first_id);
    assert_eq!(providers[0]["sort_order"], 10);
    assert_eq!(providers[1]["id"], second_id);
    assert_eq!(providers[1]["sort_order"], 20);
}

#[tokio::test]
async fn provider_sort_order_rejects_negative_values() {
    let (_app, db) = setup().await;

    let create_resp = system_routes(build_state(&db))
        .oneshot(json_request(
            "POST",
            "/api/providers",
            json!({
                "platform": "openai",
                "name": "Invalid Priority",
                "base_url": "https://api.openai.com",
                "api_key": "sk-test",
                "sort_order": -1
            }),
        ))
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::BAD_REQUEST);

    let create_valid = system_routes(build_state(&db))
        .oneshot(json_request(
            "POST",
            "/api/providers",
            json!({
                "platform": "openai",
                "name": "Valid Priority",
                "base_url": "https://api.openai.com",
                "api_key": "sk-test",
                "sort_order": 0
            }),
        ))
        .await
        .unwrap();
    assert_eq!(create_valid.status(), StatusCode::CREATED);
    let created_json = body_json(create_valid).await;
    let id = created_json["data"]["id"].as_str().unwrap();

    let update_resp = system_routes(build_state(&db))
        .oneshot(json_request("PUT", &format!("/api/providers/{id}"), json!({"sort_order": -1})))
        .await
        .unwrap();
    assert_eq!(update_resp.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// POST /api/providers — create
// ===========================================================================

#[tokio::test]
async fn create_provider_success() {
    let (app, _db) = setup().await;
    let resp = app
        .oneshot(json_request("POST", "/api/providers", sample_create_body()))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);

    let data = &json["data"];
    assert!(data["id"].as_str().unwrap().starts_with("prov_"));
    assert_eq!(data["platform"], "anthropic");
    assert_eq!(data["name"], "Anthropic");
    assert_eq!(data["base_url"], "https://api.anthropic.com");
    assert_eq!(data["api_key"], "sk-ant-api03-test1234");
    assert!(data["enabled"].as_bool().unwrap());
    assert!(data["models"].as_array().unwrap().is_empty());
    assert!(data["created_at"].as_i64().unwrap() > 0);
    assert!(data["updated_at"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn create_provider_with_supplied_id() {
    let (app, _db) = setup().await;
    let provider_id = ProviderId::new().into_string();
    let body = json!({
        "id": provider_id.clone(),
        "platform": "openai",
        "name": "OpenAI",
        "base_url": "https://api.openai.com",
        "api_key": "sk-test",
        "model_enabled": {"gpt-4": true, "gpt-3.5": false}
    });
    let resp = app.oneshot(json_request("POST", "/api/providers", body)).await.unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let data = &json["data"];
    assert_eq!(data["id"], provider_id);
    assert_eq!(data["api_key"], "sk-test");
    assert_eq!(data["model_enabled"]["gpt-4"], true);
    assert_eq!(data["model_enabled"]["gpt-3.5"], false);
}

#[tokio::test]
async fn create_provider_with_duplicate_id_returns_conflict() {
    let (_app, db) = setup().await;
    let provider_id = ProviderId::new().into_string();
    let body = json!({
        "id": provider_id.clone(),
        "platform": "openai",
        "name": "OpenAI",
        "base_url": "https://api.openai.com",
        "api_key": "sk-test"
    });

    let app1 = system_routes(build_state(&db));
    let resp = app1
        .oneshot(json_request("POST", "/api/providers", body.clone()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let app2 = system_routes(build_state(&db));
    let resp = app2
        .oneshot(json_request("POST", "/api/providers", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn create_provider_with_invalid_id_rejected() {
    let (app, _db) = setup().await;
    let body = json!({
        "id": "bad/slash",
        "platform": "openai",
        "name": "OpenAI",
        "base_url": "https://api.openai.com",
        "api_key": "sk-test"
    });
    let resp = app.oneshot(json_request("POST", "/api/providers", body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_provider_with_optional_fields() {
    let (app, _db) = setup().await;
    let body = json!({
        "platform": "bedrock",
        "name": "AWS Bedrock",
        "base_url": "https://bedrock.us-east-1.amazonaws.com",
        "api_key": "test-key-abcd",
        "models": ["anthropic.claude-3-sonnet"],
        "enabled": false,
        "capabilities": [{"type": "text"}, {"type": "vision", "is_user_selected": true}],
        "context_limit": 200000,
        "bedrock_config": {
            "auth_method": "accessKey",
            "region": "us-east-1",
            "access_key_id": "AKIA...",
            "secret_access_key": "secret"
        }
    });

    let resp = app.oneshot(json_request("POST", "/api/providers", body)).await.unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let data = &json["data"];
    assert!(!data["enabled"].as_bool().unwrap());
    assert_eq!(data["models"].as_array().unwrap().len(), 1);
    assert_eq!(data["capabilities"].as_array().unwrap().len(), 2);
    assert_eq!(data["context_limit"], 200000);
    assert_eq!(data["bedrock_config"]["auth_method"], "accessKey");
    assert_eq!(data["bedrock_config"]["region"], "us-east-1");
}

#[tokio::test]
async fn create_provider_missing_platform() {
    let (app, _db) = setup().await;
    let body = json!({
        "name": "Test",
        "base_url": "https://api.example.com",
        "api_key": "sk-test"
    });
    let resp = app.oneshot(json_request("POST", "/api/providers", body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_provider_missing_name() {
    let (app, _db) = setup().await;
    let body = json!({
        "platform": "openai",
        "base_url": "https://api.example.com",
        "api_key": "sk-test"
    });
    let resp = app.oneshot(json_request("POST", "/api/providers", body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_provider_missing_base_url() {
    let (app, _db) = setup().await;
    let body = json!({
        "platform": "openai",
        "name": "Test",
        "api_key": "sk-test"
    });
    let resp = app.oneshot(json_request("POST", "/api/providers", body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_provider_missing_api_key() {
    let (app, _db) = setup().await;
    let body = json!({
        "platform": "openai",
        "name": "Test",
        "base_url": "https://api.example.com"
    });
    let resp = app.oneshot(json_request("POST", "/api/providers", body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_provider_invalid_url() {
    let (app, _db) = setup().await;
    let body = json!({
        "platform": "openai",
        "name": "Test",
        "base_url": "not-a-url",
        "api_key": "sk-test"
    });
    let resp = app.oneshot(json_request("POST", "/api/providers", body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// PUT /api/providers/{id} — update
// ===========================================================================

#[tokio::test]
async fn update_provider_name() {
    let (_app, db) = setup().await;
    let (_, id) = create_one(&db).await;

    let app2 = system_routes(build_state(&db));
    let resp = app2
        .oneshot(json_request(
            "PUT",
            &format!("/api/providers/{id}"),
            json!({"name": "New Name"}),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "New Name");
    assert_eq!(json["data"]["platform"], "anthropic");
}

#[tokio::test]
async fn update_provider_api_key_returns_plaintext() {
    let (_app, db) = setup().await;
    let (_, id) = create_one(&db).await;

    let app2 = system_routes(build_state(&db));
    let resp = app2
        .oneshot(json_request(
            "PUT",
            &format!("/api/providers/{id}"),
            json!({"api_key": "new-key-abcdefgh"}),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let api_key = json["data"]["api_key"].as_str().unwrap();
    assert_eq!(api_key, "new-key-abcdefgh");
}

#[tokio::test]
async fn update_provider_nonexistent() {
    let (app, _db) = setup().await;
    let provider_id = ProviderId::new().into_string();
    let resp = app
        .oneshot(json_request("PUT", &format!("/api/providers/{provider_id}"), json!({"name": "X"})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// DELETE /api/providers/{id}
// ===========================================================================

#[tokio::test]
async fn delete_provider_success() {
    let (_app, db) = setup().await;
    let (_, id) = create_one(&db).await;

    let app2 = system_routes(build_state(&db));
    let resp = app2
        .oneshot(delete_request(&format!("/api/providers/{id}")))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
}

#[tokio::test]
async fn delete_provider_then_list_excludes_deleted() {
    let (_app, db) = setup().await;
    let (_, id) = create_one(&db).await;

    let app2 = system_routes(build_state(&db));
    let resp = app2
        .oneshot(delete_request(&format!("/api/providers/{id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app3 = system_routes(build_state(&db));
    let resp = app3.oneshot(get_request("/api/providers")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"], json!([]));
}

#[tokio::test]
async fn delete_provider_nonexistent() {
    let (app, _db) = setup().await;
    let provider_id = ProviderId::new().into_string();
    let resp = app
        .oneshot(delete_request(&format!("/api/providers/{provider_id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// Full CRUD flow
// ===========================================================================

#[tokio::test]
async fn full_crud_flow() {
    let (_app, db) = setup().await;

    // 1. Create
    let (create_json, id) = create_one(&db).await;
    assert_eq!(create_json["data"]["platform"], "anthropic");

    // 2. List — should contain one
    let app2 = system_routes(build_state(&db));
    let resp = app2.oneshot(get_request("/api/providers")).await.unwrap();
    let list_json = body_json(resp).await;
    assert_eq!(list_json["data"].as_array().unwrap().len(), 1);

    // 3. Update
    let app3 = system_routes(build_state(&db));
    let resp = app3
        .oneshot(json_request(
            "PUT",
            &format!("/api/providers/{id}"),
            json!({"name": "Updated", "enabled": false}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let update_json = body_json(resp).await;
    assert_eq!(update_json["data"]["name"], "Updated");
    assert!(!update_json["data"]["enabled"].as_bool().unwrap());

    // 4. Verify update via list
    let app4 = system_routes(build_state(&db));
    let resp = app4.oneshot(get_request("/api/providers")).await.unwrap();
    let list_json = body_json(resp).await;
    assert_eq!(list_json["data"][0]["name"], "Updated");

    // 5. Delete
    let app5 = system_routes(build_state(&db));
    let resp = app5
        .oneshot(delete_request(&format!("/api/providers/{id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 6. Verify deleted
    let app6 = system_routes(build_state(&db));
    let resp = app6.oneshot(get_request("/api/providers")).await.unwrap();
    let list_json = body_json(resp).await;
    assert_eq!(list_json["data"], json!([]));
}
