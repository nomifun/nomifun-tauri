ALTER TABLE providers ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0;

WITH ordered AS (
  SELECT id, ROW_NUMBER() OVER (ORDER BY created_at ASC, id ASC) - 1 AS rn
  FROM providers
)
UPDATE providers
SET sort_order = (SELECT rn FROM ordered WHERE ordered.id = providers.id);

CREATE INDEX IF NOT EXISTS idx_providers_sort_order ON providers(sort_order, created_at);
