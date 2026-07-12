# macOS Cross-Device Updater Guard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent macOS auto-update installation from invoking Tauri's cross-device rename path and replace raw EXDEV failures with actionable NomiFun guidance.

**Architecture:** The desktop shell exposes a pure-tested install-topology classifier through one Tauri command. The renderer runs that preflight immediately before installation through a dependency-injected helper, and the update UI maps both the stable guard error and legacy raw EXDEV strings to localized recovery instructions.

**Tech Stack:** Rust 2024, Tauri v2 commands, serde, TypeScript, Bun test, React/i18next.

## Global Constraints

- Do not fork or patch `tauri-plugin-updater`.
- Do not call the Tauri install operation when macOS install context is unsafe.
- Automatic installation remains supported for a normal same-device `/Applications/NomiFun.app` install.
- Windows, Linux, update checking, release signature verification, and update downloading remain unchanged.
- Unsafe paths fail closed on macOS and expose the existing manual-download controls.
- Local filesystem paths are written only to local logs and are not sent to remote services.

---

### Task 1: Classify the desktop updater install topology

**Files:**
- Create: `apps/desktop/src/updater_install_context.rs`
- Modify: `apps/desktop/src/main.rs:30-31,795-810`
- Test: inline `#[cfg(test)]` module in `apps/desktop/src/updater_install_context.rs`

**Interfaces:**
- Consumes: `std::env::current_exe()`, `std::env::temp_dir()`, macOS `MetadataExt::dev()`.
- Produces: `updater_install_context::get_updater_install_context() -> UpdaterInstallContext`, registered as Tauri command `get_updater_install_context`.

- [ ] **Step 1: Write failing pure-classifier tests**

Create the module with tests that describe the wished-for API before defining it:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn classify(path: Option<&str>, app_dev: Option<u64>, temp_dev: Option<u64>) -> InstallDecision {
        classify_macos_install(
            path.map(Path::new),
            Path::new("/private/var/folders/tmp"),
            app_dev,
            temp_dev,
        )
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
}
```

- [ ] **Step 2: Run the tests and verify RED**

Run:

```bash
cargo test -p nomifun-desktop updater_install_context::tests -- --nocapture
```

Expected: compilation fails because `InstallDecision`, `UpdaterInstallReason`, and `classify_macos_install` do not exist.

- [ ] **Step 3: Implement the classifier, filesystem probe, and command**

Use these stable serialized fields and reason strings:

```rust
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
    _temp_dir: &Path,
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
    classify_macos_install(app_bundle, Path::new(""), app_device_id, temp_device_id)
}

fn app_bundle_from_executable(executable: &Path) -> Option<PathBuf> {
    executable
        .ancestors()
        .find(|path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("app")))
        .map(Path::to_path_buf)
}
```

On macOS, get `dev()` from metadata for the bundle and temp directory, classify, and emit one `tracing::warn!` when unsupported. On non-macOS, return `autoInstallSupported: true`, no reason, and no device identifiers. Annotate only the command wrapper with `#[tauri::command]`.

Register the module at the top of `main.rs` and add `updater_install_context::get_updater_install_context` to `tauri::generate_handler!`.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run:

```bash
cargo test -p nomifun-desktop updater_install_context::tests -- --nocapture
```

Expected: all classifier tests pass.

- [ ] **Step 5: Commit Task 1**

```bash
git add apps/desktop/src/updater_install_context.rs apps/desktop/src/main.rs
git commit -m "fix(desktop): detect unsafe macOS updater locations"
```

---

### Task 2: Enforce the preflight immediately before installation

**Files:**
- Create: `ui/src/common/adapter/tauriUpdateInstall.ts`
- Create: `ui/src/common/adapter/tauriUpdateInstall.test.ts`
- Modify: `ui/src/common/adapter/tauriShell.ts:80-130`
- Modify: `ui/src/common/adapter/tauriUpdater.ts:190-207`

**Interfaces:**
- Consumes: Tauri command `get_updater_install_context`, pending updater handle `install()`, process plugin `relaunch()`.
- Produces: `installUpdateWithPreflight(deps: InstallUpdateDependencies): Promise<void>` and stable error prefix `NOMIFUN_UPDATER_AUTO_INSTALL_UNSUPPORTED`.

- [ ] **Step 1: Write failing installer-control-flow tests**

Create `tauriUpdateInstall.test.ts`:

```ts
import { describe, expect, test } from 'bun:test';
import {
  AUTO_INSTALL_UNSUPPORTED_ERROR,
  installUpdateWithPreflight,
  type UpdaterInstallContext,
} from './tauriUpdateInstall';

const safe: UpdaterInstallContext = {
  platform: 'macos',
  appBundlePath: '/Applications/NomiFun.app',
  tempDir: '/private/var/folders/tmp',
  appDeviceId: 7,
  tempDeviceId: 7,
  autoInstallSupported: true,
  reason: null,
};

test('safe context installs and then relaunches', async () => {
  const calls: string[] = [];
  await installUpdateWithPreflight({
    getContext: async () => safe,
    install: async () => void calls.push('install'),
    relaunch: async () => void calls.push('relaunch'),
  });
  expect(calls).toEqual(['install', 'relaunch']);
});

test('unsafe context never calls install or relaunch', async () => {
  const calls: string[] = [];
  const result = installUpdateWithPreflight({
    getContext: async () => ({ ...safe, autoInstallSupported: false, reason: 'mounted_volume' }),
    install: async () => void calls.push('install'),
    relaunch: async () => void calls.push('relaunch'),
  });
  await expect(result).rejects.toThrow(`${AUTO_INSTALL_UNSUPPORTED_ERROR}:mounted_volume`);
  expect(calls).toEqual([]);
});
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```bash
cd ui && bun test src/common/adapter/tauriUpdateInstall.test.ts
```

Expected: module-not-found failure for `./tauriUpdateInstall`.

- [ ] **Step 3: Implement the dependency-injected install gate**

Create the production helper:

```ts
export type UpdaterInstallReason =
  | 'app_bundle_not_found'
  | 'app_translocation'
  | 'mounted_volume'
  | 'cross_device'
  | 'metadata_unavailable';

export interface UpdaterInstallContext {
  platform: string;
  appBundlePath: string | null;
  tempDir: string;
  appDeviceId: number | null;
  tempDeviceId: number | null;
  autoInstallSupported: boolean;
  reason: UpdaterInstallReason | null;
}

export const AUTO_INSTALL_UNSUPPORTED_ERROR = 'NOMIFUN_UPDATER_AUTO_INSTALL_UNSUPPORTED';

export interface InstallUpdateDependencies {
  getContext: () => Promise<UpdaterInstallContext>;
  install: () => Promise<void>;
  relaunch: () => Promise<void>;
}

export async function installUpdateWithPreflight(deps: InstallUpdateDependencies): Promise<void> {
  const context = await deps.getContext();
  if (!context.autoInstallSupported) {
    throw new Error(`${AUTO_INSTALL_UNSUPPORTED_ERROR}:${context.reason ?? 'metadata_unavailable'}`);
  }
  await deps.install();
  await deps.relaunch();
}
```

Add `tauriGetUpdaterInstallContext()` to `tauriShell.ts` using dynamic `@tauri-apps/api/core` import. In `tauriUpdater.ts`, keep the existing Tauri runtime guard, return without relaunch when `pendingUpdate` is absent, dynamically import `relaunch`, and call `installUpdateWithPreflight` with the real context/install/relaunch functions. Do not change download state.

- [ ] **Step 4: Run the tests and verify GREEN**

Run:

```bash
cd ui && bun test src/common/adapter/tauriUpdateInstall.test.ts
```

Expected: 2 tests pass and unsafe context records no install/relaunch calls.

- [ ] **Step 5: Commit Task 2**

```bash
git add ui/src/common/adapter/tauriUpdateInstall.ts ui/src/common/adapter/tauriUpdateInstall.test.ts ui/src/common/adapter/tauriShell.ts ui/src/common/adapter/tauriUpdater.ts
git commit -m "fix(updater): block unsafe macOS auto installs"
```

---

### Task 3: Localize EXDEV recovery guidance

**Files:**
- Modify: `ui/src/renderer/components/settings/updateErrorMessage.ts`
- Modify: `ui/src/renderer/components/settings/updateErrorMessage.test.ts`
- Modify: `ui/src/renderer/components/settings/UpdateModal.tsx:180-188`
- Modify: `ui/src/renderer/services/i18n/locales/zh-CN/update.json`
- Modify: `ui/src/renderer/services/i18n/locales/en-US/update.json`
- Regenerate: `ui/src/renderer/services/i18n/i18n-keys.d.ts`

**Interfaces:**
- Consumes: stable guard prefix and legacy raw strings `Cross-device link`, `crosses devices`, `os error 18`.
- Produces: i18n key `update.crossDeviceInstallUnsupported` and translated recovery instructions.

- [ ] **Step 1: Extend the error-mapping test first**

Add cases before changing production code:

```ts
test.each([
  'NOMIFUN_UPDATER_AUTO_INSTALL_UNSUPPORTED:mounted_volume',
  'Cross-device link (os error 18)',
  'operation crosses devices',
])('maps unsafe macOS install error %s to recovery guidance', (message) => {
  expect(getUpdateErrorMessageKey(message)).toBe('update.crossDeviceInstallUnsupported');
});
```

- [ ] **Step 2: Run the mapping test and verify RED**

Run:

```bash
cd ui && bun test src/renderer/components/settings/updateErrorMessage.test.ts
```

Expected: new cases receive `update.checkFailed`, so the test fails for the intended missing mapping.

- [ ] **Step 3: Implement mapping, UI use, and localized copy**

Extend the key union and match the stable prefix plus all three legacy EXDEV spellings before release-feed checks. Change the install catch block to log the raw error, map it, and show `Message.error(t(errorMessageKey))`.

Add exact localized copy:

```json
"crossDeviceInstallUnsupported": "NomiFun 当前从磁盘映像、外置磁盘或其他卷运行，无法安全地自动替换应用。请退出 NomiFun，将 NomiFun.app 移到“应用程序”文件夹，推出安装磁盘后重新打开并重试；也可以使用手动下载。"
```

```json
"crossDeviceInstallUnsupported": "NomiFun is running from a disk image, external drive, or another volume and cannot safely replace itself. Quit NomiFun, move NomiFun.app to Applications, eject the installer volume, then reopen and retry, or use the manual download."
```

Regenerate the key type from the repository root:

```bash
bun run gen:i18n
```

- [ ] **Step 4: Run focused tests and i18n contract**

Run:

```bash
cd ui && bun test src/renderer/components/settings/updateErrorMessage.test.ts src/common/adapter/tauriUpdateInstall.test.ts
cd .. && bun run check:i18n
```

Expected: all focused tests pass and generated i18n types are current.

- [ ] **Step 5: Commit Task 3**

```bash
git add ui/src/renderer/components/settings/updateErrorMessage.ts ui/src/renderer/components/settings/updateErrorMessage.test.ts ui/src/renderer/components/settings/UpdateModal.tsx ui/src/renderer/services/i18n/locales/zh-CN/update.json ui/src/renderer/services/i18n/locales/en-US/update.json ui/src/renderer/services/i18n/i18n-keys.d.ts
git commit -m "fix(updater): explain cross-device install recovery"
```

---

### Task 4: Verify the integrated change

**Files:**
- Verify all files changed by Tasks 1-3.

**Interfaces:**
- Consumes: Rust classifier command, Tauri adapter, install gate, UI error mapper, locale contracts.
- Produces: fresh proof that the affected behavior and adjacent build surfaces pass.

- [ ] **Step 1: Run Rust formatting and desktop tests**

```bash
cargo fmt --check
cargo test -p nomifun-desktop updater_install_context::tests -- --nocapture
cargo test -p nomifun-desktop
```

Expected: formatting check and all desktop tests pass.

- [ ] **Step 2: Run frontend tests and static checks**

```bash
cd ui
bun test src/common/adapter/tauriUpdateInstall.test.ts src/renderer/components/settings/updateErrorMessage.test.ts
bun run typecheck
cd ..
bun run check:i18n
bun run check:icons
```

Expected: focused Bun tests, TypeScript, i18n, and icon checks pass with zero errors.

- [ ] **Step 3: Run a production compile boundary**

```bash
cargo check -p nomifun-desktop
```

Expected: desktop shell and its linked backend compile successfully.

- [ ] **Step 4: Inspect the final diff and status**

```bash
git diff HEAD~3 --check
git diff HEAD~3 --stat
git status --short --branch
```

Expected: no whitespace errors, only updater-guard files changed, and no uncommitted implementation files remain.
