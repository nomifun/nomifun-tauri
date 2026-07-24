import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./RequirementForm.tsx', import.meta.url), 'utf8');

describe('requirement form status authority', () => {
  test('does not replay the status snapshot when only metadata or attachments changed', () => {
    const explanation = source.indexOf('Status is execution authority, not ordinary form metadata');
    const explicitTransition = source.indexOf(
      'if (isEdit && !isExecutionOwnedStatus && values.status !== initial?.status)',
      explanation
    );
    const assignment = source.indexOf('payload.status = values.status;', explicitTransition);

    expect(explanation).toBeGreaterThan(-1);
    expect(explicitTransition).toBeGreaterThan(explanation);
    expect(assignment).toBeGreaterThan(explicitTransition);
    expect(source.includes('if (isEdit) {\n      payload.status = values.status;')).toBe(false);
  });

  test('cannot mint or mutate the execution-owned in_progress status', () => {
    expect(source.includes("const EDITABLE_STATUSES: RequirementStatus[] = [\n  'pending',\n  'in_progress'")).toBe(
      false
    );
    expect(source.includes("const isExecutionOwnedStatus = isEdit && initial?.status === 'in_progress'")).toBe(
      true
    );
    expect(source.includes('disabled={isExecutionOwnedStatus}')).toBe(true);
    expect(source.includes("isExecutionOwnedStatus ? ['in_progress'] : EDITABLE_STATUSES")).toBe(true);
  });
});
