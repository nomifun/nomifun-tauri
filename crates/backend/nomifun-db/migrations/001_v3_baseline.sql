-- NomiFun clean baseline (data contract v3).
--
-- This is a new, intentionally incompatible dataset lineage. Historical
-- product rows are not copied into this schema.
--
-- Every product-owned persistent table has the same local technical key:
--
--     id INTEGER PRIMARY KEY AUTOINCREMENT
--
-- Cross-boundary identities use explicitly named UUIDv7 columns. Relations
-- are indexed logical links maintained by repositories and audited by the
-- runtime schema contract. SQLite does not own relation deletion behavior.

CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id       TEXT NOT NULL UNIQUE
                  CHECK (
                      length(user_id) = 36
                      AND lower(user_id) = user_id
                      AND user_id GLOB '????????-????-7???-[89ab]???-????????????'
                      AND replace(user_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                  ),
    username      TEXT NOT NULL UNIQUE,
    email         TEXT UNIQUE,
    password_hash TEXT NOT NULL,
    avatar_path   TEXT,
    jwt_secret    TEXT,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    last_login    INTEGER
);

CREATE TABLE conversations (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id       TEXT NOT NULL UNIQUE
                          CHECK (
                              length(conversation_id) = 36
                              AND lower(conversation_id) = conversation_id
                              AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????'
                              AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                          ),
    user_id               TEXT NOT NULL,
    name                  TEXT NOT NULL,
    type                  TEXT NOT NULL
                          CHECK (type IN (
                              'acp',
                              'openclaw-gateway',
                              'nanobot',
                              'remote',
                              'nomi'
                          )),
    extra                 TEXT NOT NULL DEFAULT '{}'
                          CHECK (json_valid(extra) AND json_type(extra) = 'object'),
    model                 TEXT CHECK (
                              model IS NULL
                              OR (json_valid(model) AND json_type(model) = 'object')
                          ),
    status                TEXT NOT NULL DEFAULT 'pending'
                          CHECK (status IN ('pending', 'running', 'finished')),
    source                TEXT,
    channel_chat_id       TEXT,
    pinned                INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    pinned_at             INTEGER,
    cron_job_id           TEXT
                          CHECK (
                              cron_job_id IS NULL
                              OR (
                                  length(cron_job_id) = 36
                                  AND lower(cron_job_id) = cron_job_id
                                  AND cron_job_id GLOB '????????-????-7???-[89ab]???-????????????'
                                  AND replace(cron_job_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                              )
                          ),
    preset_id             TEXT,
    preset_revision       INTEGER,
    preset_snapshot       TEXT,
    delegation_policy     TEXT NOT NULL DEFAULT 'automatic'
                          CHECK (delegation_policy IN ('disabled', 'automatic', 'prefer_parallel')),
    execution_model_pool  TEXT CHECK (
                              execution_model_pool IS NULL
                              OR (json_valid(execution_model_pool)
                                  AND json_type(execution_model_pool) = 'object')
                          ),
    decision_policy       TEXT NOT NULL DEFAULT 'automatic'
                          CHECK (decision_policy IN ('automatic', 'ask_user')),
    execution_template_id TEXT,
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL,
    CHECK (execution_template_id IS NULL OR (length(execution_template_id) = 36 AND lower(execution_template_id) = execution_template_id AND execution_template_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(execution_template_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (preset_id IS NULL OR (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (length(user_id) = 36 AND lower(user_id) = user_id AND user_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(user_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id      TEXT NOT NULL UNIQUE
                    CHECK (
                        length(message_id) = 36
                        AND lower(message_id) = message_id
                        AND message_id GLOB '????????-????-7???-[89ab]???-????????????'
                        AND replace(message_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                    ),
    conversation_id TEXT NOT NULL,
    msg_id          TEXT,
    type            TEXT NOT NULL,
    content         TEXT NOT NULL DEFAULT '{}',
    position        TEXT CHECK (position IN ('left', 'right', 'center', 'pop')),
    status          TEXT CHECK (status IN ('finish', 'pending', 'error', 'work')),
    hidden          INTEGER NOT NULL DEFAULT 0 CHECK (hidden IN (0, 1)),
    created_at      INTEGER NOT NULL,
    CHECK (length(conversation_id) = 36 AND lower(conversation_id) = conversation_id AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
    CHECK (msg_id IS NULL OR (length(msg_id) = 36 AND lower(msg_id) = msg_id AND msg_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(msg_id, '-', '') NOT GLOB '*[^0-9a-f]*'))
);

CREATE TABLE terminal_sessions (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    terminal_id   TEXT NOT NULL UNIQUE
                  CHECK (
                      length(terminal_id) = 36
                      AND lower(terminal_id) = terminal_id
                      AND terminal_id GLOB '????????-????-7???-[89ab]???-????????????'
                      AND replace(terminal_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                  ),
    name          TEXT NOT NULL,
    cwd           TEXT NOT NULL,
    command       TEXT NOT NULL,
    args          TEXT NOT NULL DEFAULT '[]',
    env           TEXT,
    backend       TEXT,
    mode          TEXT,
    cols          INTEGER NOT NULL DEFAULT 80,
    rows          INTEGER NOT NULL DEFAULT 24,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    last_status   TEXT NOT NULL DEFAULT 'running'
                  CHECK (last_status IN ('running', 'exited', 'error')),
    exit_code     INTEGER,
    user_id       TEXT NOT NULL,
    pinned        INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    pinned_at     INTEGER,
    autowork      TEXT,
    idmm          TEXT CHECK (
                      idmm IS NULL
                      OR (json_valid(idmm) AND json_type(idmm) = 'object')
                  ),
    CHECK (length(user_id) = 36 AND lower(user_id) = user_id AND user_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(user_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE providers (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    provider_id          TEXT NOT NULL UNIQUE
                         CHECK (
                             length(provider_id) = 36
                             AND lower(provider_id) = provider_id
                             AND provider_id GLOB '????????-????-7???-[89ab]???-????????????'
                             AND replace(provider_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                         ),
    platform             TEXT NOT NULL,
    name                 TEXT NOT NULL,
    base_url             TEXT NOT NULL,
    api_key_encrypted    TEXT NOT NULL,
    models               TEXT NOT NULL DEFAULT '[]',
    enabled              INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    capabilities         TEXT NOT NULL DEFAULT '[]',
    model_protocols      TEXT,
    model_enabled        TEXT,
    model_health         TEXT,
    bedrock_config       TEXT,
    is_full_url          INTEGER NOT NULL DEFAULT 0 CHECK (is_full_url IN (0, 1)),
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    model_descriptions   TEXT NOT NULL DEFAULT '{}',
    model_context_limits TEXT NOT NULL DEFAULT '{}',
    sort_order           INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE agent_metadata (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id            TEXT NOT NULL UNIQUE
                        CHECK (
                            length(agent_id) = 36
                            AND lower(agent_id) = agent_id
                            AND agent_id GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(agent_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                        ),
    icon                TEXT,
    name                TEXT NOT NULL,
    name_i18n           TEXT,
    description         TEXT,
    description_i18n    TEXT,
    backend             TEXT,
    agent_type          TEXT NOT NULL,
    agent_source        TEXT NOT NULL,
    agent_source_info   TEXT,
    source_key          TEXT UNIQUE
                        CHECK (source_key IS NULL OR trim(source_key) <> ''),
    enabled             INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
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

CREATE TABLE agent_execution_templates (
    id                     INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_template_id  TEXT NOT NULL UNIQUE
                           CHECK (
                               length(execution_template_id) = 36
                               AND lower(execution_template_id) = execution_template_id
                               AND execution_template_id GLOB '????????-????-7???-[89ab]???-????????????'
                               AND replace(execution_template_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                           ),
    user_id                TEXT NOT NULL,
    name                   TEXT NOT NULL CHECK (trim(name) <> ''),
    description            TEXT,
    max_parallel           INTEGER CHECK (max_parallel IS NULL OR max_parallel BETWEEN 1 AND 64),
    work_dir               TEXT,
    context                TEXT CHECK (context IS NULL OR json_valid(context)),
    primary_participant_id TEXT NOT NULL
                           CHECK (
                               length(primary_participant_id) = 36
                               AND lower(primary_participant_id) = primary_participant_id
                               AND primary_participant_id GLOB '????????-????-7???-[89ab]???-????????????'
                               AND replace(primary_participant_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                           ),
    version                INTEGER NOT NULL DEFAULT 0 CHECK (version >= 0),
    created_at             INTEGER NOT NULL,
    updated_at             INTEGER NOT NULL CHECK (updated_at >= created_at),
    CHECK (length(user_id) = 36 AND lower(user_id) = user_id AND user_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(user_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE agent_executions (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id        TEXT NOT NULL UNIQUE
                        CHECK (
                            length(execution_id) = 36
                            AND lower(execution_id) = execution_id
                            AND execution_id GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(execution_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                        ),
    user_id             TEXT NOT NULL,
    goal                TEXT NOT NULL CHECK (trim(goal) <> ''),
    status              TEXT NOT NULL CHECK (status IN (
                            'planning', 'awaiting_approval', 'running', 'paused',
                            'waiting_input', 'completed', 'completed_with_failures',
                            'failed', 'cancelled'
                        )),
    plan_gate           TEXT NOT NULL CHECK (plan_gate IN ('automatic', 'require_approval')),
    adaptation_policy   TEXT NOT NULL CHECK (adaptation_policy IN ('fixed', 'adaptive')),
    decision_policy     TEXT NOT NULL CHECK (decision_policy IN ('automatic', 'ask_user')),
    delegation_policy   TEXT NOT NULL CHECK (delegation_policy IN ('disabled', 'automatic', 'prefer_parallel')),
    max_parallel        INTEGER NOT NULL DEFAULT 4 CHECK (max_parallel BETWEEN 1 AND 64),
    work_dir            TEXT,
    initial_plan_input  TEXT NOT NULL CHECK (
                            json_valid(initial_plan_input)
                            AND json_type(initial_plan_input) = 'object'
                        ),
    summary             TEXT,
    total_tokens        INTEGER CHECK (total_tokens IS NULL OR total_tokens >= 0),
    version             INTEGER NOT NULL DEFAULT 0 CHECK (version >= 0),
    plan_revision       INTEGER NOT NULL DEFAULT 0 CHECK (plan_revision >= 0),
    event_sequence      INTEGER NOT NULL DEFAULT 0 CHECK (event_sequence >= 0),
    lease_owner         TEXT,
    lease_expires_at    INTEGER,
    deleted_at          INTEGER,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    CHECK (
        (lease_owner IS NULL AND lease_expires_at IS NULL)
        OR (trim(lease_owner) <> '' AND lease_expires_at IS NOT NULL)
    ),
    CHECK (updated_at >= created_at),
    CHECK (length(user_id) = 36 AND lower(user_id) = user_id AND user_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(user_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE knowledge_bases (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    knowledge_base_id TEXT NOT NULL UNIQUE
                      CHECK (
                          length(knowledge_base_id) = 36
                          AND lower(knowledge_base_id) = knowledge_base_id
                          AND knowledge_base_id GLOB '????????-????-7???-[89ab]???-????????????'
                          AND replace(knowledge_base_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                      ),
    name              TEXT NOT NULL,
    description       TEXT NOT NULL DEFAULT '',
    root_path         TEXT NOT NULL,
    managed           INTEGER NOT NULL DEFAULT 1 CHECK (managed IN (0, 1)),
    extra             TEXT NOT NULL DEFAULT '{}',
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    tags              TEXT
);

CREATE TABLE attachments (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    attachment_id  TEXT NOT NULL UNIQUE
                   CHECK (
                       length(attachment_id) = 36
                       AND lower(attachment_id) = attachment_id
                       AND attachment_id GLOB '????????-????-7???-[89ab]???-????????????'
                       AND replace(attachment_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                   ),
    requirement_id TEXT NOT NULL,
    file_name      TEXT NOT NULL,
    rel_path       TEXT NOT NULL,
    mime           TEXT NOT NULL,
    size_bytes     INTEGER NOT NULL,
    created_by     TEXT,
    created_at     INTEGER NOT NULL,
    UNIQUE (requirement_id, file_name),
    CHECK (length(requirement_id) = 36 AND lower(requirement_id) = requirement_id AND requirement_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(requirement_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE remote_agents (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    remote_agent_id    TEXT NOT NULL UNIQUE
                       CHECK (
                           length(remote_agent_id) = 36
                           AND lower(remote_agent_id) = remote_agent_id
                           AND remote_agent_id GLOB '????????-????-7???-[89ab]???-????????????'
                           AND replace(remote_agent_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                       ),
    name               TEXT NOT NULL,
    protocol           TEXT NOT NULL,
    url                TEXT NOT NULL,
    auth_type          TEXT NOT NULL,
    auth_token         TEXT,
    allow_insecure     INTEGER NOT NULL DEFAULT 0 CHECK (allow_insecure IN (0, 1)),
    avatar             TEXT,
    description        TEXT,
    device_id          TEXT,
    device_public_key  TEXT,
    device_private_key TEXT,
    device_token       TEXT,
    status             TEXT NOT NULL DEFAULT 'unknown',
    last_connected_at  INTEGER,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
);

CREATE TABLE presets (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    preset_id           TEXT NOT NULL UNIQUE
                        CHECK (
                            length(preset_id) = 36
                            AND lower(preset_id) = preset_id
                            AND preset_id GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                        ),
    source_kind         TEXT NOT NULL DEFAULT 'user'
                        CHECK (source_kind IN ('builtin', 'user', 'extension')),
    source_key          TEXT,
    revision            INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    name                TEXT NOT NULL,
    description         TEXT,
    routing_description TEXT,
    instructions        TEXT NOT NULL DEFAULT '',
    avatar              TEXT,
    fallback_allowed    INTEGER NOT NULL DEFAULT 0 CHECK (fallback_allowed IN (0, 1)),
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    CHECK (
        source_kind = 'user'
        OR (source_key IS NOT NULL AND trim(source_key) <> '')
    )
);

CREATE TABLE workshop_canvases (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    canvas_id          TEXT NOT NULL UNIQUE
                       CHECK (
                           length(canvas_id) = 36
                           AND lower(canvas_id) = canvas_id
                           AND canvas_id GLOB '????????-????-7???-[89ab]???-????????????'
                           AND replace(canvas_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                       ),
    title              TEXT NOT NULL,
    thumbnail_rel_path TEXT,
    node_count         INTEGER NOT NULL DEFAULT 0,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
);

CREATE TABLE workshop_assets (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    asset_id       TEXT NOT NULL UNIQUE
                   CHECK (
                       length(asset_id) = 36
                       AND lower(asset_id) = asset_id
                       AND asset_id GLOB '????????-????-7???-[89ab]???-????????????'
                       AND replace(asset_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                   ),
    kind           TEXT NOT NULL,
    title          TEXT NOT NULL,
    collection     TEXT,
    tags           TEXT NOT NULL DEFAULT '[]',
    rel_path       TEXT,
    thumb_rel_path TEXT,
    mime           TEXT,
    width          INTEGER,
    height         INTEGER,
    bytes          INTEGER,
    text_content   TEXT,
    in_library     INTEGER NOT NULL DEFAULT 1 CHECK (in_library IN (0, 1)),
    origin         TEXT CHECK (
                       origin IS NULL
                       OR (
                           json_valid(origin)
                           AND json_type(origin) = 'object'
                           AND json_type(origin, '$.task_id') IS NULL
                           AND json_type(origin, '$.providerId') IS NULL
                           AND json_type(origin, '$.canvasId') IS NULL
                           AND json_type(origin, '$.nodeId') IS NULL
                           AND json_type(origin, '$.creationTaskId') IS NULL
                           AND (
                               json_type(origin, '$.provider_id') IS NULL
                               OR (
                                   json_type(origin, '$.provider_id') = 'text'
                                   AND length(json_extract(origin, '$.provider_id')) = 36
                                   AND lower(json_extract(origin, '$.provider_id')) =
                                       json_extract(origin, '$.provider_id')
                                   AND json_extract(origin, '$.provider_id')
                                       GLOB '????????-????-7???-[89ab]???-????????????'
                                   AND replace(json_extract(origin, '$.provider_id'), '-', '')
                                       NOT GLOB '*[^0-9a-f]*'
                               )
                           )
                           AND (
                               json_type(origin, '$.canvas_id') IS NULL
                               OR (
                                   json_type(origin, '$.canvas_id') = 'text'
                                   AND length(json_extract(origin, '$.canvas_id')) = 36
                                   AND lower(json_extract(origin, '$.canvas_id')) =
                                       json_extract(origin, '$.canvas_id')
                                   AND json_extract(origin, '$.canvas_id')
                                       GLOB '????????-????-7???-[89ab]???-????????????'
                                   AND replace(json_extract(origin, '$.canvas_id'), '-', '')
                                       NOT GLOB '*[^0-9a-f]*'
                               )
                           )
                           AND (
                               json_type(origin, '$.node_id') IS NULL
                               OR (
                                   json_type(origin, '$.node_id') = 'text'
                                   AND length(json_extract(origin, '$.node_id')) = 36
                                   AND lower(json_extract(origin, '$.node_id')) =
                                       json_extract(origin, '$.node_id')
                                   AND json_extract(origin, '$.node_id')
                                       GLOB '????????-????-7???-[89ab]???-????????????'
                                   AND replace(json_extract(origin, '$.node_id'), '-', '')
                                       NOT GLOB '*[^0-9a-f]*'
                               )
                           )
                           AND (
                               json_type(origin, '$.creation_task_id') IS NULL
                               OR (
                                   json_type(origin, '$.creation_task_id') = 'text'
                                   AND length(json_extract(origin, '$.creation_task_id')) = 36
                                   AND lower(json_extract(origin, '$.creation_task_id')) =
                                       json_extract(origin, '$.creation_task_id')
                                   AND json_extract(origin, '$.creation_task_id')
                                       GLOB '????????-????-7???-[89ab]???-????????????'
                                   AND replace(json_extract(origin, '$.creation_task_id'), '-', '')
                                       NOT GLOB '*[^0-9a-f]*'
                               )
                           )
                       )
                   ),
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

CREATE TABLE channel_sessions (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_session_id TEXT NOT NULL UNIQUE
                       CHECK (
                           length(channel_session_id) = 36
                           AND lower(channel_session_id) = channel_session_id
                           AND channel_session_id GLOB '????????-????-7???-[89ab]???-????????????'
                           AND replace(channel_session_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                       ),
    channel_user_id    TEXT NOT NULL
                       CHECK (
                           length(channel_user_id) = 36
                           AND lower(channel_user_id) = channel_user_id
                           AND channel_user_id GLOB '????????-????-7???-[89ab]???-????????????'
                           AND replace(channel_user_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                       ),
    agent_type         TEXT NOT NULL
                       CHECK (agent_type IN (
                           'acp',
                           'openclaw-gateway',
                           'nanobot',
                           'remote',
                           'nomi'
                       )),
    conversation_id    TEXT,
    workspace          TEXT,
    chat_id            TEXT,
    channel_plugin_id  TEXT
                       CHECK (
                           channel_plugin_id IS NULL
                           OR (
                               length(channel_plugin_id) = 36
                               AND lower(channel_plugin_id) = channel_plugin_id
                               AND channel_plugin_id GLOB '????????-????-7???-[89ab]???-????????????'
                               AND replace(channel_plugin_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                           )
                       ),
    created_at         INTEGER NOT NULL,
    last_activity      INTEGER NOT NULL,
    CHECK (conversation_id IS NULL OR (length(conversation_id) = 36 AND lower(conversation_id) = conversation_id AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'))
);

CREATE TABLE agent_execution_participants (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    participant_id          TEXT NOT NULL UNIQUE
                            CHECK (
                                length(participant_id) = 36
                                AND lower(participant_id) = participant_id
                                AND participant_id GLOB '????????-????-7???-[89ab]???-????????????'
                                AND replace(participant_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                            ),
    execution_id            TEXT NOT NULL,
    source_agent_id         TEXT NOT NULL
                            CHECK (
                                length(source_agent_id) = 36
                                AND lower(source_agent_id) = source_agent_id
                                AND source_agent_id GLOB '????????-????-7???-[89ab]???-????????????'
                                AND replace(source_agent_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                            ),
    preset_id               TEXT,
    preset_revision         INTEGER CHECK (preset_revision IS NULL OR preset_revision > 0),
    preset_snapshot         TEXT,
    provider_id             TEXT,
    model                   TEXT,
    role                    TEXT,
    capability              TEXT,
    constraints             TEXT,
    description             TEXT,
    system_prompt           TEXT,
    enabled_skills          TEXT NOT NULL DEFAULT '[]',
    disabled_builtin_skills TEXT NOT NULL DEFAULT '[]',
    sort_order              INTEGER NOT NULL DEFAULT 0,
    introduced_in_revision  INTEGER NOT NULL CHECK (introduced_in_revision >= 0),
    retired_in_revision     INTEGER,
    created_at              INTEGER NOT NULL,
    CHECK (
        (provider_id IS NULL AND model IS NULL)
        OR (provider_id IS NOT NULL AND model IS NOT NULL)
    ),
    CHECK (length(execution_id) = 36 AND lower(execution_id) = execution_id AND execution_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(execution_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
    CHECK (preset_id IS NULL OR (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (provider_id IS NULL OR (length(provider_id) = 36 AND lower(provider_id) = provider_id AND provider_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(provider_id, '-', '') NOT GLOB '*[^0-9a-f]*'))
);

CREATE TABLE agent_execution_steps (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    step_id                 TEXT NOT NULL UNIQUE
                            CHECK (
                                length(step_id) = 36
                                AND lower(step_id) = step_id
                                AND step_id GLOB '????????-????-7???-[89ab]???-????????????'
                                AND replace(step_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                            ),
    execution_id            TEXT NOT NULL,
    title                   TEXT NOT NULL CHECK (trim(title) <> ''),
    spec                    TEXT NOT NULL,
    role                    TEXT,
    tool_policy             TEXT NOT NULL DEFAULT 'full'
                            CHECK (tool_policy IN ('full', 'read_only', 'read_shell')),
    kind                    TEXT NOT NULL CHECK (kind IN ('agent', 'verify', 'judge', 'loop')),
    agent_mode              TEXT,
    profile                 TEXT,
    fanout_group            TEXT,
    control_policy          TEXT,
    delegation_depth        INTEGER NOT NULL DEFAULT 0 CHECK (delegation_depth BETWEEN 0 AND 4),
    status                  TEXT NOT NULL CHECK (status IN (
                                'pending', 'running', 'waiting_input', 'completed',
                                'failed', 'skipped', 'cancelled'
                            )),
    assigned_participant_id TEXT
                            CHECK (
                                assigned_participant_id IS NULL
                                OR (
                                    length(assigned_participant_id) = 36
                                    AND lower(assigned_participant_id) = assigned_participant_id
                                    AND assigned_participant_id GLOB '????????-????-7???-[89ab]???-????????????'
                                    AND replace(assigned_participant_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                                )
                            ),
    assignment_score        REAL,
    assignment_rationale    TEXT,
    assignment_source       TEXT,
    assignment_locked       INTEGER NOT NULL DEFAULT 0 CHECK (assignment_locked IN (0, 1)),
    failure_policy          TEXT NOT NULL DEFAULT 'fail_execution'
                            CHECK (failure_policy IN ('fail_execution', 'skip_dependents')),
    preset_prompt           TEXT,
    graph_x                 REAL,
    graph_y                 REAL,
    dispatch_after          INTEGER,
    version                 INTEGER NOT NULL DEFAULT 0 CHECK (version >= 0),
    introduced_in_revision  INTEGER NOT NULL CHECK (introduced_in_revision >= 0),
    superseded_in_revision  INTEGER,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL,
    CHECK (length(execution_id) = 36 AND lower(execution_id) = execution_id AND execution_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(execution_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE agent_execution_attempts (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id       TEXT NOT NULL UNIQUE
                     CHECK (
                         length(attempt_id) = 36
                         AND lower(attempt_id) = attempt_id
                         AND attempt_id GLOB '????????-????-7???-[89ab]???-????????????'
                         AND replace(attempt_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                     ),
    execution_id     TEXT NOT NULL,
    step_id          TEXT NOT NULL
                     CHECK (
                         length(step_id) = 36
                         AND lower(step_id) = step_id
                         AND step_id GLOB '????????-????-7???-[89ab]???-????????????'
                         AND replace(step_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                     ),
    attempt_no       INTEGER NOT NULL CHECK (attempt_no >= 0),
    participant_id   TEXT
                     CHECK (
                         participant_id IS NULL
                         OR (
                             length(participant_id) = 36
                             AND lower(participant_id) = participant_id
                             AND participant_id GLOB '????????-????-7???-[89ab]???-????????????'
                             AND replace(participant_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                         )
                     ),
    status           TEXT NOT NULL CHECK (status IN (
                         'queued', 'running', 'waiting_input', 'completed',
                         'failed', 'cancelled', 'interrupted'
                     )),
    trigger_reason   TEXT NOT NULL CHECK (trim(trigger_reason) <> ''),
    effective_config TEXT NOT NULL DEFAULT '{}',
    question         TEXT,
    error            TEXT,
    output_summary   TEXT,
    output_files     TEXT NOT NULL DEFAULT '[]',
    tokens           INTEGER,
    retry_after      INTEGER,
    runtime_state    TEXT,
    started_at       INTEGER,
    finished_at      INTEGER,
    version          INTEGER NOT NULL DEFAULT 0 CHECK (version >= 0),
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    UNIQUE (execution_id, step_id, attempt_no),
    CHECK (length(execution_id) = 36 AND lower(execution_id) = execution_id AND execution_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(execution_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE agent_execution_events (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id          TEXT NOT NULL,
    sequence              INTEGER NOT NULL CHECK (sequence > 0),
    event_type            TEXT NOT NULL CHECK (event_type IN (
                              'created', 'status_changed', 'plan_changed',
                              'step_changed', 'attempt_changed', 'decision_requested',
                              'decision_answered', 'deleted'
                          )),
    step_id               TEXT
                          CHECK (
                              step_id IS NULL
                              OR (
                                  length(step_id) = 36
                                  AND lower(step_id) = step_id
                                  AND step_id GLOB '????????-????-7???-[89ab]???-????????????'
                                  AND replace(step_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                              )
                          ),
    attempt_id            TEXT
                          CHECK (
                              attempt_id IS NULL
                              OR (
                                  length(attempt_id) = 36
                                  AND lower(attempt_id) = attempt_id
                                  AND attempt_id GLOB '????????-????-7???-[89ab]???-????????????'
                                  AND replace(attempt_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                              )
                          ),
    actor_type            TEXT NOT NULL CHECK (actor_type IN ('system', 'user', 'agent')),
    actor_id              TEXT,
    actor_conversation_id TEXT,
    actor_attempt_id      TEXT
                          CHECK (
                              actor_attempt_id IS NULL
                              OR (
                                  length(actor_attempt_id) = 36
                                  AND lower(actor_attempt_id) = actor_attempt_id
                                  AND actor_attempt_id GLOB '????????-????-7???-[89ab]???-????????????'
                                  AND replace(actor_attempt_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                              )
                          ),
    on_behalf_of_user_id  TEXT NOT NULL,
    payload               TEXT NOT NULL CHECK (json_valid(payload)),
    created_at            INTEGER NOT NULL,
    published_at          INTEGER,
    UNIQUE (execution_id, sequence),
    CHECK (attempt_id IS NULL OR step_id IS NOT NULL),
    CHECK (actor_conversation_id IS NULL OR (length(actor_conversation_id) = 36 AND lower(actor_conversation_id) = actor_conversation_id AND actor_conversation_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(actor_conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (actor_id IS NULL OR (length(actor_id) = 36 AND lower(actor_id) = actor_id AND actor_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(actor_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (length(execution_id) = 36 AND lower(execution_id) = execution_id AND execution_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(execution_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
    CHECK (length(on_behalf_of_user_id) = 36 AND lower(on_behalf_of_user_id) = on_behalf_of_user_id AND on_behalf_of_user_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(on_behalf_of_user_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE agent_execution_template_participants (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    template_participant_id TEXT NOT NULL UNIQUE
                            CHECK (
                                length(template_participant_id) = 36
                                AND lower(template_participant_id) = template_participant_id
                                AND template_participant_id GLOB '????????-????-7???-[89ab]???-????????????'
                                AND replace(template_participant_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                            ),
    template_id             TEXT NOT NULL,
    source_agent_id         TEXT NOT NULL
                            CHECK (
                                length(source_agent_id) = 36
                                AND lower(source_agent_id) = source_agent_id
                                AND source_agent_id GLOB '????????-????-7???-[89ab]???-????????????'
                                AND replace(source_agent_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                            ),
    preset_id               TEXT,
    preset_revision         INTEGER,
    preset_snapshot         TEXT,
    provider_id             TEXT,
    model                   TEXT,
    role                    TEXT,
    capability              TEXT,
    constraints             TEXT,
    description             TEXT,
    system_prompt           TEXT,
    enabled_skills          TEXT NOT NULL DEFAULT '[]',
    disabled_builtin_skills TEXT NOT NULL DEFAULT '[]',
    sort_order              INTEGER NOT NULL DEFAULT 0,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL,
    CHECK (preset_id IS NULL OR (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (provider_id IS NULL OR (length(provider_id) = 36 AND lower(provider_id) = provider_id AND provider_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(provider_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (length(template_id) = 36 AND lower(template_id) = template_id AND template_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(template_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE conversation_artifacts (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_artifact_id TEXT NOT NULL UNIQUE
                    CHECK (
                        length(conversation_artifact_id) = 36
                        AND lower(conversation_artifact_id) = conversation_artifact_id
                        AND conversation_artifact_id GLOB '????????-????-7???-[89ab]???-????????????'
                        AND replace(conversation_artifact_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                    ),
    conversation_id TEXT NOT NULL,
    cron_job_id     TEXT
                    CHECK (
                        cron_job_id IS NULL
                        OR (
                            length(cron_job_id) = 36
                            AND lower(cron_job_id) = cron_job_id
                            AND cron_job_id GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(cron_job_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                        )
                    ),
    kind            TEXT NOT NULL CHECK (kind IN ('cron_trigger', 'skill_suggest')),
    status          TEXT NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active', 'pending', 'dismissed', 'saved')),
    payload         TEXT NOT NULL DEFAULT '{}',
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    CHECK (length(conversation_id) = 36 AND lower(conversation_id) = conversation_id AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE conversation_execution_links (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id      TEXT NOT NULL,
    execution_id         TEXT NOT NULL,
    relation             TEXT NOT NULL CHECK (relation IN ('lead', 'attempt')),
    step_id              TEXT
                         CHECK (
                             step_id IS NULL
                             OR (
                                 length(step_id) = 36
                                 AND lower(step_id) = step_id
                                 AND step_id GLOB '????????-????-7???-[89ab]???-????????????'
                                 AND replace(step_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                             )
                         ),
    attempt_id           TEXT
                         CHECK (
                             attempt_id IS NULL
                             OR (
                                 length(attempt_id) = 36
                                 AND lower(attempt_id) = attempt_id
                                 AND attempt_id GLOB '????????-????-7???-[89ab]???-????????????'
                                 AND replace(attempt_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                             )
                         ),
    active               INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    cleanup_completed_at INTEGER,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    CHECK (
        (relation = 'lead' AND step_id IS NULL AND attempt_id IS NULL)
        OR (relation = 'attempt' AND step_id IS NOT NULL AND attempt_id IS NOT NULL)
    ),
    CHECK (length(conversation_id) = 36 AND lower(conversation_id) = conversation_id AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
    CHECK (length(execution_id) = 36 AND lower(execution_id) = execution_id AND execution_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(execution_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE cron_jobs (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    cron_job_id          TEXT NOT NULL UNIQUE
                         CHECK (
                             length(cron_job_id) = 36
                             AND lower(cron_job_id) = cron_job_id
                             AND cron_job_id GLOB '????????-????-7???-[89ab]???-????????????'
                             AND replace(cron_job_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                         ),
    user_id              TEXT NOT NULL,
    name                 TEXT NOT NULL,
    enabled              INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    schedule_kind        TEXT NOT NULL CHECK (schedule_kind IN ('at', 'every', 'cron')),
    schedule_value       TEXT NOT NULL,
    schedule_tz          TEXT,
    schedule_description TEXT,
    payload_message      TEXT NOT NULL,
    execution_mode       TEXT NOT NULL DEFAULT 'existing'
                         CHECK (execution_mode IN ('existing', 'new_conversation')),
    agent_config         TEXT CHECK (
                             agent_config IS NULL
                             OR (json_valid(agent_config) AND json_type(agent_config) = 'object')
                         ),
    preset_id            TEXT,
    preset_revision      INTEGER,
    preset_snapshot      TEXT,
    conversation_id      TEXT,
    conversation_title   TEXT,
    agent_type           TEXT NOT NULL,
    created_by           TEXT NOT NULL CHECK (created_by IN ('user', 'agent')),
    skill_content        TEXT,
    description          TEXT,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    next_run_at          INTEGER,
    last_run_at          INTEGER,
    last_status          TEXT CHECK (last_status IN ('ok', 'error', 'skipped', 'missed')),
    last_error           TEXT,
    run_count            INTEGER NOT NULL DEFAULT 0,
    retry_count          INTEGER NOT NULL DEFAULT 0,
    max_retries          INTEGER NOT NULL DEFAULT 3 CHECK (max_retries >= 0),
    CHECK (conversation_id IS NULL OR (length(conversation_id) = 36 AND lower(conversation_id) = conversation_id AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (preset_id IS NULL OR (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (length(user_id) = 36 AND lower(user_id) = user_id AND user_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(user_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE cron_job_runs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    cron_job_run_id TEXT NOT NULL UNIQUE
                    CHECK (
                        length(cron_job_run_id) = 36
                        AND lower(cron_job_run_id) = cron_job_run_id
                        AND cron_job_run_id GLOB '????????-????-7???-[89ab]???-????????????'
                        AND replace(cron_job_run_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                    ),
    cron_job_id     TEXT NOT NULL
                    CHECK (
                        length(cron_job_id) = 36
                        AND lower(cron_job_id) = cron_job_id
                        AND cron_job_id GLOB '????????-????-7???-[89ab]???-????????????'
                        AND replace(cron_job_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                    ),
    executed_at_ms  INTEGER NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('ok', 'error', 'skipped', 'missed')),
    created_at_ms   INTEGER NOT NULL
);

CREATE TABLE mcp_servers (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    mcp_server_id    TEXT NOT NULL UNIQUE
                     CHECK (
                         length(mcp_server_id) = 36
                         AND lower(mcp_server_id) = mcp_server_id
                         AND mcp_server_id GLOB '????????-????-7???-[89ab]???-????????????'
                         AND replace(mcp_server_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                     ),
    name             TEXT NOT NULL UNIQUE,
    description      TEXT,
    enabled          INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    transport_type   TEXT NOT NULL,
    transport_config TEXT NOT NULL,
    tools            TEXT,
    last_test_status TEXT NOT NULL DEFAULT 'disconnected',
    last_connected   INTEGER,
    original_json    TEXT,
    builtin          INTEGER NOT NULL DEFAULT 0 CHECK (builtin IN (0, 1)),
    deleted_at       INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);

CREATE TABLE webhooks (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    webhook_id  TEXT NOT NULL UNIQUE
                CHECK (
                    length(webhook_id) = 36
                    AND lower(webhook_id) = webhook_id
                    AND webhook_id GLOB '????????-????-7???-[89ab]???-????????????'
                    AND replace(webhook_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                ),
    name        TEXT NOT NULL,
    platform    TEXT NOT NULL DEFAULT 'lark',
    url         TEXT NOT NULL,
    secret      TEXT,
    description TEXT NOT NULL DEFAULT '',
    enabled     INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE connector_credentials (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    credential_id     TEXT NOT NULL UNIQUE
                      CHECK (
                          length(credential_id) = 36
                          AND lower(credential_id) = credential_id
                          AND credential_id GLOB '????????-????-7???-[89ab]???-????????????'
                          AND replace(credential_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                      ),
    kind              TEXT NOT NULL,
    name              TEXT NOT NULL,
    payload_encrypted TEXT NOT NULL,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);

CREATE TABLE channel_plugins (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_plugin_id TEXT NOT NULL UNIQUE
                      CHECK (
                          length(channel_plugin_id) = 36
                          AND lower(channel_plugin_id) = channel_plugin_id
                          AND channel_plugin_id GLOB '????????-????-7???-[89ab]???-????????????'
                          AND replace(channel_plugin_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                      ),
    type              TEXT NOT NULL,
    name              TEXT NOT NULL,
    enabled           INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    config            TEXT NOT NULL,
    status            TEXT,
    last_connected    INTEGER,
    companion_id      TEXT,
    bot_key           TEXT,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    public_agent_id   TEXT
                      CHECK (
                          public_agent_id IS NULL
                          OR (
                              length(public_agent_id) = 36
                              AND lower(public_agent_id) = public_agent_id
                              AND public_agent_id GLOB '????????-????-7???-[89ab]???-????????????'
                              AND replace(public_agent_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                          )
                      ),
    CHECK (
        companion_id IS NULL
        OR (
            length(companion_id) = 36
            AND lower(companion_id) = companion_id
            AND companion_id GLOB '????????-????-7???-[89ab]???-????????????'
            AND replace(companion_id, '-', '') NOT GLOB '*[^0-9a-f]*'
        )
    ),
    CHECK (companion_id IS NULL OR public_agent_id IS NULL)
);

CREATE TABLE channel_users (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_user_id    TEXT NOT NULL UNIQUE
                       CHECK (
                           length(channel_user_id) = 36
                           AND lower(channel_user_id) = channel_user_id
                           AND channel_user_id GLOB '????????-????-7???-[89ab]???-????????????'
                           AND replace(channel_user_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                       ),
    platform_user_id   TEXT NOT NULL,
    platform_type      TEXT NOT NULL,
    channel_plugin_id  TEXT
                       CHECK (
                           channel_plugin_id IS NULL
                           OR (
                               length(channel_plugin_id) = 36
                               AND lower(channel_plugin_id) = channel_plugin_id
                               AND channel_plugin_id GLOB '????????-????-7???-[89ab]???-????????????'
                               AND replace(channel_plugin_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                           )
                       ),
    display_name       TEXT,
    authorized_at      INTEGER NOT NULL,
    last_active        INTEGER,
    UNIQUE (platform_user_id, platform_type, channel_plugin_id)
);

CREATE TABLE creation_tasks (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    creation_task_id TEXT NOT NULL UNIQUE
                     CHECK (
                         length(creation_task_id) = 36
                         AND lower(creation_task_id) = creation_task_id
                         AND creation_task_id GLOB '????????-????-7???-[89ab]???-????????????'
                         AND replace(creation_task_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                     ),
    canvas_id        TEXT
                     CHECK (
                         canvas_id IS NULL
                         OR (
                             length(canvas_id) = 36
                             AND lower(canvas_id) = canvas_id
                             AND canvas_id GLOB '????????-????-7???-[89ab]???-????????????'
                             AND replace(canvas_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                         )
                     ),
    node_id          TEXT
                     CHECK (
                         node_id IS NULL
                         OR (
                             length(node_id) = 36
                             AND lower(node_id) = node_id
                             AND node_id GLOB '????????-????-7???-[89ab]???-????????????'
                             AND replace(node_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                         )
                     ),
    provider_id      TEXT NOT NULL,
    model            TEXT NOT NULL,
    capability       TEXT NOT NULL,
    params           TEXT NOT NULL,
    status           TEXT NOT NULL,
    error            TEXT,
    result_asset_ids TEXT NOT NULL DEFAULT '[]'
                     CHECK (json_valid(result_asset_ids) AND json_type(result_asset_ids) = 'array'),
    remote_task_id   TEXT,
    attempt          INTEGER NOT NULL DEFAULT 0,
    submitted_at     INTEGER NOT NULL,
    started_at       INTEGER,
    finished_at      INTEGER,
    CHECK (length(provider_id) = 36 AND lower(provider_id) = provider_id AND provider_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(provider_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE idmm_interventions (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    intervention_id TEXT NOT NULL UNIQUE
                   CHECK (
                       length(intervention_id) = 36
                       AND lower(intervention_id) = intervention_id
                       AND intervention_id GLOB '????????-????-7???-[89ab]???-????????????'
                       AND replace(intervention_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                   ),
    user_id      TEXT NOT NULL,
    target_kind  TEXT NOT NULL CHECK (target_kind IN ('conversation', 'terminal')),
    target_id    TEXT NOT NULL
                 CHECK (
                     length(target_id) = 36
                     AND lower(target_id) = target_id
                     AND target_id GLOB '????????-????-7???-[89ab]???-????????????'
                     AND replace(target_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                 ),
    watch        TEXT NOT NULL,
    at           INTEGER NOT NULL,
    signal       TEXT NOT NULL,
    tier_used    TEXT NOT NULL,
    category     TEXT,
    action       TEXT NOT NULL,
    detail       TEXT,
    reason       TEXT,
    confidence   REAL,
    bypass_model TEXT,
    outcome      TEXT NOT NULL,
    CHECK (length(user_id) = 36 AND lower(user_id) = user_id AND user_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(user_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE requirements (
    id                     INTEGER PRIMARY KEY AUTOINCREMENT,
    requirement_id         TEXT NOT NULL UNIQUE
                           CHECK (
                               length(requirement_id) = 36
                               AND lower(requirement_id) = requirement_id
                               AND requirement_id GLOB '????????-????-7???-[89ab]???-????????????'
                               AND replace(requirement_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                           ),
    display_no             INTEGER NOT NULL UNIQUE CHECK (display_no > 0),
    title                  TEXT NOT NULL,
    content                TEXT NOT NULL DEFAULT '',
    tag                    TEXT NOT NULL,
    order_key              TEXT NOT NULL DEFAULT '',
    sort_seq               TEXT NOT NULL DEFAULT '',
    status                 TEXT NOT NULL DEFAULT 'pending',
    priority               INTEGER NOT NULL DEFAULT 0,
    completion_note        TEXT,
    owner_conversation_id  TEXT,
    owner_terminal_id      TEXT,
    active_turn_started_at INTEGER,
    lease_expires_at       INTEGER,
    started_at             INTEGER,
    completed_at           INTEGER,
    attempt_count          INTEGER NOT NULL DEFAULT 0,
    created_by             TEXT NOT NULL DEFAULT 'user',
    extra                  TEXT NOT NULL DEFAULT '{}',
    created_at             INTEGER NOT NULL,
    updated_at             INTEGER NOT NULL,
    CHECK (owner_conversation_id IS NULL OR owner_terminal_id IS NULL),
    CHECK (owner_conversation_id IS NULL OR (length(owner_conversation_id) = 36 AND lower(owner_conversation_id) = owner_conversation_id AND owner_conversation_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(owner_conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (owner_terminal_id IS NULL OR (length(owner_terminal_id) = 36 AND lower(owner_terminal_id) = owner_terminal_id AND owner_terminal_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(owner_terminal_id, '-', '') NOT GLOB '*[^0-9a-f]*'))
);

CREATE TABLE knowledge_bindings (
    id                     INTEGER PRIMARY KEY AUTOINCREMENT,
    knowledge_binding_id   TEXT NOT NULL UNIQUE
                           CHECK (
                               length(knowledge_binding_id) = 36
                               AND lower(knowledge_binding_id) = knowledge_binding_id
                               AND knowledge_binding_id GLOB '????????-????-7???-[89ab]???-????????????'
                               AND replace(knowledge_binding_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                           ),
    target_kind            TEXT NOT NULL,
    target_workpath        TEXT,
    target_conversation_id TEXT
                           CHECK (
                               target_conversation_id IS NULL
                               OR (
                                   length(target_conversation_id) = 36
                                   AND lower(target_conversation_id) = target_conversation_id
                                   AND target_conversation_id GLOB '????????-????-7???-[89ab]???-????????????'
                                   AND replace(target_conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                               )
                           ),
    target_terminal_id     TEXT
                           CHECK (
                               target_terminal_id IS NULL
                               OR (
                                   length(target_terminal_id) = 36
                                   AND lower(target_terminal_id) = target_terminal_id
                                   AND target_terminal_id GLOB '????????-????-7???-[89ab]???-????????????'
                                   AND replace(target_terminal_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                               )
                           ),
    target_companion_id    TEXT,
    enabled                INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    writeback              INTEGER NOT NULL DEFAULT 0 CHECK (writeback IN (0, 1)),
    writeback_mode         TEXT NOT NULL DEFAULT 'staged'
                           CHECK (writeback_mode IN ('staged', 'direct')),
    writeback_eagerness    TEXT NOT NULL DEFAULT 'conservative'
                           CHECK (writeback_eagerness IN ('conservative', 'aggressive')),
    updated_at             INTEGER NOT NULL,
    channel_write_enabled  INTEGER NOT NULL DEFAULT 0
                           CHECK (channel_write_enabled IN (0, 1)),
    CHECK (
        (target_kind = 'workpath' AND target_workpath IS NOT NULL
            AND target_conversation_id IS NULL AND target_terminal_id IS NULL AND target_companion_id IS NULL)
        OR (target_kind = 'conversation' AND target_conversation_id IS NOT NULL
            AND target_workpath IS NULL AND target_terminal_id IS NULL AND target_companion_id IS NULL)
        OR (target_kind = 'terminal' AND target_terminal_id IS NOT NULL
            AND target_workpath IS NULL AND target_conversation_id IS NULL AND target_companion_id IS NULL)
        OR (target_kind = 'companion' AND target_companion_id IS NOT NULL
            AND target_workpath IS NULL AND target_conversation_id IS NULL AND target_terminal_id IS NULL)
    ),
    CHECK (
        target_companion_id IS NULL
        OR (
            length(target_companion_id) = 36
            AND lower(target_companion_id) = target_companion_id
            AND target_companion_id GLOB '????????-????-7???-[89ab]???-????????????'
            AND replace(target_companion_id, '-', '') NOT GLOB '*[^0-9a-f]*'
        )
    )
);

CREATE TABLE preset_tags (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    preset_tag_id TEXT NOT NULL UNIQUE
                  CHECK (
                      length(preset_tag_id) = 36
                      AND lower(preset_tag_id) = preset_tag_id
                      AND preset_tag_id GLOB '????????-????-7???-[89ab]???-????????????'
                      AND replace(preset_tag_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                  ),
    key           TEXT NOT NULL UNIQUE CHECK (trim(key) <> ''),
    dimension     TEXT NOT NULL CHECK (dimension IN ('audience', 'scenario')),
    label         TEXT NOT NULL,
    sort_order    INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL
);

CREATE TABLE agent_execution_step_dependencies (
    id                     INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id           TEXT NOT NULL,
    blocker_step_id        TEXT NOT NULL
                           CHECK (
                               length(blocker_step_id) = 36
                               AND lower(blocker_step_id) = blocker_step_id
                               AND blocker_step_id GLOB '????????-????-7???-[89ab]???-????????????'
                               AND replace(blocker_step_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                           ),
    blocked_step_id        TEXT NOT NULL
                           CHECK (
                               length(blocked_step_id) = 36
                               AND lower(blocked_step_id) = blocked_step_id
                               AND blocked_step_id GLOB '????????-????-7???-[89ab]???-????????????'
                               AND replace(blocked_step_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                           ),
    introduced_in_revision INTEGER NOT NULL CHECK (introduced_in_revision >= 0),
    superseded_in_revision INTEGER,
    UNIQUE (execution_id, blocker_step_id, blocked_step_id, introduced_in_revision),
    CHECK (blocker_step_id <> blocked_step_id),
    CHECK (length(execution_id) = 36 AND lower(execution_id) = execution_id AND execution_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(execution_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE channel_pairing_codes (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    code              TEXT NOT NULL UNIQUE,
    platform_user_id  TEXT NOT NULL,
    platform_type     TEXT NOT NULL,
    channel_plugin_id TEXT
                      CHECK (
                          channel_plugin_id IS NULL
                          OR (
                              length(channel_plugin_id) = 36
                              AND lower(channel_plugin_id) = channel_plugin_id
                              AND channel_plugin_id GLOB '????????-????-7???-[89ab]???-????????????'
                              AND replace(channel_plugin_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                          )
                      ),
    display_name      TEXT,
    requested_at      INTEGER NOT NULL,
    expires_at        INTEGER NOT NULL,
    status            TEXT NOT NULL DEFAULT 'pending'
                      CHECK (status IN ('pending', 'approved', 'rejected', 'expired'))
);

CREATE TABLE client_preferences (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    key        TEXT NOT NULL UNIQUE,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE conversation_creation_keys (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    creation_key    TEXT NOT NULL UNIQUE CHECK (trim(creation_key) <> ''),
    user_id         TEXT NOT NULL,
    conversation_id TEXT NOT NULL UNIQUE,
    created_at      INTEGER NOT NULL,
    CHECK (length(conversation_id) = 36 AND lower(conversation_id) = conversation_id AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
    CHECK (length(user_id) = 36 AND lower(user_id) = user_id AND user_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(user_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE conversation_delivery_receipts (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    operation_id    TEXT NOT NULL UNIQUE CHECK (trim(operation_id) <> ''),
    message_id      TEXT NOT NULL UNIQUE,
    conversation_id TEXT NOT NULL,
    user_id         TEXT NOT NULL,
    kind            TEXT NOT NULL CHECK (kind IN ('turn', 'steer', 'projection')),
    request_payload TEXT NOT NULL CHECK (json_valid(request_payload)),
    status          TEXT NOT NULL CHECK (status IN ('accepted', 'completed')),
    result_ok       INTEGER CHECK (result_ok IS NULL OR result_ok IN (0, 1)),
    result_text     TEXT,
    result_error    TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    completed_at    INTEGER,
    CHECK (length(conversation_id) = 36 AND lower(conversation_id) = conversation_id AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
    CHECK (length(message_id) = 36 AND lower(message_id) = message_id AND message_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(message_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
    CHECK (length(user_id) = 36 AND lower(user_id) = user_id AND user_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(user_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE conversation_mcp_servers (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL,
    mcp_server_id   TEXT NOT NULL
                    CHECK (
                        length(mcp_server_id) = 36
                        AND lower(mcp_server_id) = mcp_server_id
                        AND mcp_server_id GLOB '????????-????-7???-[89ab]???-????????????'
                        AND replace(mcp_server_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                    ),
    sort_order      INTEGER NOT NULL DEFAULT 0,
    UNIQUE (conversation_id, mcp_server_id),
    CHECK (length(conversation_id) = 36 AND lower(conversation_id) = conversation_id AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE knowledge_binding_bases (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    knowledge_binding_id TEXT NOT NULL
                         CHECK (
                             length(knowledge_binding_id) = 36
                             AND lower(knowledge_binding_id) = knowledge_binding_id
                             AND knowledge_binding_id GLOB '????????-????-7???-[89ab]???-????????????'
                             AND replace(knowledge_binding_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                         ),
    knowledge_base_id    TEXT NOT NULL
                         CHECK (
                             length(knowledge_base_id) = 36
                             AND lower(knowledge_base_id) = knowledge_base_id
                             AND knowledge_base_id GLOB '????????-????-7???-[89ab]???-????????????'
                             AND replace(knowledge_base_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                         ),
    position             INTEGER NOT NULL DEFAULT 0,
    UNIQUE (knowledge_binding_id, knowledge_base_id)
);

CREATE TABLE knowledge_tags (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    key        TEXT NOT NULL UNIQUE,
    label      TEXT NOT NULL,
    color      TEXT,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE TABLE message_correlations (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id  TEXT NOT NULL,
    turn_message_id  TEXT NOT NULL,
    message_type     TEXT NOT NULL CHECK (length(trim(message_type)) > 0),
    correlation_key  TEXT NOT NULL CHECK (length(trim(correlation_key)) > 0),
    message_id       TEXT NOT NULL UNIQUE,
    UNIQUE (conversation_id, turn_message_id, message_type, correlation_key),
    CHECK (length(conversation_id) = 36 AND lower(conversation_id) = conversation_id AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
    CHECK (length(message_id) = 36 AND lower(message_id) = message_id AND message_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(message_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
    CHECK (length(turn_message_id) = 36 AND lower(turn_message_id) = turn_message_id AND turn_message_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(turn_message_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE model_profiles (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    provider_id TEXT NOT NULL,
    model       TEXT NOT NULL,
    tasks       TEXT NOT NULL DEFAULT '[]',
    traits      TEXT NOT NULL DEFAULT '[]',
    params      TEXT NOT NULL DEFAULT '{}',
    source      TEXT NOT NULL DEFAULT 'inferred',
    updated_at  INTEGER NOT NULL,
    UNIQUE (provider_id, model),
    CHECK (length(provider_id) = 36 AND lower(provider_id) = provider_id AND provider_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(provider_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE oauth_tokens (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    server_url    TEXT NOT NULL UNIQUE,
    access_token  TEXT NOT NULL,
    refresh_token TEXT,
    token_type    TEXT NOT NULL DEFAULT 'bearer',
    expires_at    INTEGER,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

CREATE TABLE preset_agent_preferences (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    preset_id TEXT NOT NULL,
    agent_id  TEXT NOT NULL,
    rank      INTEGER NOT NULL DEFAULT 0,
    required  INTEGER NOT NULL DEFAULT 0 CHECK (required IN (0, 1)),
    UNIQUE (preset_id, agent_id),
    CHECK (length(agent_id) = 36 AND lower(agent_id) = agent_id AND agent_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(agent_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
    CHECK (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE preset_examples (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    preset_id  TEXT NOT NULL,
    locale     TEXT NOT NULL DEFAULT '',
    sort_order INTEGER NOT NULL DEFAULT 0,
    prompt     TEXT NOT NULL,
    UNIQUE (preset_id, locale, sort_order),
    CHECK (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE preset_knowledge_bases (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    preset_id         TEXT NOT NULL,
    knowledge_base_id TEXT NOT NULL,
    sort_order        INTEGER NOT NULL DEFAULT 0,
    required          INTEGER NOT NULL DEFAULT 0 CHECK (required IN (0, 1)),
    UNIQUE (preset_id, knowledge_base_id),
    CHECK (length(knowledge_base_id) = 36 AND lower(knowledge_base_id) = knowledge_base_id AND knowledge_base_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(knowledge_base_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
    CHECK (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE preset_localizations (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    preset_id           TEXT NOT NULL,
    locale              TEXT NOT NULL,
    name                TEXT,
    description         TEXT,
    routing_description TEXT,
    instructions        TEXT,
    UNIQUE (preset_id, locale),
    CHECK (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE preset_model_preferences (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    preset_id   TEXT NOT NULL,
    provider_id TEXT,
    model       TEXT NOT NULL,
    rank        INTEGER NOT NULL DEFAULT 0,
    required    INTEGER NOT NULL DEFAULT 0 CHECK (required IN (0, 1)),
    UNIQUE (preset_id, rank),
    CHECK (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
    CHECK (provider_id IS NULL OR (length(provider_id) = 36 AND lower(provider_id) = provider_id AND provider_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(provider_id, '-', '') NOT GLOB '*[^0-9a-f]*'))
);

CREATE TABLE preset_skill_bindings (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    preset_id  TEXT NOT NULL,
    skill_name TEXT NOT NULL,
    binding    TEXT NOT NULL CHECK (binding IN ('include', 'exclude_auto')),
    required   INTEGER NOT NULL DEFAULT 0 CHECK (required IN (0, 1)),
    sort_order INTEGER NOT NULL DEFAULT 0,
    UNIQUE (preset_id, skill_name, binding),
    CHECK (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE preset_tag_bindings (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    preset_id    TEXT NOT NULL,
    preset_tag_id TEXT NOT NULL
                 CHECK (
                     length(preset_tag_id) = 36
                     AND lower(preset_tag_id) = preset_tag_id
                     AND preset_tag_id GLOB '????????-????-7???-[89ab]???-????????????'
                     AND replace(preset_tag_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                 ),
    dimension    TEXT NOT NULL CHECK (dimension IN ('audience', 'scenario')),
    UNIQUE (preset_id, preset_tag_id, dimension),
    CHECK (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE preset_targets (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    preset_id   TEXT NOT NULL,
    target_kind TEXT NOT NULL CHECK (target_kind IN (
                    'conversation', 'execution_step', 'companion',
                    'public_companion', 'cron'
                )),
    UNIQUE (preset_id, target_kind),
    CHECK (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE requirement_tags (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    tag                   TEXT NOT NULL UNIQUE,
    paused                INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0, 1)),
    paused_reason         TEXT,
    paused_requirement_id TEXT,
    paused_at             INTEGER,
    CHECK (paused_requirement_id IS NULL OR (length(paused_requirement_id) = 36 AND lower(paused_requirement_id) = paused_requirement_id AND paused_requirement_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(paused_requirement_id, '-', '') NOT GLOB '*[^0-9a-f]*'))
);

CREATE TABLE skill_tags (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    skill_name    TEXT NOT NULL UNIQUE,
    audience_tags TEXT,
    scenario_tags TEXT,
    updated_at    INTEGER NOT NULL
);

CREATE TABLE tag_settings (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    tag           TEXT NOT NULL UNIQUE,
    webhook_id    TEXT
                  CHECK (
                      webhook_id IS NULL
                      OR (
                          length(webhook_id) = 36
                          AND lower(webhook_id) = webhook_id
                          AND webhook_id GLOB '????????-????-7???-[89ab]???-????????????'
                          AND replace(webhook_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                      )
                  ),
    description   TEXT NOT NULL DEFAULT '',
    updated_at    INTEGER NOT NULL,
    notify_events TEXT NOT NULL DEFAULT 'done,failed,needs_review'
);

CREATE TABLE acp_session (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL UNIQUE,
    agent_backend  TEXT NOT NULL,
    agent_source   TEXT NOT NULL,
    agent_id       TEXT,
    acp_session_id TEXT,
    session_status TEXT NOT NULL DEFAULT 'idle',
    session_config TEXT NOT NULL DEFAULT '{}',
    last_active_at INTEGER,
    suspended_at   INTEGER,
    CHECK (agent_id IS NULL OR (length(agent_id) = 36 AND lower(agent_id) = agent_id AND agent_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(agent_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (length(conversation_id) = 36 AND lower(conversation_id) = conversation_id AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE companion_access_token (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    companion_id TEXT NOT NULL UNIQUE
                 CHECK (
                     length(companion_id) = 36
                     AND lower(companion_id) = companion_id
                     AND companion_id GLOB '????????-????-7???-[89ab]???-????????????'
                     AND replace(companion_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                 ),
    token_hash   TEXT NOT NULL,
    created_at   INTEGER NOT NULL
);

CREATE TABLE installation_identity (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    singleton_key TEXT NOT NULL UNIQUE CHECK (singleton_key = 'installation'),
    owner_user_id TEXT NOT NULL UNIQUE,
    CHECK (length(owner_user_id) = 36 AND lower(owner_user_id) = owner_user_id AND owner_user_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(owner_user_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE preset_knowledge_policy (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    preset_id TEXT NOT NULL UNIQUE,
    enabled   INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    mode      TEXT NOT NULL DEFAULT 'inherit',
    writeback INTEGER NOT NULL DEFAULT 0 CHECK (writeback IN (0, 1)),
    eagerness TEXT CHECK (eagerness IS NULL OR eagerness IN ('conservative', 'aggressive')),
    grounded  INTEGER NOT NULL DEFAULT 0 CHECK (grounded IN (0, 1)),
    CHECK (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE preset_user_state (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    preset_id          TEXT NOT NULL UNIQUE,
    enabled            INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    auto_selectable    INTEGER NOT NULL DEFAULT 0 CHECK (auto_selectable IN (0, 1)),
    preferred_agent_id TEXT,
    sort_order         INTEGER NOT NULL DEFAULT 0,
    last_used_at       INTEGER,
    updated_at         INTEGER NOT NULL,
    CHECK (preferred_agent_id IS NULL OR (length(preferred_agent_id) = 36 AND lower(preferred_agent_id) = preferred_agent_id AND preferred_agent_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preferred_agent_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (length(preset_id) = 36 AND lower(preset_id) = preset_id AND preset_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(preset_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

CREATE TABLE requirement_display_sequence (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    singleton_key TEXT NOT NULL UNIQUE CHECK (singleton_key = 'requirements'),
    last_no       INTEGER NOT NULL DEFAULT 0 CHECK (last_no >= 0)
);

CREATE TABLE system_settings (
    id                        INTEGER PRIMARY KEY AUTOINCREMENT,
    singleton_key             TEXT NOT NULL UNIQUE CHECK (singleton_key = 'system'),
    language                  TEXT NOT NULL DEFAULT 'en-US',
    notification_enabled      INTEGER NOT NULL DEFAULT 1 CHECK (notification_enabled IN (0, 1)),
    cron_notification_enabled INTEGER NOT NULL DEFAULT 0 CHECK (cron_notification_enabled IN (0, 1)),
    command_queue_enabled     INTEGER NOT NULL DEFAULT 0 CHECK (command_queue_enabled IN (0, 1)),
    save_upload_to_workspace  INTEGER NOT NULL DEFAULT 0 CHECK (save_upload_to_workspace IN (0, 1)),
    updated_at                INTEGER NOT NULL
);

CREATE TABLE terminal_scrollback (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    terminal_id TEXT NOT NULL UNIQUE,
    data        BLOB NOT NULL,
    updated_at  INTEGER NOT NULL,
    CHECK (length(terminal_id) = 36 AND lower(terminal_id) = terminal_id AND terminal_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(terminal_id, '-', '') NOT GLOB '*[^0-9a-f]*')
);

-- Logical-link indexes. Their names are part of the runtime registry contract.
CREATE INDEX idx_conversations_user_id ON conversations(user_id);
CREATE INDEX idx_conversations_cron_job_id ON conversations(cron_job_id);
CREATE INDEX idx_conversations_preset_id ON conversations(preset_id);
CREATE INDEX idx_conversations_execution_template_id ON conversations(execution_template_id);
CREATE INDEX idx_messages_conversation_id ON messages(conversation_id);
CREATE INDEX idx_messages_msg_id ON messages(msg_id, conversation_id);
CREATE INDEX idx_terminal_sessions_user_id ON terminal_sessions(user_id);
CREATE INDEX idx_execution_templates_user_id ON agent_execution_templates(user_id);
CREATE INDEX idx_execution_templates_primary_participant_id ON agent_execution_templates(primary_participant_id);
CREATE INDEX idx_agent_executions_user_id ON agent_executions(user_id);
CREATE INDEX idx_attachments_requirement_id ON attachments(requirement_id);
CREATE INDEX idx_channel_sessions_channel_user_id ON channel_sessions(channel_user_id);
CREATE INDEX idx_channel_sessions_conversation_id ON channel_sessions(conversation_id);
CREATE INDEX idx_channel_sessions_channel_plugin_id ON channel_sessions(channel_plugin_id);
CREATE INDEX idx_execution_participants_execution_id ON agent_execution_participants(execution_id);
CREATE INDEX idx_execution_participants_source_agent_id ON agent_execution_participants(source_agent_id);
CREATE INDEX idx_execution_participants_preset_id ON agent_execution_participants(preset_id);
CREATE INDEX idx_execution_participants_provider_id ON agent_execution_participants(provider_id);
CREATE INDEX idx_execution_steps_execution_id ON agent_execution_steps(execution_id);
CREATE INDEX idx_execution_steps_assigned_participant_id ON agent_execution_steps(assigned_participant_id);
CREATE INDEX idx_execution_attempts_execution_id ON agent_execution_attempts(execution_id);
CREATE INDEX idx_execution_attempts_step_id ON agent_execution_attempts(step_id);
CREATE INDEX idx_execution_attempts_participant_id ON agent_execution_attempts(participant_id);
CREATE INDEX idx_execution_events_execution_id ON agent_execution_events(execution_id);
CREATE INDEX idx_execution_events_step_id ON agent_execution_events(step_id);
CREATE INDEX idx_execution_events_attempt_id ON agent_execution_events(attempt_id);
CREATE INDEX idx_execution_events_actor_user_id
    ON agent_execution_events(actor_id)
    WHERE actor_type = 'user' AND actor_id IS NOT NULL;
CREATE INDEX idx_execution_events_actor_local_agent_id
    ON agent_execution_events(actor_id)
    WHERE actor_type = 'agent' AND actor_conversation_id IS NOT NULL AND actor_id IS NOT NULL;
CREATE INDEX idx_execution_events_actor_external_agent_id
    ON agent_execution_events(actor_id)
    WHERE actor_type = 'agent' AND actor_conversation_id IS NULL AND actor_id IS NOT NULL;
CREATE INDEX idx_execution_events_actor_conversation_id ON agent_execution_events(actor_conversation_id);
CREATE INDEX idx_execution_events_actor_attempt_id ON agent_execution_events(actor_attempt_id);
CREATE INDEX idx_execution_events_on_behalf_of_user_id ON agent_execution_events(on_behalf_of_user_id);
CREATE INDEX idx_template_participants_template_id ON agent_execution_template_participants(template_id);
CREATE INDEX idx_template_participants_source_agent_id ON agent_execution_template_participants(source_agent_id);
CREATE INDEX idx_template_participants_preset_id ON agent_execution_template_participants(preset_id);
CREATE INDEX idx_template_participants_provider_id ON agent_execution_template_participants(provider_id);
CREATE INDEX idx_conversation_artifacts_conversation_id ON conversation_artifacts(conversation_id);
CREATE INDEX idx_conversation_artifacts_cron_job_id ON conversation_artifacts(cron_job_id);
CREATE UNIQUE INDEX uq_conversation_artifacts_skill_suggest
    ON conversation_artifacts(conversation_id, cron_job_id)
    WHERE kind = 'skill_suggest';
CREATE INDEX idx_conversation_execution_links_conversation_id ON conversation_execution_links(conversation_id);
CREATE INDEX idx_conversation_execution_links_execution_id ON conversation_execution_links(execution_id);
CREATE INDEX idx_conversation_execution_links_step_id ON conversation_execution_links(step_id);
CREATE INDEX idx_conversation_execution_links_attempt_id ON conversation_execution_links(attempt_id);
CREATE INDEX idx_cron_jobs_user_id ON cron_jobs(user_id);
CREATE INDEX idx_cron_jobs_preset_id ON cron_jobs(preset_id);
CREATE INDEX idx_cron_jobs_conversation_id ON cron_jobs(conversation_id);
CREATE INDEX idx_cron_job_runs_cron_job_id ON cron_job_runs(cron_job_id);
CREATE INDEX idx_channel_plugins_companion_id ON channel_plugins(companion_id);
CREATE INDEX idx_channel_plugins_public_agent_id ON channel_plugins(public_agent_id);
CREATE INDEX idx_channel_users_channel_plugin_id ON channel_users(channel_plugin_id);
CREATE INDEX idx_creation_tasks_canvas_id ON creation_tasks(canvas_id);
CREATE INDEX idx_creation_tasks_provider_id ON creation_tasks(provider_id);
CREATE INDEX idx_idmm_interventions_user_id ON idmm_interventions(user_id);
CREATE INDEX idx_idmm_interventions_target_id ON idmm_interventions(target_id);
CREATE INDEX idx_idmm_interventions_target ON idmm_interventions(target_kind, target_id);
CREATE INDEX idx_idmm_interventions_conversation_target_id
    ON idmm_interventions(target_id)
    WHERE target_kind = 'conversation';
CREATE INDEX idx_idmm_interventions_terminal_target_id
    ON idmm_interventions(target_id)
    WHERE target_kind = 'terminal';
CREATE INDEX idx_requirements_owner_conversation_id ON requirements(owner_conversation_id);
CREATE INDEX idx_requirements_owner_terminal_id ON requirements(owner_terminal_id);
CREATE UNIQUE INDEX uq_knowledge_bindings_target_workpath
    ON knowledge_bindings(target_workpath)
    WHERE target_kind = 'workpath' AND target_workpath IS NOT NULL;
CREATE UNIQUE INDEX uq_knowledge_bindings_target_conversation_id
    ON knowledge_bindings(target_conversation_id)
    WHERE target_kind = 'conversation' AND target_conversation_id IS NOT NULL;
CREATE UNIQUE INDEX uq_knowledge_bindings_target_terminal_id
    ON knowledge_bindings(target_terminal_id)
    WHERE target_kind = 'terminal' AND target_terminal_id IS NOT NULL;
CREATE UNIQUE INDEX uq_knowledge_bindings_target_companion_id
    ON knowledge_bindings(target_companion_id)
    WHERE target_kind = 'companion' AND target_companion_id IS NOT NULL;
CREATE INDEX idx_execution_dependencies_execution_id ON agent_execution_step_dependencies(execution_id);
CREATE INDEX idx_execution_dependencies_blocker_step_id ON agent_execution_step_dependencies(blocker_step_id);
CREATE INDEX idx_execution_dependencies_blocked_step_id ON agent_execution_step_dependencies(blocked_step_id);
CREATE INDEX idx_channel_pairing_codes_channel_plugin_id ON channel_pairing_codes(channel_plugin_id);
CREATE INDEX idx_conversation_creation_keys_user_id ON conversation_creation_keys(user_id);
CREATE INDEX idx_conversation_creation_keys_conversation_id ON conversation_creation_keys(conversation_id);
CREATE INDEX idx_delivery_receipts_message_id ON conversation_delivery_receipts(message_id);
CREATE INDEX idx_delivery_receipts_conversation_id ON conversation_delivery_receipts(conversation_id);
CREATE INDEX idx_delivery_receipts_user_id ON conversation_delivery_receipts(user_id);
CREATE INDEX idx_conversation_mcp_servers_conversation_id ON conversation_mcp_servers(conversation_id);
CREATE INDEX idx_conversation_mcp_servers_mcp_server_id ON conversation_mcp_servers(mcp_server_id);
CREATE INDEX idx_knowledge_binding_bases_knowledge_binding_id
    ON knowledge_binding_bases(knowledge_binding_id);
CREATE INDEX idx_knowledge_binding_bases_knowledge_base_id ON knowledge_binding_bases(knowledge_base_id);
CREATE INDEX idx_message_correlations_conversation_id ON message_correlations(conversation_id);
CREATE INDEX idx_message_correlations_turn_message_id ON message_correlations(turn_message_id);
CREATE INDEX idx_message_correlations_message_id ON message_correlations(message_id);
CREATE INDEX idx_model_profiles_provider_id ON model_profiles(provider_id);
CREATE INDEX idx_preset_agent_preferences_preset_id ON preset_agent_preferences(preset_id);
CREATE INDEX idx_preset_agent_preferences_agent_id ON preset_agent_preferences(agent_id);
CREATE INDEX idx_preset_examples_preset_id ON preset_examples(preset_id);
CREATE INDEX idx_preset_knowledge_bases_preset_id ON preset_knowledge_bases(preset_id);
CREATE INDEX idx_preset_knowledge_bases_knowledge_base_id ON preset_knowledge_bases(knowledge_base_id);
CREATE INDEX idx_preset_localizations_preset_id ON preset_localizations(preset_id);
CREATE INDEX idx_preset_model_preferences_preset_id ON preset_model_preferences(preset_id);
CREATE INDEX idx_preset_model_preferences_provider_id ON preset_model_preferences(provider_id);
CREATE INDEX idx_preset_skill_bindings_preset_id ON preset_skill_bindings(preset_id);
CREATE INDEX idx_preset_tag_bindings_preset_id ON preset_tag_bindings(preset_id);
CREATE INDEX idx_preset_tag_bindings_preset_tag_id ON preset_tag_bindings(preset_tag_id);
CREATE INDEX idx_preset_targets_preset_id ON preset_targets(preset_id);
CREATE UNIQUE INDEX uq_presets_catalog_source_key
    ON presets(source_kind, source_key)
    WHERE source_kind IN ('builtin', 'extension');
CREATE INDEX idx_requirement_tags_paused_requirement_id ON requirement_tags(paused_requirement_id);
CREATE INDEX idx_tag_settings_webhook_id ON tag_settings(webhook_id);
CREATE INDEX idx_acp_session_conversation_id ON acp_session(conversation_id);
CREATE INDEX idx_acp_session_agent_id ON acp_session(agent_id);
CREATE INDEX idx_companion_access_token_companion_id ON companion_access_token(companion_id);
CREATE INDEX idx_installation_identity_owner_user_id ON installation_identity(owner_user_id);
CREATE INDEX idx_preset_knowledge_policy_preset_id ON preset_knowledge_policy(preset_id);
CREATE INDEX idx_preset_user_state_preset_id ON preset_user_state(preset_id);
CREATE INDEX idx_preset_user_state_preferred_agent_id ON preset_user_state(preferred_agent_id);
CREATE INDEX idx_terminal_scrollback_terminal_id ON terminal_scrollback(terminal_id);

-- JSON logical-link access paths. Scalar references use expression indexes;
-- array references use the owning document/key index because SQLite cannot
-- index every json_each() element without physical triggers.
CREATE INDEX idx_conversations_model_provider_id
    ON conversations(json_extract(model, '$.provider_id'))
    WHERE model IS NOT NULL AND json_valid(model);
CREATE INDEX idx_conversations_execution_model_pool_json
    ON conversations(execution_model_pool)
    WHERE execution_model_pool IS NOT NULL;
CREATE INDEX idx_conversations_extra_idmm_fault_provider_id
    ON conversations(json_extract(extra, '$.idmm.fault_watch.bypass_model.provider_id'));
CREATE INDEX idx_conversations_extra_idmm_decision_provider_id
    ON conversations(json_extract(extra, '$.idmm.decision_watch.bypass_model.provider_id'));
CREATE INDEX idx_conversations_extra_remote_agent_id
    ON conversations(json_extract(extra, '$.remote_agent_id'));
CREATE INDEX idx_conversations_extra_agent_id
    ON conversations(json_extract(extra, '$.agent_id'));
CREATE INDEX idx_conversations_extra_custom_agent_id
    ON conversations(json_extract(extra, '$.custom_agent_id'));
CREATE INDEX idx_conversations_extra_companion_id
    ON conversations(json_extract(extra, '$.companion_id'));
CREATE INDEX idx_conversations_extra_public_agent_id
    ON conversations(json_extract(extra, '$.public_agent_id'));
CREATE INDEX idx_terminal_sessions_idmm_fault_provider_id
    ON terminal_sessions(json_extract(idmm, '$.fault_watch.bypass_model.provider_id'))
    WHERE idmm IS NOT NULL;
CREATE INDEX idx_terminal_sessions_idmm_decision_provider_id
    ON terminal_sessions(json_extract(idmm, '$.decision_watch.bypass_model.provider_id'))
    WHERE idmm IS NOT NULL;
CREATE INDEX idx_cron_jobs_nomi_provider_id
    ON cron_jobs(
        CASE
            WHEN json_valid(agent_config) THEN json_extract(agent_config, '$.provider_id')
            ELSE NULL
        END
    )
    WHERE agent_type = 'nomi' AND agent_config IS NOT NULL;
CREATE INDEX idx_workshop_assets_origin_provider_id
    ON workshop_assets(json_extract(origin, '$.provider_id'))
    WHERE origin IS NOT NULL;
CREATE INDEX idx_workshop_assets_origin_canvas_id
    ON workshop_assets(json_extract(origin, '$.canvas_id'))
    WHERE origin IS NOT NULL;
CREATE INDEX idx_workshop_assets_origin_creation_task_id
    ON workshop_assets(json_extract(origin, '$.creation_task_id'))
    WHERE origin IS NOT NULL;
CREATE INDEX idx_knowledge_bases_extra_credential_ref
    ON knowledge_bases(json_extract(extra, '$.source.credentialRef'))
    WHERE extra IS NOT NULL AND json_valid(extra);
CREATE INDEX idx_workshop_assets_origin_node_id
    ON workshop_assets(json_extract(origin, '$.node_id'))
    WHERE origin IS NOT NULL;
CREATE INDEX idx_creation_tasks_result_asset_ids_json
    ON creation_tasks(result_asset_ids);
CREATE INDEX idx_client_preferences_provider_key
    ON client_preferences(key);

-- Operational indexes retained independently from logical-link enforcement.
CREATE INDEX idx_agent_metadata_agent_type ON agent_metadata(agent_type);
CREATE INDEX idx_agent_metadata_backend ON agent_metadata(backend);
CREATE INDEX idx_agent_metadata_sort_order ON agent_metadata(sort_order);
CREATE INDEX idx_agent_executions_owner_updated ON agent_executions(user_id, updated_at DESC);
CREATE INDEX idx_agent_executions_status_lease ON agent_executions(status, lease_expires_at);
CREATE INDEX idx_execution_events_unpublished ON agent_execution_events(published_at, id);
CREATE INDEX idx_conversations_updated_at ON conversations(updated_at);
CREATE INDEX idx_conversations_source_chat ON conversations(source, channel_chat_id, updated_at DESC);
CREATE INDEX idx_messages_conv_created ON messages(conversation_id, created_at, message_id);
CREATE INDEX idx_creation_tasks_status ON creation_tasks(status);
CREATE INDEX idx_cron_jobs_next_run ON cron_jobs(enabled, next_run_at);
CREATE INDEX idx_idmm_interventions_at ON idmm_interventions(at);
CREATE INDEX idx_mcp_servers_deleted_at ON mcp_servers(deleted_at);
CREATE INDEX idx_mcp_servers_enabled ON mcp_servers(enabled);
CREATE INDEX idx_providers_platform ON providers(platform);
CREATE INDEX idx_providers_sort_order ON providers(sort_order, created_at);
CREATE INDEX idx_remote_agents_status ON remote_agents(status);
CREATE INDEX idx_requirements_status ON requirements(status);
CREATE INDEX idx_requirements_tag_order ON requirements(tag, sort_seq);
CREATE INDEX idx_workshop_assets_kind ON workshop_assets(kind);
CREATE INDEX idx_workshop_assets_library ON workshop_assets(in_library);
CREATE INDEX idx_workshop_canvases_updated ON workshop_canvases(updated_at);
CREATE UNIQUE INDEX uq_channel_plugins_type_bot_key
    ON channel_plugins(type, bot_key) WHERE bot_key IS NOT NULL;

INSERT INTO requirement_display_sequence (singleton_key, last_no)
VALUES ('requirements', 0);

-- Stable builtin agent catalog. agent_id is always a bare UUIDv7 business ID;
-- the installation/catalog natural key is stored separately in source_key.
INSERT OR IGNORE INTO agent_metadata
    (agent_id, icon, name, backend, agent_type, agent_source, agent_source_info, source_key,
     enabled, command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     agent_capabilities, auth_methods,
     sort_order, created_at, updated_at)
VALUES
    -- ACP builtin agents
    ('0190f5fe-7c00-7a00-8000-000000000101', '/api/assets/logos/ai-major/claude.svg', 'Claude Code',
     'claude', 'acp', 'builtin', '{"binary_name":"claude","bridge_binary":"bun"}', 'agent_builtin_claude',
     1, 'bun', '["x","--bun","@agentclientprotocol/claude-agent-acp@0.33.1"]', '[]',
     '[".claude/skills"]',
     '{"supports_side_question":true,"self_identity_sticky":true,"session_load_via_meta_field":true}',
     'bypassPermissions',
     NULL, NULL,
     3100,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-000000000102', '/api/assets/logos/tools/coding/codex.svg', 'Codex CLI',
     'codex', 'acp', 'builtin', '{"binary_name":"codex","bridge_binary":"bun"}', 'agent_builtin_codex',
     1, 'bun', '["x","--bun","@zed-industries/codex-acp@0.14.0"]', '[]',
     '[".codex/skills"]',
     '{"supports_side_question":false}',
     'full-access',
     NULL, NULL,
     3110,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-000000000103', '/api/assets/logos/ai-major/gemini.svg', 'Gemini CLI',
     'gemini', 'acp', 'builtin', '{"binary_name":"gemini"}', 'agent_builtin_gemini',
     1, 'gemini', '["--experimental-acp"]', '[]',
     '[".gemini/skills"]',
     '{"supports_side_question":false}',
     'yolo',
     NULL, NULL,
     3120,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-000000000104', '/api/assets/logos/ai-china/qwen.svg', 'Qwen',
     'qwen', 'acp', 'builtin', '{"binary_name":"qwen"}', 'agent_builtin_qwen',
     1, 'qwen', '["--acp"]', '[]',
     '[".qwen/skills"]',
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"prompt_capabilities":{"image":true,"audio":true,"embedded_context":true},"session_capabilities":{"list":{},"resume":{}},"mcp_capabilities":{"sse":true,"http":true}}',
     '[{"id":"openai","name":"Use OpenAI API key","description":"Requires setting the `OPENAI_API_KEY` environment variable","_meta":{"type":"terminal","args":["--auth-type=openai"]}},{"id":"qwen-oauth","name":"Qwen OAuth","description":"Qwen OAuth (free tier discontinued 2026-04-15)","_meta":{"type":"terminal","args":["--auth-type=qwen-oauth"]}}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-000000000105', '/api/assets/logos/tools/coding/codebuddy.svg', 'CodeBuddy',
     'codebuddy', 'acp', 'builtin', '{"binary_name":"codebuddy","bridge_binary":"bun"}', 'agent_builtin_codebuddy',
     1, 'bun', '["x","--bun","@tencent-ai/codebuddy-code@2.97.0","--acp"]', '[]',
     '[".codebuddy/skills"]',
     '{"supports_side_question":false}',
     'bypassPermissions',
     '{"prompt_capabilities":{"image":true,"embedded_context":true},"mcp_capabilities":{"http":true,"sse":true},"load_session":true,"delegate_tools_support":true}',
     '[{"id":"iOA","name":"Login with iOA","description":null},{"id":"external","name":"Login with Google/Github","description":null},{"id":"internal","name":"Login with WeChat","description":null},{"id":"selfhosted","name":"Login with Enterprise Domain","description":null}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-000000000106', '/api/assets/logos/brand/droid.svg', 'Droid',
     'droid', 'acp', 'builtin', '{"binary_name":"droid"}', 'agent_builtin_droid',
     1, 'droid', '["exec","--output-format","acp"]', '[]',
     '[".factory/skills"]',
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"session_capabilities":{"list":{},"resume":{}},"prompt_capabilities":{"image":true,"embedded_context":true},"_meta":{"terminal_output":true,"terminal-auth":true}}',
     '[{"id":"device-pairing","name":"Login","description":"Authenticate with Factory using a device pairing code in your browser."},{"id":"factory-api-key","name":"Factory API Key","description":"Authenticate using a Factory API key set in the FACTORY_API_KEY environment variable."}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-000000000107', '/api/assets/logos/tools/goose.svg', 'Goose',
     'goose', 'acp', 'builtin', '{"binary_name":"goose"}', 'agent_builtin_goose',
     1, 'goose', '["acp"]', '[]',
     '[".goose/skills"]',
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"prompt_capabilities":{"image":true,"audio":false,"embedded_context":true},"mcp_capabilities":{"http":true,"sse":false},"session_capabilities":{"list":{},"close":{}},"auth":{}}',
     '[{"id":"goose-provider","name":"Configure Provider","description":"Run `goose configure` to set up your AI provider and API key"}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-000000000108', '/api/assets/logos/brand/auggie.svg', 'Auggie',
     'auggie', 'acp', 'builtin', '{"binary_name":"auggie"}', 'agent_builtin_auggie',
     1, 'auggie', '["--acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"prompt_capabilities":{"image":true},"session_capabilities":{"list":{}}}',
     '[]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-000000000109', '/api/assets/logos/ai-china/kimi.svg', 'Kimi',
     'kimi', 'acp', 'builtin', '{"binary_name":"kimi"}', 'agent_builtin_kimi',
     1, 'kimi', '["acp"]', '[]',
     '[".kimi/skills"]',
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"mcp_capabilities":{"http":true,"sse":false},"prompt_capabilities":{"audio":false,"embedded_context":true,"image":true},"session_capabilities":{"list":{},"resume":{}}}',
     '[{"_meta":{"terminal-auth":{"command":"kimi","args":["login"],"label":"Kimi Code Login","env":{},"type":"terminal"}},"description":"Run `kimi login` command in the terminal, then follow the instructions to finish login.","id":"login","name":"Login with Kimi account"}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-00000000010a', '/api/assets/logos/tools/coding/opencode-light.svg', 'OpenCode',
     'opencode', 'acp', 'builtin', '{"binary_name":"opencode"}', 'agent_builtin_opencode',
     1, 'opencode', '["acp"]', '[]',
     '[".opencode/skills"]',
     '{"supports_side_question":false}',
     'build',
     NULL, NULL,
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-00000000010b', '/api/assets/logos/tools/github.svg', 'Copilot',
     'copilot', 'acp', 'builtin', '{"binary_name":"copilot"}', 'agent_builtin_copilot',
     1, 'copilot', '["--acp","--stdio"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"mcp_capabilities":{"http":true,"sse":true},"prompt_capabilities":{"image":true,"audio":false,"embedded_context":true},"session_capabilities":{"list":{}}}',
     '[{"id":"copilot-login","name":"Log in with Copilot CLI","description":"Run `copilot login` in the terminal","_meta":{"terminal-auth":{"command":"copilot","args":["login"],"label":"Copilot Login"}}}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-00000000010c', '/api/assets/logos/tools/coding/qoder.png', 'Qoder',
     'qoder', 'acp', 'builtin', '{"binary_name":"qodercli"}', 'agent_builtin_qoder',
     1, 'qodercli', '["--acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"session_capabilities":{"list":{}},"prompt_capabilities":{"image":true,"audio":true,"embedded_context":true},"mcp_capabilities":{"http":true,"sse":true}}',
     '[{"id":"qodercli-login","name":"Use qodercli login","description":"Use your existing qodercli login for this agent. If needed, sign in from qodercli first."},{"type":"env_var","id":"qoder-personal-access-token","name":"Use QODER_PERSONAL_ACCESS_TOKEN","description":"Requires `QODER_PERSONAL_ACCESS_TOKEN` in the agent environment.","vars":[{"name":"QODER_PERSONAL_ACCESS_TOKEN"}]}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-00000000010d', '/api/assets/logos/ai-major/mistral.svg', 'Vibe',
     'vibe', 'acp', 'builtin', '{"binary_name":"vibe-acp"}', 'agent_builtin_vibe',
     1, 'vibe-acp', '[]', '[]',
     '[".vibe/skills"]',
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"prompt_capabilities":{"audio":false,"embedded_context":true,"image":false},"session_capabilities":{"close":{},"fork":{},"list":{}}}',
     '[]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-00000000010e', '/api/assets/logos/tools/coding/cursor.png', 'Cursor',
     'cursor', 'acp', 'builtin', '{"binary_name":"agent"}', 'agent_builtin_cursor',
     1, 'agent', '["acp"]', '[]',
     '[".cursor/skills"]',
     '{"supports_side_question":false}',
     'agent',
     NULL, NULL,
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-00000000010f', NULL, 'Kiro',
     'kiro', 'acp', 'builtin', '{"binary_name":"kiro-cli"}', 'agent_builtin_kiro',
     1, 'kiro-cli', '["acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo',
     '{"load_session":true,"prompt_capabilities":{"image":true,"audio":false,"embedded_context":false},"mcp_capabilities":{"http":true,"sse":false},"session_capabilities":{}}',
     '[{"id":"kiro-login","name":"Kiro Login","description":"Run ''kiro-cli login'' in terminal to authenticate. See https://kiro.dev/docs/cli/authentication/"}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-000000000110', '/api/assets/logos/brand/hermes.svg', 'Hermes',
     'hermes', 'acp', 'builtin', '{"binary_name":"hermes"}', 'agent_builtin_hermes',
     1, 'hermes', '["acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo',
     NULL, NULL,
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-000000000111', '/api/assets/logos/tools/coding/snow.png', 'Snow',
     'snow', 'acp', 'builtin', '{"binary_name":"snow"}', 'agent_builtin_snow',
     1, 'snow', '["--acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo',
     NULL, NULL,
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    -- Non-ACP builtins
    ('0190f5fe-7c00-7a00-8000-000000000112', '/api/assets/logos/tools/nanobot.svg', 'Nanobot',
     NULL, 'nanobot', 'builtin', '{"binary_name":"nanobot"}', 'agent_builtin_nanobot',
     1, 'nanobot', '["--experimental-acp"]', '[]',
     NULL,
     '{}',
     'yolo',
     NULL, NULL,
     3990,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('0190f5fe-7c00-7a00-8000-000000000113', '/api/assets/logos/tools/openclaw.svg', 'OpenClaw',
     NULL, 'openclaw-gateway', 'builtin', '{"binary_name":"openclaw"}', 'agent_builtin_openclaw',
     1, 'openclaw', '[]', '[]',
     NULL,
     '{}',
     'yolo',
     NULL, NULL,
     3900,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    -- Internal
    ('0190f5fe-7c00-7a00-8000-000000000114', '/api/assets/logos/brand/nomi.svg', 'Nomi',
     NULL, 'nomi', 'internal', '{}', 'agent_builtin_nomi',
     1, NULL, '[]', '[]',
     '[".nomi/skills"]',
     '{}',
     'yolo',
     NULL, NULL,
     100,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000);

INSERT INTO system_settings (
    singleton_key,
    language,
    notification_enabled,
    cron_notification_enabled,
    command_queue_enabled,
    save_upload_to_workspace,
    updated_at
) VALUES ('system', 'en-US', 1, 0, 0, 0, unixepoch('now', 'subsec') * 1000);
