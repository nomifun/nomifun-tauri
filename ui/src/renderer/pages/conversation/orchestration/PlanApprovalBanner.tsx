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
import { useOrchestrationSafe } from './OrchestrationContext';
import styles from './planApprovalBanner.module.css';

// Match ProjectedWorkerView's toast conventions (brief, click-through so they
// never block the banner action).
const TOAST_CLASS = 'nomifun-message-passthrough';
const TOAST_OK_MS = 1500;
const TOAST_ERR_MS = 2500;

/**
 * PlanApprovalBanner — 智能编排「编排后不自动执行」的会话内提示条.
 *
 * A run started from a conversation defaults to `interactive`, so after the lead
 * plans the task graph it PARKS at `awaiting_plan_approval` instead of executing
 * — leaving the user room to adjust (canvas / 调整编排 / 节点启动前配置). But the
 * only「批准执行」control lived in the right-side canvas overlay, which can be
 * collapsed out of view — so the user had no in-conversation hint that the plan
 * was ready or how to start it.
 *
 * This slim strip mounts at the top of the lead conversation content area and
 * renders ONLY while the linked run is parked awaiting approval: it says the plan
 * is ready (keep adjusting if you like) and surfaces a one-click 批准执行 button
 * that reuses the exact same approve path as the canvas control
 * (`orchestrator.runs.approve` → `approve_plan` + `engine.start`). Approving flips
 * the run to `running` (via the `run.statusChanged` WS event → `useRunLive`
 * refetch), and this banner then renders `null` again.
 *
 * Outside an {@link OrchestrationProvider} (companion chat) or for any non-parked
 * run, it renders `null` — zero footprint on ordinary conversations.
 */
const PlanApprovalBanner: React.FC = () => {
  const { t } = useTranslation();
  const orch = useOrchestrationSafe();
  const [message, msgCtx] = useArcoMessage();
  const [approving, setApproving] = useState(false);

  const runId = orch?.runId ?? null;
  const parked = orch?.detail?.run.status === 'awaiting_plan_approval';

  const doApprove = async () => {
    if (approving || !runId) return;
    setApproving(true);
    try {
      await ipcBridge.orchestrator.runs.approve.invoke({ id: runId });
      message.success({
        content: t('orchestrator.run.approve.ok', { defaultValue: '已批准,开始执行' }),
        duration: TOAST_OK_MS,
        className: TOAST_CLASS,
      });
      await orch?.refetch();
    } catch (e) {
      message.error({
        content: t('orchestrator.run.approve.error', { defaultValue: '批准失败:{{error}}', error: String(e) }),
        duration: TOAST_ERR_MS,
        className: TOAST_CLASS,
      });
    } finally {
      setApproving(false);
    }
  };

  // Only surface while the linked run is parked awaiting approval.
  if (!orch || !runId || !parked) return null;

  return (
    <div className={styles.banner}>
      {msgCtx}
      <div className={styles.lead}>
        <span className={styles.badge}>
          <PlayOne theme='outline' size='15' strokeWidth={3} />
        </span>
        <div className={styles.copy}>
          <span className={styles.eyebrow}>
            {t('orchestrator.run.approve.bannerEyebrow', { defaultValue: '待批准' })}
          </span>
          <span className={styles.text}>
            {t('orchestrator.run.approve.bannerText', {
              defaultValue: '计划已就绪,可继续调整;准备好后点「批准执行」开始。',
            })}
          </span>
        </div>
      </div>

      <div
        role='button'
        tabIndex={0}
        aria-label={t('orchestrator.run.approve.button', { defaultValue: '批准执行' })}
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
        <span>{t('orchestrator.run.approve.button', { defaultValue: '批准执行' })}</span>
      </div>
    </div>
  );
};

export default PlanApprovalBanner;
