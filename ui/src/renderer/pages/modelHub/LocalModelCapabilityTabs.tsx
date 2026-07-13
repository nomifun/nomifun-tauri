/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { Tooltip } from '@arco-design/web-react';
import { DataServer, HeadsetOne, Pic, VolumeNotice } from '@icon-park/react';
import classNames from 'classnames';
import { useTranslation } from 'react-i18next';
import {
  LOCAL_MODEL_CAPABILITIES,
  type CapabilityActivity,
  type LocalModelCapabilityKey,
} from './localModelCapabilityView';

export interface LocalModelCapabilityTabsProps {
  className?: string;
  activeKey: LocalModelCapabilityKey;
  activity: Partial<Record<LocalModelCapabilityKey, CapabilityActivity>>;
  onChange: (key: LocalModelCapabilityKey) => void;
}

const CapabilityIcon: React.FC<{ capability: LocalModelCapabilityKey }> = ({ capability }) => {
  const iconProps = { theme: 'outline' as const, size: '15', strokeWidth: 3 };
  switch (capability) {
    case 'text':
      return <DataServer {...iconProps} />;
    case 'image':
      return <Pic {...iconProps} />;
    case 'speech_recognition':
      return <HeadsetOne {...iconProps} />;
    case 'speech_synthesis':
      return <VolumeNotice {...iconProps} />;
  }
};

const LocalModelCapabilityTabs: React.FC<LocalModelCapabilityTabsProps> = ({
  activeKey,
  activity,
  className,
  onChange,
}) => {
  const { t } = useTranslation();
  const labels: Record<LocalModelCapabilityKey, string> = {
    text: t('settings.modelHub.local.capabilityCenter.tabs.text'),
    image: t('settings.modelHub.local.capabilityCenter.tabs.image'),
    speech_recognition: t('settings.modelHub.local.capabilityCenter.tabs.speechRecognition'),
    speech_synthesis: t('settings.modelHub.local.capabilityCenter.tabs.speechSynthesis'),
  };

  return (
    <div
      role='tablist'
      aria-label={t('settings.modelHub.local.title')}
      className={classNames(
        'flex max-w-full items-center gap-3px overflow-x-auto rd-12px bg-[var(--color-fill-2)] p-4px scrollbar-hide',
        className
      )}
    >
      {LOCAL_MODEL_CAPABILITIES.map((capability) => {
        const active = activeKey === capability.key;
        const tabActivity = activity[capability.key] ?? 'idle';
        const tab = (
          <button
            key={capability.key}
            type='button'
            role='tab'
            aria-selected={active}
            aria-disabled={!capability.implemented}
            disabled={!capability.implemented}
            onClick={() => capability.implemented && onChange(capability.key)}
            className={classNames(
              'group relative h-36px shrink-0 border-none outline-none flex items-center justify-center gap-6px rd-9px px-12px text-13px font-500 transition-all duration-180',
              capability.implemented ? 'cursor-pointer' : 'cursor-not-allowed opacity-55',
              active
                ? '!bg-[var(--color-bg-2)] text-primary-6 shadow-[0_1px_2px_rgba(0,0,0,0.04),0_3px_10px_rgba(0,0,0,0.06)]'
                : 'bg-transparent text-t-secondary hover:text-t-primary'
            )}
          >
            <span className={classNames('flex items-center', active ? 'text-primary-6' : 'text-t-tertiary')}>
              <CapabilityIcon capability={capability.key} />
            </span>
            <span className='whitespace-nowrap'>{labels[capability.key]}</span>
            {!capability.implemented && (
              <span className='rd-100px bg-[var(--color-fill-3)] px-5px py-2px text-9px leading-none text-t-tertiary'>
                {t('settings.modelHub.local.capabilityCenter.planned')}
              </span>
            )}
            {capability.implemented && tabActivity !== 'idle' && (
              <span
                aria-label={t(
                  tabActivity === 'error'
                    ? 'settings.modelHub.local.capabilityCenter.needsAttention'
                    : 'settings.modelHub.local.capabilityCenter.backgroundRunning'
                )}
                className={classNames(
                  'size-6px rd-full',
                  tabActivity === 'error'
                    ? 'bg-[rgb(var(--danger-6))]'
                    : 'bg-[rgb(var(--primary-6))] animate-pulse'
                )}
              />
            )}
          </button>
        );

        return capability.implemented ? (
          <React.Fragment key={capability.key}>{tab}</React.Fragment>
        ) : (
          <Tooltip key={capability.key} content={t('settings.modelHub.local.capabilityCenter.plannedHint')}>
            <span className='inline-flex'>{tab}</span>
          </Tooltip>
        );
      })}
    </div>
  );
};

export default LocalModelCapabilityTabs;
