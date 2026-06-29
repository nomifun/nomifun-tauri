/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import { Button, Dropdown, Menu } from '@arco-design/web-react';
import { Brain, Down, Plus } from '@icon-park/react';
import { configService } from '@/common/config/configService';
import { useConfig } from '@/renderer/hooks/config/useConfig';
import { iconColors } from '@/renderer/styles/colors';
import { useModelProviderList } from '@/renderer/hooks/agent/useModelProviderList';
import { useProvidersQuery } from '@/renderer/hooks/agent/useModelProviderList';

/**
 * A picked provider+model pair for the knowledge AI generators, or `null` to
 * mean "let the backend fall back to its own default completer". The two fields
 * are always sent together (or neither) — the backend rejects a half-specified
 * pair with 400.
 */
export type KnowledgeModelChoice = { provider_id: string; model: string } | null;

const STORAGE_KEY = 'knowledge.autogenModel';

/**
 * Persisted-default selection for the knowledge-base AI description/overview
 * generators. Reads/writes `knowledge.autogenModel`; an absent or now-invalid
 * stored pair resolves to `null` (backend default). Exposes the choice plus a
 * setter so callers can hand the pair straight to the three knowledge invokes.
 */
export function useKnowledgeAutogenModel() {
  const { providers, getAvailableModels } = useModelProviderList();

  // Read reactively (useSyncExternalStore subscription), NOT a one-shot
  // configService.get(): setChoice writes via set/remove, which notify
  // subscribers — without subscribing, the selector kept showing the old label
  // ("默认模型") until the modal remounted ("点击切换模型没有任何反应").
  const [stored] = useConfig(STORAGE_KEY);

  // A stored pair is only honoured while the provider is still enabled and the
  // model still available; otherwise we fall back to the backend default so a
  // deleted/disabled provider can never pin a broken selection.
  const choice = useMemo<KnowledgeModelChoice>(() => {
    if (!stored?.provider_id || !stored.model) return null;
    const provider = providers.find((p) => p.id === stored.provider_id);
    if (!provider) return null;
    if (!getAvailableModels(provider).includes(stored.model)) return null;
    return { provider_id: stored.provider_id, model: stored.model };
  }, [stored?.provider_id, stored?.model, providers, getAvailableModels]);

  const setChoice = useCallback(async (next: KnowledgeModelChoice) => {
    if (next) {
      await configService.set(STORAGE_KEY, { provider_id: next.provider_id, model: next.model });
    } else {
      await configService.remove(STORAGE_KEY);
    }
  }, []);

  return { choice, setChoice };
}

type KnowledgeModelSelectorProps = {
  choice: KnowledgeModelChoice;
  onChange: (choice: KnowledgeModelChoice) => void;
  /** Match the surrounding AI buttons (mini in the form label, small in headers). */
  size?: 'mini' | 'small';
  disabled?: boolean;
};

/**
 * Compact provider+model dropdown sitting next to the knowledge AI buttons.
 * Selecting a model persists it as the default; "Default Model" clears the
 * override so the backend picks. Mirrors GuidModelSelector's look (Arco Button
 * trigger — never a raw <button>, which leaks a WebView2 black border here).
 */
const KnowledgeModelSelector: React.FC<KnowledgeModelSelectorProps> = ({
  choice,
  onChange,
  size = 'mini',
  disabled,
}) => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { providers, getAvailableModels } = useModelProviderList();
  const { data: modelConfig } = useProvidersQuery();

  const defaultLabel = t('common.defaultModel');
  const buttonLabel = choice ? choice.model : defaultLabel;

  const droplist = (
    <Menu selectedKeys={choice ? [`${choice.provider_id}:${choice.model}`] : ['__default__']}>
      <Menu.Item key='__default__' onClick={() => onChange(null)}>
        {defaultLabel}
      </Menu.Item>
      {providers.length === 0
        ? [
            <Menu.Item
              key='add-model'
              className='text-12px text-t-secondary'
              onClick={() => navigate('/settings/model')}
            >
              <Plus theme='outline' size='12' />
              {t('settings.addModel')}
            </Menu.Item>,
          ]
        : providers.map((provider) => {
            const models = getAvailableModels(provider);
            if (models.length === 0) return null;
            return (
              <Menu.ItemGroup title={provider.name} key={provider.id}>
                {models.map((modelName) => {
                  const matched = modelConfig?.find((p) => p.id === provider.id);
                  const healthStatus = matched?.model_health?.[modelName]?.status || 'unknown';
                  const healthColor =
                    healthStatus === 'healthy'
                      ? 'bg-green-500'
                      : healthStatus === 'unhealthy'
                        ? 'bg-red-500'
                        : 'bg-gray-400';
                  return (
                    <Menu.Item
                      key={`${provider.id}:${modelName}`}
                      onClick={() => onChange({ provider_id: provider.id, model: modelName })}
                    >
                      <div className='flex items-center gap-8px w-full'>
                        {healthStatus !== 'unknown' && (
                          <div className={`w-6px h-6px rounded-full shrink-0 ${healthColor}`} />
                        )}
                        <span>{modelName}</span>
                      </div>
                    </Menu.Item>
                  );
                })}
              </Menu.ItemGroup>
            );
          })}
    </Menu>
  );

  return (
    <Dropdown trigger='click' droplist={droplist} disabled={disabled}>
      <Button size={size} type='text' disabled={disabled} title={t('knowledge.form.modelSelectTooltip')}>
        <span className='flex items-center gap-4px min-w-0 max-w-160px'>
          <Brain theme='outline' size='12' fill={iconColors.secondary} className='shrink-0' />
          <span className='truncate'>{buttonLabel}</span>
          <Down theme='outline' size='10' fill={iconColors.secondary} className='shrink-0' />
        </span>
      </Button>
    </Dropdown>
  );
};

export default KnowledgeModelSelector;
