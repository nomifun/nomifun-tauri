use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use nomifun_db::{
    SqliteClientPreferenceRepository, SqliteModelProfileRepository, SqliteProviderRepository,
    SqliteSettingsRepository, init_database_memory,
};
use nomifun_system::{
    ClientPrefService, ImageModelService, ModelFetchService, ModelProfileService,
    ProtocolDetectionService, ProviderService, SettingsService, SystemRouterState,
    VersionCheckService, system_routes,
};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

const TEST_KEY: [u8; 32] = [0x72; 32];
const Z_IMAGE_TURBO_MODEL_ID: &str = "z-image-turbo-q3-k";

async fn setup() -> (axum::Router, TempDir) {
    let temp = TempDir::new().unwrap();
    let db = init_database_memory().await.unwrap();
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let http = reqwest::Client::new();
    let image = ImageModelService::new(temp.path()).await.unwrap();
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
        local_model_service: None,
        image_model_service: Some(image),
        asr_model_service: None,
        lazy_local_model_runtime: None,
        protocol_detection_service: ProtocolDetectionService::new(http.clone()),
        version_check_service: VersionCheckService::new(http, "0.1.0".into()),
        data_dir: temp.path().to_path_buf(),
    };
    (system_routes(state), temp)
}

fn request(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn files_below(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

#[tokio::test]
async fn startup_and_read_only_routes_never_download_artifacts() {
    let (app, temp) = setup().await;

    for uri in [
        "/api/model-services/local/image/catalog",
        "/api/model-services/local/image/status",
    ] {
        let response = app.clone().oneshot(request("GET", uri)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let artifact_files = files_below(temp.path())
        .into_iter()
        .filter(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            name.ends_with(".part")
                || name.ends_with(".gguf")
                || name.ends_with(".safetensors")
                || name.ends_with(".zip")
                || name == "sd-cli"
                || name == "sd-cli.exe"
        })
        .collect::<Vec<_>>();
    assert!(artifact_files.is_empty(), "unexpected artifacts: {artifact_files:?}");
}

#[tokio::test]
async fn catalog_exposes_only_safe_user_facing_metadata() {
    let (app, _temp) = setup().await;
    let response = app
        .oneshot(request("GET", "/api/model-services/local/image/catalog"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    let entry = &body["data"][0];
    assert_eq!(entry["id"], Z_IMAGE_TURBO_MODEL_ID);
    assert!(entry["downloadSizeBytes"].as_u64().unwrap() > 5_976_144_612);
    assert_eq!(
        entry["components"],
        serde_json::json!(["runtime", "diffusion_model", "text_encoder", "vae"])
    );

    let serialized = serde_json::to_string(entry).unwrap();
    for forbidden in [
        "downloadUrl",
        "sha256",
        "revision",
        "localPath",
        "fileName",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "catalog leaked internal field {forbidden}"
        );
    }
}

#[tokio::test]
async fn invalid_lifecycle_mutations_fail_without_network_work() {
    let (app, temp) = setup().await;
    let unknown = app
        .clone()
        .oneshot(request(
            "POST",
            "/api/model-services/local/image/models/not-in-catalog/install",
        ))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    let pause = app
        .clone()
        .oneshot(request(
            "POST",
            "/api/model-services/local/image/models/z-image-turbo-q3-k/pause",
        ))
        .await
        .unwrap();
    assert_eq!(pause.status(), StatusCode::CONFLICT);

    let resume = app
        .clone()
        .oneshot(request(
            "POST",
            "/api/model-services/local/image/models/z-image-turbo-q3-k/resume",
        ))
        .await
        .unwrap();
    assert_eq!(resume.status(), StatusCode::CONFLICT);

    let delete = app
        .oneshot(request(
            "DELETE",
            "/api/model-services/local/image/models/z-image-turbo-q3-k",
        ))
        .await
        .unwrap();
    assert_eq!(delete.status(), StatusCode::OK);
    let body = json_body(delete).await;
    assert_eq!(body["data"]["models"][0]["installPhase"], "not_installed");

    assert!(
        files_below(temp.path()).iter().all(|path| {
            !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".part"))
        }),
        "invalid lifecycle calls must not begin a transfer"
    );
}
