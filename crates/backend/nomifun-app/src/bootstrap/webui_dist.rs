use std::{fs, path::Path};

use anyhow::{Context, ensure};
use serde::Deserialize;

pub const UI_BUILD_MANIFEST_FILE: &str = "nomifun-build.json";
pub const UI_BUILD_MANIFEST_SCHEMA: u32 = 1;

const UI_API_CONTRACT_VERSION_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../ui-api-contract-version.txt"
));

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBuildManifest {
    pub schema: u32,
    pub app_version: String,
    pub api_contract_version: u32,
    pub frontend_build_id: String,
}

pub fn validate_webui_dist(
    dist_dir: &Path,
    expected_app_version: &str,
    expected_build_id: Option<&str>,
) -> anyhow::Result<UiBuildManifest> {
    let index_path = dist_dir.join("index.html");
    ensure!(
        index_path.is_file(),
        "WebUI entry point is missing at {}. Run `bun run build:ui` and restart the backend.",
        index_path.display()
    );

    let manifest_path = dist_dir.join(UI_BUILD_MANIFEST_FILE);
    let manifest_json = fs::read_to_string(&manifest_path).with_context(|| {
        format!(
            "WebUI build manifest is missing or unreadable at {}. Run `bun run build:ui` and restart the backend.",
            manifest_path.display()
        )
    })?;
    let manifest: UiBuildManifest = serde_json::from_str(&manifest_json).with_context(|| {
        format!(
            "WebUI build manifest at {} is invalid or uses an unsupported legacy shape. Run `bun run build:ui` and restart the backend.",
            manifest_path.display()
        )
    })?;

    ensure!(
        manifest.schema == UI_BUILD_MANIFEST_SCHEMA,
        "WebUI manifest schema mismatch at {}: expected {}, found {}. Run `bun run build:ui` and restart the backend.",
        manifest_path.display(),
        UI_BUILD_MANIFEST_SCHEMA,
        manifest.schema
    );
    ensure!(
        manifest.app_version == expected_app_version,
        "WebUI app_version mismatch at {}: backend expects {:?}, manifest contains {:?}. Run `bun run build:ui` and restart the backend.",
        manifest_path.display(),
        expected_app_version,
        manifest.app_version
    );

    let expected_contract_version = ui_api_contract_version();
    ensure!(
        manifest.api_contract_version == expected_contract_version,
        "WebUI api_contract_version mismatch at {}: backend expects {}, manifest contains {}. Run `bun run build:ui` and restart the backend.",
        manifest_path.display(),
        expected_contract_version,
        manifest.api_contract_version
    );
    ensure!(
        !manifest.frontend_build_id.trim().is_empty(),
        "WebUI frontend_build_id is blank at {}. Run `bun run build:ui` and restart the backend.",
        manifest_path.display()
    );

    if let Some(expected_build_id) = expected_build_id {
        ensure!(
            manifest.frontend_build_id == expected_build_id,
            "WebUI frontend_build_id mismatch at {}: backend expects {:?}, manifest contains {:?}. Run `bun run build:ui` and restart the backend.",
            manifest_path.display(),
            expected_build_id,
            manifest.frontend_build_id
        );
    }

    Ok(manifest)
}

pub fn ui_api_contract_version() -> u32 {
    UI_API_CONTRACT_VERSION_SOURCE
        .trim()
        .parse::<u32>()
        .expect("ui-api-contract-version.txt must contain one unsigned integer")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;

    const EXPECTED_APP_VERSION: &str = "0.2.20";

    fn valid_manifest() -> Value {
        json!({
            "schema": UI_BUILD_MANIFEST_SCHEMA,
            "app_version": EXPECTED_APP_VERSION,
            "api_contract_version": ui_api_contract_version(),
            "frontend_build_id": "frontend-sha256-abc123"
        })
    }

    fn dist_with_manifest(manifest: &Value) -> TempDir {
        let dist = TempDir::new().expect("create temp dist");
        fs::write(dist.path().join("index.html"), "<!doctype html>")
            .expect("write index.html");
        fs::write(
            dist.path().join(UI_BUILD_MANIFEST_FILE),
            serde_json::to_vec(manifest).expect("serialize manifest"),
        )
        .expect("write build manifest");
        dist
    }

    fn validation_error(dist: &Path) -> String {
        validate_webui_dist(dist, EXPECTED_APP_VERSION, None)
            .expect_err("distribution should be rejected")
            .to_string()
    }

    #[test]
    fn accepts_a_complete_matching_distribution() {
        let dist = dist_with_manifest(&valid_manifest());

        let manifest = validate_webui_dist(dist.path(), EXPECTED_APP_VERSION, None)
            .expect("matching distribution should be accepted");

        assert_eq!(manifest.schema, UI_BUILD_MANIFEST_SCHEMA);
        assert_eq!(manifest.app_version, EXPECTED_APP_VERSION);
        assert_eq!(
            manifest.api_contract_version,
            ui_api_contract_version()
        );
        assert_eq!(manifest.frontend_build_id, "frontend-sha256-abc123");
    }

    #[test]
    fn rejects_a_distribution_without_index_html() {
        let dist = TempDir::new().expect("create temp dist");
        fs::write(
            dist.path().join(UI_BUILD_MANIFEST_FILE),
            serde_json::to_vec(&valid_manifest()).expect("serialize manifest"),
        )
        .expect("write build manifest");

        let error = validation_error(dist.path());

        assert!(error.contains("index.html"), "unexpected error: {error}");
        assert!(error.contains("build:ui"), "unexpected error: {error}");
    }

    #[test]
    fn rejects_a_distribution_without_a_build_manifest() {
        let dist = TempDir::new().expect("create temp dist");
        fs::write(dist.path().join("index.html"), "<!doctype html>")
            .expect("write index.html");

        let error = validation_error(dist.path());

        assert!(
            error.contains(UI_BUILD_MANIFEST_FILE),
            "unexpected error: {error}"
        );
        assert!(error.contains("build:ui"), "unexpected error: {error}");
    }

    #[test]
    fn rejects_malformed_build_manifest_json() {
        let dist = TempDir::new().expect("create temp dist");
        fs::write(dist.path().join("index.html"), "<!doctype html>")
            .expect("write index.html");
        fs::write(
            dist.path().join(UI_BUILD_MANIFEST_FILE),
            b"{not-json",
        )
        .expect("write malformed manifest");

        let error = validation_error(dist.path());

        assert!(error.contains("invalid"), "unexpected error: {error}");
        assert!(error.contains("build:ui"), "unexpected error: {error}");
    }

    #[test]
    fn rejects_legacy_or_partial_manifest_shape() {
        let dist = dist_with_manifest(&json!({
            "schema": UI_BUILD_MANIFEST_SCHEMA,
            "app_version": EXPECTED_APP_VERSION
        }));

        let error = validation_error(dist.path());

        assert!(error.contains("invalid"), "unexpected error: {error}");
        assert!(error.contains("build:ui"), "unexpected error: {error}");
    }

    #[test]
    fn rejects_unknown_manifest_fields() {
        let mut manifest = valid_manifest();
        manifest["legacy_version"] = json!("0.1.0");
        let dist = dist_with_manifest(&manifest);

        let error = validation_error(dist.path());

        assert!(error.contains("invalid"), "unexpected error: {error}");
        assert!(error.contains("build:ui"), "unexpected error: {error}");
    }

    #[test]
    fn rejects_an_unsupported_manifest_schema() {
        let mut manifest = valid_manifest();
        manifest["schema"] = json!(UI_BUILD_MANIFEST_SCHEMA + 1);
        let dist = dist_with_manifest(&manifest);

        let error = validation_error(dist.path());

        assert!(error.contains("schema"), "unexpected error: {error}");
        assert!(
            error.contains(&UI_BUILD_MANIFEST_SCHEMA.to_string()),
            "unexpected error: {error}"
        );
        assert!(error.contains("build:ui"), "unexpected error: {error}");
    }

    #[test]
    fn rejects_an_app_version_mismatch() {
        let mut manifest = valid_manifest();
        manifest["app_version"] = json!("0.2.19");
        let dist = dist_with_manifest(&manifest);

        let error = validation_error(dist.path());

        assert!(error.contains("app_version"), "unexpected error: {error}");
        assert!(error.contains("0.2.19"), "unexpected error: {error}");
        assert!(
            error.contains(EXPECTED_APP_VERSION),
            "unexpected error: {error}"
        );
        assert!(error.contains("build:ui"), "unexpected error: {error}");
    }

    #[test]
    fn rejects_an_api_contract_version_mismatch() {
        let mut manifest = valid_manifest();
        manifest["api_contract_version"] = json!(ui_api_contract_version() + 1);
        let dist = dist_with_manifest(&manifest);

        let error = validation_error(dist.path());

        assert!(
            error.contains("api_contract_version"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains(&ui_api_contract_version().to_string()),
            "unexpected error: {error}"
        );
        assert!(error.contains("build:ui"), "unexpected error: {error}");
    }

    #[test]
    fn rejects_a_blank_frontend_build_id() {
        let mut manifest = valid_manifest();
        manifest["frontend_build_id"] = json!("   ");
        let dist = dist_with_manifest(&manifest);

        let error = validation_error(dist.path());

        assert!(
            error.contains("frontend_build_id"),
            "unexpected error: {error}"
        );
        assert!(error.contains("build:ui"), "unexpected error: {error}");
    }

    #[test]
    fn rejects_a_frontend_build_id_mismatch_when_an_exact_id_is_expected() {
        let dist = dist_with_manifest(&valid_manifest());

        let error = validate_webui_dist(
            dist.path(),
            EXPECTED_APP_VERSION,
            Some("frontend-sha256-different"),
        )
        .expect_err("mismatched frontend build id should be rejected")
        .to_string();

        assert!(
            error.contains("frontend_build_id"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains("frontend-sha256-abc123"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains("frontend-sha256-different"),
            "unexpected error: {error}"
        );
        assert!(error.contains("build:ui"), "unexpected error: {error}");
    }

    #[test]
    fn shared_api_contract_version_is_loaded_from_the_root_file() {
        let source_version = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../ui-api-contract-version.txt"
        ))
        .trim()
        .parse::<u32>()
        .expect("root API contract version must be an integer");

        assert_eq!(ui_api_contract_version(), source_version);
    }
}
