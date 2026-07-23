# Data and Storage

Database and identifier changes must also follow the repository-wide
[Data and Identifier Standards](../contributing/data-and-identifier-standards.md).
This page describes storage behavior; the executable schema and
logical-reference registry remain the implementation-level sources of truth.

NomiFun keeps its state in three places: a SQLite database (the source of
truth for everything structured), a per-installation **data directory**
(database file, logs, OS-cached runtimes), and per-conversation **work
directories** that hold the files agents read and write. This page explains
what lives where, how it's named, and how it's protected.

## The data directory

| Host | Default path | Override |
| --- | --- | --- |
| Desktop (`nomifun-desktop`) | Per-user app data: `%LOCALAPPDATA%\NomiFun\Nomi` on Windows, `~/Library/Application Support/NomiFun/Nomi` on macOS, `$XDG_DATA_HOME/NomiFun/Nomi` (usually `~/.local/share/NomiFun/Nomi`) on Linux. With `NOMIFUN_DATA_DIR` set, becomes `$NOMIFUN_DATA_DIR/Nomi`. A pre-v3 dataset at this or an older root is retired as a complete dataset; its product rows are not relocated into v3. | env `NOMIFUN_DATA_DIR` |
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
The v3 contract does not copy a pre-existing product dataset from
`<system temp>/nomifun-data/Nomi` or any other legacy root into the active
data directory. A detected historical managed dataset is moved intact to a
retired/quarantine location by the dataset reset state machine. NomiFun then
creates a new v3 dataset rather than rewriting historical database paths.

### One directory, one state

Sharing one default across every host is deliberate: the dev loops
(`bun run serve:web`, `dev:web`, `dev`) and the installed desktop app
read and write the same state, so a provider or companion configured once is
testable everywhere, and troubleshooting only ever has one directory to
look at. When you *do* want an isolated sandbox, `NOMIFUN_DATA_DIR` or
`--data-dir` is the escape hatch. (The dev scripts no longer pass a
repo-relative `--data-dir`; the old `data/` and `.dev-data/` directories
are not imported into v3. Pointing `NOMIFUN_DATA_DIR` at one of them subjects
that whole root to the same v3 contract check and reset policy.)

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

Persisted identity follows the layered v3 contract in
[`id-system.md`](id-system.md):

- every product table has `id INTEGER PRIMARY KEY AUTOINCREMENT`;
- stable cross-dataset entities add a named, bare canonical UUIDv7 field;
- internal-only relation, singleton, cache, and event rows use the table `id`
  only inside the active dataset; product-addressable entities use named
  UUIDv7 business fields;
- relationships are indexed logical references, not physical foreign keys.

The local `id` is the table primary key but is not a portable business
identity. It is never exported as a technical row key through APIs, events,
managed files, or backup graphs. Product-addressable entities use named
UUIDv7 locators; no product wire contract introduces an integer business ID.

In particular, Agent Execution Participant, Step, Attempt, and Template
Participant use `participant_id`, `step_id`, `attempt_id`, and
`template_participant_id`. Channel Plugin, User, and Session use
`channel_plugin_id`, `channel_user_id`, and `channel_session_id`. All seven
are bare canonical UUIDv7 business IDs, not local integer identities. The same
rule applies to MCP servers, webhooks, connector credentials, creation tasks,
conversation artifacts, and IDMM interventions.

- `Database` — owns the `sqlx::SqlitePool` and the v3 baseline schema state.
  Exposed via `nomifun-db::SqlitePool` re-export.
- `init_database` — opens or initializes the v3 baseline database.
- `init_database_memory` — in-memory variant used by tests.

The crate exposes ~20 repository **trait + Sqlite-impl** pairs. A non-exhaustive
list (see the `pub use repository::{...}` block in `lib.rs` for all of them):

| Trait | Sqlite implementation | Stores |
| --- | --- | --- |
| `IUserRepository` | `SqliteUserRepository` | Users and password hashes |
| `IConversationRepository` | `SqliteConversationRepository` | Conversations + messages, with filters and full-text search rows |
| `IAgentMetadataRepository` | `SqliteAgentMetadataRepository` | ACP handshake results, available models, agent-binary metadata |
| `IAcpSessionRepository` | `SqliteAcpSessionRepository` | Persistent ACP sessions for resume after restart |
| `IMcpServerRepository` | `SqliteMcpServerRepository` | Configured MCP servers (CRUD) |
| `IOAuthTokenRepository` | `SqliteOAuthTokenRepository` | Encrypted OAuth tokens for HTTP MCP servers |
| `IProviderRepository` | `SqliteProviderRepository` | LLM provider credentials (encrypted) |
| `IRemoteAgentRepository` | `SqliteRemoteAgentRepository` | Remote-agent endpoints |
| `IAgentExecutionRepository` | `SqliteAgentExecutionRepository` | AgentExecution, immutable Participants, revisioned Steps/Dependencies, Attempts, Conversation Links, and the Event outbox; see the [unified model](agent-execution.zh.md) |
| `IRequirementRepository` | `SqliteRequirementRepository` | AutoWork requirements; owner links follow the same application-managed logical-reference policy as every other repository |
| `ICronRepository` | `SqliteCronRepository` | Scheduled tasks and their timezone-normalized expressions |
| `ITerminalRepository` | `SqliteTerminalRepository` | Terminal session metadata |
| `IPresetRepository` / `IPresetStateRepository` | `SqlitePresetRepository` / `SqlitePresetRepository` | Relational presets and per-user selection state |
| `IChannelRepository` | `SqliteChannelRepository` | External chat-channel plugin configs (Telegram / Lark / DingTalk / WeChat) |
| `IClientPreferenceRepository` | `SqliteClientPreferenceRepository` | Per-client preferences |
| `ITagSettingRepository` | `SqliteTagSettingRepository` | Tag-based grouping (used by AutoWork) |
| `ISettingsRepository` | `SqliteSettingsRepository` | Misc app settings |
| `IWebhookRepository` | `SqliteWebhookRepository` | Outbound webhook destinations (Lark) |

A few row-update params types travel alongside (`UpdateAgentHandshakeParams`,
`ConversationFilters`, `ConversationRowUpdate`, `MessageRowUpdate`,
`MessageSearchRow`, `UpdateCronJobParams`, `UpsertOAuthTokenParams`,
`CreateProviderParams`, `UpdateRemoteAgentParams`,
`CreateAgentExecutionParams`, `ReconcileAgentExecutionPlanParams`,
`SettleAgentExecutionAttemptParams`, etc.). Repository traits are the feature
contract. Domain services use them rather than the pool; narrowly scoped
bootstrap/schema maintenance remains the documented exception.

### v3 baseline and dataset reset

The embedded SQL defines the clean v3 baseline for a new, empty database.
`init_database` may record and verify that baseline, but it does not transform
pre-v3 product rows into v3 rows.

Before SQLite is opened, bootstrap checks the dataset contract and generation.
An absent dataset is initialized as v3. An incompatible or historical dataset
is retired as a whole and replaced with a new empty v3 dataset. There is no
table-by-table historical migration, compatibility read path, ID normalization,
or downgrade path.

The baseline contract is checked at runtime:

- every product table has `id INTEGER PRIMARY KEY AUTOINCREMENT`;
- stable business-ID columns contain bare canonical UUIDv7 strings;
- the schema has no physical `FOREIGN KEY`, `REFERENCES`, trigger, database
  cascade, or `*_row_id`;
- every logical reference has its required index and registry entry.

### Scheduled-task ownership

`cron_jobs.user_id` is the non-null, immutable owner of the scheduled-task
aggregate, not a request-time hint inferred from a Conversation. A new task
receives the authenticated canonical user ID explicitly. Optional Conversation
bindings must already have the same owner; a missing target, multiple inverse
owners, or disagreement between the two directions is rejected rather than
guessed or silently repaired.

Public HTTP, Gateway, service, and repository operations all carry `user_id`;
cross-owner access is indistinguishable from a missing job. The scheduler
addresses jobs by bare UUIDv7 `cron_job_id`, captures the owner, and re-verifies
that pair before execution, closing delete/recreate races. Repository
transactions enforce ownership for
optional Conversation bindings and generated Conversation Artifacts. Ownership
cannot be moved in place. There is no runtime installation-owner fallback.
Scheduled work has one target—an Agent—so the v3 domain model, API, and baseline
do not include a target-type discriminator or old terminal-only fields.

### Installation execution authority

The canonical user referenced by `installation_identity.owner_user_id` is the
installation owner. The owner may
start host runtimes and use files, terminals, skills, presets, knowledge mounts,
Office preview and Platform Gateway capabilities. Every other authenticated
principal is limited to ordinary Nomi model calls in Conversations and
scheduled tasks; identity, role text or open-ended JSON cannot widen that
authority.

The v3 baseline creates this authority model directly. It does not retain or
canonicalize rows from older schema generations. Loopback capability roots and
renewable leases are process memory only and are never persisted.

### Logical-reference policy

No product table has a physical foreign key. Stable-parent links such as
`messages.conversation_id` and `cron_job_runs.cron_job_id` store the parent's
bare UUIDv7 business ID. The repository layer validates the target and applies the
registered `RESTRICT`, application `CASCADE`, `SET_NULL`, or `KEEP_HISTORY`
delete policy in a transaction. Application `CASCADE` is service/repository
behavior, not a SQLite cascade or trigger. Orphan audits cover the database
and managed side stores.

`requirements.owner_conversation_id` is deliberately a logical link with a
`SET_NULL` lifecycle: Conversation deletion clears the binding in the
application transaction, so the persistent AutoWork runner can survive
without a database cascade.

## Encryption at rest — AES-GCM

Sensitive strings (provider API keys, OAuth tokens, channel-bot tokens, ...)
are encrypted before insertion using AES-256-GCM via
`nomifun_common::crypto::{encrypt_string, decrypt_string}` and the
data-encryption key loaded by `nomifun_app::load_or_create_data_encryption_key`.

The master key is a per-v3-dataset file at `<data_dir>/encryption_key`, created
when the new dataset is initialized. Password changes and JWT rotation do not
alter it. Historical ciphertext and keys are not imported during the v3 reset.
Losing the active dataset's `encryption_key` renders its encrypted columns
unreadable.

The `aes-gcm` crate version pinned in the workspace is `0.10`.

## Per-conversation workspaces

Each conversation owns a directory the agent can freely read and write:

```
{work_dir}/conversations/{workspace_id}/
```

- `work_dir` — the runtime work directory; falls back to the data dir when
  not set explicitly. Sources, in order: `--work-dir` flag → env
  `NOMIFUN_WORK_DIR` → `<data_dir>`.
- `workspace_id` — a backend-minted bare lowercase UUIDv7 stored as
  `extra.temp_workspace_id`. It is always 36 characters. Directory names do
  not contain type prefixes, title slugs, or a `temp` marker.

For a conversation without a user-selected workspace, the directory is
provisioned immediately after the conversation row is created.
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

The virtual companion's data stays outside the main product database tables.
It is a file domain that can be exported or wiped as a whole (see the
[Companions guide](../guides/companions.md)). Its v3 files are reset or
restored as part of the managed dataset; they are not imported through
historical row migrations. The multi-companion layout:

```
<data_dir>/companion/
├── shared/                      shared memory hub (one copy for all companions)
│   ├── config.json              SharedCompanionConfig: collect switches, learn interval & model, default_companion_id
│   ├── events/YYYYMMDD.jsonl    raw events from the collection pipeline (privacy-sensitive; export is opt-in)
│   └── memory.db                standalone SQLite (PRAGMA user_version ladder):
│                                shared memories/suggestions/learn history + per-companion runtime
│                                state (companion_runtime_state: XP, …)
└── companions/
    └── {companion_id}/                bare UUIDv7 companion ID; the directory is the source of truth
        └── config.json          CompanionProfileConfig: name/character/persona/per-companion model/desktop-companion toggle & position
```

The historical single-companion layout `companion/nomi/` is not migrated into
v3. If detected, it is retired together with the complete old managed dataset.

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
| Spawn | `nomi_process_runtime::ChildProcessBuilder` inherits the boot-time merged `PATH`, so `npx`, `bun`, and other JS tools resolve correctly. |
| Cleanup | `nomi_process_runtime::ProcessSupervisor` or `kill_process_tree` owns and reaps Agent / MCP child-process trees. |

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
4. bootstrap::prepare_v3_dataset    check generation; hard reset/quarantine as a whole
5. bootstrap::init_data_layer       initialize/open the v3 database baseline
6. bootstrap::write_v3_receipt      write and finalize the dataset reset receipt
7. AppServices::from_config         instantiate every service
8. ensure_admin_credentials (web)   pre-seed admin if NOMIFUN_ADMIN_PASSWORD is set
9. create_router → axum::serve      bind and start serving
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

- **Database** — create a consistent SQLite snapshot with the SQLite Backup API
  or `VACUUM INTO` while the database is open. Do **not** copy
  `nomifun-backend.db` directly: WAL data may still be in
  `nomifun-backend.db-wal`, and a raw file copy can be incomplete.
- **Bundle manifest** — record the v3 schema, storage-generation/dataset ID,
  creation time, and checksums for every included file. Restore preserves
  stable business UUIDv7 values; technical `id` values are rebuilt in the
  destination dataset, and relationships are reconstructed from registered
  business, natural, JSON, and side-store references.
- **Encryption key** — the offline bundle includes
  `<data_dir>/encryption_key` when present. Without this file, provider API
  keys, OAuth tokens, channel bot tokens, and other encrypted columns cannot
  be decrypted.
- **Workspaces** — the bundle recursively includes only the backend-managed
  `<work_dir>/conversations/` tree. User-selected/custom workspaces elsewhere
  on disk are external user projects and are never copied implicitly.
- **Companion data** — the bundle recursively includes
  `<data_dir>/companion/` (shared memory hub + per-companion profiles; see the
  [Companions guide](../guides/companions.md)).
- **Bun runtime cache** — disposable; will be re-extracted on next boot.

Offline CLI commands are provided by `nomicore`:

```text
nomicore --data-dir <source> backup --output <bundle-dir>
nomicore restore --bundle <bundle-dir> --destination-data-dir <new-data-dir>
```

`backup` acquires the per-data-directory `server.lock` before opening SQLite,
so it fails instead of racing a live backend. It resolves `work_dir` with the
same CLI/persisted/environment rules as server boot. The output directory must
not already exist and must be outside both source roots. Backup accepts only a
v3 dataset and never migrates, recovers, or quarantines an old dataset; an
invalid or historical source fails closed. A complete bundle contains the
WAL-safe database snapshot,
the persistent encryption key when present, the companion file domain, and
managed conversation workspaces. Logs, `server.lock`, database WAL/SHM
sidecars, runtime/Bun caches, browser profiles, process/session scratch data,
and custom external workspaces are excluded.

Every payload file has a portable relative path, byte size, and SHA-256 digest
in the manifest; directory entries preserve empty companion/workspace
directories. Backup and restore reject symlinks, Windows junctions/reparse
points, path traversal, special files, undeclared payload files/directories,
and bundles above 8 GiB per file, 64 GiB total, 200,000 files, or 200,000
directories; the JSON manifest itself is capped at 64 MiB. `restore` verifies the complete bundle
before writing, accepts only an absent or empty destination, stages and
validates all files beside the destination, and installs the data directory
with one rename. A failure leaves no partial destination. Stable business
UUIDv7 values are preserved, while local technical `id` values are reassigned
without being used as relationship locators; the complete registered logical
reference graph is then audited. A new `storage-generation` is written so
browser caches from the source dataset cannot be mistaken for restored state.
Managed workspaces
from a source custom work directory are intentionally rebased to
`<destination-data-dir>/conversations`; custom external workspaces must be
backed up separately by their owner.

The bundle contains the database encryption key and encrypted credentials.
Treat the entire bundle as sensitive data; store and transfer it with the same
access controls as the live data directory. If encrypted rows exist while the
persistent key is missing, or if the key file is invalid, backup refuses to
create an unrestorable bundle.

The restore command has no destination `--work-dir` option: it intentionally
creates the managed workspace tree below the new data directory. To use a
separate work root, move that restored managed tree and set the normal
work-directory override before the first server boot; never point restore at
an existing external project.

These commands implement v3 offline backup/restore. They do not migrate
historical data or provide a historical Merge operation. Clone preserves the
supplied business UUIDv7 values; it does not mint or implicitly rewrite them.
Any target business-ID collision fails closed without partial insertion.
Technical `id` values are rebuilt, while relationships are reconstructed from
portable business/natural/external references; source auto-increment values
are never portable identity.

A clean uninstall therefore deletes the data dir, the work dir (if set
separately), and the OS cache dir.

## Cross-references

- The repository traits and their consumers are catalogued in
  [`backend-crates.md`](backend-crates.md).
- The HTTP routes that hit each repository, and the WS topics that mirror
  state changes, are summarized in [`communication.md`](communication.md).
- The agent-side data (TOML config, skills, file cache) is described in
  [`agent-engine.md`](agent-engine.md).
