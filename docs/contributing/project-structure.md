# Project Structure

This is the authoritative repo map for **NomiFun**. It tells you which
directory holds what, what each Rust crate is responsible for, and the one
architectural rule that keeps the agent engine extractable. For the
deep-dive on backend layering see
[`../architecture/backend-crates.md`](../architecture/backend-crates.md);
for the runtime story (how the two app hosts boot the same backend) see
[`../architecture/overview.md`](../architecture/overview.md).

## Top-level layout

```
nomifun-tauri/
├── apps/
│   ├── web/                      nomifun-web bin: standalone server (API + SPA)
│   └── desktop/                  nomifun-desktop bin: Tauri shell (embedded backend)
├── crates/
│   ├── agent/                    15 nomi-* crates — the AI agent engine
│   ├── backend/                  29 nomifun-* crates — the HTTP/WS backend
│   └── shared/                   2 genuine cross-layer crates
├── ui/                           React SPA (Vite + UnoCSS), the only Bun workspace
│   ├── src/common/               cross-host code: API clients, types, utils
│   ├── src/platform/             tiny host bridge (storage / logger / theme)
│   ├── src/renderer/             pages, components, hooks, services, styles
│   ├── public/                   static assets
│   ├── index.html                Vite entry
│   └── vite.config.ts            Vite config
├── docs/
│   ├── getting-started/          install + first run
│   ├── guides/                   task-focused user/operator guides
│   ├── architecture/             current runtime, crates, and frontend architecture
│   ├── reference/                configuration, API surface, troubleshooting
│   ├── contributing/             contributor standards and this repo map
│   ├── continuity/               historical decisions, handoff context, release audits
│   ├── skills/                   exported instructions for external agents
│   └── images/                   screenshot manifest and referenced assets
├── packaging/
│   └── linux/                    nomifun-web.service systemd unit + README
├── Cargo.toml                    Rust workspace (resolver "3", edition 2024)
├── package.json                  root scripts (dev:ui/build, web, dev/build)
├── Dockerfile                    nomifun-web container image
├── docker-compose.yml            single-service compose for the web host
├── Caddyfile                     optional TLS reverse proxy (commented in compose)
├── README.md                     project introduction
└── STATUS.md                     current technical status snapshot
```

The Cargo workspace members are exactly:

```toml
[workspace]
resolver = "3"
members = ["crates/agent/*", "crates/backend/*", "crates/shared/*", "apps/web", "apps/desktop"]
```

`crates/shared/*` is now active. Keep new shared crates rare: if a crate belongs
only to the backend or only to the agent engine, keep it in that owning group.

## App hosts

| Path | Binary | Role |
| --- | --- | --- |
| [`apps/web`](../../apps/web) | `nomifun-web` | Standalone server. Boots the unified backend in-process and serves the built SPA from the same port. Authentication on by default; `--insecure-no-auth` opts back into the desktop trust model. Replaces the old Node `web-host`. |
| [`apps/desktop`](../../apps/desktop) | `nomifun-desktop` | Tauri shell. Picks a free localhost port, starts the same backend in-process, injects `window.__backendPort` and `window.__nomiLocalTrust`, and loads the SPA into the WebView. Single-instance + dialog + notification + deep-link + updater plugins registered. |

Both hosts link `nomifun-app` directly — there is no spawned `nomicore`
binary in either flow. The `nomicore` binary still exists as the
`[[bin]]` of `nomifun-app` for headless / CI use and for the
`nomicore doctor` self-check.

## Crate groups

The Rust crates are grouped by origin and naming convention. The grouping
is the migration unit: each top-level directory under `crates/` corresponds
to a future independent repository.

| Directory | Prefix | Count | Role | Future repo |
| --- | --- | --- | --- | --- |
| [`crates/agent/`](../../crates/agent) | `nomi-*` | 15 | AI agent engine. Self-contained — no dependency on any `nomifun-*` crate. | historical extraction target |
| [`crates/backend/`](../../crates/backend) | `nomifun-*` | 32 | HTTP/WS server, data layer, auth, sessions, cron, knowledge, terminal, companion, public gateway, ... | historical extraction target |
| [`crates/shared/`](../../crates/shared) | mixed | 2 | Cross-layer utilities used by both sides. | shared |

## The agent-layer seam

Backend feature code should normally go through
[`crates/backend/nomifun-ai-agent`](../../crates/backend/nomifun-ai-agent)
when it needs agent types or agent execution. Most backend crates import
agent-facing types via
`nomifun_ai_agent::{nomi_config, nomi_types, RequirementSink}`.

The current workspace has feature-gated direct-dependency exceptions in
`nomifun-app` and `nomifun-gateway` for browser/computer-use bridge tooling.
When you add a new backend crate that needs an agent type:

1. Prefer not to add `nomi-* = ...` to your `Cargo.toml`.
2. Re-export what you need through `nomifun-ai-agent` or use what is already
   re-exported there.
3. Consume it via `use nomifun_ai_agent::nomi_types::...;` etc.
4. If a direct dependency is required for a bridge/facade, gate it behind a
   feature and document the exception in the crate manifest and architecture
   docs.

Why: this keeps the agent engine mostly independent and prevents feature crates
from silently tying themselves to engine internals.

## `crates/agent/` — 15 `nomi-*` crates (the AI agent engine)

| Crate | One-line role |
| --- | --- |
| [`nomi-types`](../../crates/agent/nomi-types) | Pure, provider-neutral data types shared across all `nomi-*` crates. No dependencies on other agent crates. |
| [`nomi-protocol`](../../crates/agent/nomi-protocol) | JSON stream protocol for host ↔ agent communication: events (agent → host), commands (host → agent), approval manager. |
| [`nomi-compact`](../../crates/agent/nomi-compact) | Conversation-window compaction: fold / json / level / sanitize / TOON formatting. |
| [`nomi-config`](../../crates/agent/nomi-config) | Runtime configuration layer — `Config`, `ProviderCompat`, auth, hooks, provider-specific configs, file-cache. |
| [`nomi-providers`](../../crates/agent/nomi-providers) | LLM provider clients: Anthropic, Bedrock, OpenAI, Vertex; shared retry / streaming. |
| [`nomi-tools`](../../crates/agent/nomi-tools) | Built-in tools registry: bash, edit, glob, grep, read, tool-search, file-cache. |
| [`nomi-mcp`](../../crates/agent/nomi-mcp) | MCP client used by the agent: config, manager, protocol, tool-proxy, transports. |
| [`nomi-skills`](../../crates/agent/nomi-skills) | Skills system: discovery, frontmatter, loader, executor, hooks, conditional / context modifiers, bundled. |
| [`nomi-memory`](../../crates/agent/nomi-memory) | Long-term cross-session memory — preferences, feedback, project context, external references. |
| [`nomi-agent`](../../crates/agent/nomi-agent) | Core engine: turn execution, bootstrap, commands, compaction, confirmation, delegation, output sinks. |
| [`nomi-cli`](../../crates/agent/nomi-cli) | Standalone `nomi` binary that drives the engine without a host process. |
| [`nomi-computer`](../../crates/agent/nomi-computer) | Desktop computer-use tool implementation. |
| [`nomi-a11y`](../../crates/agent/nomi-a11y) | Accessibility helpers used by computer-use flows. |
| [`nomi-browser-engine`](../../crates/agent/nomi-browser-engine) | Self-hosted browser/CDP automation engine. |
| [`nomi-browser`](../../crates/agent/nomi-browser) | Browser-use tool layer. |

## `crates/backend/` — 29 `nomifun-*` crates (the backend)

| Crate | One-line role |
| --- | --- |
| [`nomifun-common`](../../crates/backend/nomifun-common) | Shared primitives: `AppError`, enums, ID generation, AES-GCM crypto, timestamps, pagination, common constants. |
| [`nomifun-assets`](../../crates/backend/nomifun-assets) | Backend-served static logo assets (`include_dir!`). |
| [`nomifun-db`](../../crates/backend/nomifun-db) | SQLite layer: `init_database`, embedded migrations, models, repository traits + sqlx implementations. |
| [`nomifun-api-types`](../../crates/backend/nomifun-api-types) | Every HTTP request/response DTO and the `WebSocketMessage` envelope; the renderer's TS types mirror this crate. |
| [`nomifun-realtime`](../../crates/backend/nomifun-realtime) | WebSocket connection manager, broadcaster, token-validated upgrade handler, message router. |
| [`nomifun-runtime`](../../crates/backend/nomifun-runtime) | Embeds bun (zstd-compressed) at build time, extracts to OS cache on first run; `enhance_process_path` merge for child processes. |
| [`nomifun-auth`](../../crates/backend/nomifun-auth) | JWT auth, bcrypt, login / refresh / setup routes, CSRF double-submit, security headers, rate limiting, `CurrentUser` extractor. |
| [`nomifun-system`](../../crates/backend/nomifun-system) | System services: provider management, model fetching, settings, version checks, Bedrock probe. |
| [`nomifun-file`](../../crates/backend/nomifun-file) | Filesystem operations: read/write, path safety, file watching, snapshots, zip. |
| [`nomifun-office`](../../crates/backend/nomifun-office) | Office-document preview, format conversion, proxy, snapshot management. |
| [`nomifun-shell`](../../crates/backend/nomifun-shell) | OS shell integration: opener, tool detection, speech-to-text. |
| [`nomifun-ai-agent`](../../crates/backend/nomifun-ai-agent) | **The single bridge to `crates/agent/`.** Agent factory, runtime registry, runtime handles, and ACP session persistence; re-exports `nomi_config` / `nomi_types` / `RequirementSink`. |
| [`nomifun-mcp`](../../crates/backend/nomifun-mcp) | MCP server config, multi-agent sync adapters, OAuth, connection testing. |
| [`nomifun-conversation`](../../crates/backend/nomifun-conversation) | Conversation + message CRUD with streaming relay, ACP error recovery, response middleware. |
| [`nomifun-extension`](../../crates/backend/nomifun-extension) | Extension registry: manifest parsing, hub installer, skill scanning, lifecycle hooks. |
| [`nomifun-channel`](../../crates/backend/nomifun-channel) | External channel integration: plugin system, pairing handshake, per-session messaging, formatter. |
| [`nomifun-agent-execution`](../../crates/backend/nomifun-agent-execution) | Persistent single- and multi-Agent execution aggregate: participants, steps, attempts, scheduling, decisions, recovery, and events. |
| [`nomifun-cron`](../../crates/backend/nomifun-cron) | Scheduled-job engine: cron scheduler, executor, lifecycle event emitter, busy-guard. |
| [`nomifun-requirement`](../../crates/backend/nomifun-requirement) | Requirements Platform: CRUD store + persistent AutoWork runner + completion notifier hooks. |
| [`nomifun-idmm`](../../crates/backend/nomifun-idmm) | Intelligent Decision-Making Mode: per-session supervision keeping agent / terminal sessions alive through provider faults. |
| [`nomifun-webhook`](../../crates/backend/nomifun-webhook) | Webhook management + AutoWork completion notifications (Lark/飞书 custom bots), per-tag bindings. |
| [`nomifun-terminal`](../../crates/backend/nomifun-terminal) | PTY-backed terminal sessions managed alongside conversations; streams output via the realtime broadcaster. |
| [`nomifun-preset`](../../crates/backend/nomifun-preset) | Reusable launch configurations; merges builtin + user + extension presets, resolves target-specific immutable snapshots, and serves `/api/presets`. |
| [`nomifun-knowledge`](../../crates/backend/nomifun-knowledge) | Knowledge bases, bound-base state, and scoped knowledge MCP search. |
| [`nomifun-companion`](../../crates/backend/nomifun-companion) | Desktop companions, figures, shared memory, and companion-bound state. |
| [`nomifun-gateway`](../../crates/backend/nomifun-gateway) | Platform Gateway MCP registry and process-issued capability tools. |
| [`nomifun-public`](../../crates/backend/nomifun-public) | Public `/mcp`, `/mcp-agent`, and `/v1` front doors with companion-token auth. |
| [`nomifun-secret`](../../crates/backend/nomifun-secret) | Per-companion browser-use secret storage. |
| [`nomifun-app`](../../crates/backend/nomifun-app) | Application crate: assembles every domain crate into the axum server with DI + middleware. Ships the `nomicore` binary. |

> The full backend layering — request lifecycle, who owns which routes, the
> agent seam in detail — is in
> [`../architecture/backend-crates.md`](../architecture/backend-crates.md).

## `apps/web` and `apps/desktop`

Both app crates are thin: they parse a small CLI, call into `nomifun-app`'s
public boot helpers, and own the shape of the host process.

```text
apps/web/src/main.rs         ~165 lines
  init runtime → init data layer → AppServices → create_router →
  ServeDir(ui/dist) fallback → axum::serve

apps/desktop/src/main.rs     ~250 lines
  pick free port → init runtime → spawn embedded backend on a tokio
  thread → tauri::Builder with single-instance/dialog/notification/
  deep-link/updater plugins → window init-script injects window.__backendPort
```

`nomifun-app` exposes the boot entry as a library: `bootstrap`, `cli`,
`commands`, and a `run_embedded_server` helper, plus `AppServices` and
`create_router`. The `nomicore` bin is just one of three consumers.

## `ui/` — the React SPA

The frontend is a single Bun workspace, built with **plain Vite + UnoCSS**
(no `electron-vite`).

| Path | What lives here |
| --- | --- |
| [`ui/src/common/`](../../ui/src/common) | Cross-host code reused regardless of shell: `adapter/` (HTTP / WS bridges), `api/`, `chat/`, `config/`, `platform/`, `types/`, `update/`, `utils/`, plus the package barrel `index.ts`. |
| [`ui/src/platform/`](../../ui/src/platform) | The tiny host-bridge layer: `bridge.ts`, `logger.ts`, `storage.ts`, `theme.ts`. The renderer never imports Tauri / Electron APIs directly — it goes through this layer. |
| [`ui/src/renderer/`](../../ui/src/renderer) | The app itself: `pages/`, `components/`, `hooks/`, `services/`, `styles/`, `utils/`, `assets/`, `main.tsx`, `index.html`, `types.d.ts`. |
| [`ui/src/common/utils/shims/`](../../ui/src/common/utils/shims) | Stubs for renderer-safe compatibility paths and build-time aliases. |
| [`ui/public/`](../../ui/public) | Static assets copied straight to `ui/dist/` (icons, etc.). |
| `ui/vite.config.ts` | Vite config, including the externalized-shim aliases. |
| `ui/uno.config.ts` | UnoCSS preset config. |
| `ui/tsconfig.json` | TypeScript paths and aliases that match the directory shape above. |

## Other references

| Path | Contents |
| --- | --- |
| [`STATUS.md`](../../STATUS.md) | Current technical status snapshot. |
| [`apps/desktop/updater/README.md`](../../apps/desktop/updater/README.md) | Auto-update scaffold and release-key notes. |
| [`packaging/linux/README.md`](../../packaging/linux) | Headless Linux deployment: Docker (recommended), or native binary + systemd unit. |

## Where artifacts go

| Build | Output |
| --- | --- |
| `bun run build:ui` | `ui/dist/` (the SPA) |
| `cargo build -p nomifun-web` | `target/<profile>/nomifun-web` |
| `cargo build -p nomifun-app --bin nomicore` | `target/<profile>/nomicore` |
| `bun run build` | `target/<profile>/bundle/<format>/...` (per-OS Tauri bundles) |
| `docker compose build` | local image `nomifun-web:local` |

`target/`, `ui/dist/`, `data/`, and `node_modules/` are all gitignored. See
[`building-and-packaging.md`](building-and-packaging.md) for the per-output
details.
