# Running NomiFun as a Desktop App

The desktop app (`nomifun-desktop`) is a [Tauri](https://tauri.app/) shell that links the Rust backend (`nomifun-app`) **into the same process**. There is no spawned backend binary, no Electron, no bundled `nomicore`. The shell starts the backend as an async task on a free `127.0.0.1` port, then loads the bundled SPA (`ui/dist`) into a WebView and points it at `http://127.0.0.1:<port>/api`.

The desktop WebView does not show a login screen. Instead, the embedded backend
runs under `AuthPolicy::TrustLocalToken`: the shell injects a per-boot local
trust secret into its own WebView, and only requests carrying that secret are
treated as the desktop user. If you want login + remote browser/phone access,
see [WebUI Remote Access](./webui-remote-access.md) for the in-app feature, or
[Self-Host the Web Server](./web-server-deployment.md) for the standalone
server.

![NomiFun desktop main window](../images/desktop-01-main-window.png)

## Quick start

### Prerequisites

The desktop app requires:

- A platform Tauri supports (Windows 10+, macOS 11+, mainstream Linux distros).
- A WebView runtime: **WebView2** on Windows (preinstalled on Win 11; on Win 10 install the [Evergreen Bootstrapper](https://developer.microsoft.com/microsoft-edge/webview2/)), **WKWebView** on macOS (built-in), **WebKitGTK** on Linux (`libwebkit2gtk-4.1-0`).
- For development: Rust toolchain, [Bun](https://bun.sh) ≥ 1.3.13, and the platform Tauri build deps (see the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/)).

### Run from source (development)

From the repo root:

```bash
bun install
bun run dev
```

This runs `tauri dev --config apps/desktop/tauri.conf.json`. It starts the Vite dev server (`http://localhost:5173`) for the SPA, builds and launches `nomifun-desktop`, and the embedded backend is started on a fresh free localhost port at every boot.

### Build a release bundle

```bash
bun run build
```

Output bundles land under `target/release/bundle/` per platform (an NSIS installer (`.exe`) on Windows, `.app` + `.dmg` on macOS, `.deb` + `.AppImage` on Linux). To produce signed updater artifacts (extra `.sig` files), use `bun run build:updater` after configuring signing keys (see [Updater status](#updater-status) below).

A successful build prints the bundle locations, for example on macOS:

```text
$ bun run build
   Compiling nomifun-app v0.1.0
    Finished `release` profile [optimized] target(s)
    Bundling NomiFun.app (macos)
    Bundling NomiFun_0.1.0_aarch64.dmg (macos)
    Finished 2 bundles at:
      target/release/bundle/macos/NomiFun.app
      target/release/bundle/dmg/NomiFun_0.1.0_aarch64.dmg
```

## Window and titlebar

The main window is **frameless** on Windows and Linux: the React titlebar component draws min/maximize/close on the same row as the in-app navigation. On macOS the native traffic-light buttons are kept via Tauri's `Overlay` title-bar style, with content extending under the bar.

- Default size: `1280 × 832`, minimum `880 × 600`.
- Resizable everywhere (edge-resize and Snap still work on Windows even without OS-drawn decorations).
- Title bar: `NomiFun`.

> The exact chrome differs per OS: a frameless titlebar with in-app controls on
> Windows and Linux, and the native traffic-light buttons (content under an
> `Overlay` bar) on macOS.

## Single instance

`tauri-plugin-single-instance` enforces a single running copy of the app on Windows and Linux. Trying to launch a second `nomifun-desktop` will silently focus the existing window instead of starting another backend on a different port.

## Deep links

The app registers the `nomifun://` URL scheme (configured in `apps/desktop/tauri.conf.json` under `plugins.deep-link.desktop.schemes`). When the OS launches Nomi via a `nomifun://...` URL, the shell forwards the URLs to the renderer over the Tauri event `deep-link://received`. The renderer can subscribe with `listen('deep-link://received', ...)` from `@tauri-apps/api/event` to handle the payload.

`register_all()` is called at startup to install the scheme; on platforms that need an out-of-band registration step (some Linux desktops, dev contexts) the call is best-effort and a failure is ignored.

## Autostart

The shell ships `tauri-plugin-autostart` so the renderer can opt the app into "launch at login" via the plugin's invoke API. On macOS this uses a `LaunchAgent`; on Windows the registry's `Run` key; on Linux a `.desktop` file in the autostart folder. The user-facing toggle lives in app settings.

## Notifications

`tauri-plugin-notification` is enabled. The renderer can show OS-level notifications (e.g. when an agent finishes a long task or AutoWork has results). On macOS the user is asked for permission the first time; on Windows, notifications use the modern Action Center; on Linux they go through `libnotify`.

## Where data is stored

The installed desktop app persists the SQLite database, agent state, logs, and the Bun runtime cache under the stable per-user application-data directory — **`%LOCALAPPDATA%\NomiFun\Nomi`** on Windows, **`~/Library/Application Support/NomiFun/Nomi`** on macOS, **`$XDG_DATA_HOME/NomiFun/Nomi`** on Linux (resolved by `nomifun_app::cli::default_data_dir()`). Hosts on the same build channel share a default; development scripts use the isolated `Nomi-dev` sibling instead. Use `bun run seed:dev` when dev needs a copy of stable state.

Set `NOMIFUN_DATA_DIR=<absolute path>` before launching the app and the data dir becomes `$NOMIFUN_DATA_DIR/Nomi`. The backend takes an exclusive `server.lock` on the data dir at startup; if it fails to start — for example because another instance already holds the directory — the desktop shell shows a native error dialog and exits.

> Older builds defaulted to `<system temp>/nomifun-data/Nomi`. An install found there is relocated to the per-user location automatically on launch (one-shot): data is copied, absolute paths stored in the database are rewritten, and the legacy directory is kept as a backup. Regenerable caches (the extracted Bun runtime, logs, browser profile, …) are not carried over — they rebuild on first use.

To start fresh, **quit the app** and delete that directory. To migrate, copy the directory to a new machine.

```text
~/Library/Application Support/NomiFun/Nomi/    # macOS (see paths above for Windows/Linux)
├── nomifun-backend.db        # SQLite state (conversations, settings, sessions, …)
├── logs/                     # nomicore.log
├── companion/                # companions + the shared memory hub
├── knowledge/                # managed knowledge bases
├── runtime/                  # extracted Bun runtime cache (regenerable)
└── server.lock               # exclusive lock held while a backend is running
```

## Authentication and local trust

The desktop shell does not expose the old blanket no-auth backend to every
localhost caller. It starts the embedded backend with `TrustLocalToken`, injects
`window.__nomiLocalTrust` into the WebView, and the renderer presents that secret
on HTTP and WebSocket calls. A process that only knows
`127.0.0.1:<port>/api` is not automatically trusted.

The desktop app is still a single-user tool: the OS account that starts it owns
everything the agent can do, including shell and file access.

If you want to access the same install from another device, do **not** expose the embedded port. Use one of:

- **WebUI remote access** (a per-instance feature, see [WebUI Remote Access](./webui-remote-access.md)) — turns on a separate authenticated server and gives you a QR-code login.
- **Self-hosted web server** ([Web Server Deployment](./web-server-deployment.md)) — runs the same backend headlessly under `nomifun-web` with auth required.

## Updater status

The Tauri updater plugin (`tauri-plugin-updater`) is wired in and the renderer exposes `invoke('check_for_updates')` (returns the new version string or `null` if up to date). However:

- The endpoint configured in `apps/desktop/tauri.conf.json` (`plugins.updater.endpoints`) is a **placeholder** (`https://REPLACE-WITH-YOUR-HOST/...`). Until you replace it with a real HTTPS URL serving a signed `latest.json`, the updater check will fail.
- The included `pubkey` is a **development key** generated for local testing. **Replace it before any public release** and store your private key in a CI secret.
- `bun run build:updater` produces signed update artifacts (extra `.sig` files next to each installer).

The full updater flow (signing env vars, `latest.json` schema, supported platform
keys) is documented in `apps/desktop/updater/README.md`. OS-level code signing /
notarization is separate. macOS Developer ID signing and notarization are wired
through `bun run build:signed` and documented in
`apps/desktop/signing/README.md`; Windows signing still requires an external
code-signing certificate.

## Troubleshooting

**The window opens to a blank white area.**
Make sure the WebView runtime is installed (WebView2 on Windows 10 needs the Evergreen Bootstrapper). On Linux, `libwebkit2gtk-4.1-0` is required.

**"Failed to bind backend port".**
Another process is holding `127.0.0.1` ephemeral ports. The backend tries `pick_free_port()` and falls back to `8799` if that fails — quit any other NomiFun instance and try again.

**Agent commands fail with `bun: command not found`.**
The agent engine spawns Bun as a child process for tool execution. Install Bun (`curl -fsSL https://bun.sh/install | bash`) and make sure it is on the system `PATH`, or build the desktop bundle with `NOMIFUN_EMBED_BUN=1` to embed it.

## See also

- [Web Server Deployment](./web-server-deployment.md) — run the same backend headlessly under `nomifun-web`.
- [WebUI Remote Access](./webui-remote-access.md) — expose your desktop instance for remote browser/phone use.
