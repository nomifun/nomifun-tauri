# Configuration Reference

Every flag and environment variable NomiFun reads, with defaults and the file that owns each one. Values are taken from the source — if a setting is not in this page it does not exist.

NomiFun ships **one** Rust backend (`nomifun-app`, binary `nomicore`) and two hosts that embed it:

- `nomifun-desktop` — the Tauri desktop shell. Boots the backend under `AuthPolicy::TrustLocalToken` on a chosen loopback port and injects a per-boot trust secret into its own WebView.
- `nomifun-web` — the standalone web/server host. Boots the same backend in **authenticated** mode by default and serves the SPA on the same port.

Both hosts share the same configuration surface for the backend; the per-host CLIs only override the bits each one owns.

## `nomifun-web` flags and environment variables

Source: [`apps/web/src/main.rs`](../../apps/web/src/main.rs).

| Flag | Env var | Default | Purpose |
|---|---|---|---|
| `--host` | `NOMIFUN_WEB_HOST` | `127.0.0.1` | IP to bind on. `0.0.0.0` accepts LAN/VPN/public traffic; pre-seed or complete first-run setup before broad exposure. Hostnames are not parsed; bad input fails fast at startup. |
| `--port` | `NOMIFUN_WEB_PORT` | `8787` | TCP port. Serves the API, the WebSocket at `/ws`, and the SPA from one socket. |
| `--data-dir` | `NOMIFUN_DATA_DIR` | per-user app-data dir | Backend data directory (SQLite database, agent state, logs, Bun cache). Defaults to the per-user location shared by every host — `%LOCALAPPDATA%\NomiFun\Nomi` on Windows, `~/Library/Application Support/NomiFun/Nomi` on macOS, `$XDG_DATA_HOME/NomiFun/Nomi` on Linux. Override with the flag or `NOMIFUN_DATA_DIR` (taken literally, no suffix); use an absolute path in production. |
| `--dist` | `NOMIFUN_WEB_DIST` | `../../ui/dist` | Directory containing the built SPA. Set this explicitly when deploying outside the repo. |
| `--admin-user` | `NOMIFUN_ADMIN_USERNAME` | `admin` | Username used when pre-seeding the first admin. Ignored once an admin exists. |
| `--admin-password` | `NOMIFUN_ADMIN_PASSWORD` | — | Pre-seeds the first admin password at boot, skipping interactive setup. Ignored once an admin exists. |
| `--insecure-no-auth` | `NOMIFUN_WEB_INSECURE_NO_AUTH` | `false` | DANGER. Disables authentication entirely (desktop-style local mode). Only use on loopback or a fully trusted private network. |

Boolean envs accept `1`, `true`, `yes`, `on` (case-insensitive).

## `nomicore` (backend) flags

Source: [`crates/backend/nomifun-app/src/cli.rs`](../../crates/backend/nomifun-app/src/cli.rs).

These are the flags exposed by the standalone `nomicore` binary. The two hosts construct a defaulted `Cli` and override only what they own — so the same flags apply when the backend is run on its own.

| Flag | Default | Purpose |
|---|---|---|
| `--host` | `127.0.0.1` (`DEFAULT_HOST`) | Host address to listen on. |
| `--port` | `25808` (`DEFAULT_PORT`) | Port to listen on. |
| `--data-dir` | per-user app-data dir | Database + file storage root. Bound to the `NOMIFUN_DATA_DIR` env (literal value) via clap; with neither set it resolves `default_data_dir()` — the same per-user location all hosts share. |
| `--work-dir` | (none) | Working directory for conversation workspaces. Falls back to `NOMIFUN_WORK_DIR` env, then to the data dir itself. |
| `--app-version` | crate version | Host application version reported to the extension engine for compatibility checks. |
| `--local` | `false` | No-auth local mode for standalone `nomicore`. `nomifun-web --insecure-no-auth` maps to the same policy. The desktop shell does not use this flag; it uses `TrustLocalToken` instead. |
| `--log-dir` | `<data-dir>/logs` | Directory for rolling daily log files. |
| `--log-level` | `info` | Log level filter. Supports per-target overrides — e.g. `info,nomifun_mcp=trace`. |

Subcommands (used internally by the agent CLI bridge and for diagnostics):

| Subcommand | Purpose |
|---|---|
| `mcp-requirement-stdio` | MCP stdio server for AutoWork requirement declaration tools. |
| `mcp-knowledge-stdio` | MCP stdio server for per-session knowledge search. |
| `mcp-gateway-stdio` | MCP stdio server for Desktop Gateway tools. |
| `mcp-open-stdio` | MCP stdio server exposing a reliable OS `open` tool. |
| `mcp-computer-stdio` | MCP stdio server exposing desktop computer-use tools. |
| `mcp-browser-stdio` | MCP stdio server exposing browser-use tools. |
| `terminal-hook --event <kind>` | One-shot terminal lifecycle hook relay. |
| `doctor` | Self-check: hydrate the agent registry, probe every CLI on `$PATH`, print a per-agent availability table. |
| `tools` | List public Remote capability names and descriptions as JSON. |
| `call <name> [json-args]` | Invoke a public Remote capability on a running instance via `/v1`. |
| `agent "<goal>"` | Convenience wrapper over the `nomi_agent_run` capability. |

## Shared environment variables

These are read by the backend regardless of which host embeds it.

| Env var | Read by | Effect |
|---|---|---|
| `NOMIFUN_DATA_DIR` | all hosts | Source of truth for the backend data directory when the host wants to honour it. The desktop shell appends `/Nomi`: with the env set the dir is `$NOMIFUN_DATA_DIR/Nomi`; with it unset the dir is the per-user app-data default (see [below](#data-directory-and-work-directory-semantics)). The standalone web host and the `nomicore` binary use it literally as the default for `--data-dir` (no extra suffix). |
| `NOMIFUN_WORK_DIR` | `nomicore` | Fallback for `--work-dir` (per-conversation workspace root). |
| `JWT_SECRET` | `nomifun-app` | Secret used to sign session JWTs. See [Auth secret resolution](#auth-secret-resolution) for the resolution order. |
| `NOMIFUN_HTTPS` | `nomifun-auth::CookieConfig` | When truthy, session and CSRF cookies get the `Secure` flag and `SameSite=Strict`. Set it whenever the app is reached over HTTPS (TLS reverse proxy, etc.). Default is `false` → no `Secure` flag, `SameSite=Lax`. |
| `SHELL` | agent engine (Linux/macOS) | Shell used when the agent engine spawns child processes. On Linux servers under systemd, set this explicitly (the system account often has no `$SHELL`). |
| `NOMIFUN_URL` | `nomicore call`, `nomicore agent` | Base URL for a running instance when invoking Remote capabilities. |
| `NOMIFUN_COMPANION_TOKEN` | `nomicore call`, `nomicore agent` | Companion access token used against `/v1` Remote capability routes. |

There is no `SENTRY_DSN` integration: the codebase does not read that environment variable.

## Backend constants

Source: [`crates/backend/nomifun-common/src/constants.rs`](../../crates/backend/nomifun-common/src/constants.rs). These are compile-time values, not environment variables — they are listed here so operators know the limits.

| Constant | Value | Used for |
|---|---|---|
| `DEFAULT_HOST` | `127.0.0.1` | Default `--host` for `nomicore`. |
| `DEFAULT_PORT` | `25808` | Default `--port` for `nomicore`. (The web host overrides this to `8787`.) |
| `BODY_LIMIT` | `10 MiB` | Default request body limit applied to every route. Routes that need more (e.g. `/api/fs/upload`) install their own larger limit. |
| `UPLOAD_MAX_SIZE` | `30 MiB` | Cap for the file upload route (`/api/fs/upload`). |
| `REMOTE_IMAGE_MAX_SIZE` | `5 MiB` | Cap for downloading a remote image referenced in chat. |
| `COOKIE_NAME` | `nomifun-session` | Session cookie. |
| `CSRF_COOKIE_NAME` | `nomifun-csrf-token` | CSRF cookie (NOT HttpOnly — JavaScript reads it). |
| `CSRF_HEADER_NAME` | `x-csrf-token` | Header that mirrors the CSRF cookie value (Double Submit Cookie). |
| `COOKIE_MAX_AGE_DAYS` | `30` | Cookie `Max-Age`. |
| `SESSION_EXPIRY` | `24h` | JWT validity window before refresh is required. |
| `HEARTBEAT_INTERVAL_MS` / `HEARTBEAT_TIMEOUT_MS` | `30000` / `60000` | WebSocket heartbeat ping/pong. |

## Data directory and work directory semantics

- `data-dir` holds the SQLite database (`nomifun-backend.db*`), per-agent state, the Bun cache, log files, and any embedded extension data. Treat it like any other database — back it up and restrict permissions. Sharing it between two running backends is prevented mechanically (see the server lock below).
- All three hosts (`nomifun-desktop`, `nomifun-web`, the standalone `nomicore` binary) resolve the **same default** data dir via `nomifun_app::cli::default_data_dir()`: `%LOCALAPPDATA%\NomiFun\Nomi` on Windows, `~/Library/Application Support/NomiFun/Nomi` on macOS, `$XDG_DATA_HOME/NomiFun/Nomi` on Linux (usually `~/.local/share/NomiFun/Nomi`), resolved via the `dirs` crate, with `<system temp>/nomifun-data/Nomi` as the extreme fallback when the OS reports no user directory. One default for every host is deliberate: the dev loops (`bun run serve:web`, `dev:web`, `dev`) and the installed desktop app read and write the same state — a provider or companion configured once is testable everywhere, and troubleshooting only ever has one directory to look at. For an isolated sandbox, point `NOMIFUN_DATA_DIR` or `--data-dir` somewhere else.
- At startup (before the database is opened) the backend takes an OS-level **exclusive lock** on `{data_dir}/server.lock`. A second backend process on the same data dir fails fast with an error naming the holder (pid + executable) and the two ways out: close the other instance, or give this one its own directory via `NOMIFUN_DATA_DIR` / `--data-dir`. The lock is advisory (`flock` / `LockFileEx` via `fs2`) and is released by the OS when the process exits or crashes — a leftover `server.lock` file is harmless. `nomicore doctor` and the `mcp-*` stdio subcommands do not take the lock (doctor is designed to run alongside a live server).
- `work-dir` holds per-conversation workspaces. When unset, it resolves in this order: `--work-dir` → non-empty `NOMIFUN_WORK_DIR` env → the data dir itself. Conversations create subdirectories under `<work-dir>/conversations/`; deleting a conversation deletes its workspace.
- The desktop shell uses the shared default above. With `NOMIFUN_DATA_DIR` set, the dir becomes `$NOMIFUN_DATA_DIR/Nomi` — the override semantics are unchanged. Older builds defaulted to `<system temp>/nomifun-data/Nomi`; on first launch with the new default, an existing temp-rooted install is relocated automatically (one-shot, the legacy dir is kept as a backup and absolute paths stored in the database are rewritten).
- The web host applies the value literally — `--data-dir` (or `NOMIFUN_DATA_DIR`) is used as given, with no `/Nomi` suffix — so Docker (`/data`) and systemd (`/var/lib/nomifun`) deployments are unaffected. With neither set it falls back to the shared per-user default; the old relative `data` default is gone.

## Auth secret resolution

`JwtService` is constructed from a single secret; `AppServices::from_config` resolves it in this order:

1. `JWT_SECRET` environment variable, if set.
2. Otherwise, the value persisted in the system user row (`system_default_user.jwt_secret`).
3. Otherwise, a fresh cryptographically random secret is generated and **persisted to the database** for future boots.

The change-password flow rotates the JWT secret as a side effect, invalidating every existing session.

At-rest encryption uses a separate persistent key stored at `<data-dir>/encryption_key`.
On older installs where that file does not exist yet, startup seeds it from the currently resolved JWT secret so existing encrypted fields remain readable. After that first seed, changing the password or rotating the JWT secret does not change the data-encryption key.

## TLS / HTTPS cookie handling

NomiFun does not terminate TLS itself — put a TLS-terminating reverse proxy (Caddy, nginx, …) in front. When you do:

- Set `NOMIFUN_HTTPS=true` so cookies are flagged `Secure` and `SameSite=Strict`. Without this, browsers reject `Secure` cookies on HTTPS responses, and login appears to silently fail.
- The WebSocket upgrade at `/ws` passes through any standards-compliant proxy without extra headers; Caddy handles it out of the box.

See [`guides/web-server-deployment.md`](../guides/web-server-deployment.md) for a worked Caddy + Docker setup.

## Logging

- All logs go to both stdout (so `journalctl`/`docker logs` capture them) and a daily-rolling file at `<log-dir>/nomicore.log`.
- `--log-level` accepts a full [`tracing` `EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) directive: a global level, or a comma-separated list of per-target overrides.

  Examples:

  - `info` — global info.
  - `debug` — global debug. Verbose; useful for short reproductions.
  - `info,nomifun_mcp=trace` — info everywhere, trace for the MCP module.
  - `warn,nomifun_conversation=info,nomifun_terminal=debug` — quieter overall, normal for the conversation engine, debug for terminals.

There is no separate `RUST_LOG` plumbing — `--log-level` (or its env-driven equivalent in the running host) is the single switch.

## See also

- [Web Server Deployment](../guides/web-server-deployment.md) — running `nomifun-web` with Docker, systemd, Caddy.
- [Running NomiFun as a Desktop App](../guides/desktop-app.md) — desktop-specific configuration.
- [API Overview](./api-overview.md) — what the backend exposes once it is configured and running.
- [Troubleshooting](./troubleshooting.md) — symptoms and fixes when configuration ends up wrong at runtime.
