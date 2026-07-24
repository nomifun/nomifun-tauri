/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { CheckOne, PlayOne } from '@icon-park/react';
import { ipcBridge } from '@/common';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { useExecutionSafe } from './ExecutionContext';
import styles from './planApprovalBanner.module.css';
import { refreshOnVersionConflict } from './refreshOnVersionConflict';

// Toasts stay click-through so they never block the banner action.
const TOAST_CLASS = 'nomifun-message-passthrough';
const TOAST_OK_MS = 1500;
const TOAST_ERR_MS = 2500;

/** In-conversation approval affordance for executions waiting at their plan gate. */
const PlanApprovalBanner: React.FC = () => {
  const { t } = useTranslation();
  const execution = useExecutionSafe();
  const [message, msgCtx] = useArcoMessage();
  const [approving, setApproving] = useState(false);

  const executionId = execution?.executionId ?? null;
  const parked = execution?.detail?.execution.status === 'awaiting_approval';

  const doApprove = async () => {
    if (approving || !executionId) return;
    setApproving(true);
    try {
      await ipcBridge.agentExecution.approve.invoke({
        execution_id: executionId,
        updates: {
          expected_version: execution?.detail?.execution.version ?? 0,
        },
      });
      message.success({
        content: t('agentExecution.approval.ok', {
          defaultValue: '已批准，开始协作',
        }),
        duration: TOAST_OK_MS,
        className: TOAST_CLASS,
      });
      await execution?.refetch();
    } catch (e) {
      await refreshOnVersionConflict(e, execution?.refetch ?? (async () => {}));
      message.error({
        content: t('agentExecution.approval.error', {
          defaultValue: '批准失败：{{error}}',
          error: String(e),
        }),
        duration: TOAST_ERR_MS,
        className: TOAST_CLASS,
      });
    } finally {
      setApproving(false);
    }
  };

  // Only surface while the linked execution is awaiting approval.
  if (!execution || !executionId || !parked) return null;

  return (
    <div className={styles.banner}>
      {msgCtx}
      <div className={styles.lead}>
        <span className={styles.badge}>
          <PlayOne theme='outline' size='15' strokeWidth={3} />
        </span>
        <div className={styles.copy}>
          <span className={styles.eyebrow}>{t('agentExecution.approval.eyebrow', { defaultValue: '待批准' })}</span>
          <span className={styles.text}>
            {t('agentExecution.approval.text', {
              defaultValue: '协作计划已就绪，可继续调整；准备好后批准执行。',
            })}
          </span>
        </div>
      </div>

      <div
        role='button'
        tabIndex={0}
        aria-label={t('agentExecution.approval.button', {
          defaultValue: '批准执行',
        })}
        aria-disabled={approving}
        className={styles.action}
        onClick={approving ? undefined : () => void doApprove()}
        onKeyDown={(e) => {
          if ((e.key === 'Enter' || e.key === ' ') && !approving) {
            e.preventDefault();
            void doApprove();
          }
        }}
      >
        <CheckOne theme='outline' size='14' strokeWidth={3} />
        <span>{t('agentExecution.approval.button', { defaultValue: '批准执行' })}</span>
      </div>
    </div>
  );
};

export default PlanApprovalBanner;
