use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use nomifun_common::ProviderId;
use nomifun_db::{
    SqliteClientPreferenceRepository, SqliteModelProfileRepository,
    SqliteProviderRepository, SqliteSettingsRepository, init_database_memory,
};
use nomifun_system::{
    ClientPrefService, ManagedModelServer, ModelFetchService,
    ModelProfileService, ProtocolDetectionService, ProviderService, SettingsService,
    SystemRouterState, VersionCheckService, start_and_provision_free_model,
    system_routes,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const TEST_KEY: [u8; 32] = [0x42; 32];
async fn setup() -> (
    axum::Router,
    nomifun_db::Database,
    ManagedModelServer,
) {
    let db = init_database_memory().await.unwrap();
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let (managed, server) =
        start_and_provision_free_model(provider_repo.clone(), TEST_KEY)
            .await
            .unwrap();
    let http = reqwest::Client::new();
    let state = SystemRouterState {
        settings_service: SettingsService::new(Arc::new(
            SqliteSettingsRepository::new(db.pool().clone()),
        )),
        client_pref_service: ClientPrefService::new(Arc::new(
            SqliteClientPreferenceRepository::new(db.pool().clone()),
        )),
        provider_service: ProviderService::new(provider_repo.clone(), TEST_KEY),
        model_fetch_service: ModelFetchService::new(
            provider_repo,
            TEST_KEY,
            http.clone(),
        ),
        model_profile_service: ModelProfileService::new(Arc::new(
            SqliteModelProfileRepository::new(db.pool().clone()),
        )),
        managed_model_service: Some(managed),
        protocol_detection_service: ProtocolDetectionService::new(http.clone()),
        version_check_service: VersionCheckService::new(http, "0.1.0".into()),
        data_dir: std::env::temp_dir(),
    };
    (system_routes(state), db, server)
}

fn request(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn free_status_matches_wire_contract() {
    let (app, _db, _server) = setup().await;
    let free = app
        .clone()
        .oneshot(request("GET", "/api/model-services/free/status", None))
        .await
        .unwrap();
    assert_eq!(free.status(), StatusCode::OK);
    let free = json_body(free).await;
    ProviderId::parse(free["data"]["providerId"].as_str().unwrap()).unwrap();
    assert_eq!(free["data"]["protocolVersion"], "1");
    assert!(free["data"]["models"].as_array().is_some_and(|m| !m.is_empty()));

}

#[tokio::test]
async fn activate_and_model_patch_return_latest_status() {
    let (app, _db, _server) = setup().await;
    let disabled = app
        .clone()
        .oneshot(request(
            "POST",
            "/api/model-services/free/activate",
            Some(json!({"enabled": false})),
        ))
        .await
        .unwrap();
    assert_eq!(disabled.status(), StatusCode::OK);
    assert_eq!(json_body(disabled).await["data"]["enabled"], false);

    let enabled = app
        .clone()
        .oneshot(request(
            "POST",
            "/api/model-services/free/activate",
            Some(json!({"enabled": true})),
        ))
        .await
        .unwrap();
    assert_eq!(enabled.status(), StatusCode::OK);
    assert_eq!(json_body(enabled).await["data"]["enabled"], true);

    let patched = app
        .oneshot(request(
            "PATCH",
            "/api/model-services/free/models/big-pickle",
            Some(json!({"enabled": false})),
        ))
        .await
        .unwrap();
    assert_eq!(patched.status(), StatusCode::OK);
    let patched = json_body(patched).await;
    let big_pickle = patched["data"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["id"] == "big-pickle")
        .unwrap();
    assert_eq!(big_pickle["enabled"], false);
}

#[tokio::test]
async fn disabled_model_health_route_returns_safe_unknown_and_snapshot() {
    let (app, _db, _server) = setup().await;
    let patched = app
        .clone()
        .oneshot(request(
            "PATCH",
            "/api/model-services/free/models/big-pickle",
            Some(json!({"enabled": false})),
        ))
        .await
        .unwrap();
    assert_eq!(patched.status(), StatusCode::OK);

    let checked = app
        .clone()
        .oneshot(request(
            "POST",
            "/api/model-services/free/models/big-pickle/health",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(checked.status(), StatusCode::OK);
    let checked = json_body(checked).await;
    assert_eq!(checked["data"]["modelId"], "big-pickle");
    assert_eq!(checked["data"]["status"], "unknown");
    assert_eq!(checked["data"]["errorKind"], "model_disabled");
    assert!(checked["data"]["checkedAt"].as_i64().is_some());

    let snapshot = app
        .oneshot(request(
            "GET",
            "/api/model-services/free/health",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(snapshot.status(), StatusCode::OK);
    let snapshot = json_body(snapshot).await;
    assert_eq!(snapshot["data"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["data"][0]["modelId"], "big-pickle");
}

#[tokio::test]
async fn health_route_rejects_unknown_model() {
    let (app, _db, _server) = setup().await;
    let response = app
        .oneshot(request(
            "POST",
            "/api/model-services/free/models/not-in-list/health",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
