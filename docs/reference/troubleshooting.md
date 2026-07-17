# Troubleshooting

Symptoms you might hit running NomiFun, and the actual mechanism behind each one. If you find a problem that is not on this list, the source is the fastest reference — every behaviour described below is grounded in a specific file in `crates/backend/`.

## Backend port / connection problems

### `nomifun-web: invalid --host '<value>'`

The host argument must parse as an IP address (`127.0.0.1`, `0.0.0.0`, an explicit interface IP). Hostnames like `localhost` are not parsed — `nomifun-web` fails fast with this message rather than producing a cryptic socket error later. Pass an IP literal.

### `address already in use` on the configured port

Another process is bound to the same port. The web host uses `8787` by default (`NOMIFUN_WEB_PORT`). The desktop shell does not have this problem because it asks the OS for a free localhost port at startup and then tells the renderer over IPC.

To find the offender on Linux/macOS: `lsof -i :8787`. On Windows: `Get-NetTCPConnection -LocalPort 8787`. Either kill it, or change `--port` / `NOMIFUN_WEB_PORT`.

### Browser cannot reach the server on a non-loopback address

`nomifun-web` defaults to binding `127.0.0.1`. To accept LAN/VPN traffic, either pass `--host 0.0.0.0` or set `NOMIFUN_WEB_HOST=0.0.0.0`. Pre-seed the admin or complete first-run setup before doing this on a broadly reachable host. On Windows / macOS hosts also check the firewall — the OS may silently drop the connection.

If the goal is remote access from a phone or another device on the LAN, [WebUI Remote Access](../guides/webui-remote-access.md) usually wants less configuration than full server deployment.

## First-run admin and login problems

### `GET /api/auth/status` returns `needs_setup: true` after I started the server

This is the expected state on a fresh install when `NOMIFUN_ADMIN_PASSWORD` is not set. The first browser visitor's username + password become the admin via `POST /api/auth/setup`. Open the URL, fill in the form, and you are logged in.

If you want to close this window before the server is publicly reachable, set `NOMIFUN_ADMIN_PASSWORD` (and optionally `NOMIFUN_ADMIN_USERNAME`) before the first boot.

### `409 Conflict` on `/api/auth/setup`

An admin already exists. The setup endpoint is one-time only. Use `POST /login` instead, or — if the password has been lost on a self-hosted instance — recover via the local-only WebUI flow described in [WebUI Remote Access](../guides/webui-remote-access.md).

### Login appears to succeed but the next request gets `401`

Almost always a cookie problem behind a TLS proxy. The `Secure` flag is added to cookies only when `NOMIFUN_HTTPS=true`. On HTTPS responses without that flag, browsers reject the cookie outright and the next request has no session. Set `NOMIFUN_HTTPS=true` and reload.

A second cause: clock skew on the server. If the system clock is far off, the JWT may be considered expired by the same server that signed it. Make sure NTP is running.

### `Current password is incorrect` on change-password despite a correct password

The endpoint runs a constant-time bcrypt compare against the stored hash. If
you suspect data corruption: stop the server, back up the data dir, resolve the
installation owner through `installation_identity.owner_user_id`, and inspect
that user's `password_hash`. A surgical fix is possible
(`/api/auth/internal/users/{id}/password` in local mode) but the easiest path is
to restore from backup or re-bootstrap.

### "Username/password rejected" with a vaguely worded validation error

The validators reject a small set of obvious patterns — passwords under 8 characters, common dictionary entries, usernames outside `[a-zA-Z0-9_-]` or starting/ending with `-`/`_`. Pick something else.

## CSRF errors

### `403 CSRF token validation failed` on a POST/PUT/PATCH/DELETE

The `nomifun-csrf-token` cookie value must match the `x-csrf-token` request header. The middleware sets the cookie automatically on the first response that does not have one, so a freshly-loaded SPA acquires it on its first GET. This usually breaks for one of these reasons:

- The client assumes no-auth local mode while the server is running in authenticated mode (or vice versa). `nomicore --local` and `nomifun-web --insecure-no-auth` skip CSRF; normal `nomifun-web` requires it. The desktop shell uses `TrustLocalToken`, so its own WebView should not see CSRF failures unless the injected trust header/cookie flow is broken.
- A reverse proxy is stripping cookies or rewriting `Set-Cookie`. The standard Caddy/nginx configurations leave them alone; custom rewrite rules can break them.
- The browser has third-party cookie blocking that affects the deployment domain.

`/login`, `/api/auth/setup`, and `/api/auth/qr-login` are CSRF-exempt; CSRF only applies to the *post-login* state-changing routes.

## WebSocket disconnects

### Connection closes immediately with code 1008

Code 1008 is "policy violation" — the server uses it for two specific cases:

- "no token provided" — the WebSocket upgrade request did not carry a JWT.
- An `auth-expired` event followed by close — the token was present but invalid or expired.

Both are usually caused by a stale token. Refresh the token via `GET /api/ws-token` and reconnect. If you see this immediately after login, check that cookies are flowing correctly (see the cookies-don't-stick case above) and that `Sec-WebSocket-Protocol` (or whichever header you use) is reaching the server unmodified.

### WebSocket connects then quietly stops receiving events

The server pings every 30 s and considers a client dead at 60 s. If the network drops a connection silently (mobile NAT, captive portals, a flaky proxy), the client side still appears connected until the server prunes it. The client is expected to reconnect; the SPA does this automatically. If you wrote a custom client, implement an exponential-backoff reconnect on the close event.

## "Agent CLI not found" and bun problems

### Conversation fails immediately with "agent not available" / "command not found"

The agent engine spawns ACP agent CLIs (`claude`, `codex`, `gemini`, `nomi`, `codebuddy`, …) and they must be on the **process** `PATH`. The process PATH is enhanced at startup (`nomifun_runtime::enhance_process_path`) but if the binary lives somewhere unusual it can still be missed.

Run the doctor:

```bash
nomicore doctor
```

This hydrates the agent registry and probes every CLI on `$PATH`, printing a per-agent availability table. Run it from the same shell that launched the app to see exactly what the app sees. If an agent is missing, install its CLI or add its bin directory to `PATH` and restart.

### Under systemd: `bun: command not found`

The agent engine requires **`bun ≥ 1.3.13`**. A `nologin` system account does not see `~/.bun/bin/`; install bun system-wide (`sudo install ~/.bun/bin/bun /usr/local/bin/bun`) or build with `NOMIFUN_EMBED_BUN=1` so bun is bundled into the binary and self-extracts into the data dir on first run. See [Web Server Deployment](../guides/web-server-deployment.md#bun-must-be-on-the-system-path) for the worked recipe.

Verify with `sudo -u nomifun -s -- which bun` after installing.

### "bun runtime extraction" log line followed by no agent activity

The embedded-bun build extracts bun into the data directory on first run. If extraction fails (typically permissions), the agent engine has no runtime. Check the data-dir for the bun binary, ensure the service user owns the data dir, and look in the log for the actual extraction error.

## Office preview

### Word/Excel/PPT preview returns "LibreOffice not detected"

The `/api/star-office/detect` route probes the system for a LibreOffice install. The Office preview features (`/api/word-preview/*`, `/api/excel-preview/*`, `/api/ppt-preview/*`, `/api/document/convert`) need LibreOffice to render documents.

- Linux: `apt install libreoffice` (or distribution equivalent).
- macOS: `brew install --cask libreoffice`.
- Windows: install from libreoffice.org.

After installing, restart the backend so it re-detects.

### Preview iframe stays blank

The Office preview routes spawn LibreOffice subprocesses and proxy them via `/api/ppt-proxy/*` and `/api/office-watch-proxy/*`. These proxy routes are **public** (no auth) on purpose — the iframe content needs to load without sending the SPA's session cookie. If your reverse proxy strips the URL path components or applies auth at the edge to `/api/*`, exempt the proxy paths.

## Data directory permissions

### Server starts but database writes fail / "unable to open database file"

The configured data directory must be writable by the process. Common cases:

- Running under systemd with `User=nomifun` but a data dir owned by another user. Fix: `chown -R nomifun:nomifun /var/lib/nomifun`.
- A read-only mount (`RootDirectory=`, `ProtectHome=yes`, …) covering the data path. Drop the over-broad sandbox; keep the moderate hardening from the shipped unit (`NoNewPrivileges=yes`, `PrivateTmp=yes`).
- On Docker, mounting a host directory whose UID does not match the container's. Use a named volume instead, or `chown` the host directory to the right UID.

The desktop shell's default data dir is the **per-user application-data location** (`%LOCALAPPDATA%\NomiFun\Nomi` on Windows, `~/Library/Application Support/NomiFun/Nomi` on macOS, `$XDG_DATA_HOME/NomiFun/Nomi` on Linux), which is writable by the launching user by construction. Set `NOMIFUN_DATA_DIR=<absolute path>` and the dir becomes `$NOMIFUN_DATA_DIR/Nomi`. Legacy installs under `<system temp>/nomifun-data/Nomi` are relocated to the new default automatically on launch (the old dir is kept as a backup); if the relocation cannot complete, the app keeps starting from the legacy dir and retries next launch.

### `data directory ... is already in use by another running NomiFun backend`

Every host (desktop shell, `nomifun-web`, the `nomicore` binary) defaults to the **same** per-user data directory, and the backend takes an OS-level exclusive lock on `{data_dir}/server.lock` at startup — a second backend on the same directory fails fast with this message instead of silently corrupting shared state. The classic trigger: the desktop app is still running and you start `bun run serve:web` / `dev:web` (or vice versa). Two ways out: close the other instance (the message names the holder's pid and executable), or give the new one its own directory via `NOMIFUN_DATA_DIR` / `--data-dir`. The lock is released by the OS when the holder exits or crashes; a leftover `server.lock` file is harmless. `nomicore doctor` and the `mcp-*` stdio subcommands do not take the lock, so they are unaffected.

## Docker specifics

### `docker compose up` builds, starts, then exits immediately

Read the logs (`docker compose logs nomifun`). The most common causes are:

- The data volume is empty *and* `NOMIFUN_ADMIN_PASSWORD` is missing — the server runs fine, but you have no way in until you complete first-run setup over HTTP. This is not actually a failure; it is a state.
- The `--dist` directory inside the image points at the wrong path. The shipped Dockerfile copies `ui/dist` to `/opt/nomifun/web` and the `CMD` references that — only an issue if you have customised the Dockerfile.
- A bind-mounted data dir that the container user cannot write to.

### Logs say `nomifun-web: embedded backend + SPA on one port` but the browser cannot connect

Confirm the port mapping (`docker compose ps`). The default compose file publishes `8787:8787`; if you put Caddy in front, it should be `expose: ["8787"]` instead. Connecting to the wrong port is the usual culprit.

### Build is slow or fails on a constrained network

Identify the stage that failed first:

- `load metadata for docker.io/...` / `TLS handshake timeout`: no base image
  was pulled. Configure a Docker daemon registry mirror, or set `BUN_IMAGE`,
  `RUST_IMAGE`, and `RUNTIME_IMAGE` to trusted compatible image references.
- `apt-get` / `bun install` / `Updating crates.io index`: set `APT_MIRROR`,
  `BUN_REGISTRY`, or `CARGO_REGISTRY_MIRROR`, respectively.

Compose accepts all package-layer settings together:

```bash
APT_MIRROR=<trusted-debian-mirror> \
CARGO_REGISTRY_MIRROR=<trusted-sparse-cargo-index> \
BUN_REGISTRY=<trusted-npm-registry> \
docker compose up -d --build
```

The Dockerfile retries apt and Cargo downloads and uses the smaller Rust slim
base image; all mirror values must still be trusted and reachable by the
operator.

## Logging

The backend writes to **both** stdout and a daily-rolling file at `<log-dir>/nomicore.log` (default `<data-dir>/logs`). When something is going wrong:

- The `journalctl -u nomifun-web` / `docker compose logs nomifun` view shows the last few minutes.
- The rotating file under `<log-dir>` keeps history.
- Crank up the level for the affected module: `--log-level info,nomifun_mcp=trace` for MCP issues, `info,nomifun_terminal=debug` for terminals, `info,nomifun_conversation=debug` for agent conversations.

## When all else fails

Read the source. Every route handler is in the `routes.rs` (or `routes/`) file of its owning crate; the assembly is in `crates/backend/nomifun-app/src/router/routes.rs`. The error messages thrown by handlers are the literal strings that appear in HTTP responses, so a quick `grep` for the exact message usually lands you on the offending check in seconds.

## See also

- [Configuration Reference](./configuration.md) — every flag and env var.
- [API Overview](./api-overview.md) — orientation to routes, auth, and the WebSocket model.
- [FAQ](./faq.md) — short answers to the most common "is X true?" questions.
