/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { Button, Tooltip } from '@arco-design/web-react';
import { DataServer, Refresh } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';
import NomiScrollArea from '@/renderer/components/base/NomiScrollArea';
import { useSettingsViewMode } from '@/renderer/components/settings/SettingsModal/settingsViewContext';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import AsrModelsPanel from './AsrModelsPanel';
import ImageModelsPanel from './ImageModelsPanel';
import LocalModelCapabilityTabs from './LocalModelCapabilityTabs';
import TextModelsPanel from './TextModelsPanel';
import {
  capabilityActivity,
  type CapabilityActivity,
  type LocalModelCapabilityKey,
  type ModelTransferPhase,
} from './localModelCapabilityView';
import { useLocalImageModels } from './useLocalImageModels';
import { useLocalAsrModels } from './useLocalAsrModels';
import { useLocalModels } from './useLocalModels';

const LocalModelsContent: React.FC = () => {
  const { t } = useTranslation();
  const [searchParams] = useSearchParams();
  const viewMode = useSettingsViewMode();
  const [message, messageContext] = useArcoMessage();
  const [activeCapability, setActiveCapability] = useState<LocalModelCapabilityKey>('text');
  const text = useLocalModels();
  const image = useLocalImageModels();
  const asr = useLocalAsrModels();

  useEffect(() => {
    const capability = searchParams.get('capability');
    if (capability === 'speech_recognition') {
      setActiveCapability('speech_recognition');
    }
  }, [searchParams]);

  const activity: Partial<Record<LocalModelCapabilityKey, CapabilityActivity>> = {
    text: capabilityActivity(
      (text.status?.models.map((model) => model.installPhase) ?? []) as ModelTransferPhase[],
      Boolean(text.statusError || text.status?.lastError)
    ),
    image: capabilityActivity(
      (image.status?.models.map((model) => model.installPhase) ?? []) as ModelTransferPhase[],
      Boolean(image.statusError || image.status?.lastError)
    ),
    speech_recognition: capabilityActivity(
      (asr.status?.models.map((model) => model.installPhase) ?? []) as ModelTransferPhase[],
      Boolean(asr.statusError || asr.status?.lastError)
    ),
  };

  const activeController =
    activeCapability === 'image' ? image : activeCapability === 'speech_recognition' ? asr : text;

  const refreshActiveCapability = (): void => {
    void activeController.refresh().catch((error) => {
      console.error(`Local ${activeCapability} model refresh failed:`, error);
      message.error(t('settings.modelHub.local.loadFailed'));
    });
  };

  return (
    <div className='flex min-h-0 flex-col rd-16px bg-2 px-24px py-16px'>
      {messageContext}
      <header className='flex-shrink-0'>
        <div className='flex items-center justify-between gap-12px flex-wrap'>
          <div className='flex min-w-0 items-center gap-9px'>
            <span className='size-30px shrink-0 flex items-center justify-center rd-9px bg-primary-1 text-primary-6'>
              <DataServer theme='outline' size='18' strokeWidth={3} />
            </span>
            <div className='min-w-0'>
              <h2 className='m-0 text-20px font-650 leading-28px text-t-primary'>
                {t('settings.modelHub.local.title')}
              </h2>
              <p className='m-0 mt-2px text-12px leading-18px text-t-secondary'>
                {t('settings.modelHub.local.capabilityCenter.subtitle')}
              </p>
            </div>
          </div>
          <Tooltip content={t('settings.modelHub.local.refreshHint')}>
            <Button
              size='small'
              type='secondary'
              shape='round'
              icon={<Refresh theme='outline' size='14' />}
              loading={activeController.isLoading}
              disabled={activeController.pendingAction != null}
              onClick={refreshActiveCapability}
            >
              {t('settings.modelHub.local.refresh')}
            </Button>
          </Tooltip>
        </div>

        <LocalModelCapabilityTabs
          className='mt-14px'
          activeKey={activeCapability}
          activity={activity}
          onChange={setActiveCapability}
        />
      </header>

      <NomiScrollArea className='mt-14px flex-1 min-h-0' disableOverflow={viewMode === 'page'}>
        <div role='tabpanel' aria-label={t('settings.modelHub.local.capabilityCenter.tabs.text')} hidden={activeCapability !== 'text'}>
          <TextModelsPanel controller={text} />
        </div>
        <div role='tabpanel' aria-label={t('settings.modelHub.local.capabilityCenter.tabs.image')} hidden={activeCapability !== 'image'}>
          <ImageModelsPanel controller={image} />
        </div>
        <div
          role='tabpanel'
          aria-label={t('settings.modelHub.local.capabilityCenter.tabs.speechRecognition')}
          hidden={activeCapability !== 'speech_recognition'}
        >
          <AsrModelsPanel controller={asr} />
        </div>
      </NomiScrollArea>
    </div>
  );
};

export default LocalModelsContent;
