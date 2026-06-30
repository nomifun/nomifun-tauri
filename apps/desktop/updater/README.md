# NomiFun Desktop Updater

In-app auto-update for `nomifun-desktop`, built on **Tauri's native updater**
(`tauri-plugin-updater` + minisign signatures + a `latest.json` manifest hosted
on GitHub Releases). The app checks → downloads → verifies → installs → relaunches.

## How it works

```
App (running version, from workspace Cargo.toml)
  └─ check() ──► plugins.updater.endpoints  (GitHub Releases latest.json)
        └─ newer version? ──► download the platform artifact + its .sig
              └─ verify signature against plugins.updater.pubkey
                    └─ install (swap .app / run NSIS) ──► relaunch
```

- **Frontend wiring (done):** `ui/src/common/adapter/tauriUpdater.ts` wraps
  `@tauri-apps/plugin-updater` (+ `plugin-process` for relaunch) and backs the
  `ipcBridge.update` / `ipcBridge.autoUpdate` channels. The in-app `UpdateModal`
  drives check → download (progress) → install. Entry points:
  - **About page** "检查更新" button (shell-gated on `isDesktopShell()`).
  - **Startup silent check** (`Layout.tsx`): on launch, if a newer version is
    available the modal opens automatically; otherwise it stays silent.
- **Config:** `apps/desktop/tauri.conf.json` →
  - `plugins.updater.endpoints` = `https://github.com/nomifun/nomifun-tauri/releases/latest/download/latest.json`
  - `plugins.updater.pubkey` = the project updater public key (committed; safe).
- **Permissions:** `apps/desktop/capabilities/default.json` grants
  `updater:default` + `updater:allow-check` + `updater:allow-download-and-install`,
  and `process:default` (relaunch).

## Signing keys

A single minisign keypair signs **every** platform/chip; the matching public key
is embedded in the app for verification.

- **Public key** → `plugins.updater.pubkey` in `tauri.conf.json` (committed).
- **Private key** → `apps/desktop/signing/nomifun-updater.key` — **gitignored**
  (`*.key`), never committed, never printed. **Back it up** to your release
  secret store: lose it and already-installed users stop receiving auto-updates
  (they must reinstall once); the app itself keeps working.
- Generated with no password (set one and pass it via
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` if you prefer). Regenerate with:

  ```bash
  bun x tauri signer generate -w apps/desktop/signing/nomifun-updater.key --password "" --ci -f
  # then paste the printed public key into tauri.conf.json → plugins.updater.pubkey
  ```

Updater signing is **separate** from OS code signing (Apple Developer ID /
Windows Authenticode). See `apps/desktop/signing/README.md`. macOS note: an
auto-updated `.app.tar.gz` should be signed + notarized, or Gatekeeper may block
the swapped bundle on other machines — that's the code-signing concern, not the
updater signature.

## Building signed update artifacts

`createUpdaterArtifacts` makes Tauri emit a `.sig` next to each updatable bundle.
Set the private key **content** (not the path) in the environment first:

```bash
export TAURI_SIGNING_PRIVATE_KEY="$(cat apps/desktop/signing/nomifun-updater.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""   # empty for the default key

# Windows (NSIS .exe + .sig):
bun run build:updater

# macOS (Universal .app.tar.gz + .sig — one artifact serves both darwin chips):
bun run build:mac --config '{"bundle":{"createUpdaterArtifacts":true}}'

# Linux (AppImage + .sig):
bun run build:updater   # on a Linux machine
```

You **cannot cross-compile**: build Windows on Windows, macOS on macOS, Linux on
Linux. Each platform's artifacts land under `target/**/release/bundle/`.

## Generating `latest.json`

`bun run make:latest` scans `target/` for this machine's updater artifacts + their
`.sig` files, derives the `<os>-<arch>` platform keys, and **merges** them into
`apps/desktop/updater/latest.json` (preserving entries built on other machines):

```bash
bun run make:latest                     # version from Cargo.toml, notes from CHANGELOG
bun run make:latest --collect           # also copy artifacts + .sig + latest.json → dist/desktop/
bun run make:latest --version 0.1.11 --notes "..."   # overrides
```

Run it once per build machine; carry `latest.json` between them (or commit it) so
the merged manifest ends up complete. The report prints which platform keys are
filled and which are still missing — **a missing `<os>-<arch>` entry means those
users silently get no update**, so make sure every platform you ship has one.

## Releasing

1. Build signed updater artifacts on each platform (above).
2. `bun run make:latest` on each, merging into one `latest.json`.
3. Create a GitHub Release tagged `v<version>`.
4. Upload **all** installers + their `.sig` files + `latest.json` as release assets.
5. The endpoint `releases/latest/download/latest.json` resolves to the newest
   release automatically — clients pick up the update on their next check.

## Safety checklist

- Private updater key stored only in your release secret store; never committed.
- `plugins.updater.pubkey` matches the private key used to sign.
- `plugins.updater.endpoints` is HTTPS (GitHub Releases).
- Every shipped `<os>-<arch>` has a `latest.json` entry with the exact `.sig` content.
- OS code signing / notarization handled separately per platform.
