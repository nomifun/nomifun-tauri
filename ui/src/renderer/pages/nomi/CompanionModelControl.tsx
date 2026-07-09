/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { Select, Tooltip } from '@arco-design/web-react';
import { useModelProviderList, useProvidersQuery } from '@renderer/hooks/agent/useModelProviderList';
import type { useCompanion } from './useNomi';

interface Props {
  /** 伙伴 profile + 乐观 patch 通道。 */
  companion: ReturnType<typeof useCompanion>;
}

/**
 * 桌面伙伴对话模型的【唯一】配置入口（紧凑内联，置于「对话」会话头部与总览）。
 *
 * 写入 profile.model —— 全局唯一事实源：本地专属会话与远程连接(IM 机器人)都跟随此模型，
 * 切换后所有会话即时跟随（后端 service.patch_companion 会同步会话行并清空渠道会话）。
 *
 * 供应商下拉列出【所有已启用的供应商】（不再按「是否含主模型」过滤），这样：
 *   - 用户始终能看到自己配置的供应商，当前选择也能正常显示名字（而非生 provider id）；
 *   - 只有图像/视频/嵌入等生成类模型的供应商也可见，其模型下拉为空并给出说明。
 * 模型下拉只列出可作对话主模型的文本模型（图像等生成类模型经 excludeFromPrimary 排除，
 * 不能作对话模型）。当前存储的模型若已不在该供应商的可用列表里（供应商改配后失效），
 * 会以「(不可用)」禁用项显式呈现并给出重选提示，避免出现无法解释的残留值。
 */
const CompanionModelControl: React.FC<Props> = ({ companion }) => {
  const { t } = useTranslation();
  const { profile, patchCompanion } = companion;
  const { getAvailableModels } = useModelProviderList();
  const { data: rawProviders } = useProvidersQuery();

  // 所有已启用供应商（默认启用）。不按可用模型过滤，保证用户能看到自己的供应商，
  // 且已存储的当前供应商能被映射为名字（此前会被过滤掉而显示成生 id）。
  const enabledProviders = useMemo(() => (rawProviders ?? []).filter((p) => p.enabled !== false), [rawProviders]);

  const currentProvider = useMemo(
    () => enabledProviders.find((p) => p.id === profile?.model.provider_id),
    [enabledProviders, profile?.model.provider_id]
  );

  const availableModels = useMemo(
    () => (currentProvider ? getAvailableModels(currentProvider) : []),
    [currentProvider, getAvailableModels]
  );

  // 是否至少有一个供应商提供可用于对话的文本模型。
  const anyChatModel = useMemo(
    () => enabledProviders.some((p) => getAvailableModels(p).length > 0),
    [enabledProviders, getAvailableModels]
  );

  if (!profile) return null;

  const providerId = profile.model.provider_id;
  const selectedModel = profile.model.model;
  // 当前模型仅在其确实出现在该供应商的可用列表里时才算「有效」。
  const modelValid = Boolean(selectedModel && availableModels.includes(selectedModel));
  // 已存储的供应商 id 不在启用列表里（供应商被删）→ 供应商本身也已失效。
  const providerStale = Boolean(providerId) && !currentProvider;
  const configured = Boolean(providerId) && !providerStale && modelValid;

  // 当前模型已失效（供应商仍在，但模型已不在其可用列表）→ 作为禁用项显式呈现。
  const showStaleModel = Boolean(selectedModel) && Boolean(currentProvider) && !modelValid;

  const hint = !anyChatModel
    ? t('nomi.chat.modelNoTextModel')
    : providerStale
      ? t('nomi.chat.modelStale', { model: selectedModel })
      : currentProvider && availableModels.length === 0
        ? t('nomi.chat.modelProviderNoChat')
        : showStaleModel
          ? t('nomi.chat.modelStale', { model: selectedModel })
          : '';

  return (
    <div className='flex flex-col gap-6px'>
      <div className='flex items-center gap-6px flex-wrap'>
        <Tooltip content={t('nomi.chat.modelConfigHint')}>
          <span className='flex items-center gap-4px text-12px text-t-tertiary shrink-0 cursor-help'>
            <span
              className='w-7px h-7px rd-full shrink-0'
              style={{ background: configured ? 'rgb(var(--success-6))' : 'rgb(var(--warning-6))' }}
            />
            {t('nomi.chat.modelConfig')}
          </span>
        </Tooltip>
        <Select
          size='mini'
          style={{ width: 148 }}
          placeholder={t('nomi.chat.modelProvider')}
          value={providerId || undefined}
          onChange={(provider_id: string) => void patchCompanion({ model: { provider_id, model: '' } })}
        >
          {/* 供应商被删时，把生 id 作为禁用项展示，让用户看到失效来源。 */}
          {providerStale && (
            <Select.Option key={providerId} value={providerId} disabled>
              {t('nomi.chat.modelUnavailableOption', { model: providerId })}
            </Select.Option>
          )}
          {enabledProviders.map((p) => (
            <Select.Option key={p.id} value={p.id}>
              {p.name}
            </Select.Option>
          ))}
        </Select>
        <Select
          size='mini'
          style={{ width: 176 }}
          placeholder={t('nomi.chat.modelName')}
          value={selectedModel || undefined}
          disabled={!currentProvider}
          onChange={(model: string) => void patchCompanion({ model: { model } })}
        >
          {/* 失效的当前模型：禁用项，明确标注「(不可用)」，用户须改选有效模型。 */}
          {showStaleModel && (
            <Select.Option key={selectedModel} value={selectedModel} disabled>
              {t('nomi.chat.modelUnavailableOption', { model: selectedModel })}
            </Select.Option>
          )}
          {availableModels.map((m) => (
            <Select.Option key={m} value={m}>
              {m}
            </Select.Option>
          ))}
        </Select>
      </div>
      {hint && (
        <span className='text-11px leading-tight' style={{ color: 'rgb(var(--warning-6))' }}>
          {hint}
        </span>
      )}
    </div>
  );
};

export default CompanionModelControl;
