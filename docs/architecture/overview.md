# Architecture Overview

NomiFun is built around a single principle: **one Rust backend, two host modes,
one frontend**. Whether you launch the desktop product **NomiFun** or self-host the
web server, the same `axum` HTTP/WS server (`nomifun-app`, binary `nomicore`)
executes inside the host process. The React 19 SPA in `ui/` is the only client,
and it always speaks plain HTTP and WebSocket — no Electron preload, no Tauri
custom protocol.

This document is the map. The sibling documents drill into the parts:

- [`backend-crates.md`](backend-crates.md) — the 32 `nomifun-*` backend crates.
- [`agent-engine.md`](agent-engine.md) — the 15 `nomi-*` agent crates.
- [`agent-execution.zh.md`](agent-execution.zh.md) — the unified persistent AgentExecution model.
- [`frontend.md`](frontend.md) — the React SPA, adapter layer, routing.
- [`communication.md`](communication.md) — HTTP / WebSocket / Tauri IPC / ACP / MCP.
- [`data-and-storage.md`](data-and-storage.md) — SQLite, workspaces, runtimes.
- [`id-system.md`](id-system.md) — the v3 technical-key, business-ID, internal-row, and
  logical-reference contract.

## The two-host model

```
                        ┌─────────────────────────────────────┐
                        │  ui/   React 19 SPA (Vite build)    │
                        │  HashRouter · SWR · Arco · UnoCSS   │
                        │  http://127.0.0.1:<port>/api  +  /ws│
                        └─────────────────────────────────────┘
                                   ▲                 ▲
                          HTTP/REST│        WebSocket│  /ws
                                   │                 │
   ┌───────────────── desktop ─────┴────┐  ┌─────── web ───────┴──────┐
   │ apps/desktop  (nomifun-desktop)    │  │ apps/web  (nomifun-web)  │
   │  Tauri 2 shell · WebView2/WKWebKit │  │ standalone axum server   │
   │  ─ thread "nomifun-backend"        │  │  serves /api  +  /ws     │
   │    └ tokio · nomifun_app embedded  │  │  + ServeDir(ui/dist) SPA │
   │  picks free localhost port,        │  │  port 8787 (default)     │
   │  injects window.__backendPort      │  │  authenticated by default│
   │  uses TrustLocalToken auth         │  │  --insecure-no-auth opts │
   │  injects x-nomi-local-trust        │  │  into no-auth mode       │
   │  Tauri commands for desktop shell  │  │  serves SPA as fallback  │
   └────────────────────────────────────┘  └──────────────────────────┘
                                   │                 │
                                   ▼                 ▼
                        ┌─────────────────────────────────────┐
                        │  nomifun-app  (binary nomicore)     │
                        │  composition root · axum router     │
                        │  bootstrap → data layer → services  │
                        │  /api · /ws · public /mcp · /v1     │
                        └─────────────────────────────────────┘
                          │                       │
                          ▼                       ▼
              ┌─────────────────────┐   ┌─────────────────────┐
              │  nomifun-* (32)     │   │  nomi-* (15)         │
              │  backend crates     │◀─▶│  agent engine crates │
              │  data, auth, MCP,   │   │  via the SEAM:       │
              │  conversation, etc. │   │  nomifun-ai-agent     │
              └─────────────────────┘   └─────────────────────┘
                          │
                          ├─▶ SQLite (sqlx)         see data-and-storage.md
                          ├─▶ ACP agent CLIs         see agent-engine.md
                          ├─▶ MCP stdio bridges      see communication.md
                          └─▶ bundled bun runtime    see data-and-storage.md
```

## How a request flows

A typical user message — "send a chat to my Claude agent in conversation X" —
crosses every layer in the diagram. The trace below names the real types and
files that participate.

```
1. UI keypress → React handler
   ui/src/renderer/pages/conversation/...
   calls ipcBridge.conversation.sendMessage.invoke(...)
   (a thin wrapper produced by the adapter factory in ui/src/common/adapter)
2. httpBridge → fetch
   ui/src/common/adapter/httpBridge.ts
   POST http://127.0.0.1:<port>/api/conversations/{id}/messages
   In WebUI mode, the CSRF cookie is echoed into x-csrf-token (double-submit).
3. axum router (composition root)
   crates/backend/nomifun-app/src/router/  — assembled in create_router()
   middlewares: trace, body-limit, CORS, auth, CSRF, rate-limit, response wrapper
4. Conversation service
   crates/backend/nomifun-conversation/src/service.rs
   persists the message, looks up the conversation's bound agent
5. Agent seam
   crates/backend/nomifun-ai-agent  — the primary backend bridge to nomi-*
   AgentRegistry resolves the Agent kind; AgentRuntimeRegistry reuses its Conversation runtime
6. Agent turn
   nomi-agent  drives the engine: providers (anthropic/openai/bedrock/vertex),
   tools (bash/read/write/...), MCP servers, skills, plan/confirm/output sinks
   For ACP-protocol agents (Claude Code, Codex, Gemini CLI, ...), the backend
   speaks ACP over stdio to a child process spawned with the bundled runtime
7. Streaming back to the UI
   nomifun-realtime  broadcasts each token as a WS event over /ws
   ui/src/common/adapter/httpBridge.ts ensureWs() routes events to listeners
8. UI renders the streaming reply (react-markdown + KaTeX + mermaid)
```

## The three crate groups

The Cargo workspace (root [`Cargo.toml`](../../Cargo.toml), `resolver = "3"`,
`edition = "2024"`) is grouped into three folders so the boundaries are visible
on disk, not just in package names:

| Folder | Purpose | Crate prefix | Count |
| --- | --- | --- | --- |
| `crates/agent/` | AI engine — providers, tools, sessions, MCP, skills, computer/browser use | `nomi-*` | 15 |
| `crates/backend/` | The HTTP/WS server, data, auth, features, public capability gateway | `nomifun-*` | 32 |
| `crates/shared/` | Cross-layer utilities used by both groups | mixed | 3 |

The agent group is **self-contained** — no `nomi-*` crate references any
`nomifun-*` crate, the workspace root, or frameworks like Tauri / sqlx / axum.
The reverse direction normally goes through `nomifun-ai-agent`, which re-exports
`nomi_config`, `nomi_types`, and `RequirementSink` for backend consumers.
`nomifun-app` and `nomifun-gateway` have feature-gated direct dependencies for
browser/computer bridge surfaces; those are documented exceptions, not the
default pattern.

## What lives where

```
nomifun-tauri/
├─ apps/
│   ├─ desktop/   nomifun-desktop  (Tauri 2 shell, this is "NomiFun" the product)
│   └─ web/       nomifun-web      (standalone server: /api + SPA on one port)
├─ crates/
│   ├─ agent/     15 nomi-* crates  → see agent-engine.md
│   ├─ backend/   32 nomifun-* crates → see backend-crates.md
│   └─ shared/    3 shared crates
├─ ui/            React 19 + Vite 6 + Arco + UnoCSS  → see frontend.md
└─ docs/
    ├─ architecture/   (this folder)
    └─ specs/          dated engineering design specs
```

## Brand and identifiers

- **NomiFun** — the desktop product and project / brand wordmark (camelCase,
  capital N and F). "NomiFun is an AI Workstation (desktop app plus
  self-hosted web server)."
- The lowercase `nomifun` is reserved for technical identifiers only —
  the npm/JS package id, the Rust crate prefix `nomifun-*`, the Tauri bundle
  identifier `com.nomifun.desktop`, environment variables `NOMIFUN_*`, and
  repository / directory names.

## Hosts at a glance

| Aspect | Desktop (`nomifun-desktop`) | Web (`nomifun-web`) |
| --- | --- | --- |
| Binary | `nomifun-desktop` (Tauri shell) | `nomifun-web` (axum server) |
| Backend | embedded in-process (own thread + tokio runtime) | embedded in-process |
| Auth mode | `TrustLocalToken`: the desktop webview receives a per-boot secret and sends it as `x-nomi-local-trust` | required by default; opt-out via `--insecure-no-auth` |
| Port | a free localhost port chosen at boot (`bind 127.0.0.1:0`) | `127.0.0.1:8787` (configurable via `--host`/`--port`) |
| Backend port reaches the SPA via | initialization script `window.__backendPort = <p>` | same-origin (`/api` and `/ws` served on the same port as the SPA) |
| Static SPA | bundled into the Tauri app (`tauri.conf.json` distDir) | served by `tower_http::services::ServeDir` from `ui/dist` |
| OS-shell features | window controls, deep-link, updater, autostart, dialog, notification, single-instance | none — browser is the host |
| Tauri commands | update check, companion-window sync, WebUI LAN status/start/stop, keep-awake, tray labels | not applicable |

The desktop also has an optional LAN WebUI listener controlled by Tauri commands
(`webui_start`, `webui_stop`, `webui_get_status`). That listener is separate
from the loopback listener used by the desktop's own webview.

The desktop binary's `main.rs` ([`apps/desktop/src/main.rs`](../../apps/desktop/src/main.rs))
is intentionally short — the bulk of the logic is `nomifun_app::run_embedded_server`.
The web binary ([`apps/web/src/main.rs`](../../apps/web/src/main.rs)) reuses the
same boot helpers (`init_environment`, `init_data_layer`, `AppServices::from_config`,
`create_router`) and adds the SPA fallback plus first-run admin provisioning
(`ensure_admin_credentials`).

The full app router also exposes companion-token authenticated public fronts at
`/mcp`, `/mcp-agent`, and `/v1`. These are intentionally separate from the
normal `/api` browser-auth tree and are mounted in
[`crates/backend/nomifun-app/src/router/routes.rs`](../../crates/backend/nomifun-app/src/router/routes.rs).
