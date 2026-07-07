-- Migration 001: Baseline schema for nomifun-backend.
--
-- This file is the 2026-06-13 "primary-key redesign" baseline (see
-- docs/superpowers/specs/2026-06-13-primary-key-redesign-design.md). It
-- supersedes and squashes the former 001(seq baseline)/002(attachments)/
-- 003(channel_per_pet). The system has never shipped, so the baseline only
-- serves BRAND-NEW databases: final-state schema + seed data, NO backfill /
-- normalization. Any pre-baseline database is renamed `*.pre-baseline.bak`
-- and recreated by database.rs during the pre-launch window.
--
-- ID model (single id per entity, NO seq dual-track; display == primary key):
--   * Cross-device entities (id leaves this machine via remote protocol /
--     ACP transcript / external IM / cross-device coordination key) use the
--     string global id `{prefix}_{uuidv7}` minted by generate_prefixed_id:
--       messages(msg_) cron_jobs(cron_) agent_metadata(agent_builtin_/agent_)
--       providers(prov_) assistants assistant_plugins assistant_users(achu_)
--       assistant_sessions knowledge_bases(kb_) teams(team_) team_agents(slot_)
--       team_tasks(task_) attachments(att_) pets(pet_, filesystem) device_id.
--   * Local-only entities use INTEGER PRIMARY KEY AUTOINCREMENT (monotonic,
--     ordered, never reused): conversations, requirements, terminal_sessions,
--     conversation_artifacts, mcp_servers, remote_agents, webhooks, mailbox,
--     knowledge_bindings(binding_id). The user-facing conversation/requirement/
--     terminal ids render as `#N`; see
--     docs/superpowers/specs/2026-06-14-numeric-session-requirement-id-design.md.
--   * Natural keys unchanged: users.id, client_preferences.key,
--     oauth_tokens.server_url, requirement_tags.tag, tag_settings.tag,
--     assistant_pairing_codes.code, system_settings(id=1).
--
-- INVARIANT: a cross-device INTEGER id (conversation/requirement/terminal)
-- enters a remote payload only as the client-provided correlation tag (safe;
-- the real remote routing key is the peer-minted sessionKey). FK column type
-- == referenced table PK type. agent-address columns (teams.lead_agent_id,
-- mailbox.to_agent_id/from_agent_id, team_tasks.owner) are NOT foreign keys:
-- they hold a slot_id OR the 'user'/'lead' sentinels, so a FK would reject
-- valid rows.
--
-- Builtin agent seed ids use the stable slug scheme `agent_builtin_{backend}`.
-- Runtime lookup goes through find_builtin_by_backend, never id literals.
--
-- Requires PRAGMA foreign_keys = ON on every connection for the ON DELETE
-- cascades below to fire (set in database.rs connect options).

------------------------------------------------------------------------
-- Core tables
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS users (
    id            TEXT PRIMARY KEY NOT NULL,
    username      TEXT NOT NULL UNIQUE,
    email         TEXT UNIQUE,
    password_hash TEXT NOT NULL,
    avatar_path   TEXT,
    jwt_secret    TEXT,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    last_login    INTEGER
);
CREATE INDEX IF NOT EXISTS idx_users_username ON users(username);
CREATE INDEX IF NOT EXISTS idx_users_email ON users(email);

-- Singleton settings row. Deliberately NOT seeded: the settings repository
-- treats a missing row as "defaults" (get_settings returns Option) and lazily
-- creates it on first upsert.
CREATE TABLE IF NOT EXISTS system_settings (
    id                        INTEGER PRIMARY KEY CHECK (id = 1),
    language                  TEXT    NOT NULL DEFAULT 'en-US',
    notification_enabled      INTEGER NOT NULL DEFAULT 1,
    cron_notification_enabled INTEGER NOT NULL DEFAULT 0,
    command_queue_enabled     INTEGER NOT NULL DEFAULT 0,
    save_upload_to_workspace  INTEGER NOT NULL DEFAULT 0,
    updated_at                INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS client_preferences (
    key        TEXT PRIMARY KEY NOT NULL,
    value      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Cross-device: provider_id is embedded as a value-object snapshot in
-- conversation.model / pet config / idmm.sidecar and travels with them.
CREATE TABLE IF NOT EXISTS providers (
    id                TEXT PRIMARY KEY NOT NULL,   -- prov_{uuidv7}
    platform          TEXT    NOT NULL,
    name              TEXT    NOT NULL,
    base_url          TEXT    NOT NULL,
    api_key_encrypted TEXT    NOT NULL,
    models            TEXT    NOT NULL DEFAULT '[]',
    enabled           INTEGER NOT NULL DEFAULT 1,
    capabilities      TEXT    NOT NULL DEFAULT '[]',
    context_limit     INTEGER,
    model_protocols   TEXT,
    model_enabled     TEXT,
    model_health      TEXT,
    bedrock_config    TEXT,
    is_full_url       INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_providers_platform ON providers(platform);

------------------------------------------------------------------------
-- Conversations & Messages
------------------------------------------------------------------------

-- cron_job_id: the cron job that created this conversation (was the JSON
-- key extra.cronJobId + an expression index; now a real nullable FK column).
-- This forms a ring with cron_jobs.conversation_id: writers must INSERT the
-- cron row with conversation_id=NULL first, then the conversation, then
-- backfill both (see spec §9.A).
CREATE TABLE IF NOT EXISTS conversations (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id         TEXT    NOT NULL,
    name            TEXT    NOT NULL,
    type            TEXT    NOT NULL,
    extra           TEXT    NOT NULL DEFAULT '{}',
    model           TEXT,
    status          TEXT    NOT NULL DEFAULT 'pending'
                            CHECK(status IN ('pending', 'running', 'finished')),
    source          TEXT,
    channel_chat_id TEXT,
    pinned          INTEGER NOT NULL DEFAULT 0,
    pinned_at       INTEGER,
    cron_job_id     TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (cron_job_id) REFERENCES cron_jobs(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_conversations_user_id ON conversations(user_id);
CREATE INDEX IF NOT EXISTS idx_conversations_updated_at ON conversations(updated_at);
CREATE INDEX IF NOT EXISTS idx_conversations_type ON conversations(type);
CREATE INDEX IF NOT EXISTS idx_conversations_user_updated ON conversations(user_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_conversations_source ON conversations(source);
CREATE INDEX IF NOT EXISTS idx_conversations_source_updated ON conversations(source, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_conversations_source_chat ON conversations(source, channel_chat_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_conversations_cron_job_id ON conversations(cron_job_id);

CREATE TABLE IF NOT EXISTS messages (
    id              TEXT    PRIMARY KEY NOT NULL,   -- msg_{uuidv7}
    conversation_id INTEGER NOT NULL,
    msg_id          TEXT,
    type            TEXT    NOT NULL,
    content         TEXT    NOT NULL DEFAULT '{}',
    position        TEXT    CHECK(position IN ('left', 'right', 'center', 'pop')),
    status          TEXT    CHECK(status IN ('finish', 'pending', 'error', 'work')),
    hidden          INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_messages_conversation_id ON messages(conversation_id);
CREATE INDEX IF NOT EXISTS idx_messages_created_at ON messages(created_at);
CREATE INDEX IF NOT EXISTS idx_messages_type ON messages(type);
CREATE INDEX IF NOT EXISTS idx_messages_msg_id ON messages(msg_id);
CREATE INDEX IF NOT EXISTS idx_messages_conv_created ON messages(conversation_id, created_at);
CREATE INDEX IF NOT EXISTS idx_messages_conv_created_desc ON messages(conversation_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_messages_type_created ON messages(type, created_at DESC);

-- Local-only INTEGER surrogate id. Idempotency moved off the old composite
-- text id onto a partial unique index: skill_suggest is unique per
-- (conversation, cron_job); cron_trigger has NO unique (one row per fire).
CREATE TABLE IF NOT EXISTS conversation_artifacts (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id INTEGER NOT NULL,
    cron_job_id     TEXT,
    kind            TEXT    NOT NULL
                            CHECK(kind IN ('cron_trigger', 'skill_suggest')),
    status          TEXT    NOT NULL DEFAULT 'active'
                            CHECK(status IN ('active', 'pending', 'dismissed', 'saved')),
    payload         TEXT    NOT NULL DEFAULT '{}',
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
    FOREIGN KEY (cron_job_id) REFERENCES cron_jobs(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_conversation_artifacts_conversation_id ON conversation_artifacts(conversation_id);
CREATE INDEX IF NOT EXISTS idx_conversation_artifacts_created_at ON conversation_artifacts(created_at);
CREATE INDEX IF NOT EXISTS idx_conversation_artifacts_conversation_created ON conversation_artifacts(conversation_id, created_at);
CREATE INDEX IF NOT EXISTS idx_conversation_artifacts_cron_job ON conversation_artifacts(cron_job_id);
CREATE INDEX IF NOT EXISTS idx_conversation_artifacts_kind_status ON conversation_artifacts(kind, status);
CREATE UNIQUE INDEX IF NOT EXISTS uq_conversation_artifacts_skill_suggest
    ON conversation_artifacts(conversation_id, cron_job_id) WHERE kind = 'skill_suggest';

------------------------------------------------------------------------
-- ACP Sessions
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS acp_session (
    conversation_id INTEGER PRIMARY KEY,
    agent_backend   TEXT    NOT NULL,
    agent_source    TEXT    NOT NULL,
    -- Nullable: an ACP conversation can be created before a concrete catalog
    -- agent is picked (legacy clients post only `backend`, or nothing at all).
    -- A non-NULL value is RESTRICT-bound to agent_metadata; NULL means
    -- "no agent chosen yet" and is exempt from FK enforcement (SQLite does not
    -- check FKs on NULL child columns). Replaces the old empty-string sentinel,
    -- which the new RESTRICT FK would reject.
    agent_id        TEXT,
    session_id      TEXT,
    session_status  TEXT    NOT NULL DEFAULT 'idle',
    session_config  TEXT    NOT NULL DEFAULT '{}',
    last_active_at  INTEGER,
    suspended_at    INTEGER,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
    FOREIGN KEY (agent_id) REFERENCES agent_metadata(id) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS idx_acp_session_status ON acp_session(session_status);
CREATE INDEX IF NOT EXISTS idx_acp_session_suspended ON acp_session(session_status, suspended_at) WHERE session_status = 'suspended';
CREATE INDEX IF NOT EXISTS idx_acp_session_agent_id ON acp_session(agent_id);

------------------------------------------------------------------------
-- Agent Metadata
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS agent_metadata (
    id                  TEXT PRIMARY KEY NOT NULL,
    icon                TEXT,
    name                TEXT NOT NULL,
    name_i18n           TEXT,
    description         TEXT,
    description_i18n    TEXT,
    backend             TEXT,
    agent_type          TEXT NOT NULL,
    agent_source        TEXT NOT NULL,
    agent_source_info   TEXT,
    enabled             INTEGER NOT NULL DEFAULT 1,
    command             TEXT,
    args                TEXT,
    env                 TEXT,
    native_skills_dirs  TEXT,
    behavior_policy     TEXT,
    yolo_id             TEXT,
    agent_capabilities  TEXT,
    auth_methods        TEXT,
    config_options      TEXT,
    available_modes     TEXT,
    available_models    TEXT,
    available_commands  TEXT,
    sort_order          INTEGER NOT NULL DEFAULT 1000,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_agent_metadata_backend ON agent_metadata(backend);
CREATE INDEX IF NOT EXISTS idx_agent_metadata_agent_type ON agent_metadata(agent_type);
CREATE INDEX IF NOT EXISTS idx_agent_metadata_sort_order ON agent_metadata(sort_order);

-- Seed agent_metadata with builtin agents.
--
-- Values are the post-001/003/004/010/012 final state of the legacy migration
-- chain: bun package pins from 004, ACP handshake captures (agent_capabilities
-- / auth_methods) from 003, command/binary_name fixes for Qoder/Vibe/Kiro from
-- 003, and the internal agent display name "Nomi" from 012. Agents without a
-- 003 handshake capture (Claude, Codex, Gemini, OpenCode, Cursor, Hermes,
-- Snow, and the non-ACP rows) keep NULL capabilities until first spawn.
INSERT OR IGNORE INTO agent_metadata
    (id, icon, name, backend, agent_type, agent_source, agent_source_info,
     enabled, command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     agent_capabilities, auth_methods,
     sort_order, created_at, updated_at)
VALUES
    -- ACP builtin agents
    ('agent_builtin_claude', '/api/assets/logos/ai-major/claude.svg', 'Claude Code',
     'claude', 'acp', 'builtin', '{"binary_name":"claude","bridge_binary":"bun"}',
     1, 'bun', '["x","--bun","@agentclientprotocol/claude-agent-acp@0.33.1"]', '[]',
     '[".claude/skills"]',
     '{"supports_side_question":true,"self_identity_sticky":true,"session_load_via_meta_field":true,"supports_team":true}',
     'bypassPermissions',
     NULL, NULL,
     3100,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_codex', '/api/assets/logos/tools/coding/codex.svg', 'Codex CLI',
     'codex', 'acp', 'builtin', '{"binary_name":"codex","bridge_binary":"bun"}',
     1, 'bun', '["x","--bun","@zed-industries/codex-acp@0.14.0"]', '[]',
     '[".codex/skills"]',
     '{"supports_side_question":false,"supports_team":true}',
     'full-access',
     NULL, NULL,
     3110,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_gemini', '/api/assets/logos/ai-major/gemini.svg', 'Gemini CLI',
     'gemini', 'acp', 'builtin', '{"binary_name":"gemini"}',
     1, 'gemini', '["--experimental-acp"]', '[]',
     '[".gemini/skills"]',
     '{"supports_side_question":false,"supports_team":true}',
     'yolo',
     NULL, NULL,
     3120,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_qwen', '/api/assets/logos/ai-china/qwen.svg', 'Qwen',
     'qwen', 'acp', 'builtin', '{"binary_name":"qwen"}',
     1, 'qwen', '["--acp"]', '[]',
     '[".qwen/skills"]',
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"prompt_capabilities":{"image":true,"audio":true,"embedded_context":true},"session_capabilities":{"list":{},"resume":{}},"mcp_capabilities":{"sse":true,"http":true}}',
     '[{"id":"openai","name":"Use OpenAI API key","description":"Requires setting the `OPENAI_API_KEY` environment variable","_meta":{"type":"terminal","args":["--auth-type=openai"]}},{"id":"qwen-oauth","name":"Qwen OAuth","description":"Qwen OAuth (free tier discontinued 2026-04-15)","_meta":{"type":"terminal","args":["--auth-type=qwen-oauth"]}}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_codebuddy', '/api/assets/logos/tools/coding/codebuddy.svg', 'CodeBuddy',
     'codebuddy', 'acp', 'builtin', '{"binary_name":"codebuddy","bridge_binary":"bun"}',
     1, 'bun', '["x","--bun","@tencent-ai/codebuddy-code@2.97.0","--acp"]', '[]',
     '[".codebuddy/skills"]',
     '{"supports_side_question":false,"supports_team":true}',
     'bypassPermissions',
     '{"prompt_capabilities":{"image":true,"embedded_context":true},"mcp_capabilities":{"http":true,"sse":true},"load_session":true,"delegate_tools_support":true}',
     '[{"id":"iOA","name":"Login with iOA","description":null},{"id":"external","name":"Login with Google/Github","description":null},{"id":"internal","name":"Login with WeChat","description":null},{"id":"selfhosted","name":"Login with Enterprise Domain","description":null}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_droid', '/api/assets/logos/brand/droid.svg', 'Droid',
     'droid', 'acp', 'builtin', '{"binary_name":"droid"}',
     1, 'droid', '["exec","--output-format","acp"]', '[]',
     '[".factory/skills"]',
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"session_capabilities":{"list":{},"resume":{}},"prompt_capabilities":{"image":true,"embedded_context":true},"_meta":{"terminal_output":true,"terminal-auth":true}}',
     '[{"id":"device-pairing","name":"Login","description":"Authenticate with Factory using a device pairing code in your browser."},{"id":"factory-api-key","name":"Factory API Key","description":"Authenticate using a Factory API key set in the FACTORY_API_KEY environment variable."}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_goose', '/api/assets/logos/tools/goose.svg', 'Goose',
     'goose', 'acp', 'builtin', '{"binary_name":"goose"}',
     1, 'goose', '["acp"]', '[]',
     '[".goose/skills"]',
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"prompt_capabilities":{"image":true,"audio":false,"embedded_context":true},"mcp_capabilities":{"http":true,"sse":false},"session_capabilities":{"list":{},"close":{}},"auth":{}}',
     '[{"id":"goose-provider","name":"Configure Provider","description":"Run `goose configure` to set up your AI provider and API key"}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_auggie', '/api/assets/logos/brand/auggie.svg', 'Auggie',
     'auggie', 'acp', 'builtin', '{"binary_name":"auggie"}',
     1, 'auggie', '["--acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"prompt_capabilities":{"image":true},"session_capabilities":{"list":{}}}',
     '[]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_kimi', '/api/assets/logos/ai-china/kimi.svg', 'Kimi',
     'kimi', 'acp', 'builtin', '{"binary_name":"kimi"}',
     1, 'kimi', '["acp"]', '[]',
     '[".kimi/skills"]',
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"mcp_capabilities":{"http":true,"sse":false},"prompt_capabilities":{"audio":false,"embedded_context":true,"image":true},"session_capabilities":{"list":{},"resume":{}}}',
     '[{"_meta":{"terminal-auth":{"command":"kimi","args":["login"],"label":"Kimi Code Login","env":{},"type":"terminal"}},"description":"Run `kimi login` command in the terminal, then follow the instructions to finish login.","id":"login","name":"Login with Kimi account"}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_opencode', '/api/assets/logos/tools/coding/opencode-light.svg', 'OpenCode',
     'opencode', 'acp', 'builtin', '{"binary_name":"opencode"}',
     1, 'opencode', '["acp"]', '[]',
     '[".opencode/skills"]',
     '{"supports_side_question":false}',
     'build',
     NULL, NULL,
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_copilot', '/api/assets/logos/tools/github.svg', 'Copilot',
     'copilot', 'acp', 'builtin', '{"binary_name":"copilot"}',
     1, 'copilot', '["--acp","--stdio"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"mcp_capabilities":{"http":true,"sse":true},"prompt_capabilities":{"image":true,"audio":false,"embedded_context":true},"session_capabilities":{"list":{}}}',
     '[{"id":"copilot-login","name":"Log in with Copilot CLI","description":"Run `copilot login` in the terminal","_meta":{"terminal-auth":{"command":"copilot","args":["login"],"label":"Copilot Login"}}}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_qoder', '/api/assets/logos/tools/coding/qoder.png', 'Qoder',
     'qoder', 'acp', 'builtin', '{"binary_name":"qodercli"}',
     1, 'qodercli', '["--acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"session_capabilities":{"list":{}},"prompt_capabilities":{"image":true,"audio":true,"embedded_context":true},"mcp_capabilities":{"http":true,"sse":true}}',
     '[{"id":"qodercli-login","name":"Use qodercli login","description":"Use your existing qodercli login for this agent. If needed, sign in from qodercli first."},{"type":"env_var","id":"qoder-personal-access-token","name":"Use QODER_PERSONAL_ACCESS_TOKEN","description":"Requires `QODER_PERSONAL_ACCESS_TOKEN` in the agent environment.","vars":[{"name":"QODER_PERSONAL_ACCESS_TOKEN"}]}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_vibe', '/api/assets/logos/ai-major/mistral.svg', 'Vibe',
     'vibe', 'acp', 'builtin', '{"binary_name":"vibe-acp"}',
     1, 'vibe-acp', '[]', '[]',
     '[".vibe/skills"]',
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"prompt_capabilities":{"audio":false,"embedded_context":true,"image":false},"session_capabilities":{"close":{},"fork":{},"list":{}}}',
     '[]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_cursor', '/api/assets/logos/tools/coding/cursor.png', 'Cursor',
     'cursor', 'acp', 'builtin', '{"binary_name":"agent"}',
     1, 'agent', '["acp"]', '[]',
     '[".cursor/skills"]',
     '{"supports_side_question":false}',
     'agent',
     NULL, NULL,
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_kiro', NULL, 'Kiro',
     'kiro', 'acp', 'builtin', '{"binary_name":"kiro-cli"}',
     1, 'kiro-cli', '["acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"prompt_capabilities":{"image":true,"audio":false,"embedded_context":false},"mcp_capabilities":{"http":true,"sse":false},"session_capabilities":{}}',
     '[{"id":"kiro-login","name":"Kiro Login","description":"Run ''kiro-cli login'' in terminal to authenticate. See https://kiro.dev/docs/cli/authentication/"}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_hermes', '/api/assets/logos/brand/hermes.svg', 'Hermes',
     'hermes', 'acp', 'builtin', '{"binary_name":"hermes"}',
     1, 'hermes', '["acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo',
     NULL, NULL,
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_snow', '/api/assets/logos/tools/coding/snow.png', 'Snow',
     'snow', 'acp', 'builtin', '{"binary_name":"snow"}',
     1, 'snow', '["--acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo',
     NULL, NULL,
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    -- Non-ACP builtins
    ('agent_builtin_nanobot', '/api/assets/logos/tools/nanobot.svg', 'Nanobot',
     NULL, 'nanobot', 'builtin', '{"binary_name":"nanobot"}',
     1, 'nanobot', '["--experimental-acp"]', '[]',
     NULL,
     '{}',
     'yolo',
     NULL, NULL,
     3990,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('agent_builtin_openclaw', '/api/assets/logos/tools/openclaw.svg', 'OpenClaw',
     NULL, 'openclaw-gateway', 'builtin', '{"binary_name":"openclaw"}',
     1, 'openclaw', '[]', '[]',
     NULL,
     '{}',
     'yolo',
     NULL, NULL,
     3900,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    -- Internal
    ('agent_builtin_nomi', '/api/assets/logos/brand/nomi.svg', 'Nomi',
     NULL, 'nomi', 'internal', '{}',
     1, NULL, '[]', '[]',
     '[".nomi/skills"]',
     '{"supports_team":true}',
     'yolo',
     NULL, NULL,
     100,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000);

------------------------------------------------------------------------
-- Remote Agents & MCP
------------------------------------------------------------------------

-- Local-only INTEGER id; the cross-device identity is device_id (a derived
-- key), not this row id.
CREATE TABLE IF NOT EXISTS remote_agents (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    name               TEXT    NOT NULL,
    protocol           TEXT    NOT NULL,
    url                TEXT    NOT NULL,
    auth_type          TEXT    NOT NULL,
    auth_token         TEXT,
    allow_insecure     INTEGER NOT NULL DEFAULT 0,
    avatar             TEXT,
    description        TEXT,
    device_id          TEXT,
    device_public_key  TEXT,
    device_private_key TEXT,
    device_token       TEXT,
    status             TEXT    NOT NULL DEFAULT 'unknown',
    last_connected_at  INTEGER,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_remote_agents_status ON remote_agents(status);

CREATE TABLE IF NOT EXISTS mcp_servers (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    name             TEXT    NOT NULL UNIQUE,
    description      TEXT,
    enabled          INTEGER NOT NULL DEFAULT 0,
    transport_type   TEXT    NOT NULL,
    transport_config TEXT    NOT NULL,
    tools            TEXT,
    last_test_status TEXT    NOT NULL DEFAULT 'disconnected',
    last_connected   INTEGER,
    original_json    TEXT,
    builtin          INTEGER NOT NULL DEFAULT 0,
    deleted_at       INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mcp_servers_name ON mcp_servers(name);
CREATE INDEX IF NOT EXISTS idx_mcp_servers_enabled ON mcp_servers(enabled);
CREATE INDEX IF NOT EXISTS idx_mcp_servers_deleted_at ON mcp_servers(deleted_at);

CREATE TABLE IF NOT EXISTS oauth_tokens (
    server_url    TEXT PRIMARY KEY NOT NULL,
    access_token  TEXT    NOT NULL,
    refresh_token TEXT,
    token_type    TEXT    NOT NULL DEFAULT 'bearer',
    expires_at    INTEGER,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

------------------------------------------------------------------------
-- Assistants  (channel / IM-facing: cross-device TEXT ids)
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS assistants (
    id                      TEXT PRIMARY KEY,
    name                    TEXT NOT NULL,
    description             TEXT,
    avatar                  TEXT,
    preset_agent_type       TEXT NOT NULL DEFAULT 'gemini',
    enabled_skills          TEXT,
    custom_skill_names      TEXT,
    disabled_builtin_skills TEXT,
    prompts                 TEXT,
    models                  TEXT,
    name_i18n               TEXT,
    description_i18n        TEXT,
    prompts_i18n            TEXT,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_assistants_updated_at ON assistants(updated_at DESC);

-- assistant_id may reference a builtin assistant defined in JSON (not a row
-- in `assistants`), so NO foreign key: orphans are GC'd by the assistant
-- service's delete_orphans(valid_ids).
CREATE TABLE IF NOT EXISTS assistant_overrides (
    assistant_id      TEXT PRIMARY KEY,
    enabled           INTEGER NOT NULL DEFAULT 1,
    sort_order        INTEGER NOT NULL DEFAULT 0,
    preset_agent_type TEXT,
    last_used_at      INTEGER,
    updated_at        INTEGER NOT NULL
);

-- Multi-row channel plugins: one row per connected bot. `pet_id` binds the
-- bot to one pet (filesystem entity, no FK). `bot_key` is the platform-level
-- bot identity; the partial unique index guarantees one bot binds to at most
-- one pet. (Merged from former migration 003; legacy backfill dropped.)
CREATE TABLE IF NOT EXISTS assistant_plugins (
    id             TEXT PRIMARY KEY NOT NULL,
    type           TEXT    NOT NULL,
    name           TEXT    NOT NULL,
    enabled        INTEGER NOT NULL DEFAULT 0,
    config         TEXT    NOT NULL,
    status         TEXT,
    last_connected INTEGER,
    pet_id         TEXT,
    bot_key        TEXT,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_assistant_plugins_type_bot_key
    ON assistant_plugins(type, bot_key) WHERE bot_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS assistant_users (
    id               TEXT PRIMARY KEY NOT NULL,   -- achu_{uuidv7}
    platform_user_id TEXT    NOT NULL,
    platform_type    TEXT    NOT NULL,
    display_name     TEXT,
    authorized_at    INTEGER NOT NULL,
    last_active      INTEGER,
    session_id       TEXT,
    UNIQUE (platform_user_id, platform_type)
);

-- channel_id: which bot plugin owns this session (so two bots sharing a chat
-- get isolated sessions). Merged from former migration 003.
CREATE TABLE IF NOT EXISTS assistant_sessions (
    id              TEXT PRIMARY KEY NOT NULL,
    user_id         TEXT    NOT NULL,
    agent_type      TEXT    NOT NULL,
    conversation_id INTEGER,
    workspace       TEXT,
    chat_id         TEXT,
    channel_id      TEXT,
    created_at      INTEGER NOT NULL,
    last_activity   INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES assistant_users(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE SET NULL,
    FOREIGN KEY (channel_id) REFERENCES assistant_plugins(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_assistant_sessions_user_id ON assistant_sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_assistant_sessions_user_chat ON assistant_sessions(user_id, chat_id);
CREATE INDEX IF NOT EXISTS idx_assistant_sessions_channel ON assistant_sessions(channel_id);

CREATE TABLE IF NOT EXISTS assistant_pairing_codes (
    code             TEXT PRIMARY KEY NOT NULL,
    platform_user_id TEXT    NOT NULL,
    platform_type    TEXT    NOT NULL,
    display_name     TEXT,
    requested_at     INTEGER NOT NULL,
    expires_at       INTEGER NOT NULL,
    status           TEXT    NOT NULL DEFAULT 'pending'
                             CHECK (status IN ('pending', 'approved', 'rejected', 'expired'))
);
CREATE INDEX IF NOT EXISTS idx_pairing_codes_status ON assistant_pairing_codes(status);

------------------------------------------------------------------------
-- Teams  (cross-device TEXT ids: team_/slot_/task_ travel via guide MCP
-- result + team wake prompts + MCP env, recorded in ACP transcripts)
------------------------------------------------------------------------

-- lead_agent_id is an agent-address (a slot_id, or the 'lead'/'user'
-- sentinel), NOT a foreign key.
CREATE TABLE IF NOT EXISTS teams (
    id             TEXT PRIMARY KEY NOT NULL,   -- team_{uuidv7}
    user_id        TEXT    NOT NULL DEFAULT 'system_default_user',
    name           TEXT    NOT NULL,
    workspace      TEXT    NOT NULL DEFAULT '',
    workspace_mode TEXT    NOT NULL DEFAULT 'shared',
    lead_agent_id  TEXT,
    session_mode   TEXT,
    agents_version TEXT    NOT NULL DEFAULT '1.0.0',
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_teams_user_id ON teams(user_id);
CREATE INDEX IF NOT EXISTS idx_teams_updated_at ON teams(updated_at);

-- Columnized from the former teams.agents JSON array. slot_id stays a string
-- PK because it is transmitted in the MCP env (TEAM_AGENT_SLOT_ID) and remote
-- protocol. conversation_id FK CASCADE: create flow inserts the slot's
-- conversation before the slot row (see spec §9.A).
CREATE TABLE IF NOT EXISTS team_agents (
    slot_id           TEXT PRIMARY KEY NOT NULL,   -- slot_{uuidv7}
    team_id           TEXT    NOT NULL,
    name              TEXT    NOT NULL DEFAULT '',
    role              TEXT    NOT NULL DEFAULT 'teammate',
    conversation_id   INTEGER,
    backend           TEXT    NOT NULL DEFAULT '',
    model             TEXT    NOT NULL DEFAULT '',
    custom_agent_id   TEXT,
    status            TEXT,
    conversation_type TEXT,
    cli_path          TEXT,
    sort_order        INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (team_id) REFERENCES teams(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_team_agents_team ON team_agents(team_id, sort_order);
CREATE INDEX IF NOT EXISTS idx_team_agents_conversation ON team_agents(conversation_id);

-- to_agent_id / from_agent_id are agent-addresses (slot_id or 'user'/'lead'
-- sentinel), NOT foreign keys.
CREATE TABLE IF NOT EXISTS mailbox (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    team_id       TEXT    NOT NULL,
    to_agent_id   TEXT    NOT NULL,
    from_agent_id TEXT    NOT NULL,
    type          TEXT    NOT NULL CHECK (type IN ('message', 'idle_notification', 'shutdown_request')),
    content       TEXT    NOT NULL,
    summary       TEXT,
    files         TEXT,
    read          INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    FOREIGN KEY (team_id) REFERENCES teams(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_mailbox_team_to_read ON mailbox(team_id, to_agent_id, read);
CREATE INDEX IF NOT EXISTS idx_mailbox_team_id ON mailbox(team_id);

-- owner is an agent-address (slot_id or sentinel), NOT a foreign key.
-- blocked_by/blocks JSON arrays are columnized into team_task_deps.
CREATE TABLE IF NOT EXISTS team_tasks (
    id          TEXT    PRIMARY KEY NOT NULL,   -- task_{uuidv7}
    team_id     TEXT    NOT NULL,
    subject     TEXT    NOT NULL,
    description TEXT,
    status      TEXT    NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'in_progress', 'completed', 'deleted')),
    owner       TEXT,
    metadata    TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    FOREIGN KEY (team_id) REFERENCES teams(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_team_tasks_team_id ON team_tasks(team_id);

-- Single-directed dependency edge (replaces the bidirectional blocked_by/
-- blocks JSON arrays). "who blocks X" = WHERE blocked_task_id=X; "what X
-- blocks" = WHERE blocker_task_id=X.
CREATE TABLE IF NOT EXISTS team_task_deps (
    blocker_task_id TEXT NOT NULL,
    blocked_task_id TEXT NOT NULL,
    PRIMARY KEY (blocker_task_id, blocked_task_id),
    CHECK (blocker_task_id <> blocked_task_id),
    FOREIGN KEY (blocker_task_id) REFERENCES team_tasks(id) ON DELETE CASCADE,
    FOREIGN KEY (blocked_task_id) REFERENCES team_tasks(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_team_task_deps_blocked ON team_task_deps(blocked_task_id);

------------------------------------------------------------------------
-- Cron Jobs
------------------------------------------------------------------------

-- conversation_id is now NULLABLE with a FK (was NOT NULL, no FK): a
-- new_conversation job has no target until first fire. terminal_session_id
-- FK SET NULL (terminal lazily created).
CREATE TABLE IF NOT EXISTS cron_jobs (
    id                   TEXT    PRIMARY KEY NOT NULL,   -- cron_{uuidv7}
    name                 TEXT    NOT NULL,
    enabled              INTEGER NOT NULL DEFAULT 1,
    schedule_kind        TEXT    NOT NULL CHECK(schedule_kind IN ('at', 'every', 'cron')),
    schedule_value       TEXT    NOT NULL,
    schedule_tz          TEXT,
    schedule_description TEXT,
    payload_message      TEXT    NOT NULL,
    execution_mode       TEXT    NOT NULL DEFAULT 'existing'
                                 CHECK(execution_mode IN ('existing', 'new_conversation')),
    agent_config         TEXT,
    conversation_id      INTEGER,
    conversation_title   TEXT,
    agent_type           TEXT    NOT NULL,
    created_by           TEXT    NOT NULL CHECK(created_by IN ('user', 'agent')),
    skill_content        TEXT,
    description          TEXT,
    target_kind          TEXT    NOT NULL DEFAULT 'agent',
    terminal_mode        TEXT,
    terminal_session_id  INTEGER,
    terminal_command     TEXT,
    terminal_args        TEXT,
    terminal_script      TEXT,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    next_run_at          INTEGER,
    last_run_at          INTEGER,
    last_status          TEXT    CHECK(last_status IN ('ok', 'error', 'skipped', 'missed')),
    last_error           TEXT,
    run_count            INTEGER NOT NULL DEFAULT 0,
    retry_count          INTEGER NOT NULL DEFAULT 0,
    max_retries          INTEGER NOT NULL DEFAULT 3,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE SET NULL,
    FOREIGN KEY (terminal_session_id) REFERENCES terminal_sessions(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_cron_jobs_conversation ON cron_jobs(conversation_id);
CREATE INDEX IF NOT EXISTS idx_cron_jobs_next_run ON cron_jobs(next_run_at) WHERE enabled = 1;
CREATE INDEX IF NOT EXISTS idx_cron_jobs_agent_type ON cron_jobs(agent_type);
CREATE INDEX IF NOT EXISTS idx_cron_jobs_terminal_session ON cron_jobs(terminal_session_id);

------------------------------------------------------------------------
-- Terminal sessions
------------------------------------------------------------------------

-- PTY-backed interactive sessions. Scrollback kept in-memory (not persisted).
--   autowork: JSON {enabled, tag, max_requirements} — AutoWork config, nullable.
--   idmm:     JSON blob — IDMM per-terminal stall-supervision config, nullable.
CREATE TABLE IF NOT EXISTS terminal_sessions (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT    NOT NULL,
    cwd           TEXT    NOT NULL,
    command       TEXT    NOT NULL,
    args          TEXT    NOT NULL DEFAULT '[]',
    env           TEXT,
    backend       TEXT,
    mode          TEXT,
    cols          INTEGER NOT NULL DEFAULT 80,
    rows          INTEGER NOT NULL DEFAULT 24,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    last_status   TEXT    NOT NULL DEFAULT 'running'
                  CHECK(last_status IN ('running', 'exited', 'error')),
    exit_code     INTEGER,
    user_id       TEXT    NOT NULL,
    pinned        INTEGER NOT NULL DEFAULT 0,
    pinned_at     INTEGER,
    autowork      TEXT,
    idmm          TEXT,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_terminal_sessions_user ON terminal_sessions(user_id);

------------------------------------------------------------------------
-- Requirements Platform
------------------------------------------------------------------------

-- owner_session_id records the executing session and is a dual-domain
-- address (a conv_* conversation id OR a term_* terminal id), discriminated
-- by owner_kind. No FK (single column cannot reference two tables); when a
-- conversation/terminal is deleted the service clears the matching owner
-- (clear_owner_for_session, spec §9.B). The owner token replaces the former
-- redundant claimed_by column.
CREATE TABLE IF NOT EXISTS requirements (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    title            TEXT    NOT NULL,
    content          TEXT    NOT NULL DEFAULT '',
    tag              TEXT    NOT NULL,
    order_key        TEXT    NOT NULL DEFAULT '',
    sort_seq         TEXT    NOT NULL DEFAULT '',           -- normalized sortable form of order_key (NOT a display seq)
    status           TEXT    NOT NULL DEFAULT 'pending',
    priority         INTEGER NOT NULL DEFAULT 0,
    completion_note  TEXT,
    owner_session_id INTEGER,                               -- conversation OR terminal id; no FK
    owner_kind       TEXT    CHECK(owner_kind IS NULL OR owner_kind IN ('conversation', 'terminal')),
    claimed_at       INTEGER,
    lease_expires_at INTEGER,
    started_at       INTEGER,
    completed_at     INTEGER,
    attempt_count    INTEGER NOT NULL DEFAULT 0,
    created_by       TEXT    NOT NULL DEFAULT 'user',
    extra            TEXT    NOT NULL DEFAULT '{}',
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    CHECK ((owner_session_id IS NULL) = (owner_kind IS NULL))
);
CREATE INDEX IF NOT EXISTS idx_requirements_tag_status ON requirements(tag, status);
CREATE INDEX IF NOT EXISTS idx_requirements_tag_order  ON requirements(tag, sort_seq);
CREATE INDEX IF NOT EXISTS idx_requirements_owner      ON requirements(owner_session_id);
CREATE INDEX IF NOT EXISTS idx_requirements_status     ON requirements(status);

-- AutoWork tag-level pause. paused_req_id FK SET NULL: the triggering
-- requirement may be deleted while the pause stays.
CREATE TABLE IF NOT EXISTS requirement_tags (
    tag           TEXT    PRIMARY KEY,
    paused        INTEGER NOT NULL DEFAULT 0,
    paused_reason TEXT,
    paused_req_id INTEGER,
    paused_at     INTEGER,
    FOREIGN KEY (paused_req_id) REFERENCES requirements(id) ON DELETE SET NULL
);

------------------------------------------------------------------------
-- Webhooks + per-tag settings (AutoWork completion notifications)
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS webhooks (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL,
    platform    TEXT NOT NULL DEFAULT 'lark',
    url         TEXT NOT NULL,
    secret      TEXT,
    description TEXT NOT NULL DEFAULT '',
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tag_settings (
    tag         TEXT PRIMARY KEY,
    webhook_id  INTEGER,
    description TEXT NOT NULL DEFAULT '',
    updated_at  INTEGER NOT NULL,
    FOREIGN KEY (webhook_id) REFERENCES webhooks(id) ON DELETE SET NULL
);

------------------------------------------------------------------------
-- Knowledge Base platform
------------------------------------------------------------------------

-- Cross-device: kb id is an agent-facing gateway tool argument/result, so it
-- enters the master agent's ACP transcript -> string global id.
CREATE TABLE IF NOT EXISTS knowledge_bases (
    id          TEXT    PRIMARY KEY,            -- kb_{uuidv7}
    name        TEXT    NOT NULL,
    description TEXT    NOT NULL DEFAULT '',
    root_path   TEXT    NOT NULL,
    managed     INTEGER NOT NULL DEFAULT 1,
    extra       TEXT    NOT NULL DEFAULT '{}',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- Per-target mount binding. The former composite PK (target_kind,target_id)
-- and JSON kb_ids array are redesigned into a surrogate binding_id +
-- type-discriminated nullable target columns (CHECK exactly-one) +
-- knowledge_binding_bases junction. target_kind set is owned by the
-- nomifun-knowledge service (BINDING_KINDS = workpath/conversation/terminal/pet).
--   workpath: normalized workspace path key (not an entity, no FK)
--   conversation/terminal: real TEXT FK CASCADE (binding dies with the session)
--   pet: pet_{} filesystem entity (no FK; pet service cleans on delete)
--   writeback_mode: 'staged' confines agent writes to {kb}/_inbox/{conversation_id}/
CREATE TABLE IF NOT EXISTS knowledge_bindings (
    binding_id      INTEGER PRIMARY KEY AUTOINCREMENT,
    target_kind     TEXT    NOT NULL,
    target_workpath TEXT,
    target_conv_id  INTEGER,
    target_term_id  INTEGER,
    target_pet_id   TEXT,
    enabled         INTEGER NOT NULL DEFAULT 0,
    writeback       INTEGER NOT NULL DEFAULT 0,
    writeback_mode  TEXT    NOT NULL DEFAULT 'staged'
                            CHECK(writeback_mode IN ('staged', 'direct')),
    updated_at      INTEGER NOT NULL,
    FOREIGN KEY (target_conv_id) REFERENCES conversations(id) ON DELETE CASCADE,
    FOREIGN KEY (target_term_id) REFERENCES terminal_sessions(id) ON DELETE CASCADE,
    CHECK (
        (target_kind = 'workpath'     AND target_workpath IS NOT NULL
            AND target_conv_id IS NULL AND target_term_id IS NULL AND target_pet_id IS NULL)
     OR (target_kind = 'conversation' AND target_conv_id  IS NOT NULL
            AND target_workpath IS NULL AND target_term_id IS NULL AND target_pet_id IS NULL)
     OR (target_kind = 'terminal'     AND target_term_id  IS NOT NULL
            AND target_workpath IS NULL AND target_conv_id IS NULL AND target_pet_id IS NULL)
     OR (target_kind = 'pet'          AND target_pet_id   IS NOT NULL
            AND target_workpath IS NULL AND target_conv_id IS NULL AND target_term_id IS NULL)
    )
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_kb_binding_workpath ON knowledge_bindings(target_workpath) WHERE target_workpath IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS uq_kb_binding_conv     ON knowledge_bindings(target_conv_id)  WHERE target_conv_id  IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS uq_kb_binding_term     ON knowledge_bindings(target_term_id)  WHERE target_term_id  IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS uq_kb_binding_pet      ON knowledge_bindings(target_pet_id)   WHERE target_pet_id   IS NOT NULL;

-- Columnized from the former knowledge_bindings.kb_ids JSON array.
CREATE TABLE IF NOT EXISTS knowledge_binding_bases (
    binding_id INTEGER NOT NULL,
    kb_id      TEXT    NOT NULL,
    position   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (binding_id, kb_id),
    FOREIGN KEY (binding_id) REFERENCES knowledge_bindings(binding_id) ON DELETE CASCADE,
    FOREIGN KEY (kb_id)      REFERENCES knowledge_bases(id)            ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_kb_binding_bases_kb ON knowledge_binding_bases(kb_id);

------------------------------------------------------------------------
-- Attachments (requirement images; was migration 002)
------------------------------------------------------------------------

-- Cross-device: att id rides the requirement DTO into the master agent's ACP
-- transcript (nomi_requirement_update result) -> string global id. The former
-- generic (kind, target_id) polymorphism is collapsed to a real
-- requirement_id FK (only the requirement kind was ever used).
CREATE TABLE IF NOT EXISTS attachments (
    id             TEXT    PRIMARY KEY,              -- att_{uuidv7}
    requirement_id INTEGER NOT NULL,
    file_name      TEXT    NOT NULL,                 -- original display name (deduped per requirement)
    rel_path       TEXT    NOT NULL,                 -- relative to data_dir
    mime           TEXT    NOT NULL,
    size_bytes     INTEGER NOT NULL,
    created_by     TEXT,
    created_at     INTEGER NOT NULL,
    FOREIGN KEY (requirement_id) REFERENCES requirements(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_attachments_requirement ON attachments(requirement_id);

------------------------------------------------------------------------
-- Conversation <-> MCP server selection (was conversations.extra.selected_mcp_server_ids)
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS conversation_mcp_servers (
    conversation_id INTEGER NOT NULL,
    mcp_server_id   INTEGER NOT NULL,
    sort_order      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (conversation_id, mcp_server_id),
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
    FOREIGN KEY (mcp_server_id)   REFERENCES mcp_servers(id)   ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_conversation_mcp_servers_mcp ON conversation_mcp_servers(mcp_server_id);
