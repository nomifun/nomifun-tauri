import type { TAgentExecutionTemplateDetail } from '@/common/types/agentExecution/agentExecutionTemplateTypes';
import type { TExecutionModelRef } from '@/common/types/agentExecution/agentExecutionTypes';
import type { ExecutionTemplateId } from '@/common/types/ids';

const MODEL_SEPARATOR = '\u0000';

const modelKey = (model: TExecutionModelRef): string =>
  `${model.provider_id}${MODEL_SEPARATOR}${model.model}`;

export type AppliedCollaborationTemplate = {
  execution_template_id: ExecutionTemplateId;
  name: string;
  participantCount: number;
  models: TExecutionModelRef[];
};

export const templateParticipantModels = (
  detail: TAgentExecutionTemplateDetail,
): TExecutionModelRef[] => {
  const seen = new Set<string>();
  const models: TExecutionModelRef[] = [];
  for (const participant of detail.participants) {
    if (!participant.provider_id?.trim() || !participant.model?.trim()) continue;
    const model = { provider_id: participant.provider_id, model: participant.model };
    const key = modelKey(model);
    if (seen.has(key)) continue;
    seen.add(key);
    models.push(model);
  }
  return models;
};

export const templateContainsModel = (
  detail: TAgentExecutionTemplateDetail,
  model: TExecutionModelRef,
): boolean => templateParticipantModels(detail).some((candidate) => modelKey(candidate) === modelKey(model));

export const toAppliedCollaborationTemplate = (
  detail: TAgentExecutionTemplateDetail,
): AppliedCollaborationTemplate => ({
  execution_template_id: detail.execution_template_id,
  name: detail.name,
  participantCount: detail.participants.length,
  models: templateParticipantModels(detail),
});
