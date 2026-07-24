import { describe, expect, test } from 'bun:test';
import type { TAgentExecutionTemplateDetail } from '@/common/types/agentExecution/agentExecutionTemplateTypes';
import { parseExecutionTemplateId, parseProviderId } from '@/common/types/ids';
import {
  templateContainsModel,
  templateParticipantModels,
  toAppliedCollaborationTemplate,
} from './collaborationTemplateModel';

const PROVIDER_A = parseProviderId('0190f5fe-7c00-7a00-8000-000000000001');
const PROVIDER_B = parseProviderId('0190f5fe-7c00-7a00-8000-000000000002');
const PROVIDER_C = parseProviderId('0190f5fe-7c00-7a00-8000-000000000003');
const TEMPLATE_ID = parseExecutionTemplateId('0190f5fe-7c00-7a00-8000-000000000001');

const detail = {
  execution_template_id: TEMPLATE_ID,
  name: 'Review plan',
  participants: [
    { provider_id: PROVIDER_A, model: 'model-a' },
    { provider_id: PROVIDER_A, model: 'model-a' },
    { provider_id: PROVIDER_B, model: 'model-b' },
    { provider_id: null, model: null },
  ],
} as TAgentExecutionTemplateDetail;

describe('collaboration template model', () => {
  test('derives one canonical model set from executable participants', () => {
    expect(templateParticipantModels(detail)).toEqual([
      { provider_id: PROVIDER_A, model: 'model-a' },
      { provider_id: PROVIDER_B, model: 'model-b' },
    ]);
    expect(
      templateContainsModel(detail, { provider_id: PROVIDER_B, model: 'model-b' }),
    ).toBe(true);
    expect(
      templateContainsModel(detail, { provider_id: PROVIDER_C, model: 'model-c' }),
    ).toBe(false);
  });

  test('keeps participant count distinct from the deduplicated model authority', () => {
    expect(toAppliedCollaborationTemplate(detail)).toEqual({
      execution_template_id: TEMPLATE_ID,
      name: 'Review plan',
      participantCount: 4,
      models: [
        { provider_id: PROVIDER_A, model: 'model-a' },
        { provider_id: PROVIDER_B, model: 'model-b' },
      ],
    });
  });
});
