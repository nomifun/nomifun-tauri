/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useState } from 'react';
import { useTranslation } from 'react-i18next';
import classNames from 'classnames';
import { Popconfirm } from '@arco-design/web-react';
import { CheckOne, Loading, Pause, PauseOne, PlayOne, Refresh } from '@icon-park/react';
import { ipcBridge } from '@/common';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { refreshOnVersionConflict } from './refreshOnVersionConflict';
import { isTerminalExecutionStatus } from './executionStatusMeta';
import type { ExecutionId } from '@/common/types/ids';

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
        hover,
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

/** Status-aware controls for a single agent execution. */
export const ExecutionControls: React.FC<{
  executionId: ExecutionId;
  executionVersion: number;
  status: string;
  inFlightCount?: number;
  refetch: () => Promise<void>;
  onReplan: () => void;
}> = ({ executionId, executionVersion, status, inFlightCount, refetch, onReplan }) => {
  const { t } = useTranslation();
  const [message, msgCtx] = useArcoMessage();
  const [busy, setBusy] = useState(false);

  const isTerminal = isTerminalExecutionStatus(status);
  // `planning` and `''` (detail not yet loaded) both
  // render a disabled busy placeholder so the header always shows a primary control.
  const isBusyPlaceholder = status === 'planning' || status === '';
  // Show how many collaborators are currently active.
  const showActiveParticipants =
    (status === 'running' || status === 'waiting_input') && typeof inFlightCount === 'number' && inFlightCount > 0;

  const executeAction = useCallback(
    async (action: () => Promise<unknown>, okKey: string, errKey: string) => {
      setBusy(true);
      try {
        await action();
        message.success(t(okKey));
        await refetch();
      } catch (e) {
        await refreshOnVersionConflict(e, refetch);
        message.error(t(errKey, { error: String(e) }));
      } finally {
        setBusy(false);
      }
    },
    [message, refetch, t],
  );

  const onApprove = () =>
    void executeAction(
      () =>
        ipcBridge.agentExecution.approve.invoke({
          execution_id: executionId,
          updates: { expected_version: executionVersion },
        }),
      'agentExecution.controls.approveOk',
      'agentExecution.controls.approveError',
    );
  const onPause = () =>
    void executeAction(
      () =>
        ipcBridge.agentExecution.pause.invoke({
          execution_id: executionId,
          updates: { expected_version: executionVersion },
        }),
      'agentExecution.controls.pauseOk',
      'agentExecution.controls.pauseError',
    );
  const onResume = () =>
    void executeAction(
      () =>
        ipcBridge.agentExecution.resume.invoke({
          execution_id: executionId,
          updates: { expected_version: executionVersion },
        }),
      'agentExecution.controls.resumeOk',
      'agentExecution.controls.resumeError',
    );
  const onCancel = () =>
    void executeAction(
      () =>
        ipcBridge.agentExecution.cancel.invoke({
          execution_id: executionId,
          updates: { expected_version: executionVersion },
        }),
      'agentExecution.controls.cancelOk',
      'agentExecution.controls.cancelError',
    );

  return (
    <div className='flex shrink-0 items-center gap-8px'>
      {msgCtx}
      {status !== '' && !isBusyPlaceholder && !isTerminal && (
        <HeaderControl label={t('agentExecution.controls.replan')} onClick={onReplan} busy={busy}>
          <Refresh theme='outline' size='14' strokeWidth={3} />
        </HeaderControl>
      )}
      {status === 'awaiting_approval' && (
        <HeaderControl label={t('agentExecution.controls.approve')} onClick={onApprove} busy={busy} tone='primary'>
          <CheckOne theme='outline' size='14' strokeWidth={3} />
        </HeaderControl>
      )}
      {isBusyPlaceholder && (
        // Disabled busy primary — clicks suppressed (busy). Guarantees the header
        // always presents a meaningful primary control, even before detail loads.
        <HeaderControl label={t('agentExecution.controls.planning')} onClick={() => {}} busy tone='primary'>
          <Loading theme='outline' size='14' strokeWidth={3} className='animate-spin line-height-0' />
        </HeaderControl>
      )}
      {(status === 'running' || status === 'waiting_input') && (
        <HeaderControl label={t('agentExecution.controls.pause')} onClick={onPause} busy={busy}>
          <PauseOne theme='outline' size='14' strokeWidth={3} />
        </HeaderControl>
      )}
      {status === 'paused' && (
        <HeaderControl label={t('agentExecution.controls.resume')} onClick={onResume} busy={busy}>
          <PlayOne theme='outline' size='14' strokeWidth={3} />
        </HeaderControl>
      )}
      {showActiveParticipants && (
        // Read-only status badge (NOT a HeaderControl) — mirrors the status pill so it
        // cannot be mis-clicked. Signals that collaborators are currently active.
        <span className='inline-flex items-center gap-4px rd-8px px-8px h-30px text-11px font-500 text-t-secondary border border-b-base'>
          <Loading theme='outline' size='12' strokeWidth={3} className='animate-spin line-height-0' />
          {t('agentExecution.controls.activeParticipants', {
            count: inFlightCount,
          })}
        </span>
      )}
      {!isTerminal && (
        <Popconfirm
          focusLock
          title={t('agentExecution.controls.cancelConfirm')}
          okText={t('agentExecution.controls.confirm')}
          cancelText={t('agentExecution.controls.back')}
          onOk={onCancel}
        >
          {/* Popconfirm needs a single focusable child; the control is busy-aware. */}
          <div
            role='button'
            tabIndex={0}
            aria-label={t('agentExecution.controls.cancel')}
            aria-disabled={busy}
            className='flex h-30px shrink-0 cursor-pointer select-none items-center gap-5px rd-8px border border-b-base px-10px text-12px font-500 text-t-secondary transition-all duration-150 hover:border-danger hover:text-danger'
            style={{
              opacity: busy ? 0.6 : undefined,
              pointerEvents: busy ? 'none' : undefined,
            }}
          >
            <Pause theme='outline' size='14' strokeWidth={3} />
            <span>{t('agentExecution.controls.cancel')}</span>
          </div>
        </Popconfirm>
      )}
    </div>
  );
};
