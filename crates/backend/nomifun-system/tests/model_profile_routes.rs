//! Black-box integration tests for the model-profile routes (multimodal model hub).
//! Exercises upsert -> list -> resolve -> delete over HTTP via `oneshot`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use nomifun_db::{
    SqliteClientPreferenceRepository, SqliteModelProfileRepository, SqliteProviderRepository,
    SqliteSettingsRepository, init_database_memory,
};
use nomifun_system::{
    ClientPrefService, ModelFetchService, ModelProfileService, ProtocolDetectionService,
    ProviderService, SettingsService, SystemRouterState, VersionCheckService, system_routes,
};

const TEST_KEY: [u8; 32] = [0x42; 32];

fn build_state(db: &nomifun_db::Database) -> SystemRouterState {
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let http_client = reqwest::Client::new();
    SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(db.pool().clone()))),
        client_pref_service: ClientPrefService::new(Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()))),
        provider_service: ProviderService::new(provider_repo.clone(), TEST_KEY),
        model_fetch_service: ModelFetchService::new(provider_repo, TEST_KEY, http_client.clone()),
        model_profile_service: ModelProfileService::new(Arc::new(SqliteModelProfileRepository::new(db.pool().clone()))),
        managed_model_service: None,
        protocol_detection_service: ProtocolDetectionService::new(http_client.clone()),
        version_check_service: VersionCheckService::new(http_client, "0.1.0".to_owned()),
        data_dir: std::env::temp_dir(),
    }
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn get_request(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

/// Create a StepFun Step Plan provider carrying the image model, return its id.
async fn create_stepfun_plan_provider(db: &nomifun_db::Database) -> String {
    let app = system_routes(build_state(db));
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/providers",
            json!({
                "platform": "stepfun-plan",
                "name": "StepFun Step Plan",
                "base_url": "https://api.stepfun.com/step_plan/v1",
                "api_key": "sk-test",
                "models": ["step-image-edit-2"]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp).await;
    v["data"]["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn upsert_list_resolve_delete_model_profile() {
    let db = init_database_memory().await.unwrap();
    let provider_id = create_stepfun_plan_provider(&db).await;

    // Upsert an explicit image profile (source=user).
    let resp = system_routes(build_state(&db))
        .oneshot(json_request(
            "POST",
            "/api/model-profiles",
            json!({
                "provider_id": provider_id,
                "model": "step-image-edit-2",
                "tasks": ["image_generation", "image_edit"],
                "traits": [],
                "params": { "steps": 8 }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["data"]["source"], "user");
    assert_eq!(v["data"]["tasks"][0], "image_generation");

    // List includes it.
    let resp = system_routes(build_state(&db)).oneshot(get_request("/api/model-profiles")).await.unwrap();
    let v = body_json(resp).await;
    assert_eq!(v["data"].as_array().unwrap().len(), 1);

    // Resolve by image_generation returns the model.
    let resp = system_routes(build_state(&db))
        .oneshot(json_request("POST", "/api/model-profiles/resolve", json!({ "task": "image_generation" })))
        .await
        .unwrap();
    let v = body_json(resp).await;
    let models = v["data"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["model"], "step-image-edit-2");
    assert_eq!(models[0]["provider_id"], provider_id);

    // Resolve by chat returns nothing (it's an image model).
    let resp = system_routes(build_state(&db))
        .oneshot(json_request("POST", "/api/model-profiles/resolve", json!({ "task": "chat" })))
        .await
        .unwrap();
    let v = body_json(resp).await;
    assert!(v["data"]["models"].as_array().unwrap().is_empty());

    // Delete.
    let resp = system_routes(build_state(&db))
        .oneshot(json_request(
            "POST",
            "/api/model-profiles/delete",
            json!({ "provider_id": provider_id, "model": "step-image-edit-2" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = system_routes(build_state(&db)).oneshot(get_request("/api/model-profiles")).await.unwrap();
    let v = body_json(resp).await;
    assert!(v["data"].as_array().unwrap().is_empty());
}
