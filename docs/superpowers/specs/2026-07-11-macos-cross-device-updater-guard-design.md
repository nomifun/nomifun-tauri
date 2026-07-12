# macOS Cross-Device Updater Guard Design

## Goal

Prevent NomiFun from invoking the Tauri macOS updater in an install topology that will fail with `Cross-device link (os error 18)`, replace the opaque OS error with actionable product guidance, and record enough diagnostics to distinguish updater failures from backend startup failures.

## Root Cause

`tauri-plugin-updater 2.10.1` extracts and backs up macOS application bundles under the system temporary directory, then uses `std::fs::rename` to move the running `.app` between that directory and its install location. A rename cannot cross filesystem devices. The failure is reproducible when NomiFun runs directly from its mounted DMG, from a removable or network volume, or from another volume whose device differs from the system temporary directory.

The desktop data relocation path is not the source of this failure: it already falls back from rename to copy-and-remove. Other critical startup renames use sibling temporary paths under the same data root and include boot-stage context when they fail.

## Supported Behavior

On macOS, automatic update installation is supported only when all of the following are true:

- The running executable belongs to a discoverable `.app` bundle.
- The bundle is not running from an App Translocation path.
- The bundle is not running under `/Volumes`.
- The application bundle and the system temporary directory are on the same filesystem device.

When any condition fails, checking for and downloading release metadata remains available, but NomiFun must not call the Tauri install operation. The update UI must explain why automatic installation is unavailable and direct the user to move `NomiFun.app` to `/Applications` or use the existing manual-download entry.

Windows and Linux updater behavior remains unchanged.

## Architecture

### Desktop install-context command

Add a small macOS-aware module under the desktop shell that resolves and classifies the current installation. It returns a serializable snapshot containing:

- platform;
- application bundle path when discoverable;
- system temporary directory;
- application and temporary-directory device identifiers when available;
- whether automatic installation is safe;
- a stable reason code when it is unsafe.

Reason codes are data, not localized strings:

- `app_bundle_not_found`
- `app_translocation`
- `mounted_volume`
- `cross_device`
- `metadata_unavailable`

The classifier must keep its decision logic pure and accept already-resolved paths/device identifiers so it can be unit-tested without mounting additional volumes. Filesystem probing and Tauri command serialization remain thin wrappers around that pure classifier.

On Windows and Linux, the command returns `autoInstallSupported: true` and no macOS-specific reason.

### Renderer adapter

Expose the install-context command through the existing Tauri shell adapter. The adapter must remain guarded by `isTauriRuntime()` so browser/WebUI builds do not evaluate Tauri IPC code.

The updater adapter checks install context immediately before installation, not only when the modal opens. This avoids a stale preflight if the application environment changes during a long-lived session. If the environment is unsafe, the adapter rejects with a stable application error code and does not invoke `pendingUpdate.install()`.

### Update UI

The update modal maps both the new application error code and legacy raw errors containing `Cross-device link`, `crosses devices`, or `os error 18` to one localized explanation. The message must state the visible recovery path:

1. Quit NomiFun.
2. Move `NomiFun.app` to `/Applications`.
3. Eject the DMG or removable installer volume.
4. Reopen NomiFun and retry, or use manual download.

The downloaded update remains recoverable: the modal stays open, continues to expose the manual-download controls, and does not relaunch after a blocked installation.

### Diagnostics

Every unsafe preflight emits one structured warning from the desktop shell containing the reason code, bundle path, temporary path, and available device identifiers. Paths are local filesystem paths and must stay in the local application log; they are not sent to remote services.

Unexpected install errors retain their original detail in the developer console while the user-facing message uses the existing generic update-failure fallback.

## Error Boundaries

This change does not replace the Tauri updater or implement cross-volume application-bundle copying. Running from a read-only DMG cannot be made safely self-updatable, and maintaining a private updater fork would add security-sensitive bundle replacement logic. The product instead prevents the known-invalid operation and gives the user a deterministic recovery path.

Backend boot errors remain handled by the existing `NomiFun backend failed to start` dialog. This updater guard must not suppress or remap backend errors.

## Testing

### Rust

- A normal `/Applications/NomiFun.app` classification on the same device allows automatic installation.
- `/Volumes/NomiFun/NomiFun.app` is rejected as `mounted_volume`.
- an App Translocation path is rejected as `app_translocation`.
- differing device identifiers are rejected as `cross_device`.
- missing bundle or metadata produces the corresponding stable reason and fails closed on macOS.
- non-macOS builds retain supported updater behavior.

### TypeScript

- The updater adapter never calls `install()` when the preflight is unsafe.
- Safe preflight calls `install()` and then relaunches.
- New stable error codes and legacy raw EXDEV messages map to the localized recovery message.
- Unrelated updater failures retain the generic failure behavior.
- Browser/WebUI mode does not invoke Tauri commands.

### Verification

Run the focused desktop Rust tests, focused updater/UI tests, Rust formatting and compilation checks for the affected crates, and the UI type check. Inspect the final diff to ensure no updater-download, release-signature, backend startup, or non-macOS behavior changed unintentionally.

## Out of Scope

- Forking or patching `tauri-plugin-updater`.
- Supporting automatic replacement of an app on removable, network, or read-only volumes.
- Changing release signing, updater manifests, or DMG layout.
- Refactoring unrelated startup-time atomic writes.
