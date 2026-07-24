-- Stable per-chat session ownership.
--
-- Historical databases may contain more than one channel_sessions row for the
-- same (plugin, user, chat) scope because the published v3 baseline did not
-- enforce uniqueness. Preserve every historical row, but deterministically
-- bind the scope to the earliest technical row so every future lookup returns
-- one canonical session.
CREATE TABLE channel_session_bindings (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_plugin_id   TEXT NOT NULL
                        CHECK (
                            length(channel_plugin_id) = 36
                            AND lower(channel_plugin_id) = channel_plugin_id
                            AND channel_plugin_id GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(channel_plugin_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                        ),
    channel_user_id     TEXT NOT NULL
                        CHECK (
                            length(channel_user_id) = 36
                            AND lower(channel_user_id) = channel_user_id
                            AND channel_user_id GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(channel_user_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                        ),
    chat_id             TEXT NOT NULL CHECK (length(chat_id) BETWEEN 1 AND 512),
    channel_session_id  TEXT NOT NULL UNIQUE
                        CHECK (
                            length(channel_session_id) = 36
                            AND lower(channel_session_id) = channel_session_id
                            AND channel_session_id GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(channel_session_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                        ),
    created_at          INTEGER NOT NULL,
    UNIQUE (channel_plugin_id, channel_user_id, chat_id)
);

INSERT INTO channel_session_bindings (
    channel_plugin_id,
    channel_user_id,
    chat_id,
    channel_session_id,
    created_at
)
SELECT
    canonical.channel_plugin_id,
    canonical.channel_user_id,
    canonical.chat_id,
    canonical.channel_session_id,
    canonical.created_at
FROM channel_sessions AS canonical
JOIN (
    SELECT
        channel_plugin_id,
        channel_user_id,
        chat_id,
        MIN(id) AS canonical_id
    FROM channel_sessions
    WHERE channel_plugin_id IS NOT NULL
      AND chat_id IS NOT NULL
      AND length(chat_id) BETWEEN 1 AND 512
    GROUP BY channel_plugin_id, channel_user_id, chat_id
) AS legacy_scope
  ON canonical.id = legacy_scope.canonical_id;

CREATE INDEX idx_channel_session_bindings_user_id
    ON channel_session_bindings(channel_user_id);
CREATE INDEX idx_channel_session_bindings_plugin_id
    ON channel_session_bindings(channel_plugin_id);
CREATE INDEX idx_channel_session_bindings_session_id
    ON channel_session_bindings(channel_session_id);

CREATE TRIGGER channel_session_bindings_identity_immutable
BEFORE UPDATE OF
    channel_plugin_id,
    channel_user_id,
    chat_id,
    channel_session_id,
    created_at
ON channel_session_bindings
BEGIN
    SELECT RAISE(ABORT, 'channel session binding identity is immutable');
END;

-- At-most-once admission for provider-owned inbound channel events.
--
-- Rows are intentionally retained indefinitely. Deleting a settled receipt
-- would allow a sufficiently delayed provider redelivery to repeat external
-- side effects. Every accepted phase is absorbing: there is no lease,
-- wall-clock timeout, or automatic redrive. A crash in `claimed` may lose one
-- event; a crash in `effects_started` may have already escaped to a provider,
-- requirement, decision, or model. Neither case can authorize a second owner.
--
-- There are deliberately no foreign keys: a receipt must be claimable before
-- a Conversation or channel session exists and must outlive later deletion of
-- the referenced business objects.
CREATE TABLE channel_inbound_receipts (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    operation_key       TEXT NOT NULL UNIQUE
                        CHECK (
                            length(operation_key) = 83
                            AND operation_key GLOB 'channel-inbound:v1:[0-9a-f]*'
                            AND substr(operation_key, 20) NOT GLOB '*[^0-9a-f]*'
                        ),
    user_scope_id       TEXT NOT NULL
                        CHECK (
                            length(user_scope_id) = 36
                            AND lower(user_scope_id) = user_scope_id
                            AND user_scope_id GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(user_scope_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                        ),
    user_id             TEXT
                        CHECK (
                            user_id IS NULL
                            OR (
                                length(user_id) = 36
                                AND lower(user_id) = user_id
                                AND user_id GLOB '????????-????-7???-[89ab]???-????????????'
                                AND replace(user_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                            )
                        ),
    channel_plugin_scope_id TEXT NOT NULL
                        CHECK (
                            length(channel_plugin_scope_id) = 36
                            AND lower(channel_plugin_scope_id) = channel_plugin_scope_id
                            AND channel_plugin_scope_id GLOB '????????-????-7???-[89ab]???-????????????'
                            AND replace(channel_plugin_scope_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                        ),
    channel_plugin_id   TEXT
                        CHECK (
                            channel_plugin_id IS NULL
                            OR (
                                length(channel_plugin_id) = 36
                                AND lower(channel_plugin_id) = channel_plugin_id
                                AND channel_plugin_id GLOB '????????-????-7???-[89ab]???-????????????'
                                AND replace(channel_plugin_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                            )
                        ),
    platform            TEXT NOT NULL CHECK (length(platform) BETWEEN 1 AND 64),
    chat_id             TEXT NOT NULL CHECK (length(chat_id) BETWEEN 1 AND 512),
    provider_event_id   TEXT NOT NULL CHECK (length(provider_event_id) BETWEEN 1 AND 512),
    payload_hash        TEXT NOT NULL
                        CHECK (
                            length(payload_hash) = 64
                            AND lower(payload_hash) = payload_hash
                            AND payload_hash NOT GLOB '*[^0-9a-f]*'
                        ),
    status              TEXT NOT NULL DEFAULT 'accepted'
                        CHECK (status IN ('accepted', 'completed', 'failed')),
    phase               TEXT NOT NULL DEFAULT 'claimed'
                        CHECK (phase IN ('claimed', 'effects_started', 'settled')),
    owner_generation    INTEGER NOT NULL DEFAULT 1 CHECK (owner_generation >= 1),
    conversation_scope_id TEXT,
    message_scope_id    TEXT,
    conversation_id     TEXT,
    message_id          TEXT,
    outcome_json        TEXT CHECK (
                            outcome_json IS NULL
                            OR (json_valid(outcome_json) AND json_type(outcome_json) = 'object')
                        ),
    error_text          TEXT,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    completed_at        INTEGER,
    CHECK (
        (status = 'accepted' AND phase IN ('claimed', 'effects_started') AND completed_at IS NULL)
        OR
        (status IN ('completed', 'failed') AND phase = 'settled' AND completed_at IS NOT NULL)
    ),
    CHECK (conversation_scope_id IS NULL OR (length(conversation_scope_id) = 36 AND lower(conversation_scope_id) = conversation_scope_id AND conversation_scope_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(conversation_scope_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (message_scope_id IS NULL OR (length(message_scope_id) = 36 AND lower(message_scope_id) = message_scope_id AND message_scope_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(message_scope_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (conversation_id IS NULL OR (length(conversation_id) = 36 AND lower(conversation_id) = conversation_id AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
    CHECK (message_id IS NULL OR (length(message_id) = 36 AND lower(message_id) = message_id AND message_id GLOB '????????-????-7???-[89ab]???-????????????' AND replace(message_id, '-', '') NOT GLOB '*[^0-9a-f]*'))
);

CREATE INDEX idx_channel_inbound_receipts_user_id
    ON channel_inbound_receipts(user_id);
CREATE INDEX idx_channel_inbound_receipts_channel_plugin_id
    ON channel_inbound_receipts(channel_plugin_id);
CREATE INDEX idx_channel_inbound_receipts_conversation_id
    ON channel_inbound_receipts(conversation_id);
CREATE INDEX idx_channel_inbound_receipts_message_id
    ON channel_inbound_receipts(message_id);
CREATE INDEX idx_channel_inbound_receipts_status_updated
    ON channel_inbound_receipts(status, updated_at);

CREATE TRIGGER channel_inbound_receipts_identity_immutable
BEFORE UPDATE OF
    operation_key,
    user_scope_id,
    channel_plugin_scope_id,
    platform,
    chat_id,
    provider_event_id,
    payload_hash,
    created_at
ON channel_inbound_receipts
BEGIN
    SELECT RAISE(ABORT, 'channel inbound receipt identity is immutable');
END;

CREATE TRIGGER channel_inbound_receipts_no_delete
BEFORE DELETE ON channel_inbound_receipts
BEGIN
    SELECT RAISE(ABORT, 'channel inbound receipts are retained indefinitely');
END;

CREATE TRIGGER channel_inbound_receipts_scope_set_once
BEFORE UPDATE OF conversation_scope_id, message_scope_id
ON channel_inbound_receipts
WHEN OLD.phase <> 'effects_started'
  OR NEW.phase <> 'settled'
  OR OLD.conversation_scope_id IS NOT NULL
  OR OLD.message_scope_id IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'channel inbound outcome scope can only be set while settling');
END;
