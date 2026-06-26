/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import { Popover, Switch } from '@arco-design/web-react';
import { SettingTwo } from '@icon-park/react';
import classNames from 'classnames';

import InstantHoverTooltip from '@renderer/components/base/InstantHoverTooltip';
import type {
  SidebarDisplayPreferences,
  SidebarDisplayPreset,
  WorkpathNameMode,
} from '@renderer/pages/conversation/SessionList/utils/sidebarDisplayPreferences';

type PresetOption = Exclude<SidebarDisplayPreset, 'custom'>;

export interface SessionDisplaySettingsPopoverProps {
  preferences: SidebarDisplayPreferences;
  onPresetChange: (preset: PresetOption) => void;
  onPreferenceChange: (patch: Partial<Omit<SidebarDisplayPreferences, 'preset'>>) => void;
}

const PRESET_OPTIONS: PresetOption[] = ['compact', 'balanced', 'detailed'];
const WORKPATH_NAME_OPTIONS: WorkpathNameMode[] = ['compressed', 'folder', 'folderWithPath', 'full'];

const SessionDisplaySettingsPopover: React.FC<SessionDisplaySettingsPopoverProps> = ({
  preferences,
  onPresetChange,
  onPreferenceChange,
}) => {
  const { t } = useTranslation();

  const presetLabel = (preset: PresetOption) =>
    preset === 'compact'
      ? t('sessionList.displayPresetCompact')
      : preset === 'balanced'
        ? t('sessionList.displayPresetBalanced')
        : t('sessionList.displayPresetDetailed');

  const workpathLabel = (mode: WorkpathNameMode) =>
    mode === 'compressed'
      ? t('sessionList.workpathNameCompressed')
      : mode === 'folder'
        ? t('sessionList.workpathNameFolder')
        : mode === 'folderWithPath'
          ? t('sessionList.workpathNameFolderWithPath')
          : t('sessionList.workpathNameFull');

  const optionClassName = (active: boolean) =>
    classNames(
      'h-28px min-w-0 px-8px rd-6px border border-solid text-12px leading-none cursor-pointer transition-colors truncate',
      active
        ? 'bg-[rgba(var(--primary-6),0.1)] border-[rgba(var(--primary-6),0.32)] text-primary'
        : 'bg-transparent border-[var(--color-border-2)] text-t-secondary hover:bg-fill-3 hover:text-t-primary'
    );

  const content = (
    <div className='w-270px p-12px flex flex-col gap-12px'>
      <div className='flex items-center justify-between gap-8px'>
        <span className='text-14px font-[500] text-t-primary'>{t('sessionList.displaySettingsTitle')}</span>
        {preferences.preset === 'custom' && (
          <span className='text-11px text-t-tertiary shrink-0'>{t('sessionList.displayPresetCustom')}</span>
        )}
      </div>

      <div className='flex flex-col gap-6px'>
        <span className='text-12px text-t-tertiary'>{t('sessionList.displayPreset')}</span>
        <div className='grid grid-cols-3 gap-6px'>
          {PRESET_OPTIONS.map((preset) => (
            <button
              key={preset}
              type='button'
              className={optionClassName(preferences.preset === preset)}
              onClick={() => onPresetChange(preset)}
            >
              {presetLabel(preset)}
            </button>
          ))}
        </div>
      </div>

      <div className='flex flex-col gap-6px'>
        <span className='text-12px text-t-tertiary'>{t('sessionList.workpathNameMode')}</span>
        <div className='grid grid-cols-2 gap-6px'>
          {WORKPATH_NAME_OPTIONS.map((mode) => (
            <button
              key={mode}
              type='button'
              className={optionClassName(preferences.workpathNameMode === mode)}
              onClick={() => onPreferenceChange({ workpathNameMode: mode })}
            >
              {workpathLabel(mode)}
            </button>
          ))}
        </div>
      </div>

      <div className='flex items-center justify-between gap-12px'>
        <span className='text-13px text-t-secondary'>{t('sessionList.showGitBranch')}</span>
        <Switch
          size='small'
          checked={preferences.showGitBranch}
          onChange={(checked: boolean) => onPreferenceChange({ showGitBranch: checked })}
        />
      </div>

      <div className='flex items-center justify-between gap-12px'>
        <span className='text-13px text-t-secondary'>{t('sessionList.showSessionAge')}</span>
        <Switch
          size='small'
          checked={preferences.sessionMetaMode === 'age'}
          onChange={(checked: boolean) => onPreferenceChange({ sessionMetaMode: checked ? 'age' : 'none' })}
        />
      </div>
    </div>
  );

  return (
    <Popover
      trigger='click'
      position='br'
      content={content}
      getPopupContainer={() => document.body}
      unmountOnExit={false}
    >
      <span className='inline-flex'>
        <InstantHoverTooltip content={t('sessionList.displaySettings')} position='bottom'>
          <button
            type='button'
            data-testid='session-display-settings-btn'
            className='size-22px rd-4px flex items-center justify-center cursor-pointer shrink-0 transition-colors text-t-secondary hover:text-t-primary hover:bg-fill-4 bg-transparent border-none outline-none focus:outline-none focus-visible:outline-none'
            aria-label={t('sessionList.displaySettings')}
          >
            <SettingTwo theme='outline' size='14' fill='currentColor' className='block leading-none' />
          </button>
        </InstantHoverTooltip>
      </span>
    </Popover>
  );
};

export default SessionDisplaySettingsPopover;
