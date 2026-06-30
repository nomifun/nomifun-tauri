# Releasing NomiFun

中文发版手册见 `RELEASING.zh-CN.md`.

This checklist is for maintainers preparing a public release.

## Versioning (single source of truth)

The release version lives in **one** place: the root `Cargo.toml`
`[workspace.package].version`. The backend's `CARGO_PKG_VERSION` / `app_version`
follows it, and `apps/desktop/tauri.conf.json` inherits it (it has no `version`
field of its own — Tauri reads it from the workspace), so the installer filename
and updater version stay in sync automatically.

Bump everything with one command:

```bash
bun run bump 1.2.3            # writes the version + syncs Cargo.lock + package.json/ui
bun run bump 1.2.3 --tag      # also: git commit + git tag v1.2.3 (needs a clean tree)
```

Tags use the `vX.Y.Z` form. The decorative `package.json` / `ui/package.json`
versions are kept in sync by the script but are not read by any build.

## Before Tagging

1. Update `CHANGELOG.md`.
2. Run the documented verification commands for the changed surface.
3. Confirm `docs/`, `README.md`, `STATUS.md`, and packaging guides match the
   release behavior.
4. Confirm no private keys, local paths, proprietary assets, or internal-only
   roadmap claims are included.
5. Confirm third-party licenses and attributions are current.

## Desktop Release

Desktop releases have two asset groups:

- **Manual installers**: files users download directly, such as macOS `.dmg`,
  Windows `.exe` / `.msi`, and Linux `.AppImage` / `.deb` / `.rpm`.
- **Updater assets**: the Tauri updater package, its `.sig`, and the merged
  `latest.json`. The updater package may also be a manual installer on Windows,
  but macOS still needs a separate `.dmg` for manual install.

Updater signing and OS code signing are separate. The updater private key proves
that an update package is ours; OS code signing controls Gatekeeper / SmartScreen
trust for people launching a manually downloaded app. You **cannot cross-compile**
reliable desktop installers — build each platform on its own machine.

### Standard Release Runbook

Use this order for every desktop release.

1. **Pick and bump the version.**

   ```bash
   VERSION=1.2.3
   bun run bump "$VERSION"
   ```

2. **Build macOS on a Mac.** The command below emits both the manual `.dmg` and
   the updater `.app.tar.gz` + `.sig`.

   ```bash
   export TAURI_SIGNING_PRIVATE_KEY="$(cat apps/desktop/signing/nomifun-updater.key)"
   export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
   bun run build:mac --config '{"bundle":{"createUpdaterArtifacts":true}}'
   bun run make:latest
   ```

   For public macOS distribution, configure `apps/desktop/signing/.env.signing`
   and use `--signed` so the `.app` / `.dmg` are Developer ID signed and
   notarized:

   ```bash
   bun run build:mac --signed --config '{"bundle":{"createUpdaterArtifacts":true}}'
   ```

3. **Build Windows on a Windows machine.** Use the same updater private key.
   If you do not yet have Authenticode signing, omit `--signed`; the Tauri updater
   signature still works, but the manual installer can show Windows SmartScreen /
   unknown-publisher warnings.

   ```powershell
   $env:TAURI_SIGNING_PRIVATE_KEY = Get-Content apps/desktop/signing/nomifun-updater.key -Raw
   $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
   bun run build:win -- --config '{"bundle":{"createUpdaterArtifacts":true}}'
   bun run make:latest
   ```

   Once a Windows code-signing certificate is available, import it into the
   current user's certificate store and build with Authenticode signing:

   ```powershell
   $env:WINDOWS_CERTIFICATE_THUMBPRINT = "A1B2C3..."
   bun run build:win --signed -- --config '{"bundle":{"createUpdaterArtifacts":true}}'
   ```

4. **Build Linux on Linux** if that platform is part of the release.

   ```bash
   export TAURI_SIGNING_PRIVATE_KEY="$(cat apps/desktop/signing/nomifun-updater.key)"
   export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
   bun run build:linux -- --config '{"bundle":{"createUpdaterArtifacts":true}}'
   bun run make:latest
   ```

5. **Merge `latest.json` before publishing.** `bun run make:latest` preserves
   existing real platform entries and fills the entries for the current build
   machine. If platforms are built on different machines, carry the newest
   `apps/desktop/updater/latest.json` to the next machine before running
   `make:latest`, or replace the Release asset later with `--clobber`.

6. **Commit and tag the release state.**

   ```bash
   git add Cargo.toml Cargo.lock package.json ui/package.json apps/desktop/updater/latest.json
   git commit -m "chore(release): v$VERSION"
   git tag "v$VERSION"
   git push origin main "v$VERSION"
   ```

7. **Create or update the GitHub Release.** Upload updater assets, `latest.json`,
   and manual installers.

   ```bash
   gh release create "v$VERSION" \
     target/universal-apple-darwin/release/bundle/macos/NomiFun.app.tar.gz \
     target/universal-apple-darwin/release/bundle/macos/NomiFun.app.tar.gz.sig \
     dist/desktop/NomiFun_${VERSION}_universal.dmg \
     apps/desktop/updater/latest.json \
     --title "v$VERSION" \
     --notes "Release notes"
   ```

   If the Release already exists, upload more platform assets instead:

   ```bash
   gh release upload "v$VERSION" <new-assets...>
   gh release upload "v$VERSION" apps/desktop/updater/latest.json --clobber
   ```

   On Windows, upload the files printed by `bun run make:latest`; those are the
   updater package, its `.sig`, and the current `latest.json`. Also upload any
   manual-only installer from `dist/desktop/` that was not already uploaded, such
   as an `.msi` if one was generated.

8. **Verify the published Release.**

   ```bash
   gh release view "v$VERSION" --json tagName,assets,url
   curl -fsSL https://github.com/nomifun/nomifun-tauri/releases/latest/download/latest.json
   ```

   Confirm the downloaded manifest version is `VERSION`, every shipped platform
   has a `platforms[...]` entry, and all URLs point to the same `v$VERSION`
   Release.

### Adding Windows After macOS Was Already Published

Use this when a macOS Release already exists and you later continue from a
Windows build machine.

1. Pull the release commit/tag and confirm the same version.

   ```powershell
   git pull
   git checkout main
   git describe --tags --exact-match  # should print v<version> only if you checked out the tag
   ```

   If you stay on `main`, confirm `Cargo.toml` still has the same version as the
   existing Release.

2. Build Windows updater artifacts with the same updater private key.

   ```powershell
   $env:TAURI_SIGNING_PRIVATE_KEY = Get-Content apps/desktop/signing/nomifun-updater.key -Raw
   $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
   bun run build:win -- --config '{"bundle":{"createUpdaterArtifacts":true}}'
   bun run make:latest
   ```

3. Upload the Windows assets printed by `bun run make:latest`, then replace the
   Release `latest.json` so it includes both macOS and Windows platform entries.

   ```powershell
   gh release upload v<version> <windows-updater-package> <windows-updater-package>.sig
   gh release upload v<version> apps/desktop/updater/latest.json --clobber
   ```

   Upload any additional manual-only Windows installer from `dist/desktop/` if
   the build produced one and it was not already the updater package.

4. Commit and push the updated `latest.json` back to `main` so the repository
   matches the Release asset.

   ```powershell
   git add apps/desktop/updater/latest.json
   git commit -m "chore(release): add Windows assets to v<version>"
   git push origin main
   ```

Windows without Authenticode signing is acceptable for internal updater testing:
the Tauri updater `.sig` still protects the automatic update package. It is not
equivalent to a trusted public Windows installer. For public distribution, obtain
a Windows code-signing certificate, set `WINDOWS_CERTIFICATE_THUMBPRINT`, and
rerun `build:win --signed -- --config ...` before publishing the Windows assets.

See:

- `apps/desktop/updater/README.md`  (full updater flow + signing keys)
- `apps/desktop/signing/README.md`  (macOS Developer ID / notarization)

## Server Release

1. Build `nomifun-web` and the SPA.
2. Build and smoke-test the Docker image.
3. Verify first-run admin setup and `NOMIFUN_ADMIN_PASSWORD` pre-seeding.
4. Verify `127.0.0.1` default binding and explicit `0.0.0.0` deployment docs.

## After Release

1. Create a GitHub release with notes from `CHANGELOG.md`.
2. Attach platform artifacts.
3. Update website/download links.
4. Watch issues for install, updater, and migration regressions.
