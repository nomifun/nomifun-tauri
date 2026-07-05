/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import classNames from 'classnames';
import { Tag, Tooltip } from '@arco-design/web-react';
import { MagicWand, Pic, Platte, VideoTwo } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { IProvider, ModelCapability } from '@/common/config/storage';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { useProvidersQuery } from '@/renderer/hooks/agent/useModelProviderList';
import NomiScrollArea from '@/renderer/components/base/NomiScrollArea';
import SegmentedTabs, { type SegmentedTabItem } from '@/renderer/components/base/SegmentedTabs';
import { useSettingsViewMode } from '@/renderer/components/settings/SettingsModal/settingsViewContext';
import {
  CREATION_CAPABILITIES,
  type CreationCapability,
  getCreationModels,
  groupCreationModelsByProvider,
  providerCapabilityOverride,
} from './creationModels';

type Filter = 'all' | CreationCapability;

/** Per-capability visual language (chip icon + accent), consistent light/dark. */
const CAP_META: Record<CreationCapability, { icon: React.ReactNode; color: string }> = {
  image_generation: { icon: <Pic theme='outline' size='12' strokeWidth={3} />, color: 'magenta' },
  video_generation: { icon: <VideoTwo theme='outline' size='12' strokeWidth={3} />, color: 'purple' },
};

/**
 * CreationModelsContent — the 创作模型 (Creative Workshop) section of Model
 * Management. Surfaces the generation-capable models across configured
 * providers, grouped by provider and filterable by capability
 * (image / video generation). Capability is a NAME heuristic (twin of the
 * backend `infer_generation_capabilities`) layered with a provider-level user
 * override (`capabilities` + `is_user_selected`), so custom platforms whose
 * model names miss the heuristic can still be marked capable.
 *
 * Visual language mirrors `ModelModalContent`: header + info banner + grouped
 * cards; no layout departure.
 */
const CreationModelsContent: React.FC = () => {
  const { t } = useTranslation();
  const viewMode = useSettingsViewMode();
  const isPageMode = viewMode === 'page';
  const { data, mutate } = useProvidersQuery();
  const [message, messageContext] = useArcoMessage();
  const [filter, setFilter] = useState<Filter>('all');

  const capabilityLabel = (cap: CreationCapability): string =>
    cap === 'image_generation'
      ? t('settings.modelHub.creation.capImage')
      : t('settings.modelHub.creation.capVideo');

  const counts = useMemo(
    () => ({
      all: getCreationModels(data, undefined).length,
      image_generation: getCreationModels(data, 'image_generation').length,
      video_generation: getCreationModels(data, 'video_generation').length,
    }),
    [data]
  );

  const groups = useMemo(
    () => groupCreationModelsByProvider(getCreationModels(data, filter === 'all' ? undefined : filter)),
    [data, filter]
  );

  const filterItems: SegmentedTabItem[] = [
    { key: 'all', label: `${t('settings.modelHub.creation.filterAll')} (${counts.all})` },
    {
      key: 'image_generation',
      label: `${t('settings.modelHub.creation.filterImage')} (${counts.image_generation})`,
      icon: <Pic theme='outline' size='14' strokeWidth={3} />,
    },
    {
      key: 'video_generation',
      label: `${t('settings.modelHub.creation.filterVideo')} (${counts.video_generation})`,
      icon: <VideoTwo theme='outline' size='14' strokeWidth={3} />,
    },
  ];

  /** Toggle a provider-level capability override (is_user_selected true ↔ unset). */
  const toggleProviderCapability = (provider: IProvider, cap: CreationCapability, next: boolean) => {
    const others = (provider.capabilities ?? []).filter((c) => c.type !== cap);
    const capabilities: ModelCapability[] = next ? [...others, { type: cap, is_user_selected: true }] : others;
    const updated: IProvider = { ...provider, capabilities };

    const nextArray = (data ?? []).map((p) => (p.id === provider.id ? updated : p));
    void mutate(nextArray, false);

    const { id, ...body } = updated;
    ipcBridge.mode.updateProvider
      .invoke({ id, ...body })
      .then(() => {
        void mutate();
      })
      .catch((error) => {
        void mutate();
        console.error('Failed to update provider capability:', error);
        message.error(t('settings.saveModelConfigFailed'));
      });
  };

  const providersWithModels = (data ?? []).filter((p) => p.enabled !== false && (p.models ?? []).length > 0).length;

  return (
    <div className='flex flex-col bg-2 rd-16px px-24px py-16px'>
      {messageContext}

      {/* Header */}
      <div className='flex-shrink-0 border-b border-[var(--color-border-2)] pb-12px mb-14px flex flex-col gap-10px'>
        <div className='flex items-center gap-8px'>
          <span className='size-28px flex items-center justify-center rd-8px bg-primary-1 text-primary-6 shrink-0'>
            <Platte theme='outline' size='18' strokeWidth={3} />
          </span>
          <div className='min-w-0'>
            <div className='text-20px font-600 text-t-primary leading-28px'>
              {t('settings.modelHub.creation.title')}
            </div>
          </div>
        </div>
        <div
          className='rd-8px px-12px py-8px text-12px leading-5 border border-solid'
          style={{
            borderColor: 'rgba(var(--primary-6),0.32)',
            backgroundColor: 'rgba(var(--primary-6),0.08)',
            color: 'rgb(var(--primary-6))',
          }}
        >
          {t('settings.modelHub.creation.note')}
        </div>
        <SegmentedTabs items={filterItems} activeKey={filter} onChange={(k) => setFilter(k as Filter)} size='sm' />
      </div>

      {/* Content */}
      <NomiScrollArea className='flex-1 min-h-0' disableOverflow={isPageMode}>
        {groups.length === 0 ? (
          <div className='flex flex-col items-center justify-center py-48px text-center'>
            <MagicWand theme='outline' size='44' className='text-t-tertiary mb-14px' />
            <h3 className='text-16px font-500 text-t-primary mb-6px'>{t('settings.modelHub.creation.empty')}</h3>
            <p className='text-13px text-t-secondary max-w-420px leading-20px'>
              {providersWithModels === 0
                ? t('settings.modelHub.creation.emptyNoProviders')
                : t('settings.modelHub.creation.emptyHint')}
            </p>
          </div>
        ) : (
          <div className='space-y-12px'>
            {groups.map((group) => {
              const provider = (data ?? []).find((p) => p.id === group.providerId);
              return (
                <div
                  key={group.providerId}
                  className='rd-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] overflow-hidden'
                >
                  {/* Group header */}
                  <div className='flex items-center justify-between gap-8px px-14px py-10px bg-[var(--fill-0)] border-b border-solid border-[var(--color-border-2)] flex-wrap'>
                    <div className='flex items-center gap-8px min-w-0'>
                      <span className='text-14px font-600 text-t-primary truncate'>{group.providerName}</span>
                      <span className='text-11px text-t-tertiary shrink-0'>{group.platform}</span>
                      <span className='text-11px text-t-tertiary shrink-0'>
                        · {t('settings.modelHub.creation.modelCount', { count: group.models.length })}
                      </span>
                    </div>
                    {/* Provider-level capability override chips (lightweight, checkable). */}
                    <div className='flex items-center gap-6px shrink-0'>
                      <span className='text-11px text-t-tertiary'>{t('settings.modelHub.creation.markAs')}</span>
                      {CREATION_CAPABILITIES.map((cap) => {
                        const checked = provider ? providerCapabilityOverride(provider, cap) === true : false;
                        return (
                          <Tooltip key={cap} content={t('settings.modelHub.creation.markHint')}>
                            <Tag
                              checkable
                              checked={checked}
                              color={checked ? CAP_META[cap].color : undefined}
                              icon={CAP_META[cap].icon}
                              onCheck={(next) => provider && toggleProviderCapability(provider, cap, next)}
                              className='cursor-pointer select-none !text-11px'
                            >
                              {capabilityLabel(cap)}
                            </Tag>
                          </Tooltip>
                        );
                      })}
                    </div>
                  </div>

                  {/* Model rows */}
                  <div className='flex flex-col'>
                    {group.models.map((entry, idx) => (
                      <div
                        key={entry.model}
                        className={classNames(
                          'flex items-center justify-between gap-8px px-14px py-10px transition-colors hover:bg-[var(--fill-0)]',
                          idx < group.models.length - 1 && 'border-b border-solid border-[var(--color-border-2)]/70'
                        )}
                      >
                        <span className='text-13px text-t-primary min-w-0 truncate' title={entry.model}>
                          {entry.model}
                        </span>
                        <div className='flex items-center gap-6px shrink-0'>
                          {entry.capabilities.map((cap) => (
                            <Tag key={cap} size='small' color={CAP_META[cap].color} icon={CAP_META[cap].icon}>
                              {capabilityLabel(cap)}
                            </Tag>
                          ))}
                        </div>
                      </div>
                    ))}
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </NomiScrollArea>
    </div>
  );
};

export default CreationModelsContent;
