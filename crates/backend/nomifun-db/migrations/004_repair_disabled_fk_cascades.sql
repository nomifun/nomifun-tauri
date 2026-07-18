-- Migration 003 removed retired local-model providers while the migration
-- runner had foreign-key enforcement disabled. SQLite therefore skipped the
-- declared CASCADE/SET NULL actions. Repair databases where 003 was already
-- committed before startup reached the post-migration foreign_key_check.

UPDATE conversations
SET execution_template_id = NULL
WHERE execution_template_id IS NOT NULL
  AND NOT EXISTS (
      SELECT 1
      FROM agent_execution_templates template
      WHERE template.id = conversations.execution_template_id
  );

DELETE FROM agent_execution_template_participants
WHERE NOT EXISTS (
    SELECT 1
    FROM agent_execution_templates template
    WHERE template.id = agent_execution_template_participants.template_id
);

DELETE FROM model_profiles
WHERE NOT EXISTS (
    SELECT 1
    FROM providers provider
    WHERE provider.id = model_profiles.provider_id
);
