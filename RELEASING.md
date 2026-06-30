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

1. Build unsigned bundles with `bun run build`.
2. For macOS public distribution, use `bun run build:signed` with the
   release-owner Developer ID credentials.
3. For updater artifacts, configure the release-owner Tauri updater key and run
   `bun run build:updater`.
4. Publish installers and signatures to the release host.
5. Publish a signed `latest.json` only after artifacts are uploaded.

Updater signing and OS code signing are separate. See:

- `apps/desktop/updater/README.md`
- `apps/desktop/signing/README.md`

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
