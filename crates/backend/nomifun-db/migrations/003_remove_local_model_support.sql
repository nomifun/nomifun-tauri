-- Retire the removed on-device model capability and all of its persisted
-- product state. Provider identities were generated dynamically, so platform
-- is the only stable selector available to upgraded installations.
CREATE TEMP TABLE retired_local_model_providers (
    id TEXT PRIMARY KEY NOT NULL
);

INSERT INTO retired_local_model_providers (id)
SELECT id
FROM providers
WHERE platform = 'nomifun-local-model';

-- Local image jobs cannot be resumed without the removed runtime. Generated
-- workshop assets are independent rows/files and remain available to users.
DELETE FROM creation_tasks
WHERE provider_id IN (SELECT id FROM retired_local_model_providers);

DELETE FROM preset_model_preferences
WHERE provider_id IN (SELECT id FROM retired_local_model_providers);

-- Execution templates are live configuration and must never retain an
-- unavailable provider. Historical execution snapshots remain immutable audit
-- records; they are deliberately not rewritten.
DELETE FROM agent_execution_templates
WHERE id IN (
    SELECT participant.template_id
    FROM agent_execution_template_participants participant
    WHERE participant.provider_id IN (SELECT id FROM retired_local_model_providers)
);

-- Participant snapshots are immutable by design. Tombstone every execution
-- that still names the retired provider so it cannot be resumed or dispatched,
-- while preserving the audit record required by the execution schema.
UPDATE agent_executions
SET deleted_at = MAX(
        updated_at,
        CAST(strftime('%s', 'now') AS INTEGER) * 1000
    ),
    updated_at = MAX(
        updated_at,
        CAST(strftime('%s', 'now') AS INTEGER) * 1000
    )
WHERE deleted_at IS NULL
  AND EXISTS (
      SELECT 1
      FROM agent_execution_participants participant
      WHERE participant.execution_id = agent_executions.id
        AND participant.provider_id IN (SELECT id FROM retired_local_model_providers)
  );

-- Preserve conversation history while clearing the now-invalid selected model.
-- Clearing the pool at the same time satisfies its lead-model authority rule;
-- other pools are pruned by provider_soft_reference_cleanup below.
UPDATE conversations
SET model = NULL,
    execution_model_pool = NULL
WHERE json_extract(model, '$.provider_id') IN (
    SELECT id FROM retired_local_model_providers
);

DELETE FROM client_preferences
WHERE key = 'idmm_backup_provider_id'
  AND value IN (SELECT id FROM retired_local_model_providers);

-- Local speech selection did not carry a provider entity ID, so it needs an
-- explicit cleanup in both the current and legacy preference locations.
DELETE FROM client_preferences
WHERE key IN ('tools.speechToText', 'speechToText')
  AND CASE
      WHEN json_valid(value) THEN json_extract(value, '$.provider') = 'local'
      ELSE 0
  END;

DELETE FROM providers
WHERE id IN (SELECT id FROM retired_local_model_providers);

-- Remove any remaining product preference whose JSON payload still points at
-- the retired provider. Generic preference rows have no foreign key, so this
-- final sweep prevents hidden stale selections outside the known model keys.
DELETE FROM client_preferences
WHERE CASE
    WHEN json_valid(value) THEN EXISTS (
        SELECT 1
        FROM json_tree(client_preferences.value) node
        WHERE node.type = 'text'
          AND node.atom IN (SELECT id FROM retired_local_model_providers)
    )
    ELSE 0
END;

-- The catalog provenance existed solely for the removed managed local model
-- inventory. Preserve any non-local profile by degrading it to inferred data.
UPDATE model_profiles
SET source = 'inferred'
WHERE source = 'catalog';

DROP TABLE retired_local_model_providers;
