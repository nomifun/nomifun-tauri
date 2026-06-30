# Releasing NomiFun

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

Installers are unsigned-OS by default; in-app auto-update uses the Tauri updater
key (separate from OS code signing). You **cannot cross-compile** — build each
platform on its own machine.

1. Build unsigned bundles with `bun run build` (or `build:mac` / `build:win` /
   `build:linux`).
2. For macOS public distribution, use `bun run build:signed` with the
   release-owner Developer ID credentials.
3. **Updater artifacts (per platform):** export the updater private key, then
   build with updater signing on. The signed `.sig` lands next to each bundle.

   ```bash
   export TAURI_SIGNING_PRIVATE_KEY="$(cat apps/desktop/signing/nomifun-updater.key)"
   export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
   bun run build:updater                                              # Windows / Linux
   bun run build:mac --config '{"bundle":{"createUpdaterArtifacts":true}}'      # macOS (Universal)
   ```

4. **Build the manifest:** `bun run make:latest` on each build machine scans its
   updater artifacts and **merges** the `<os>-<arch>` entries into
   `apps/desktop/updater/latest.json`. Carry that file between machines so the
   merged manifest covers every shipped platform/chip (the report flags missing
   ones — a missing entry = those users silently get no update).
5. **Publish to GitHub Releases:** create a release tagged `v<version>` and upload
   every installer + its `.sig` + the merged `latest.json`. The configured
   endpoint (`releases/latest/download/latest.json`) then serves the newest release.

Updater signing and OS code signing are separate. See:

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
