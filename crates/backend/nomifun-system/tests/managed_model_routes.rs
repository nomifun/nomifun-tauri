use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use nomifun_db::{
    SqliteClientPreferenceRepository, SqliteModelProfileRepository,
    SqliteProviderRepository, SqliteSettingsRepository, init_database_memory,
};
use nomifun_system::{
    ClientPrefService, LocalModelServer, ManagedModelServer, ModelFetchService,
    ModelProfileService, ProtocolDetectionService, ProviderService, SettingsService,
    SystemRouterState, VersionCheckService, start_and_provision_free_model,
    start_and_provision_local_model, system_routes,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const TEST_KEY: [u8; 32] = [0x42; 32];
static NEXT_DATA_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDataDir(PathBuf);

impl TestDataDir {
    fn new() -> Self {
        let suffix = NEXT_DATA_DIR.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nomifun-managed-model-routes-{}-{suffix}",
            std::process::id()
        )))
    }
}

impl Drop for TestDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn setup() -> (
    axum::Router,
    nomifun_db::Database,
    ManagedModelServer,
    LocalModelServer,
    TestDataDir,
) {
    let db = init_database_memory().await.unwrap();
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let (managed, server) =
        start_and_provision_free_model(provider_repo.clone(), TEST_KEY)
            .await
            .unwrap();
    let data_dir = TestDataDir::new();
    let (local, local_server) =
        start_and_provision_local_model(&data_dir.0, provider_repo.clone(), TEST_KEY)
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
        local_model_service: Some(local),
        image_model_service: None,
        asr_model_service: None,
        lazy_local_model_runtime: None,
        protocol_detection_service: ProtocolDetectionService::new(http.clone()),
        version_check_service: VersionCheckService::new(http, "0.1.0".into()),
        data_dir: data_dir.0.clone(),
    };
    (system_routes(state), db, server, local_server, data_dir)
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
async fn free_and_local_status_match_wire_contracts() {
    let (app, _db, _server, _local_server, _data_dir) = setup().await;
    let free = app
        .clone()
        .oneshot(request("GET", "/api/model-services/free/status", None))
        .await
        .unwrap();
    assert_eq!(free.status(), StatusCode::OK);
    let free = json_body(free).await;
    assert_eq!(free["data"]["kind"], "free");
    assert_eq!(free["data"]["providerId"], "nomifun-free-model");
    assert_eq!(free["data"]["protocolVersion"], "1");
    assert!(free["data"]["models"].as_array().is_some_and(|m| !m.is_empty()));

    let catalog = app
        .clone()
        .oneshot(request("GET", "/api/model-services/local/catalog", None))
        .await
        .unwrap();
    assert_eq!(catalog.status(), StatusCode::OK);
    let catalog = json_body(catalog).await;
    assert_eq!(catalog["data"].as_array().unwrap().len(), 2);
    assert_eq!(catalog["data"][0]["quantization"], "Q4_K_M");
    assert_eq!(catalog["data"][0]["parameterSize"], "4B");
    assert_eq!(catalog["data"][0]["contextWindow"], 65_536);
    assert!(catalog["data"][0].get("downloadSizeBytes").is_some());

    let local = app
        .oneshot(request("GET", "/api/model-services/local/status", None))
        .await
        .unwrap();
    assert_eq!(local.status(), StatusCode::OK);
    let local = json_body(local).await;
    assert_eq!(local["data"]["kind"], "local");
    assert_eq!(local["data"]["providerId"], "nomifun-local-model");
    assert_eq!(local["data"]["enabled"], false);
    assert_eq!(local["data"]["ready"], false);
    assert!(local["data"]["activeModelId"].is_null());
    assert_eq!(local["data"]["models"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn activate_and_model_patch_return_latest_status() {
    let (app, _db, _server, _local_server, _data_dir) = setup().await;
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
    let (app, _db, _server, _local_server, _data_dir) = setup().await;
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
    let (app, _db, _server, _local_server, _data_dir) = setup().await;
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

#[tokio::test]
async fn local_model_mutations_reject_unknown_catalog_id() {
    let (app, _db, _server, _local_server, _data_dir) = setup().await;
    for (method, path, body) in [
        (
            "POST",
            "/api/model-services/local/models/not-in-catalog/install",
            None,
        ),
        (
            "POST",
            "/api/model-services/local/models/not-in-catalog/cancel",
            None,
        ),
        (
            "DELETE",
            "/api/model-services/local/models/not-in-catalog",
            None,
        ),
        (
            "POST",
            "/api/model-services/local/models/not-in-catalog/activate",
            Some(json!({"enabled": true})),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(request(method, path, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
    }
}
