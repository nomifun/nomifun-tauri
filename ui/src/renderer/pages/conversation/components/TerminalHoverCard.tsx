/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import classNames from 'classnames';
import React from 'react';
import { useTranslation } from 'react-i18next';

import type { ITerminalSession } from '@/common/adapter/ipcBridge';
import CopyIconButton from '@/renderer/components/base/CopyIconButton';

type TerminalHoverCardProps = {
  session: ITerminalSession;
};

const statusDotClass = (status: ITerminalSession['last_status']) =>
  status === 'running' ? 'bg-green-500' : status === 'error' ? 'bg-red-500' : 'bg-t-tertiary';

const Field: React.FC<React.PropsWithChildren<{ label: string }>> = ({ label, children }) => (
  <div className='flex flex-col gap-2px'>
    <span className='text-12px text-t-tertiary'>{label}</span>
    {children}
  </div>
);

/**
 * Hover popover for a terminal-session row — the terminal-side mirror of
 * ConversationHoverCard, so both session kinds expose their config the same
 * way: name, full ID (copyable), live status, launch command, and
 * backend/mode. The working path is intentionally omitted here — it lives in
 * the sidebar workpath drawer (display + copy). Reuses CopyIconButton for the
 * elegant copy parity on the ID.
 */
const TerminalHoverCard: React.FC<TerminalHoverCardProps> = ({ session }) => {
  const { t } = useTranslation();

  const statusLabel =
    session.last_status === 'running'
      ? t('terminal.hoverCard.statusRunning')
      : session.last_status === 'error'
        ? t('terminal.hoverCard.statusError')
        : typeof session.exit_code === 'number'
          ? t('terminal.statusExited', { code: session.exit_code })
          : t('terminal.hoverCard.statusExited');

  const fullCommand = [session.command, ...(session.args ?? [])].filter(Boolean).join(' ');
  const backendMode = [session.backend, session.mode].filter(Boolean).join(' · ');

  return (
    <div className='flex flex-col gap-6px py-4px min-w-200px max-w-320px'>
      <Field label={t('terminal.hoverCard.name')}>
        <span className='text-13px text-t-primary break-all'>{session.name || t('terminal.untitled')}</span>
      </Field>

      <Field label={t('terminal.hoverCard.id')}>
        <div className='flex items-center gap-6px'>
          <span className='text-13px text-t-primary break-all font-mono leading-16px'>{session.terminal_id}</span>
          <CopyIconButton text={session.terminal_id} tooltip={t('common.copyFullId')} className='shrink-0 size-18px' />
        </div>
      </Field>

      <Field label={t('terminal.hoverCard.status')}>
        <span className='flex items-center gap-6px text-13px text-t-primary'>
          <span className={classNames('size-6px rd-full shrink-0', statusDotClass(session.last_status))} />
          {statusLabel}
        </span>
      </Field>

      {fullCommand && (
        <Field label={t('terminal.hoverCard.command')}>
          <span className='text-13px text-t-primary break-all font-mono leading-16px'>{fullCommand}</span>
        </Field>
      )}

      {backendMode && (
        <Field label={t('terminal.hoverCard.backend')}>
          <span className='text-13px text-t-primary'>{backendMode}</span>
        </Field>
      )}
    </div>
  );
};

export default TerminalHoverCard;
