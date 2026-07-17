# Installation

NomiFun has two host modes that share one Rust backend (see
[Introduction](introduction.md)). This page covers all three ways to install
it today:

- [Desktop app from source](#desktop-app-from-source) — `nomifun-desktop`
  (Tauri shell), desktop local-trust, single-user.
- [Web server from source](#web-server-from-source) — `nomifun-web`,
  authenticated, self-hosted.
- [Docker / Docker Compose](#docker--docker-compose) — the same web server,
  containerised.

> **Official pre-built installers are not yet published.** Desktop bundles,
> macOS signing, updater artifacts, Docker, and native Linux service files can
> be built locally; there is not yet an official public release channel. Until
> then, every install path below builds from source. See
> [`../contributing/building-and-packaging.md`](../contributing/building-and-packaging.md)
> for the current packaging notes.

## Prerequisites

You need a working build toolchain regardless of which mode you target. The
exact requirements:

| Tool | Minimum | Why | Notes |
| --- | --- | --- | --- |
| **Rust** | stable, edition 2024 | Compile the backend (and the Tauri shell, for desktop). | Install via [`rustup`](https://rustup.rs/). The workspace pins `edition = "2024"` and `resolver = "3"`. |
| **Bun** | **≥ 1.3.13** | Frontend package manager + build (and a hard runtime dependency of the agent engine). | `1.1.38` has a stdin bug — do not use it. |
| **Tauri CLI** | v2 | Build the desktop shell. | Pulled in as a `devDependency`; no global install needed. |
| **Git** | any recent | Clone, plus skill discovery and some built-in tools. | |
| **C/C++ build tools** | platform-specific | `rusqlite` (bundled), `aws-lc-rs`, `libgit2-sys`. | Windows: MSVC + WebView2 runtime. macOS: Xcode CLT. Linux: `build-essential cmake clang pkg-config perl`. |

Optional but recommended on the host that runs Nomi (not for building):

- **`ripgrep`** — code-search backend; falls back to `grep` if missing.
- **`node` / `npm` / `npx`** — many user-installed MCP stdio servers launch
  via `npx -y …`.

### Clone the repo

```bash
git clone <your-fork-or-mirror>/nomifun-tauri.git
cd nomifun-tauri
```

The rest of this page assumes the repository root is your working directory.

### Install JS dependencies

```bash
bun install
```

This populates `node_modules/` for the workspace and `ui/`. Re-run it any
time `package.json` or `ui/package.json` changes.

## Desktop app from source

The desktop app is a Tauri 2 shell (`apps/desktop`, binary
`nomifun-desktop`) that links the backend in-process and starts it on a free
localhost port under the desktop `TrustLocalToken` policy. Its own WebView
receives a per-boot local trust secret, so there is no login screen in the
desktop window.

### Run in development

```bash
bun run dev
```

What this does, end-to-end:

1. Tauri's `beforeDevCommand` runs `bun run --filter=./ui dev` to start the
   Vite dev server on `http://localhost:5173`.
2. `cargo` compiles `nomifun-desktop` (and the workspace it depends on).
3. The shell starts, picks a free port, spawns the embedded backend, and
   loads the Vite dev URL. Hot-reload works on the renderer side; the backend
   restarts only when its Rust code changes.

You will see a tracing line like `Server listening on 127.0.0.1:54760` in the
console — that is the embedded backend. The renderer reads
`window.__backendPort` (injected by the Tauri shell as an init script) so the
SPA always knows where to call `/api`.

![nomifun-desktop running in dev with the embedded backend](../images/gs-02-desktop-dev.png)

### Build a release binary

```bash
bun run build:ui         # build the SPA into ui/dist
bun run build    # tauri build → installers + standalone binary
```

`tauri build` produces:

- A standalone executable under
  `target/release/nomifun-desktop` (`.exe` on Windows).
- Platform installers under `target/release/bundle/` — `.exe` (NSIS,
  Windows), `.dmg`/`.app` (macOS), `.deb`/`.AppImage` (Linux).

`bun run build` artifacts are suitable for local testing. For distributable
macOS builds, configure `apps/desktop/signing/.env.signing` and use
`bun run build:signed`. Windows signing still requires an external certificate.
To test the updater scaffold, use `bun run build:updater`, which sets
`bundle.createUpdaterArtifacts` to true. The updater endpoint and public key in
`apps/desktop/tauri.conf.json` must be replaced before shipping any update.

### Where data lives (desktop)

The desktop app stores its database and runtime files under the per-user
application-data directory, joined with `Nomi`:

| OS | Default path |
| --- | --- |
| Windows | `%LOCALAPPDATA%\NomiFun\Nomi` (e.g. `C:\Users\<you>\AppData\Local\NomiFun\Nomi`) |
| macOS | `~/Library/Application Support/NomiFun/Nomi` |
| Linux | `$XDG_DATA_HOME/NomiFun/Nomi` (usually `~/.local/share/NomiFun/Nomi`) |

Override with `NOMIFUN_DATA_DIR=<absolute path>` before launching — the
shell appends `/Nomi`, so the dir becomes `$NOMIFUN_DATA_DIR/Nomi`.

> Older builds defaulted to `<system temp>/nomifun-data/Nomi`, where OS temp
> cleanup could destroy user data. On first launch the app now relocates such
> a legacy install to the per-user location automatically (one-shot): data is
> copied, absolute paths inside the database are rewritten, and the old
> directory is kept as a backup. If the relocation cannot complete, the app
> starts from the legacy directory and retries on the next launch.

> Note: the app's user-facing name is `NomiFun` everywhere — the bundle
> product name (`apps/desktop/tauri.conf.json`), the runtime window title,
> and release artifacts. The data folder keeps its existing `/Nomi`
> suffix for compatibility with current installs. Internal identifiers keep the legacy `nomifun`
> name by design (crates, `NOMIFUN_*` env vars, the `com.nomifun.*`
> bundle identifier).

## Web server from source

`nomifun-web` is an axum server that mounts the same backend in-process
**and** serves the built SPA on the same port (default `8787`). It is the
right path for self-hosting on a LAN, VPN, or VPS.

### Build and run

```bash
bun install
bun run build:ui       # ui/dist — required before serving in non-dev mode
bun run serve:web            # = cargo run -p nomifun-web
```

By default the server binds `127.0.0.1:8787` and uses the same per-user
data directory as the desktop app (see
[Where data lives (desktop)](#where-data-lives-desktop)):

```text
nomifun-web: embedded backend + SPA on one port
listening on 127.0.0.1:8787  auth=required  dist=../../ui/dist
```

On a machine that also has the desktop app installed, a bare `nomifun-web`
run opens the desktop app's data directly — an exclusive `server.lock`
guarantees the two backends never run on that directory at the same time.

Open `http://127.0.0.1:8787` in a browser. On the very first visit you will
be sent to a setup screen — the username and password you type **become the
initial admin account**. After that, login is required for everyone.

![First-run admin setup in the browser](../images/gs-03-web-first-run-setup.png)

### Common flags

`nomifun-web` (defined in `apps/web/src/main.rs`) accepts both CLI flags and
environment variables:

| Flag | Env var | Default | Meaning |
| --- | --- | --- | --- |
| `--host` | `NOMIFUN_WEB_HOST` | `127.0.0.1` | Bind address. Use `0.0.0.0` only when you intend LAN/VPN/public access; pre-seed or complete admin setup first. |
| `--port` | `NOMIFUN_WEB_PORT` | `8787` | Port for both `/api` and the SPA. |
| `--data-dir` | `NOMIFUN_DATA_DIR` | _per-user app-data dir, same as the [desktop default](#where-data-lives-desktop)_ | Backend data dir (db / logs / bun cache / agent state). The env value is taken literally (no `/Nomi` suffix). Use an absolute path in production. |
| `--dist` | `NOMIFUN_WEB_DIST` | `../../ui/dist` | SPA static directory. **Set this explicitly when running outside the repo root.** |
| `--admin-user` | `NOMIFUN_ADMIN_USERNAME` | `admin` | Username for pre-seeded admin (only honoured before the admin exists). |
| `--admin-password` | `NOMIFUN_ADMIN_PASSWORD` | _(none — interactive first-run setup)_ | Pre-seed the admin password and skip interactive first-run. |
| `--insecure-no-auth` | `NOMIFUN_WEB_INSECURE_NO_AUTH` | `false` | **Danger.** Disable authentication entirely (desktop-style local mode). Loopback / trusted private network only. |
| _(env only)_ | `NOMIFUN_HTTPS` | `false` | Set to `true` when fronted by TLS so cookies get the `Secure` flag. |

Example, opening it up to the LAN with a pre-seeded admin:

```bash
nomifun-web \
  --host 0.0.0.0 --port 8787 \
  --data-dir /var/lib/nomifun \
  --dist /opt/nomifun/web \
  --admin-user admin \
  --admin-password "change-me-to-something-strong"
```

For full deployment guidance — systemd unit, reverse-proxy, and security
notes — see
[`../guides/web-server-deployment.md`](../guides/web-server-deployment.md).

## Docker / Docker Compose

The repository ships a multi-stage `Dockerfile` and a `docker-compose.yml`
that produce a **headless** (no GUI) image: SPA + `nomifun-web` + `bun` on
`debian:bookworm-slim`.

### Quick start with Compose

From the repo root:

```bash
docker compose up -d --build
# then open http://<server-ip>:8787
```

The service is configured with `restart: unless-stopped` so installing it
**is** enabling it on boot. Persistent state (SQLite database, logs, bun
cache, agent state) lives in the named volume `nomifun-data` mounted at
`/data` inside the container.

The image's defaults are tuned for container life:

```text
NOMIFUN_WEB_HOST=0.0.0.0
NOMIFUN_WEB_PORT=8787
NOMIFUN_DATA_DIR=/data
NOMIFUN_WEB_DIST=/opt/nomifun/web
SHELL=/bin/bash
```

Authentication is on, but first-run setup can be claimed by the first browser
that reaches the service. Pre-seed the admin or complete setup on a trusted
network before publishing port `8787` broadly. For anything reachable from the
internet, put TLS in front of it — the bundled `Caddyfile` and the
commented-out `caddy` service in `docker-compose.yml` are the recommended path.
Set
`NOMIFUN_HTTPS=true` on the `nomifun` service when you do, so the session
cookie gains the `Secure` flag.

### Pre-seed the admin (recommended for non-interactive setup)

The first browser visit otherwise wins the admin account; pre-seeding closes
that race window:

```yaml
# docker-compose.yml — under services.nomifun
environment:
  NOMIFUN_ADMIN_USERNAME: admin
  NOMIFUN_ADMIN_PASSWORD: "change-me-to-something-strong"
  NOMIFUN_HTTPS: "true"   # only when behind a TLS proxy
```

### Constrained networks

The Rust stage uses BuildKit cache mounts (`/usr/local/cargo/registry` and
`/src/target`), so a one-line source change recompiles in seconds. A first
install still reaches Docker Hub, npm, Debian apt, and crates.io; a timeout at
any one of those sources can fail the build. Compose forwards a separate mirror
setting for each package layer, for example:

```bash
APT_MIRROR=<trusted-debian-mirror> \
CARGO_REGISTRY_MIRROR=<trusted-sparse-cargo-index> \
BUN_REGISTRY=<trusted-npm-registry> \
docker compose up -d --build
```

If the error occurs at `load metadata for docker.io/...`, the build has not
reached apt or Cargo yet. Configure a Docker daemon registry mirror, or point
`BUN_IMAGE`, `RUST_IMAGE`, and `RUNTIME_IMAGE` at trusted compatible images.
The default Rust build stage now uses the smaller `rust:1-slim-bookworm`, and
apt/Cargo downloads have retries and extended timeouts.

For the long-form deployment guide (TLS, reverse-proxy patterns, systemd
unit, security caveats) see
[`../guides/web-server-deployment.md`](../guides/web-server-deployment.md).

## Verifying your install

A 30-second smoke test you can run after either path:

```bash
# Rust workspace compiles cleanly
cargo check --workspace

# All three binaries build
cargo build --workspace --bins
# → target/(debug|release)/{nomicore, nomifun-web, nomifun-desktop}

# Web host responds with the SPA + auth status
curl -sS http://127.0.0.1:8787/                | head -c 200
curl -sS http://127.0.0.1:8787/api/auth/status
# → 200 {"success":true,"needs_setup":..., "user_count":...}
```

If you see `nomifun-web: embedded backend + SPA on one port` in the logs and
`/api/auth/status` returns JSON, the backend is up and the SPA is being
served from the same port.

## What's next

- [Quick Start](quick-start.md) — your first conversation in Nomi.
- [`../guides/web-server-deployment.md`](../guides/web-server-deployment.md)
  — production hardening for the web host.
- [`../contributing/development.md`](../contributing/development.md)
  — set up a developer loop (renderer hot-reload, backend rebuild, debug tools).
