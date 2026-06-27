/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import type { IProvider, TProviderWithModel } from '@/common/config/storage';
import { iconColors } from '@/renderer/styles/colors';
import { getModelDisplayLabel } from '@/renderer/utils/model/agentLogo';
import type { AcpModelInfo } from '../types';
import type { GuidModelSelectionMode } from '../hooks/useGuidModelSelection';
import { getAvailableModels } from '../utils/modelUtils';
import { Button, Checkbox, Dropdown, Menu, Tooltip } from '@arco-design/web-react';
import { Brain, Down, Plus, Robot } from '@icon-park/react';
import React from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import { useProvidersQuery } from '@/renderer/hooks/agent/useModelProviderList';

type GuidModelSelectorProps = {
  // Gemini model state
  isGeminiMode: boolean;
  modelList: IProvider[];
  current_model: TProviderWithModel | undefined;
  setCurrentModel: (model: TProviderWithModel) => Promise<void>;

  // Tri-state orchestration selection (single / auto / range). Optional: when
  // omitted (e.g. the scheduled-task dialog reuses this selector) the component
  // behaves as a classic single-select model picker with no segmented control.
  selectionMode?: GuidModelSelectionMode;
  setSelectionMode?: (mode: GuidModelSelectionMode) => void;
  selectedRange?: TProviderWithModel[];
  toggleRangeModel?: (model: TProviderWithModel) => void;

  // ACP model state
  currentAcpCachedModelInfo: AcpModelInfo | null;
  selectedAcpModel: string | null;
  setSelectedAcpModel: React.Dispatch<React.SetStateAction<string | null>>;
};

const GuidModelSelector: React.FC<GuidModelSelectorProps> = ({
  isGeminiMode,
  modelList,
  current_model,
  setCurrentModel,
  selectionMode,
  setSelectionMode,
  selectedRange,
  toggleRangeModel,
  currentAcpCachedModelInfo,
  selectedAcpModel,
  setSelectedAcpModel,
}) => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const defaultModelLabel = t('common.defaultModel');

  // Orchestration tri-state is only active when the host wired the setter (the
  // 会话 entry). Other reuse sites (scheduled-task dialog) leave it single.
  const orchestrationEnabled = typeof setSelectionMode === 'function';
  const effectiveMode: GuidModelSelectionMode = orchestrationEnabled ? (selectionMode ?? 'single') : 'single';
  const effectiveRange = selectedRange ?? [];

  // 获取模型配置数据（包含健康状态）
  const { data: modelConfig } = useProvidersQuery();

  // 过滤掉被禁用的 provider
  const enabledModelList = React.useMemo(() => {
    return modelList.filter((p) => p.enabled !== false);
  }, [modelList]);

  const geminiSelectedLabel = React.useMemo(() => {
    if (!current_model?.use_model) return '';
    return current_model.use_model;
  }, [current_model?.use_model]);

  const geminiButtonLabel = React.useMemo(() => {
    return getModelDisplayLabel({
      selected_value: current_model?.use_model,
      selectedLabel: geminiSelectedLabel,
      defaultModelLabel,
      fallbackLabel: defaultModelLabel,
    });
  }, [current_model?.use_model, defaultModelLabel, geminiSelectedLabel]);

  // The trigger button reflects the active tri-state mode:
  //   single → the selected model name;
  //   auto   → 「主管模型: <model>」 — the lead itself still runs on one model
  //            (workers fan out over every enabled model separately), so we show
  //            that lead model rather than echoing the "自动编排" segmented label;
  //   range  → N 个模型.
  const triStateButtonLabel = React.useMemo(() => {
    if (effectiveMode === 'auto') return t('guid.modelSelector.leadLabel', { model: geminiButtonLabel });
    if (effectiveMode === 'range') return t('guid.modelSelector.rangeLabel', { count: effectiveRange.length });
    return geminiButtonLabel;
  }, [effectiveMode, effectiveRange.length, geminiButtonLabel, t]);

  // The trigger icon: single + auto both point at one concrete model (the lead),
  // so they share the Brain glyph. Range stays Robot. The Robot glyph is reserved
  // for the adjacent orchestration-mode switch so the two controls don't read as
  // duplicates.
  const TriStateIcon = effectiveMode === 'range' ? Robot : Brain;

  const acpSelectedLabel = React.useMemo(() => {
    return (
      currentAcpCachedModelInfo?.available_models?.find((m) => m.id === selectedAcpModel)?.label ||
      currentAcpCachedModelInfo?.current_model_label ||
      currentAcpCachedModelInfo?.current_model_id ||
      ''
    );
  }, [
    currentAcpCachedModelInfo?.available_models,
    currentAcpCachedModelInfo?.current_model_id,
    currentAcpCachedModelInfo?.current_model_label,
    selectedAcpModel,
  ]);

  const acpButtonLabel = React.useMemo(() => {
    return getModelDisplayLabel({
      selected_value: selectedAcpModel || currentAcpCachedModelInfo?.current_model_id,
      selectedLabel: acpSelectedLabel,
      defaultModelLabel,
      fallbackLabel: defaultModelLabel,
    });
  }, [acpSelectedLabel, currentAcpCachedModelInfo?.current_model_id, defaultModelLabel, selectedAcpModel]);

  if (isGeminiMode) {
    const hasModels = !!enabledModelList && enabledModelList.length > 0;
    const rangeKeySet = new Set(effectiveRange.map((m) => m.id + m.use_model));

    // Per-model health dot color (shared by single + range bodies).
    const healthDotColor = (providerId: string, modelName: string): string | null => {
      const matchedProvider = modelConfig?.find((p) => p.id === providerId);
      const healthStatus = matchedProvider?.model_health?.[modelName]?.status || 'unknown';
      if (healthStatus === 'unknown') return null;
      return healthStatus === 'healthy' ? 'bg-green-500' : healthStatus === 'unhealthy' ? 'bg-red-500' : 'bg-gray-400';
    };

    // The orchestration tri-state switch now lives on the visible input-bar
    // toolbar (GuidOrchestrationMode), so it is the single source of truth for
    // `selectionMode`. The dropdown body below stays mode-aware (single menu /
    // range checkboxes / auto hint) driven by that same mode.

    const addModelRow = (
      <div
        role='button'
        tabIndex={0}
        className='flex items-center gap-6px px-12px py-8px text-12px text-t-secondary cursor-pointer hover:bg-2 rounded-4px'
        onClick={() => navigate('/settings/model')}
      >
        <Plus theme='outline' size='12' />
        <span>{t('settings.addModel')}</span>
      </div>
    );

    // Single-select model menu — picks the lead (主管) model. Shared by `single`
    // mode and `auto` mode (in auto it sets the lead's own model while workers
    // fan out over every enabled model).
    const singleSelectMenu = (
      <Menu selectedKeys={current_model ? [current_model.id + current_model.use_model] : []}>
        {[
          ...enabledModelList.map((provider) => {
            const available_models = getAvailableModels(provider);
            if (available_models.length === 0) return null;
            return (
              <Menu.ItemGroup title={provider.name} key={provider.id}>
                {available_models.map((modelName) => {
                  const dot = healthDotColor(provider.id, modelName);
                  return (
                    <Menu.Item
                      key={provider.id + modelName}
                      className={
                        (current_model?.id ?? '') + (current_model?.use_model ?? '') === provider.id + modelName
                          ? '!bg-2'
                          : ''
                      }
                      onClick={() => {
                        setCurrentModel({ ...provider, use_model: modelName }).catch((error) => {
                          console.error('Failed to set current model:', error);
                        });
                      }}
                    >
                      <div className='flex items-center gap-8px w-full'>
                        {dot && <div className={`w-6px h-6px rounded-full shrink-0 ${dot}`} />}
                        <span>{modelName}</span>
                      </div>
                    </Menu.Item>
                  );
                })}
              </Menu.ItemGroup>
            );
          }),
          <Menu.Item key='add-model' className='text-12px text-t-secondary' onClick={() => navigate('/settings/model')}>
            <Plus theme='outline' size='12' />
            {t('settings.addModel')}
          </Menu.Item>,
        ]}
      </Menu>
    );

    let body: React.ReactNode;
    if (!hasModels) {
      // No models configured — same empty + add-model affordance as before.
      body = (
        <div className='py-4px'>
          <div className='px-12px py-12px text-t-secondary text-14px text-center'>{t('settings.noAvailableModels')}</div>
          {addModelRow}
        </div>
      );
    } else if (effectiveMode === 'auto') {
      // Auto mode — pick the lead (主管) model here; a one-line hint explains that
      // workers fan out over every enabled model. This is a single-select picker,
      // NOT the orchestration switch, so it never duplicates the "自动编排" tab.
      body = (
        <div className='py-4px'>
          <div className='px-12px pt-8px pb-6px flex items-start gap-6px'>
            <Robot theme='outline' size='14' fill={iconColors.secondary} className='shrink-0 mt-2px' />
            <span className='text-12px text-t-secondary leading-relaxed'>{t('guid.modelSelector.leadHint')}</span>
          </div>
          {singleSelectMenu}
        </div>
      );
    } else if (effectiveMode === 'range') {
      // Range mode — multi-select checkboxes grouped per provider.
      body = (
        <div className='py-4px max-h-300px overflow-y-auto'>
          {enabledModelList.map((provider) => {
            const available_models = getAvailableModels(provider);
            if (available_models.length === 0) return null;
            return (
              <div key={provider.id} className='mb-2px'>
                <div className='px-12px pt-6px pb-2px text-12px text-t-tertiary'>{provider.name}</div>
                {available_models.map((modelName) => {
                  const checked = rangeKeySet.has(provider.id + modelName);
                  const dot = healthDotColor(provider.id, modelName);
                  return (
                    <div
                      key={provider.id + modelName}
                      role='button'
                      tabIndex={0}
                      className={`flex items-center gap-8px px-12px py-6px mx-4px rounded-4px cursor-pointer hover:bg-2 ${checked ? '!bg-2' : ''}`}
                      onClick={() => toggleRangeModel?.({ ...provider, use_model: modelName })}
                    >
                      <Checkbox checked={checked} onChange={() => toggleRangeModel?.({ ...provider, use_model: modelName })} />
                      {dot && <div className={`w-6px h-6px rounded-full shrink-0 ${dot}`} />}
                      <span className='text-14px text-t-primary truncate'>{modelName}</span>
                    </div>
                  );
                })}
              </div>
            );
          })}
          {addModelRow}
        </div>
      );
    } else {
      // Single mode — the original single-select Arco menu, unchanged behavior.
      body = singleSelectMenu;
    }

    return (
      <Dropdown
        trigger='click'
        droplist={
          <div className='min-w-260px max-w-340px bg-1 rounded-8px overflow-hidden'>
            {body}
          </div>
        }
      >
        <Button className={'sendbox-model-btn guid-config-btn'} shape='round' size='small' data-testid='guid-model-selector'>
          <span className='flex items-center gap-6px min-w-0'>
            <TriStateIcon theme='outline' size='14' fill={iconColors.secondary} className='shrink-0' />
            <span className='truncate'>{triStateButtonLabel}</span>
            <Down theme='outline' size='12' fill={iconColors.secondary} className='shrink-0' />
          </span>
        </Button>
      </Dropdown>
    );
  }

  // ACP cached model selector
  if (currentAcpCachedModelInfo && currentAcpCachedModelInfo.available_models?.length > 0) {
    if (currentAcpCachedModelInfo.available_models.length > 0) {
      return (
        <Dropdown
          trigger='click'
          droplist={
            <Menu selectedKeys={selectedAcpModel ? [selectedAcpModel] : []}>
              {currentAcpCachedModelInfo.available_models.map((model) => {
                // 获取模型健康状态
                const providerConfig = modelConfig?.find((p) => p.platform?.includes(''));
                const healthStatus = providerConfig?.model_health?.[model.id]?.status || 'unknown';
                const healthColor =
                  healthStatus === 'healthy'
                    ? 'bg-green-500'
                    : healthStatus === 'unhealthy'
                      ? 'bg-red-500'
                      : 'bg-gray-400';

                return (
                  <Menu.Item
                    key={model.id}
                    className={model.id === selectedAcpModel ? '!bg-2' : ''}
                    onClick={() => setSelectedAcpModel(model.id)}
                  >
                    <div className='flex items-center gap-8px w-full'>
                      {healthStatus !== 'unknown' && (
                        <div className={`w-6px h-6px rounded-full shrink-0 ${healthColor}`} />
                      )}
                      <span>{model.label}</span>
                    </div>
                  </Menu.Item>
                );
              })}
            </Menu>
          }
        >
          <Button className={'sendbox-model-btn guid-config-btn'} shape='round' size='small'>
            <span className='flex items-center gap-6px min-w-0'>
              <Brain theme='outline' size='14' fill={iconColors.secondary} className='shrink-0' />
              <span>{acpButtonLabel}</span>
              <Down theme='outline' size='12' fill={iconColors.secondary} className='shrink-0' />
            </span>
          </Button>
        </Dropdown>
      );
    }

    return (
      <Tooltip content={t('conversation.welcome.modelSwitchNotSupported')} position='top'>
        <Button
          className={'sendbox-model-btn guid-config-btn'}
          shape='round'
          size='small'
          style={{ cursor: 'default' }}
        >
          <span className='flex items-center gap-6px min-w-0'>
            <Brain theme='outline' size='14' fill={iconColors.secondary} className='shrink-0' />
            <span>{acpButtonLabel}</span>
          </span>
        </Button>
      </Tooltip>
    );
  }

  // Fallback: no model switching
  return (
    <Tooltip content={t('conversation.welcome.modelSwitchNotSupported')} position='top'>
      <Button className={'sendbox-model-btn guid-config-btn'} shape='round' size='small' style={{ cursor: 'default' }}>
        <span className='flex items-center gap-6px min-w-0'>
          <Brain theme='outline' size='14' fill={iconColors.secondary} className='shrink-0' />
          <span>{defaultModelLabel}</span>
        </span>
      </Button>
    </Tooltip>
  );
};

export default GuidModelSelector;
