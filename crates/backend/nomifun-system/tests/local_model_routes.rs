use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use nomifun_db::{
    IProviderRepository, SqliteClientPreferenceRepository, SqliteModelProfileRepository,
    SqliteProviderRepository, SqliteSettingsRepository, init_database_memory,
};
use nomifun_system::{
    ClientPrefService, LocalModelServer, ModelFetchService, ModelProfileService,
    ProtocolDetectionService, ProviderService, SettingsService, SystemRouterState,
    VersionCheckService, start_and_provision_local_model, system_routes,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const TEST_KEY: [u8; 32] = [0x55; 32];

async fn setup() -> (
    axum::Router,
    nomifun_db::Database,
    LocalModelServer,
    TempDir,
) {
    let temp = TempDir::new().unwrap();
    let db = init_database_memory().await.unwrap();
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let (local, server) =
        start_and_provision_local_model(temp.path(), provider_repo.clone(), TEST_KEY)
            .await
            .unwrap();
    let http = reqwest::Client::new();
    let state = SystemRouterState {
        settings_service: SettingsService::new(Arc::new(SqliteSettingsRepository::new(
            db.pool().clone(),
        ))),
        client_pref_service: ClientPrefService::new(Arc::new(
            SqliteClientPreferenceRepository::new(db.pool().clone()),
        )),
        provider_service: ProviderService::new(provider_repo.clone(), TEST_KEY),
        model_fetch_service: ModelFetchService::new(provider_repo, TEST_KEY, http.clone()),
        model_profile_service: ModelProfileService::new(Arc::new(
            SqliteModelProfileRepository::new(db.pool().clone()),
        )),
        managed_model_service: None,
        local_model_service: Some(local),
        protocol_detection_service: ProtocolDetectionService::new(http.clone()),
        version_check_service: VersionCheckService::new(http, "0.1.0".into()),
        data_dir: temp.path().to_path_buf(),
    };
    (system_routes(state), db, server, temp)
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
async fn catalog_status_and_reserved_provider_are_ready_without_downloads() {
    let (app, db, server, _temp) = setup().await;
    let catalog = app
        .clone()
        .oneshot(request("GET", "/api/model-services/local/catalog", None))
        .await
        .unwrap();
    assert_eq!(catalog.status(), StatusCode::OK);
    let catalog = json_body(catalog).await;
    assert_eq!(catalog["data"].as_array().unwrap().len(), 3);
    assert_eq!(
        catalog["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["recommended"] == true)
            .count(),
        1
    );
    assert!(catalog["data"][0].get("downloadUrl").is_none());

    let status = app
        .oneshot(request("GET", "/api/model-services/local/status", None))
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let status = json_body(status).await;
    assert_eq!(status["data"]["kind"], "local");
    assert_eq!(status["data"]["providerId"], "nomifun-local-model");
    assert_eq!(status["data"]["enabled"], false);
    assert_eq!(status["data"]["activeModelId"], Value::Null);
    assert_eq!(status["data"]["models"].as_array().unwrap().len(), 3);

    let repo = SqliteProviderRepository::new(db.pool().clone());
    let row = repo
        .find_by_id("nomifun-local-model")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.platform, "nomifun-local-model");
    assert!(!row.enabled);
    assert_eq!(row.models, "[]");
    assert_eq!(row.base_url, server.base_url());
}

#[tokio::test]
async fn invalid_mutations_fail_without_starting_a_download() {
    let (app, _db, _server, _temp) = setup().await;
    let unknown = app
        .clone()
        .oneshot(request(
            "POST",
            "/api/model-services/local/models/not-in-catalog/install",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    let not_installed = app
        .clone()
        .oneshot(request(
            "POST",
            "/api/model-services/local/models/qwen3-0.6b-q4-k-m/activate",
            Some(json!({"enabled": true})),
        ))
        .await
        .unwrap();
    assert_eq!(not_installed.status(), StatusCode::CONFLICT);

    let nothing_to_cancel = app
        .oneshot(request(
            "POST",
            "/api/model-services/local/models/qwen3-0.6b-q4-k-m/cancel",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(nothing_to_cancel.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn local_openai_facade_requires_token_and_hides_inactive_models() {
    let (_app, _db, server, _temp) = setup().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let unauthorized = client
        .get(format!("{}/models", server.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    let response = client
        .get(format!("{}/models", server.base_url()))
        .bearer_auth(server.auth_token())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["object"], "list");
    assert!(body["data"].as_array().unwrap().is_empty());
}
