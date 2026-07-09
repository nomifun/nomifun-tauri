# Data and Storage

NomiFun keeps its state in three places: a SQLite database (the source of
truth for everything structured), a per-installation **data directory**
(database file, logs, OS-cached runtimes), and per-conversation **work
directories** that hold the files agents read and write. This page explains
what lives where, how it's named, and how it's protected.

## The data directory

| Host | Default path | Override |
| --- | --- | --- |
| Desktop (`nomifun-desktop`) | Per-user app data: `%LOCALAPPDATA%\NomiFun\Nomi` on Windows, `~/Library/Application Support/NomiFun/Nomi` on macOS, `$XDG_DATA_HOME/NomiFun/Nomi` (usually `~/.local/share/NomiFun/Nomi`) on Linux. With `NOMIFUN_DATA_DIR` set, becomes `$NOMIFUN_DATA_DIR/Nomi`. Legacy installs under `<system temp>/nomifun-data/Nomi` are auto-relocated on launch (one-shot; the old dir is kept as a backup). | env `NOMIFUN_DATA_DIR` |
| Web (`nomifun-web`) and the `nomicore` bin | The **same** per-user directory as the desktop shell — `%LOCALAPPDATA%\NomiFun\Nomi` / `~/Library/Application Support/NomiFun/Nomi` / `$XDG_DATA_HOME/NomiFun/Nomi` (the old `./data`-relative default is gone). With `NOMIFUN_DATA_DIR` set, the value is taken **literally** (no `/Nomi` suffix), so Docker `/data` and systemd `/var/lib/nomifun` deployments are unaffected. | flag `--data-dir` or env `NOMIFUN_DATA_DIR` |

Inside the data directory:

```
<data_dir>/
├── nomifun-backend.db   SQLite database (sqlx)
├── server.lock          exclusive server-lock address file (the lock lives on
│                        the open OS handle; a leftover file is harmless)
├── logs/                tracing-appender file output (rotated daily)
├── conversations/       per-conversation workspaces (see below)
└── companion/                 companion file domain (shared memory hub + per-companion profiles, see below)
```

All three hosts resolve the unset default through one shared helper,
[`nomifun_app::cli::default_data_dir()`](../../crates/backend/nomifun-app/src/cli.rs):
`dirs::data_local_dir()/NomiFun/Nomi` (the per-user application-data
location), with the system temp dir (`<system temp>/nomifun-data/Nomi`)
only as an extreme fallback when the OS reports no user dir. Env semantics
stay host-specific: the desktop shell appends `"Nomi"` to `NOMIFUN_DATA_DIR`
(see [`apps/desktop/src/main.rs`](../../apps/desktop/src/main.rs)), while
`nomifun-web` and `nomicore` take the env value literally (a clap `env`
binding — new for `nomicore`, which previously ignored the variable).
A pre-existing legacy install under `<system temp>/nomifun-data/Nomi` is
relocated to the new location once at launch
([`apps/desktop/src/relocate.rs`](../../apps/desktop/src/relocate.rs)):
data is copied (regenerable caches/logs are left behind), the legacy dir is
kept as a backup, and the backend then rewrites absolute paths stored in the
database (knowledge-base roots, conversation workspaces, terminal cwds) to
the new root.

### One directory, one state

Sharing one default across every host is deliberate: the dev loops
(`bun run serve:web`, `dev:web`, `dev`) and the installed desktop app
read and write the same state, so a provider or companion configured once is
testable everywhere, and troubleshooting only ever has one directory to
look at. When you *do* want an isolated sandbox, `NOMIFUN_DATA_DIR` or
`--data-dir` is the escape hatch. (The dev scripts no longer pass a
repo-relative `--data-dir`; the old `data/` and `.dev-data/` directories
are not read by anything and their contents are **not** auto-migrated —
copy them into the new root or point `NOMIFUN_DATA_DIR` back at them if
you still need them.)

What makes the sharing safe is an **exclusive server lock**: at boot
(`bootstrap::init_environment`, before the database is opened) the backend
takes an OS-level exclusive advisory lock on `{data_dir}/server.lock`
(`fs2`: `flock` on Unix, `LockFileEx` on Windows). The OS releases the lock
when the process exits *or crashes*, so a leftover `server.lock` file is
harmless and needs no staleness heuristics. A second backend on the same
directory fails fast with an error naming the holder (pid + exe) and the
two ways out: close the other instance, or point this one at its own
directory. The desktop shell now surfaces a backend-startup failure in a
native error dialog and exits (previously a silent white window).
`nomicore doctor` and the `mcp-*` stdio subcommands are unaffected by the
lock (`doctor` is designed to run alongside a live server).

## SQLite via `sqlx`

[`nomifun-db`](../../crates/backend/nomifun-db/) is the data layer. Highlights
from [`crates/backend/nomifun-db/src/lib.rs`](../../crates/backend/nomifun-db/src/lib.rs):

- `Database` — owns the `sqlx::SqlitePool` and the migrations. Exposed via
  `nomifun-db::SqlitePool` re-export.
- `init_database` — opens the file, runs embedded migrations.
- `init_database_memory` — in-memory variant used by tests.

The crate exposes ~20 repository **trait + Sqlite-impl** pairs. A non-exhaustive
list (see the `pub use repository::{...}` block in `lib.rs` for all of them):

| Trait | Sqlite implementation | Stores |
| --- | --- | --- |
| `IUserRepository` | `SqliteUserRepository` | Users, password hashes, the system default user |
| `IConversationRepository` | `SqliteConversationRepository` | Conversations + messages, with filters and full-text search rows |
| `IAgentMetadataRepository` | `SqliteAgentMetadataRepository` | ACP handshake results, available models, agent-binary metadata |
| `IAcpSessionRepository` | `SqliteAcpSessionRepository` | Persistent ACP sessions for resume after restart |
| `IMcpServerRepository` | `SqliteMcpServerRepository` | Configured MCP servers (CRUD) |
| `IOAuthTokenRepository` | `SqliteOAuthTokenRepository` | Encrypted OAuth tokens for HTTP MCP servers |
| `IProviderRepository` | `SqliteProviderRepository` | LLM provider credentials (encrypted) |
| `IRemoteAgentRepository` | `SqliteRemoteAgentRepository` | Remote-agent endpoints |
| `ITeamRepository` | `SqliteTeamRepository` | Multi-agent teams, tasks, mailbox state |
| `IRequirementRepository` | `SqliteRequirementRepository` | AutoWork requirements (intentionally **no foreign key** to conversations — the loop survives conversation deletion) |
| `ICronRepository` | `SqliteCronRepository` | Scheduled tasks and their timezone-normalized expressions |
| `ITerminalRepository` | `SqliteTerminalRepository` | Terminal session metadata |
| `IAssistantRepository` / `IAssistantOverrideRepository` | `SqliteAssistantRepository` / `SqliteAssistantOverrideRepository` | Assistants and per-installation overrides |
| `IChannelRepository` | `SqliteChannelRepository` | External chat-channel plugin configs (Telegram / Lark / DingTalk / WeChat) |
| `IClientPreferenceRepository` | `SqliteClientPreferenceRepository` | Per-client preferences |
| `ITagSettingRepository` | `SqliteTagSettingRepository` | Tag-based grouping (used by AutoWork) |
| `ISettingsRepository` | `SqliteSettingsRepository` | Misc app settings |
| `IWebhookRepository` | `SqliteWebhookRepository` | Outbound webhook destinations (Lark) |

A few row-update params types travel alongside (`UpdateAgentHandshakeParams`,
`ConversationFilters`, `ConversationRowUpdate`, `MessageRowUpdate`,
`MessageSearchRow`, `UpdateCronJobParams`, `UpsertOAuthTokenParams`,
`CreateProviderParams`, `UpdateRemoteAgentParams`, `UpdateTeamParams`,
`UpdateTaskParams`, etc.). The repository traits are the contract; everything
above the data layer talks to them, never to the pool directly.

### Migrations

Migrations are SQL files embedded with `sqlx::migrate!`. They run on every
boot inside `init_database`. Schemas evolve forward only; downgrades are not
supported.

### Per-conversation foreign-key note

`requirements` (the AutoWork queue) intentionally has **no foreign key** on
`conversation_id`. The AutoWork orchestrator (`nomifun-requirement`) is
backend-authoritative and survives conversation deletion — the FK would couple
its lifecycle to the conversation's, defeating the boot-resume design. (See
the user memory entry "AutoWork backend-authoritative".)

## Encryption at rest — AES-GCM

Sensitive strings (provider API keys, OAuth tokens, channel-bot tokens, ...)
are encrypted before insertion using AES-256-GCM via
`nomifun_common::crypto::{encrypt_string, decrypt_string}` and the
data-encryption key loaded by `nomifun_app::load_or_create_data_encryption_key`.

The master key is a per-installation file at `<data_dir>/encryption_key`.
Older installs did not have that file, so the first boot on the newer code
seeds it from the currently resolved JWT secret to keep existing ciphertext
readable. After that, password changes and JWT rotation do not alter the data
key. Losing `encryption_key` renders encrypted columns unreadable.

The `aes-gcm` crate version pinned in the workspace is `0.10`.

## Per-conversation workspaces

Each conversation owns a directory the agent can freely read and write:

```
{work_dir}/conversations/{label}-temp-{conversation_id}/
```

- `work_dir` — the runtime work directory; falls back to the data dir when
  not set explicitly. Sources, in order: `--work-dir` flag → env
  `NOMIFUN_WORK_DIR` → `<data_dir>`.
- `label` — a short slug derived from the conversation title.
- `temp` — literal string; signals these directories are mutable scratch
  space the user can also drop files into.
- `conversation_id` — the conversation's unique id (UUID v7 with a short
  prefix from `nomifun_common::id`).

The directory is created lazily the first time the conversation needs it.
On conversation deletion the directory is removed (the
`OnConversationDelete` hook in `nomifun_common::hooks`). File operations
inside it are sandboxed and watched:

- [`nomifun-file::path_safety`](../../crates/backend/nomifun-file/src/path_safety.rs)
  rejects paths that escape the workspace (e.g. via `..` or absolute roots).
- [`nomifun-file::watch_service`](../../crates/backend/nomifun-file/src/watch_service.rs)
  uses `notify` to surface filesystem changes back to the SPA over WS.
- [`nomifun-file::snapshot_service`](../../crates/backend/nomifun-file/src/snapshot_service/)
  records before/after snapshots for tool-edit auditability.

The repo enforces an extra constraint via
`nomifun_common::error::workspace_path_has_edge_whitespace_segment`: no
directory name in a workspace path may begin or end with whitespace (or
consist entirely of whitespace). Such names break Win32 path round-tripping
and are visually indistinguishable in any UI. Interior whitespace is fully
supported — the default per-user data dir on macOS
(`~/Library/Application Support/NomiFun/Nomi`) contains a space, and every
process-spawn pipeline passes the workspace as a discrete argument
(`Command::current_dir`, PTY cwd, ACP session JSON), which is
whitespace-safe.

### Knowledge-base mounts (`.nomi/knowledge/`)

When a conversation, terminal session, or companion binding brings knowledge
bases into a workspace, they are mounted under
`{workspace}/.nomi/knowledge/` — the same `.nomi/` domain as project
skills — as junctions/symlinks with a copy fallback, plus a built-in
`.gitignore` so mounts never enter version control. A platform-managed
`README.md` (retrieval protocol, per-base digests + TOC, write-back
rules) is rewritten there on every launch. Legacy mounts under the old
`{workspace}/.nomifun/knowledge/` location are cleaned up automatically
on the next sync.

## Companion data (the `companion/` file domain)

The virtual companion's data deliberately stays **out of the main database's
migration system** — it is a file domain that can be exported or wiped
as a whole (see the [Companions guide](../guides/companions.md)). The multi-companion
layout:

```
<data_dir>/companion/
├── shared/                      shared memory hub (one copy for all companions)
│   ├── config.json              SharedCompanionConfig: collect switches, learn interval & model, default_companion_id
│   ├── events/YYYYMMDD.jsonl    raw events from the collection pipeline (privacy-sensitive; export is opt-in)
│   └── memory.db                standalone SQLite (PRAGMA user_version ladder):
│                                shared memories/suggestions/learn history + per-companion runtime
│                                state (companion_runtime_state: XP, …)
└── companions/
    └── {companion_id}/                companion_{uuid_v7}; the directory is the source of truth
        └── config.json          CompanionProfileConfig: name/character/persona/per-companion model/desktop-companion toggle & position
```

The legacy single-companion layout `companion/nomi/` is migrated automatically on
first boot into `shared/` plus a first companion named "Nomi"; the old
directory gets a `.migrated` marker and is kept around (cleanup after
one release cycle).

Knowledge bases bound to companions do not live in the `companion/` domain: the
bindings are stored in the main database as
`knowledge_bindings('companion', companion_id)`, and the base content lives in the
knowledge bases' own managed directories (URL-sourced bases keep their
fetched markdown snapshots in a `snapshots/` subdirectory there).

## Bundled bun runtime

NomiFun ships its own `bun` runtime (1.3.13) so MCP servers and tool
subprocesses do not require a system Node.js install:

| Step | What happens |
| --- | --- |
| Build time | The bun binary for the target OS/arch is **zstd-compressed** and embedded into `nomifun-runtime` via `include_dir!`. |
| First run | `nomifun_runtime::init(&data_dir)` extracts the binary into a **`<data_dir>/runtime/`** subtree (see the runtime-cache details below). |
| Boot | `enhance_process_path()` prepends the bun bin dir to the process `PATH` **before any tokio thread is built** (the order is enforced in both host `main.rs` files). |
| Spawn | `nomifun_runtime::spawn::Builder` produces children with that merged `PATH` so `npx`, `bun`, and other JS tools resolve correctly. |
| Cleanup | `kill_process_tree` cross-platform tree-kills agent / MCP children on cancellation. |

The runtime cache is anchored to the backend's `data_dir`:
[`nomifun_runtime::init(&data_dir)`](../../crates/backend/nomifun-runtime/src/cache.rs)
records `<data_dir>/runtime` as the cache root, so on the desktop the bun
binary extracts under `<data_dir>/runtime/bun-<version>-<sha12>/` —
i.e. `%LOCALAPPDATA%\NomiFun\Nomi\runtime\bun-…\` by default on Windows
(the per-user app-data equivalents on macOS/Linux), or
`$NOMIFUN_DATA_DIR/Nomi/runtime/bun-…/` when the env var is set. When
`init` has not been called (the `mcp-*` subcommands, unit tests, `build.rs`)
the cache falls back to the platform cache dir via `dirs::cache_dir()`:
`%LOCALAPPDATA%\nomifun\runtime\` on Windows, `~/Library/Caches/nomifun/runtime/`
on macOS, `$XDG_CACHE_HOME/nomifun/runtime/` (or `~/.cache/nomifun/runtime/`)
on Linux.

## Logs

Logs go to `<data_dir>/logs/` via `tracing-appender`. The default level is
`info`; override with `--log-level` (e.g. `--log-level info,nomifun_mcp=trace`)
or env `RUST_LOG`. The desktop shell additionally keeps a console attached
in debug builds (the release build sets `windows_subsystem = "windows"`).

The logging configuration types — `ResolvedLogging`, `create_file_layer` —
live in `nomi_config::logging` (the agent layer's config crate). The
backend reaches them through the seam: `nomifun_ai_agent::nomi_config::logging::*`.

## First-run state

On a brand-new install the boot sequence is:

```
1. nomifun-runtime::init           extract bun into OS cache
2. enhance_process_path             prepend cache bin dir to PATH
3. bootstrap::init_environment      resolve work_dir / log_dir, init tracing,
                                    take the exclusive {data_dir}/server.lock
4. bootstrap::init_data_layer       open database, run migrations
5. AppServices::from_config         instantiate every service
6. ensure_admin_credentials (web)   pre-seed admin if NOMIFUN_ADMIN_PASSWORD is set
7. create_router → axum::serve      bind and start serving
```

Step 3 is where a second backend on an already-claimed data dir fails fast
(see "One directory, one state" above).

In the desktop shell step 6 is skipped, but the desktop is not the old blanket
`--local` story: it uses `TrustLocalToken` and trusts only its own WebView's
per-boot secret. In the web host, if no admin exists and no
`NOMIFUN_ADMIN_PASSWORD` is set, the install enters **interactive first-run
setup**: the next browser visitor chooses a username and password through
`POST /api/auth/setup`. A warning is logged if first-run setup is exposed on a
non-loopback bind address.

## Backups and reinstall

- **Database** — copy `<data_dir>/nomifun-backend.db` (sqlx single-file SQLite).
- **Encryption key** — copy `<data_dir>/encryption_key` together with the
  database. Without this file, provider API keys, OAuth tokens, channel bot
  tokens, and other encrypted columns cannot be decrypted.
- **Workspaces** — copy `<work_dir>/conversations/` if you want to keep the
  files agents wrote.
- **Companion data** — copy `<data_dir>/companion/` (shared memory hub + per-companion
  profiles), or use the in-app migration bundles instead (see the
  [Companions guide](../guides/companions.md)).
- **Bun runtime cache** — disposable; will be re-extracted on next boot.

A clean uninstall therefore deletes the data dir, the work dir (if set
separately), and the OS cache dir.

## Cross-references

- The repository traits and their consumers are catalogued in
  [`backend-crates.md`](backend-crates.md).
- The HTTP routes that hit each repository, and the WS topics that mirror
  state changes, are summarized in [`communication.md`](communication.md).
- The agent-side data (TOML config, skills, file cache) is described in
  [`agent-engine.md`](agent-engine.md).
