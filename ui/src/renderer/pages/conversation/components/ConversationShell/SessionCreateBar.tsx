/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import { ExpandLeft, FolderPlus, ListCheckbox, Plus, Terminal } from '@icon-park/react';
import classNames from 'classnames';
import InstantHoverTooltip from '@renderer/components/base/InstantHoverTooltip';
import ConversationSearchPopover from '@renderer/pages/conversation/SessionList/ConversationSearchPopover';
import type { SidebarDisplayPreferences, SidebarDisplayPreset } from '@renderer/pages/conversation/SessionList/utils/sidebarDisplayPreferences';
import SessionDisplaySettingsPopover from './SessionDisplaySettingsPopover';

export interface SessionCreateBarProps {
  batchMode: boolean;
  onToggleBatchMode: () => void;
  onNewChat: () => void;
  onNewTerminal: () => void;
  onCreateProject: () => void;
  displayPreferences: SidebarDisplayPreferences;
  onDisplayPresetChange: (preset: Exclude<SidebarDisplayPreset, 'custom'>) => void;
  onDisplayPreferenceChange: (patch: Partial<Omit<SidebarDisplayPreferences, 'preset'>>) => void;
  /** Collapse the secondary sidebar. The stable primary toggle lives in the titlebar. */
  onCollapse: () => void;
  /** Mobile-only: close the overlay when a session is chosen from search. */
  onSessionClick?: () => void;
  /** Clear batch mode / close preview when a search result is opened. */
  onConversationSelect: () => void;
}

/**
 * SessionCreateBar — the toolbar at the top of the session secondary sidebar
 * ({@link ContentSider}). Carries the primary create CTAs (new conversation /
 * new terminal), project creation, batch selection, search, display settings,
 * and an in-panel collapse shortcut.
 *
 * The four session actions share one compact 2x2 grid. Search is deliberately
 * below the action group so creation/selection controls read as one coherent
 * command cluster before the user scans existing sessions.
 */
const SessionCreateBar: React.FC<SessionCreateBarProps> = ({
  batchMode,
  onToggleBatchMode,
  onNewChat,
  onNewTerminal,
  onCreateProject,
  displayPreferences,
  onDisplayPresetChange,
  onDisplayPreferenceChange,
  onCollapse,
  onSessionClick,
  onConversationSelect,
}) => {
  const { t } = useTranslation();
  const actionButtonClassName =
    'flex-1 basis-0 min-w-0 h-34px px-9px rd-8px border border-solid outline-none flex items-center justify-center gap-6px text-13px font-[500] leading-none transition-colors focus:outline-none focus-visible:shadow-[0_0_0_3px_rgba(var(--primary-6),0.12)]';
  const restingButtonClassName =
    'cursor-pointer bg-transparent border-[var(--color-border-2)] text-t-primary hover:bg-fill-3 active:bg-fill-4';
  const batchToggleLabel = t(batchMode ? 'sessionList.exitBatchSelect' : 'sessionList.batchSelect');

  return (
    <div className='shrink-0 px-10px pt-12px pb-8px flex flex-col gap-8px'>
      {/* Title strip + secondary actions */}
      <div className='flex items-center h-22px px-2px select-none'>
        <span className='text-13px text-t-tertiary font-[500] leading-none tracking-wide'>
          {t('sessionList.title')}
        </span>
        <div className='ml-auto flex items-center gap-2px'>
          <SessionDisplaySettingsPopover
            preferences={displayPreferences}
            onPresetChange={onDisplayPresetChange}
            onPreferenceChange={onDisplayPreferenceChange}
          />
          <InstantHoverTooltip content={t('sessionList.collapseList')} position='bottom'>
            <div
              data-testid='session-sider-collapse'
              className='size-22px rd-6px flex items-center justify-center cursor-pointer shrink-0 transition-colors text-t-secondary hover:text-t-primary hover:bg-fill-3'
              onClick={onCollapse}
              aria-label={t('sessionList.collapseList')}
            >
              <ExpandLeft
                theme='outline'
                size='15'
                fill='currentColor'
                className='block leading-none shrink-0'
                style={{ lineHeight: 0 }}
              />
            </div>
          </InstantHoverTooltip>
        </div>
      </div>

      <div data-testid='session-action-grid' className='grid grid-cols-2 gap-6px'>
        <button
          type='button'
          data-testid='session-new-conversation-entry'
          className={classNames(actionButtonClassName, restingButtonClassName)}
          onClick={onNewChat}
        >
          <Plus
            theme='outline'
            size='15'
            fill='currentColor'
            className='block leading-none shrink-0'
            style={{ lineHeight: 0 }}
          />
          <span className='truncate min-w-0'>{t('terminal.newConversation')}</span>
        </button>
        <button
          type='button'
          data-testid='session-new-terminal-entry'
          className={classNames(actionButtonClassName, restingButtonClassName)}
          onClick={onNewTerminal}
        >
          <Terminal
            theme='outline'
            size='15'
            fill='currentColor'
            className='block leading-none shrink-0'
            style={{ lineHeight: 0 }}
          />
          <span className='truncate min-w-0'>{t('terminal.newTerminal')}</span>
        </button>
        <button
          type='button'
          data-testid='workpath-create-project-btn'
          className={classNames(actionButtonClassName, restingButtonClassName)}
          onClick={onCreateProject}
        >
          <FolderPlus theme='outline' size='15' fill='currentColor' className='block leading-none shrink-0' />
          <span className='truncate min-w-0'>{t('sessionList.createProject')}</span>
        </button>
        <button
          type='button'
          data-testid='workpath-batch-select-btn'
          className={classNames(
            actionButtonClassName,
            batchMode
              ? 'cursor-pointer bg-[rgba(var(--primary-6),0.1)] border-[rgba(var(--primary-6),0.28)] text-primary hover:bg-[rgba(var(--primary-6),0.14)]'
              : restingButtonClassName
          )}
          onClick={onToggleBatchMode}
          aria-pressed={batchMode}
        >
          <ListCheckbox theme='outline' size='15' fill='currentColor' className='block leading-none shrink-0' />
          <span className='truncate min-w-0'>{batchToggleLabel}</span>
        </button>
      </div>

      <div className='w-full'>
        <ConversationSearchPopover
          onSessionClick={onSessionClick}
          onConversationSelect={onConversationSelect}
          label={t('conversation.historySearch.shortTitle')}
          fullWidth
        />
      </div>
    </div>
  );
};

export default SessionCreateBar;
