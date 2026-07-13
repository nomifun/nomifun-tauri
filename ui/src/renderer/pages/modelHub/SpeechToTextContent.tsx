/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { Button, Empty, Form, Input, Switch } from '@arco-design/web-react';
import { DataServer, HeadsetOne, LinkCloud } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import type { IProvider } from '@/common/config/storage';
import type { SpeechToTextConfig, SpeechToTextProvider } from '@/common/types/provider/speech';
import NomiSelect from '@/renderer/components/base/NomiSelect';
import { useProvidersQuery } from '@/renderer/hooks/agent/useModelProviderList';
import { useModelProfiles } from '@/renderer/hooks/agent/useModelProfiles';
import {
  DEFAULT_SPEECH_TO_TEXT_CONFIG,
  getSpeechToTextConfig,
  normalizeSpeechToTextConfig,
  saveSpeechToTextConfig,
  SPEECH_TO_TEXT_CONFIG_CHANGED_EVENT,
} from '@/renderer/services/speechToTextConfig';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { useLocalAsrModels } from './useLocalAsrModels';

type SpeechSourceOption = {
  value: string;
  label: string;
  provider: SpeechToTextProvider;
  providerId?: string;
  model: string;
};

const inferCloudSpeechService = (provider: IProvider, model: string): Exclude<SpeechToTextProvider, 'local'> => {
  const identity = `${provider.platform} ${provider.name} ${provider.base_url} ${model}`.toLowerCase();
  return identity.includes('deepgram') || identity.includes('nova-2') || identity.includes('nova-3')
    ? 'deepgram'
    : 'openai';
};

const SpeechToTextContent: React.FC = () => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [message, messageContext] = useArcoMessage({ maxCount: 2 });
  const [config, setConfig] = useState<SpeechToTextConfig>(DEFAULT_SPEECH_TO_TEXT_CONFIG);
  const { data: providers } = useProvidersQuery();
  const { profiles } = useModelProfiles();
  const localAsr = useLocalAsrModels();

  useEffect(() => {
    const syncConfig = () => setConfig(getSpeechToTextConfig());
    syncConfig();
    window.addEventListener(SPEECH_TO_TEXT_CONFIG_CHANGED_EVENT, syncConfig);
    return () => window.removeEventListener(SPEECH_TO_TEXT_CONFIG_CHANGED_EVENT, syncConfig);
  }, []);

  const localOption = useMemo<SpeechSourceOption | null>(() => {
    const activeModelId = localAsr.status?.activeModelId;
    if (!localAsr.status?.ready || !activeModelId) return null;
    const catalogEntry = localAsr.catalog?.find((entry) => entry.id === activeModelId);
    return {
      value: `local\u0000${activeModelId}`,
      label: `${t('settings.modelHub.speech.local')} · ${catalogEntry?.name ?? activeModelId}`,
      provider: 'local',
      model: activeModelId,
    };
  }, [localAsr.catalog, localAsr.status?.activeModelId, localAsr.status?.ready, t]);

  const cloudOptions = useMemo<SpeechSourceOption[]>(() => {
    const profileKeys = new Set(
      profiles
        .filter((profile) => profile.tasks.includes('speech_recognition'))
        .map((profile) => `${profile.provider_id}\u0000${profile.model}`)
    );

    return (providers ?? [])
      .filter((provider) => provider.enabled !== false && provider.api_key.trim().length > 0)
      .flatMap((provider) =>
        (provider.models ?? [])
          .filter((model) => provider.model_enabled?.[model] !== false)
          .filter((model) => {
            if (profileKeys.has(`${provider.id}\u0000${model}`)) return true;
            const name = model.toLowerCase();
            return (
              name.includes('whisper') ||
              name.includes('transcrib') ||
              name.includes('speech-to-text') ||
              name.includes('asr') ||
              name.includes('nova-2') ||
              name.includes('nova-3')
            );
          })
          .map((model) => ({
            value: `cloud\u0000${provider.id}\u0000${model}`,
            label: `${provider.name} · ${model}`,
            provider: inferCloudSpeechService(provider, model),
            providerId: provider.id,
            model,
          }))
      );
  }, [profiles, providers]);

  const sourceOptions = useMemo(
    () => (localOption ? [localOption, ...cloudOptions] : cloudOptions),
    [cloudOptions, localOption]
  );

  const selectedSource = useMemo(() => {
    if (config.provider === 'local') {
      return localOption?.value;
    }
    return cloudOptions.find(
      (option) => option.providerId === config.provider_id && option.model === config.model
    )?.value;
  }, [cloudOptions, config.model, config.provider, config.provider_id, localOption?.value]);

  const persist = useCallback(
    (next: SpeechToTextConfig) => {
      const normalized = normalizeSpeechToTextConfig(next);
      setConfig(normalized);
      void saveSpeechToTextConfig(normalized).catch((error) => {
        console.error('Failed to save speech-to-text config:', error);
        setConfig(getSpeechToTextConfig());
        message.error(error instanceof Error ? error.message : t('settings.saveModelConfigFailed'));
      });
    },
    [message, t]
  );

  const selectSource = useCallback(
    (value: string) => {
      const option = sourceOptions.find((candidate) => candidate.value === value);
      if (!option) return;
      persist({
        ...config,
        enabled: true,
        provider: option.provider,
        provider_id: option.providerId,
        model: option.model,
      });
    },
    [config, persist, sourceOptions]
  );

  return (
    <div className='flex min-h-0 flex-col rd-16px bg-2 px-24px py-16px'>
      {messageContext}
      <header className='flex items-center gap-9px border-b border-[var(--color-border-2)] pb-14px'>
        <span className='size-30px shrink-0 flex items-center justify-center rd-9px bg-primary-1 text-primary-6'>
          <HeadsetOne theme='outline' size='18' strokeWidth={3} />
        </span>
        <div className='min-w-0'>
          <h2 className='m-0 text-20px font-650 leading-28px text-t-primary'>
            {t('settings.modelHub.speech.title')}
          </h2>
          <p className='m-0 mt-2px text-12px leading-18px text-t-secondary'>
            {t('settings.modelHub.speech.subtitle')}
          </p>
        </div>
      </header>

      {sourceOptions.length === 0 ? (
        <div className='py-42px'>
          <Empty
            icon={<HeadsetOne theme='outline' size='42' className='text-t-tertiary' />}
            description={t('settings.modelHub.speech.noSources')}
          />
          <div className='mt-14px flex items-center justify-center gap-8px flex-wrap'>
            <Button
              icon={<DataServer theme='outline' size='14' />}
              onClick={() => navigate('/models?section=local&capability=speech_recognition')}
            >
              {t('settings.modelHub.speech.manageLocal')}
            </Button>
            <Button icon={<LinkCloud theme='outline' size='14' />} onClick={() => navigate('/models?section=models')}>
              {t('settings.modelHub.speech.manageProviders')}
            </Button>
          </div>
        </div>
      ) : (
        <>
          <Form layout='vertical' className='mt-18px'>
            <Form.Item label={t('settings.modelHub.speech.source')}>
              <NomiSelect value={selectedSource} onChange={selectSource}>
                {localOption && (
                  <NomiSelect.OptGroup label={t('settings.modelHub.speech.local')}>
                    <NomiSelect.Option value={localOption.value}>{localOption.label}</NomiSelect.Option>
                  </NomiSelect.OptGroup>
                )}
                {cloudOptions.length > 0 && (
                  <NomiSelect.OptGroup label={t('settings.modelHub.speech.cloud')}>
                    {cloudOptions.map((option) => (
                      <NomiSelect.Option key={option.value} value={option.value}>
                        {option.label}
                      </NomiSelect.Option>
                    ))}
                  </NomiSelect.OptGroup>
                )}
              </NomiSelect>
            </Form.Item>
            <Form.Item label={t('settings.modelHub.speech.defaultLanguage')}>
              <Input
                value={config.language}
                placeholder={t('settings.modelHub.speech.languagePlaceholder')}
                onBlur={() => persist(config)}
                onChange={(language) => setConfig((current) => ({ ...current, language }))}
              />
            </Form.Item>
            <Form.Item label={t('settings.modelHub.speech.enabled')}>
              <Switch
                checked={config.enabled && Boolean(selectedSource)}
                disabled={!selectedSource}
                onChange={(enabled) => persist({ ...config, enabled })}
              />
            </Form.Item>
          </Form>

          <div className='mt-6px flex items-center gap-8px flex-wrap'>
            <Button
              type='text'
              size='small'
              icon={<DataServer theme='outline' size='14' />}
              onClick={() => navigate('/models?section=local&capability=speech_recognition')}
            >
              {t('settings.modelHub.speech.manageLocal')}
            </Button>
            <Button
              type='text'
              size='small'
              icon={<LinkCloud theme='outline' size='14' />}
              onClick={() => navigate('/models?section=models')}
            >
              {t('settings.modelHub.speech.manageProviders')}
            </Button>
          </div>
        </>
      )}
    </div>
  );
};

export default SpeechToTextContent;
