/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useState } from 'react';
import { useTranslation } from 'react-i18next';
import classNames from 'classnames';
import { Popconfirm } from '@arco-design/web-react';
import { CheckOne, Pause, PauseOne, PlayOne, Refresh } from '@icon-park/react';
import { ipcBridge } from '@/common';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';

/** A single status-gated header control. Never a bare `<button>` — a
 * `role="button"` div, busy-aware (greyed + click-suppressed while in flight). */
const HeaderControl: React.FC<{
  label: string;
  onClick: () => void;
  busy: boolean;
  tone?: 'primary' | 'neutral' | 'danger';
  children: React.ReactNode;
}> = ({ label, onClick, busy, tone = 'neutral', children }) => {
  const primary = tone === 'primary';
  const hover =
    tone === 'danger'
      ? 'hover:border-danger hover:text-danger'
      : tone === 'primary'
        ? 'hover:opacity-90'
        : 'hover:border-primary-6 hover:text-primary-6';
  return (
    <div
      role='button'
      tabIndex={0}
      aria-label={label}
      aria-disabled={busy}
      onClick={busy ? undefined : onClick}
      onKeyDown={(e) => {
        if (busy) return;
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          onClick();
        }
      }}
      className={classNames(
        'flex h-30px shrink-0 cursor-pointer select-none items-center gap-5px rd-8px px-10px text-12px font-500 transition-all duration-150',
        primary ? 'text-white' : 'border border-b-base text-t-secondary',
        hover
      )}
      style={{
        background: primary ? 'rgb(var(--primary-6))' : undefined,
        opacity: busy ? 0.6 : undefined,
        pointerEvents: busy ? 'none' : undefined,
      }}
    >
      {children}
      <span>{label}</span>
    </div>
  );
};

/**
 * RunControls — the status-aware run-control button group, lifted UP from the
 * DAG canvas into the shared glass header so it is reachable from BOTH the 对话
 * and 编排画布 views (and rendered exactly once). Status gating mirrors the old
 * run-detail header: awaiting → approve (primary); running → pause; paused → resume;
 * any non-terminal run → confirm-guarded cancel. Each action calls its REST
 * endpoint, toasts via {@link useArcoMessage}, then refetches.
 */
export const RunControls: React.FC<{
  runId: string;
  status: string;
  refetch: () => Promise<void>;
  onReplan: () => void;
}> = ({ runId, status, refetch, onReplan }) => {
  const { t } = useTranslation();
  const [message, msgCtx] = useArcoMessage();
  const [busy, setBusy] = useState(false);

  const isTerminal = status === 'completed' || status === 'failed' || status === 'cancelled';

  const run = useCallback(
    async (
      action: () => Promise<void>,
      okKey: string,
      errKey: string,
    ) => {
      setBusy(true);
      try {
        await action();
        message.success(t(okKey));
        await refetch();
      } catch (e) {
        message.error(t(errKey, { error: String(e) }));
      } finally {
        setBusy(false);
      }
    },
    [message, refetch, t]
  );

  const onApprove = () =>
    void run(
      () => ipcBridge.orchestrator.runs.approve.invoke({ id: runId }),
      'orchestrator.run.detail.approveOk',
      'orchestrator.run.detail.approveError'
    );
  const onPause = () =>
    void run(
      () => ipcBridge.orchestrator.runs.pause.invoke({ id: runId }),
      'orchestrator.run.detail.pauseOk',
      'orchestrator.run.detail.pauseError'
    );
  const onResume = () =>
    void run(
      () => ipcBridge.orchestrator.runs.resume.invoke({ id: runId }),
      'orchestrator.run.detail.resumeOk',
      'orchestrator.run.detail.resumeError'
    );
  const onCancel = () =>
    void run(
      () => ipcBridge.orchestrator.runs.cancel.invoke({ id: runId }),
      'orchestrator.run.detail.cancelOk',
      'orchestrator.run.detail.cancelError'
    );

  return (
    <div className='flex shrink-0 items-center gap-8px'>
      {msgCtx}
      <HeaderControl label={t('orchestrator.run.detail.replan')} onClick={onReplan} busy={busy}>
        <Refresh theme='outline' size='14' strokeWidth={3} />
      </HeaderControl>
      {status === 'awaiting_plan_approval' && (
        <HeaderControl label={t('orchestrator.run.detail.approvePlan')} onClick={onApprove} busy={busy} tone='primary'>
          <CheckOne theme='outline' size='14' strokeWidth={3} />
        </HeaderControl>
      )}
      {status === 'running' && (
        <HeaderControl label={t('orchestrator.run.detail.pause')} onClick={onPause} busy={busy}>
          <PauseOne theme='outline' size='14' strokeWidth={3} />
        </HeaderControl>
      )}
      {status === 'paused' && (
        <HeaderControl label={t('orchestrator.run.detail.resume')} onClick={onResume} busy={busy}>
          <PlayOne theme='outline' size='14' strokeWidth={3} />
        </HeaderControl>
      )}
      {!isTerminal && (
        <Popconfirm
          focusLock
          title={t('orchestrator.run.detail.cancelConfirm')}
          okText={t('orchestrator.run.detail.cancelConfirmOk')}
          cancelText={t('orchestrator.run.detail.cancelConfirmCancel')}
          onOk={onCancel}
        >
          {/* Popconfirm needs a single focusable child; the control is busy-aware. */}
          <div
            role='button'
            tabIndex={0}
            aria-label={t('orchestrator.run.detail.cancel')}
            aria-disabled={busy}
            className='flex h-30px shrink-0 cursor-pointer select-none items-center gap-5px rd-8px border border-b-base px-10px text-12px font-500 text-t-secondary transition-all duration-150 hover:border-danger hover:text-danger'
            style={{ opacity: busy ? 0.6 : undefined, pointerEvents: busy ? 'none' : undefined }}
          >
            <Pause theme='outline' size='14' strokeWidth={3} />
            <span>{t('orchestrator.run.detail.cancel')}</span>
          </div>
        </Popconfirm>
      )}
    </div>
  );
};
