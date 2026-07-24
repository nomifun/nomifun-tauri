/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ipcBridge } from '@/common';
import type { TAgentExecutionDetail } from '@/common/types/agentExecution/agentExecutionTypes';
import type { ExecutionStepId } from '@/common/types/ids';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import ExecutionPlanEditor from './ExecutionPlanEditor';
import { refreshOnVersionConflict } from './refreshOnVersionConflict';

type AdjustmentSummary = {
  kept: number;
  added: number;
  removed: number;
};

function activeStepIds(detail: TAgentExecutionDetail): Set<ExecutionStepId> {
  return new Set(
    detail.steps.filter((step) => step.superseded_in_revision == null).map((step) => step.step_id),
  );
}

export function summarizeAdjustment(before: TAgentExecutionDetail, after: TAgentExecutionDetail): AdjustmentSummary {
  const beforeIds = activeStepIds(before);
  const afterIds = activeStepIds(after);
  let kept = 0;
  let added = 0;
  let removed = 0;
  for (const id of afterIds) {
    if (beforeIds.has(id)) kept += 1;
    else added += 1;
  }
  for (const id of beforeIds) {
    if (!afterIds.has(id)) removed += 1;
  }
  return { kept, added, removed };
}

const ExecutionAdjustBox: React.FC<{
  detail: TAgentExecutionDetail;
  refetch: () => Promise<void>;
  onApplied: () => void;
}> = ({ detail, refetch, onApplied }) => {
  const { t } = useTranslation();
  const [message, messageContext] = useArcoMessage();
  const [intent, setIntent] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [lastSummary, setLastSummary] = useState<AdjustmentSummary | null>(null);

  const summary = useMemo(
    () =>
      lastSummary
        ? t('agentExecution.adjust.summary', {
            kept: lastSummary.kept,
            added: lastSummary.added,
            removed: lastSummary.removed,
          })
        : null,
    [lastSummary, t],
  );

  const submit = useCallback(
    async (value: string) => {
      if (submitting) return;
      setSubmitting(true);
      try {
        const next = await ipcBridge.agentExecution.adjust.invoke({
          execution_id: detail.execution.execution_id,
          updates: {
            intent: value,
            expected_version: detail.execution.version,
          },
        });
        const nextSummary = summarizeAdjustment(detail, next);
        setLastSummary(nextSummary);
        setIntent('');
        onApplied();
        await refetch();
        message.success(
          t('agentExecution.adjust.summary', {
            kept: nextSummary.kept,
            added: nextSummary.added,
            removed: nextSummary.removed,
          }),
        );
      } catch (error) {
        await refreshOnVersionConflict(error, refetch);
        message.error(
          t('agentExecution.adjust.error', {
            error: String(error),
          }),
        );
      } finally {
        setSubmitting(false);
      }
    },
    [detail, message, onApplied, refetch, submitting, t],
  );

  return (
    <div className='shrink-0 border-t border-t-base bg-1 px-10px pb-10px pt-8px'>
      {messageContext}
      {summary && <div className='mb-6px truncate px-8px text-11px text-t-tertiary'>{summary}</div>}
      <ExecutionPlanEditor
        fluid
        value={intent}
        onChange={setIntent}
        onSubmit={submit}
        submitting={submitting}
        label={t('agentExecution.adjust.label')}
        placeholder={t('agentExecution.adjust.placeholder')}
      />
    </div>
  );
};

export default ExecutionAdjustBox;
