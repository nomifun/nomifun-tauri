use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use nomifun_db::{
    SqliteClientPreferenceRepository, SqliteModelProfileRepository,
    SqliteProviderRepository, SqliteSettingsRepository, init_database_memory,
};
use nomifun_system::{
    ClientPrefService, LazyLocalModelRuntime, ModelFetchService,
    ModelProfileService, ProtocolDetectionService, ProviderService,
    SettingsService, SystemRouterState, VersionCheckService, system_routes,
};
use tempfile::TempDir;
use tower::ServiceExt;

const TEST_KEY: [u8; 32] = [0x4a; 32];

async fn setup() -> (axum::Router, Arc<LazyLocalModelRuntime>, TempDir) {
    let temp = TempDir::new().unwrap();
    let db = init_database_memory().await.unwrap();
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let profile_repo = Arc::new(SqliteModelProfileRepository::new(db.pool().clone()));
    let lazy = LazyLocalModelRuntime::new(
        temp.path(),
        provider_repo.clone(),
        profile_repo.clone(),
        TEST_KEY,
    );
    let http = reqwest::Client::new();
    let state = SystemRouterState {
        settings_service: SettingsService::new(Arc::new(
            SqliteSettingsRepository::new(db.pool().clone()),
        )),
        client_pref_service: ClientPrefService::new(Arc::new(
            SqliteClientPreferenceRepository::new(db.pool().clone()),
        )),
        provider_service: ProviderService::new(provider_repo.clone(), TEST_KEY),
        model_fetch_service: ModelFetchService::new(provider_repo, TEST_KEY, http.clone()),
        model_profile_service: ModelProfileService::new(profile_repo),
        managed_model_service: None,
        local_model_service: None,
        image_model_service: None,
        asr_model_service: None,
        lazy_local_model_runtime: Some(lazy.clone()),
        protocol_detection_service: ProtocolDetectionService::new(http.clone()),
        version_check_service: VersionCheckService::new(http, "0.1.0".into()),
        data_dir: temp.path().to_path_buf(),
    };
    (system_routes(state), lazy, temp)
}

async fn json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn fresh_catalog_and_status_are_side_effect_free() {
    let (app, lazy, temp) = setup().await;
    let catalog = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/model-services/local/asr/catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(catalog.status(), StatusCode::OK);
    let catalog = json(catalog).await;
    assert_eq!(catalog["data"].as_array().unwrap().len(), 3);
    assert_eq!(catalog["data"][0]["id"], "funasr-paraformer-zh-q8");
    assert_eq!(catalog["data"][0]["engine"], "fun_asr_llama_cpp");
    assert!(catalog["data"][0].get("downloadUrl").is_none());

    let status = app
        .oneshot(
            Request::builder()
                .uri("/api/model-services/local/asr/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let status = json(status).await;
    assert_eq!(status["data"]["enabled"], false);
    assert_eq!(status["data"]["activeModelId"], serde_json::Value::Null);
    assert!(!lazy.is_asr_started());
    assert!(!lazy.is_started());
    assert!(!temp.path().join("local-ai").exists());
}

#[tokio::test]
async fn install_route_initializes_only_asr_control_plane() {
    let (app, lazy, _temp) = setup().await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/model-services/local/asr/models/not-in-catalog/install")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(lazy.is_asr_started());
    assert!(!lazy.is_started());
}
