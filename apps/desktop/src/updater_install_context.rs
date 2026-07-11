use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdaterInstallReason {
    AppBundleNotFound,
    AppTranslocation,
    MountedVolume,
    CrossDevice,
    MetadataUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallDecision {
    Supported,
    Unsupported(UpdaterInstallReason),
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdaterInstallContext {
    platform: &'static str,
    app_bundle_path: Option<String>,
    temp_dir: String,
    app_device_id: Option<u64>,
    temp_device_id: Option<u64>,
    auto_install_supported: bool,
    reason: Option<UpdaterInstallReason>,
}

fn classify_macos_install(
    app_bundle: Option<&Path>,
    app_device_id: Option<u64>,
    temp_device_id: Option<u64>,
) -> InstallDecision {
    let Some(app_bundle) = app_bundle else {
        return InstallDecision::Unsupported(UpdaterInstallReason::AppBundleNotFound);
    };
    let display = app_bundle.to_string_lossy();
    if display.contains("/AppTranslocation/") {
        return InstallDecision::Unsupported(UpdaterInstallReason::AppTranslocation);
    }
    if app_bundle.starts_with("/Volumes") {
        return InstallDecision::Unsupported(UpdaterInstallReason::MountedVolume);
    }
    match (app_device_id, temp_device_id) {
        (Some(app), Some(temp)) if app == temp => InstallDecision::Supported,
        (Some(_), Some(_)) => InstallDecision::Unsupported(UpdaterInstallReason::CrossDevice),
        _ => InstallDecision::Unsupported(UpdaterInstallReason::MetadataUnavailable),
    }
}

fn classify_install(
    platform: &str,
    app_bundle: Option<&Path>,
    app_device_id: Option<u64>,
    temp_device_id: Option<u64>,
) -> InstallDecision {
    if platform != "macos" {
        return InstallDecision::Supported;
    }
    classify_macos_install(app_bundle, app_device_id, temp_device_id)
}

fn app_bundle_from_executable(executable: &Path) -> Option<PathBuf> {
    executable
        .ancestors()
        .find(|path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("app")))
        .map(Path::to_path_buf)
}

#[cfg(target_os = "macos")]
fn device_id(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;

    std::fs::metadata(path).ok().map(|metadata| metadata.dev())
}

#[tauri::command]
pub fn get_updater_install_context() -> UpdaterInstallContext {
    let platform = std::env::consts::OS;
    let temp_dir = std::env::temp_dir();

    #[cfg(target_os = "macos")]
    let app_bundle = std::env::current_exe()
        .ok()
        .and_then(|executable| app_bundle_from_executable(&executable));
    #[cfg(target_os = "macos")]
    let app_device_id = app_bundle.as_deref().and_then(device_id);
    #[cfg(target_os = "macos")]
    let temp_device_id = device_id(&temp_dir);

    #[cfg(not(target_os = "macos"))]
    let app_bundle: Option<PathBuf> = None;
    #[cfg(not(target_os = "macos"))]
    let app_device_id = None;
    #[cfg(not(target_os = "macos"))]
    let temp_device_id = None;

    let decision = classify_install(platform, app_bundle.as_deref(), app_device_id, temp_device_id);
    let (auto_install_supported, reason) = match decision {
        InstallDecision::Supported => (true, None),
        InstallDecision::Unsupported(reason) => (false, Some(reason)),
    };
    let context = UpdaterInstallContext {
        platform,
        app_bundle_path: app_bundle.as_ref().map(|path| path.display().to_string()),
        temp_dir: temp_dir.display().to_string(),
        app_device_id,
        temp_device_id,
        auto_install_supported,
        reason,
    };

    if !context.auto_install_supported {
        tracing::warn!(
            reason = ?context.reason,
            app_bundle_path = ?context.app_bundle_path,
            temp_dir = %context.temp_dir,
            app_device_id = ?context.app_device_id,
            temp_device_id = ?context.temp_device_id,
            "macOS automatic update install blocked by unsafe application location"
        );
    }

    context
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(path: Option<&str>, app_dev: Option<u64>, temp_dev: Option<u64>) -> InstallDecision {
        classify_macos_install(path.map(Path::new), app_dev, temp_dev)
    }

    #[test]
    fn applications_bundle_on_same_device_is_supported() {
        assert_eq!(
            classify(Some("/Applications/NomiFun.app"), Some(7), Some(7)),
            InstallDecision::Supported,
        );
    }

    #[test]
    fn mounted_volume_is_rejected_before_device_comparison() {
        assert_eq!(
            classify(Some("/Volumes/NomiFun/NomiFun.app"), Some(9), Some(7)),
            InstallDecision::Unsupported(UpdaterInstallReason::MountedVolume),
        );
    }

    #[test]
    fn app_translocation_is_rejected() {
        assert_eq!(
            classify(
                Some("/private/var/folders/x/AppTranslocation/ABC/d/NomiFun.app"),
                Some(7),
                Some(7),
            ),
            InstallDecision::Unsupported(UpdaterInstallReason::AppTranslocation),
        );
    }

    #[test]
    fn different_devices_are_rejected() {
        assert_eq!(
            classify(Some("/Users/muri/Apps/NomiFun.app"), Some(9), Some(7)),
            InstallDecision::Unsupported(UpdaterInstallReason::CrossDevice),
        );
    }

    #[test]
    fn missing_bundle_or_metadata_fail_closed() {
        assert_eq!(
            classify(None, Some(7), Some(7)),
            InstallDecision::Unsupported(UpdaterInstallReason::AppBundleNotFound),
        );
        assert_eq!(
            classify(Some("/Applications/NomiFun.app"), None, Some(7)),
            InstallDecision::Unsupported(UpdaterInstallReason::MetadataUnavailable),
        );
    }

    #[test]
    fn non_macos_platforms_keep_existing_updater_support() {
        assert_eq!(classify_install("windows", None, None, None), InstallDecision::Supported);
        assert_eq!(classify_install("linux", None, None, None), InstallDecision::Supported);
    }

    #[test]
    fn finds_app_bundle_ancestor_from_executable() {
        assert_eq!(
            app_bundle_from_executable(Path::new("/Applications/NomiFun.app/Contents/MacOS/NomiFun")),
            Some(PathBuf::from("/Applications/NomiFun.app")),
        );
    }
}
