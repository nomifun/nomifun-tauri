# Development

This page is for people changing **NomiFun** itself: the React SPA, the Rust
backend, the agent engine, or the Tauri shell. If you only want to install or
operate the product, start with
[`../getting-started/installation.md`](../getting-started/installation.md) or
[`../guides/web-server-deployment.md`](../guides/web-server-deployment.md).

The current repository is the active Tauri monorepo. Earlier Electron-era phase
plans and audits are not kept in the repo; consult git history if you need that
background.

## Prerequisites

| Tool | Minimum | Why |
| --- | --- | --- |
| Rust | stable, edition 2024 | Workspace uses resolver `3` and edition `2024`. |
| Bun | >= 1.3.13 | Frontend package manager, Vite runner, and runtime dependency for agent tooling. |
| Tauri CLI v2 | from `devDependencies` | Invoked through `bun run dev`, `bun run build`, and related scripts. |
| Git | recent | Required by development workflows and several built-in tools. |
| Native build tools | platform-specific | Needed for SQLite, TLS, libgit2, WebKit/WebView dependencies, and bundled native crates. |

Platform notes:

- Windows: MSVC C++ build tools and WebView2 runtime.
- macOS: Xcode Command Line Tools.
- Linux: `build-essential cmake clang pkg-config perl git`; desktop builds also need WebKitGTK 4.1 development headers.

## Install

```bash
git clone <repo-url> nomifun-tauri
cd nomifun-tauri
bun install
cargo check --workspace
```

The root `package.json` has one Bun workspace: `ui/`. Rust crates are resolved by
the root `Cargo.toml`.

## Development Loops

| Command | Use when | What runs |
| --- | --- | --- |
| `bun run dev:ui` | UI-only work that can tolerate missing API calls | Vite on `http://localhost:5173`; no backend. |
| `bun run dev:web` | Browser + backend iteration with auth disabled | `NOMI_CHANNEL=dev` `nomifun-web --port 8787 --dist ui/dist --insecure-no-auth` plus Vite dev server. |
| `bun run serve:web` | Running the production-style web host from source | `nomifun-web` on `http://127.0.0.1:8787`; serves built `ui/dist`; auth on by default. |
| `bun run dev` | Desktop/Tauri work | `NOMI_CHANNEL=dev` Tauri shell, Vite, and embedded backend under the desktop local-trust policy. |

`serve:web` expects a built SPA:

```bash
bun run build:ui
bun run serve:web
```

`dev:web` is a convenience loop that starts API and UI together. It uses
`--insecure-no-auth`, so keep it on localhost or an isolated network.

The Rust-backed development loops (`dev`, `dev:web`, and `build:fast`) use the
`dev` build channel and therefore default to the `Nomi-dev` data directory.
Production-style `serve:web` and release builds remain on the stable `Nomi`
directory. Use `bun run seed:dev` when dev needs a snapshot of stable state.

The desktop loop does **not** use the old Electron process model. The Tauri
shell links `nomifun-app`, starts the backend in-process on a free localhost
port, injects `window.__backendPort` and `window.__nomiLocalTrust`, and the
renderer presents that per-boot trust secret on every request.

## Verification

| Command | Coverage |
| --- | --- |
| `cargo check --workspace` | All Rust crates and app hosts compile. |
| `cargo test -p <crate>` | Focused Rust tests for one crate. |
| `bun run test:crate <crate> [cargo args]` | Focused crate tests with the build-cache guard. |
| `bun run test:core` | Workspace regression without desktop-only features. |
| `bun run test:desktop` | Desktop shell tests without watching `ui/dist` resources. |
| `bun run test` | Full regression including desktop features and doctests. |
| `bun run typecheck` | Renderer TypeScript. |
| `bun run check:i18n` | Generated i18n key types are up to date. |
| `bun run check:theme` | Theme token contract. |
| `bun run help --check` | Root script help output is current. |
| `bun run build:ui` | Production Vite build. |
| `bun run build` | Tauri desktop release bundle for the current OS. |

For a broad pre-PR check, run:

```bash
cargo check --workspace
bun run typecheck
bun run check:i18n
bun run check:theme
bun run help --check
```

## Backend CLI

`nomifun-app` still ships a standalone `nomicore` binary. The app hosts do not
spawn it, but it is useful for diagnostics, stdio MCP bridges, and public
capability calls.

Current subcommands:

- `mcp-requirement-stdio`
- `mcp-knowledge-stdio`
- `mcp-gateway-stdio`
- `mcp-open-stdio`
- `mcp-computer-stdio`
- `mcp-browser-stdio`
- `terminal-hook --event <kind>`
- `doctor`
- `tools`
- `call <name> [json-args]`
- `agent "<goal>"`

When agents fail to launch, start with:

```bash
cargo run -p nomifun-app --bin nomicore -- doctor
```

It probes installed agent CLIs from the same PATH shape the backend uses and
prints a table to stdout.

## Data and Work Directories

All hosts share the same unset default data directory:

- Windows: `%LOCALAPPDATA%\NomiFun\Nomi`
- macOS: `~/Library/Application Support/NomiFun/Nomi`
- Linux: `$XDG_DATA_HOME/NomiFun/Nomi` or `~/.local/share/NomiFun/Nomi`

The data dir contains SQLite state, logs, Bun runtime cache, extension data,
agent state, and other persistent local state. The backend takes an exclusive
`server.lock` before opening the database, so two live backends cannot use the
same data directory at the same time.

For isolated development, set an explicit directory:

```bash
NOMIFUN_DATA_DIR=/tmp/nomifun-dev bun run serve:web
NOMIFUN_DATA_DIR=/tmp/nomifun-dev bun run dev
```

Desktop app semantics append the channel-specific `Nomi` leaf; web and
`nomicore` take the env value literally. See
[`../reference/configuration.md`](../reference/configuration.md) before relying
on this in automation.

`NOMIFUN_WORK_DIR` controls where conversation workspaces are created. If unset,
the backend falls back to the data dir.

## Logs

Logs go to stdout and to `<data-dir>/logs/nomicore.log`. Use:

```bash
NOMIFUN_LOG_LEVEL='info,nomifun_mcp=trace' bun run serve:web
```

or pass `--log-level` to `nomicore` / `nomifun-web` directly. The value is a
`tracing_subscriber::EnvFilter` directive.

## Where to Read Next

- [`project-structure.md`](project-structure.md) for the repo map.
- [`data-and-identifier-standards.md`](data-and-identifier-standards.md) before
  changing database schema, IDs, logical references, reset behavior, or
  backup/restore contracts.
- [`../architecture/backend-crates.md`](../architecture/backend-crates.md) for crate ownership.
- [`../architecture/frontend.md`](../architecture/frontend.md) for routes and host adapters.
- [`building-and-packaging.md`](building-and-packaging.md) for release artifacts.
