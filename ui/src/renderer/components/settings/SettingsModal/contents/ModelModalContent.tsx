/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { IProvider, ModelProfile, ModelTask, ModelTrait } from '@/common/config/storage';
import { prefixedId } from '@/common/utils';
import { Button, Checkbox, Collapse, Divider, Input, Message, Modal, Popconfirm, Popover, Select, Switch, Tag, Tooltip } from '@arco-design/web-react';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { Copy, DeleteFour, Info, Minus, Plus, Write, Heartbeat, Drag, TagOne } from '@icon-park/react';
import {
  closestCenter,
  DndContext,
  KeyboardSensor,
  PointerSensor,
  useSensor,
  useSensors,
  type DragEndEvent,
} from '@dnd-kit/core';
import {
  SortableContext,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
} from '@dnd-kit/sortable';
import { CSS } from '@dnd-kit/utilities';
import React, { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import { isBackendHttpError } from '@/common/adapter/httpBridge';
import {
  featureRoute,
  groupUsagesByFeature,
  parseProviderInUseDetails,
  type ProviderUsageFeature,
} from './providerInUse';
import AddModelModal from '@/renderer/pages/settings/components/AddModelModal';
import AddPlatformModal from '@/renderer/pages/settings/components/AddPlatformModal';
import { isNewApiPlatform, NEW_API_PROTOCOL_OPTIONS } from '@/renderer/utils/model/modelPlatforms';
import EditModeModal from '@/renderer/pages/settings/components/EditModeModal';
import NomiScrollArea from '@/renderer/components/base/NomiScrollArea';
import { useProvidersQuery } from '@/renderer/hooks/agent/useModelProviderList';
import { useModelProfiles } from '@/renderer/hooks/agent/useModelProfiles';
import { useContainerWidth } from '@/renderer/hooks/ui/useContainerWidth';
import { useSettingsViewMode } from '../settingsViewContext';
import { consumePendingDeepLink } from '@/renderer/hooks/system/useDeepLink';
import { ContextLimitSelect, formatContextLimit } from '@/renderer/pages/settings/components/ContextLimitSelect';
import { cloneProviderConfig } from '@/renderer/utils/model/providerClone';
import { reorderById, reorderStrings, withDenseSortOrder } from './modelProviderOrdering';
import {
  buildModelProfileUpsertRequest,
  editableModelTasks,
  editableModelTraits,
  MODEL_TASK_ORDER,
  visibleModelTaskBadges,
} from '@/renderer/hooks/agent/modelProfileEditing';
import '../model-provider.css';

/**
 * 获取协议显示标签颜色
 * Get protocol badge color
 */
const getProtocolColor = (protocol: string): string => {
  switch (protocol) {
    case 'gemini':
      return 'blue';
    case 'anthropic':
      return 'orange';
    case 'openai':
    default:
      return 'green';
  }
};

/**
 * 获取协议显示名称
 * Get protocol display name
 */
const getProtocolLabel = (protocol: string): string => {
  return NEW_API_PROTOCOL_OPTIONS.find((p) => p.value === protocol)?.label || 'OpenAI';
};

/**
 * 获取下一个协议（循环切换）
 * Get next protocol (cycle through options)
 */
const getNextProtocol = (current: string): string => {
  const idx = NEW_API_PROTOCOL_OPTIONS.findIndex((p) => p.value === current);
  const nextIdx = (idx + 1) % NEW_API_PROTOCOL_OPTIONS.length;
  return NEW_API_PROTOCOL_OPTIONS[nextIdx].value;
};

// Calculate API Key count
const getApiKeyCount = (api_key: string): number => {
  if (!api_key) return 0;
  return api_key.split(/[,\n]/).filter((k) => k.trim().length > 0).length;
};

/**
 * 获取供应商的启用状态（全选/半选/全不选）
 * Get provider enable state (all/partial/none)
 */
const getProviderState = (platform: IProvider): { checked: boolean; indeterminate: boolean } => {
  if (!platform.model_enabled) {
    // 没有 model_enabled 记录，默认全部启用
    return { checked: true, indeterminate: false };
  }

  const models = platform.models ?? [];
  const enabledCount = models.filter((model) => platform.model_enabled?.[model] !== false).length;
  const totalCount = models.length;

  if (enabledCount === 0) {
    return { checked: false, indeterminate: false }; // 全不选
  } else if (enabledCount === totalCount) {
    return { checked: true, indeterminate: false }; // 全选
  } else {
    return { checked: true, indeterminate: true }; // 半选（有模型开启，显示为开启状态）
  }
};

/**
 * 检查模型是否启用
 * Check if model is enabled
 */
const isModelEnabled = (platform: IProvider, model: string): boolean => {
  if (!platform.model_enabled) return true; // 默认启用
  return platform.model_enabled[model] !== false;
};

/**
 * 每模型描述编辑浮层 / Per-model description editor popover.
 * 描述用于自动编排选用模型；空态显示占位提示。
 * The description drives orchestration model auto-selection; empty shows placeholder.
 */
const ModelDescriptionEditor: React.FC<{
  description: string;
  onSave: (text: string) => void;
}> = ({ description, onSave }) => {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState(description);

  const placeholder = t('settings.modelDescriptionPlaceholder', {
    defaultValue: '描述该模型擅长什么，用于自动编排选用',
  });

  // 每次打开时同步最新描述，避免外部更新后草稿陈旧
  // Re-sync draft when opening so external updates aren't masked by stale draft.
  const handleVisibleChange = (visible: boolean) => {
    if (visible) setDraft(description);
    setOpen(visible);
  };

  const handleSave = () => {
    const next = draft.trim();
    if (next !== (description ?? '').trim()) {
      onSave(next);
    }
    setOpen(false);
  };

  return (
    <Popover
      trigger='click'
      position='bl'
      popupVisible={open}
      onVisibleChange={handleVisibleChange}
      content={
        <div className='flex flex-col gap-8px w-280px' onClick={(e) => e.stopPropagation()}>
          <div className='text-12px text-t-secondary'>
            {t('settings.modelDescriptionTitle', { defaultValue: '模型描述（用于自动编排）' })}
          </div>
          <Input.TextArea
            autoFocus
            value={draft}
            onChange={setDraft}
            placeholder={placeholder}
            autoSize={{ minRows: 3, maxRows: 6 }}
          />
          <div className='flex items-center justify-end gap-8px'>
            <Button size='mini' onClick={() => setOpen(false)}>
              {t('common.cancel', { defaultValue: '取消' })}
            </Button>
            <Button size='mini' type='primary' onClick={handleSave}>
              {t('common.save', { defaultValue: '保存' })}
            </Button>
          </div>
        </div>
      }
    >
      <Tooltip content={t('settings.editModelDescription', { defaultValue: '编辑模型描述' })}>
        <Button
          size='mini'
          className={`model-provider-action-btn !w-24px !h-24px !min-w-24px shrink-0 ${description ? 'text-[rgb(var(--primary-6))] hover:text-[rgb(var(--primary-5))]' : 'text-t-secondary hover:text-t-primary'}`}
          icon={<Write theme='outline' size='14' />}
          onClick={(e) => e.stopPropagation()}
        />
      </Tooltip>
    </Popover>
  );
};

/**
 * 每模型上下文窗口编辑浮层 / Per-model context window editor popover.
 */
const ModelContextLimitEditor: React.FC<{
  value?: number;
  inheritedValue?: number;
  onSave: (value?: number) => void;
}> = ({ value, inheritedValue, onSave }) => {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState<number | undefined>(value);
  const effectiveValue = value ?? inheritedValue;

  const handleVisibleChange = (visible: boolean) => {
    if (visible) setDraft(value);
    setOpen(visible);
  };

  const handleSave = () => {
    onSave(draft);
    setOpen(false);
  };

  const label = effectiveValue
    ? formatContextLimit(effectiveValue)
    : t('settings.modelContextLimitDefault', { defaultValue: '默认' });
  const inherited = value == null && inheritedValue != null;

  return (
    <Popover
      trigger='click'
      position='bl'
      popupVisible={open}
      onVisibleChange={handleVisibleChange}
      content={
        <div className='flex flex-col gap-8px w-240px' onClick={(e) => e.stopPropagation()}>
          <div className='text-12px text-t-secondary'>
            {t('settings.modelContextLimit', { defaultValue: '模型上下文窗口' })}
          </div>
          <ContextLimitSelect value={draft} onChange={setDraft} />
          {inherited && (
            <div className='text-11px text-t-tertiary leading-4'>
              {t('settings.modelContextLimitInherited', {
                value: formatContextLimit(inheritedValue),
                defaultValue: '当前继承旧供应商设置 {{value}}；选择并保存后会写入该模型自己的配置。',
              })}
            </div>
          )}
          <div className='flex items-center justify-end gap-8px'>
            <Button size='mini' onClick={() => setOpen(false)}>
              {t('common.cancel', { defaultValue: '取消' })}
            </Button>
            <Button size='mini' type='primary' onClick={handleSave}>
              {t('common.save', { defaultValue: '保存' })}
            </Button>
          </div>
        </div>
      }
    >
      <Tooltip content={t('settings.editModelContextLimit', { defaultValue: '编辑模型上下文窗口' })}>
        <Button
          size='mini'
          className={`model-provider-action-btn !h-24px !min-w-44px shrink-0 px-6px text-11px ${value ? 'text-[rgb(var(--primary-6))] hover:text-[rgb(var(--primary-5))]' : 'text-t-secondary hover:text-t-primary'}`}
          onClick={(e) => e.stopPropagation()}
        >
          {inherited ? `${label}*` : label}
        </Button>
      </Tooltip>
    </Popover>
  );
};

const ModelModalityEditor: React.FC<{
  profile?: ModelProfile;
  onSave: (tasks: ModelTask[], traits: ModelTrait[]) => Promise<void>;
}> = ({ profile, onSave }) => {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [saving, setSaving] = useState(false);
  const [draftTasks, setDraftTasks] = useState<ModelTask[]>(() => editableModelTasks(profile));
  const [draftVisionInput, setDraftVisionInput] = useState(() =>
    editableModelTraits(profile).includes('vision_input')
  );
  const taskOptions = useMemo(
    () => MODEL_TASK_ORDER.map((v) => ({ label: t(`settings.modelTask.${v}`), value: v })),
    [t]
  );
  const hasUserSelection =
    profile?.source === 'user' && ((profile.tasks?.length ?? 0) > 0 || (profile.traits?.length ?? 0) > 0);

  const handleVisibleChange = (visible: boolean) => {
    if (visible) {
      const nextTasks = editableModelTasks(profile);
      setDraftTasks(nextTasks);
      setDraftVisionInput(nextTasks.includes('chat') && editableModelTraits(profile).includes('vision_input'));
    }
    setOpen(visible);
  };

  const handleTasksChange = (value: ModelTask[]) => {
    const next = value ?? [];
    setDraftTasks(next);
    if (!next.includes('chat')) setDraftVisionInput(false);
  };

  const handleSave = async () => {
    setSaving(true);
    try {
      await onSave(draftTasks, draftTasks.includes('chat') && draftVisionInput ? ['vision_input'] : []);
      setOpen(false);
    } catch {
      // Parent save handler owns the toast; keep the editor open so the user can retry.
    } finally {
      setSaving(false);
    }
  };

  return (
    <Popover
      trigger='click'
      position='bl'
      popupVisible={open}
      onVisibleChange={handleVisibleChange}
      content={
        <div className='flex flex-col gap-8px w-280px' onClick={(e) => e.stopPropagation()}>
          <div className='text-12px text-t-secondary'>
            {t('settings.modelModality', { defaultValue: '模态能力' })}
          </div>
          <Select
            mode='multiple'
            value={draftTasks}
            onChange={handleTasksChange}
            options={taskOptions}
            placeholder={t('settings.modelModality', { defaultValue: '模态能力' })}
            triggerProps={{ getPopupContainer: () => document.body }}
          />
          {draftTasks.includes('chat') && (
            <Checkbox checked={draftVisionInput} onChange={setDraftVisionInput} className='!pl-0'>
              <span className='text-12px text-t-secondary'>
                {t('settings.modelVisionInput', { defaultValue: '支持图片输入（视觉）' })}
              </span>
            </Checkbox>
          )}
          <div className='text-11px text-t-tertiary leading-4'>
            {t('settings.modelModalityTip', {
              defaultValue: '声明该模型能做什么——探测与调用据此选择正确的端点',
            })}
          </div>
          <div className='flex items-center justify-end gap-8px'>
            <Button size='mini' onClick={() => setOpen(false)} disabled={saving}>
              {t('common.cancel', { defaultValue: '取消' })}
            </Button>
            <Button size='mini' type='primary' loading={saving} onClick={handleSave}>
              {t('common.save', { defaultValue: '保存' })}
            </Button>
          </div>
        </div>
      }
    >
      <Tooltip content={t('settings.editModelModality', { defaultValue: '编辑模型类别' })}>
        <Button
          size='mini'
          className={`model-provider-action-btn !w-24px !h-24px !min-w-24px shrink-0 ${hasUserSelection ? 'text-[rgb(var(--primary-6))] hover:text-[rgb(var(--primary-5))]' : 'text-t-secondary hover:text-t-primary'}`}
          icon={<TagOne theme='outline' size='14' />}
          onClick={(e) => e.stopPropagation()}
        />
      </Tooltip>
    </Popover>
  );
};

const providerSortableId = (providerId: string) => `provider:${providerId}`;
const modelSortableId = (providerId: string, model: string) => `model:${providerId}:${model}`;

type SortableDragData =
  | { type: 'provider'; providerId: string }
  | { type: 'model'; providerId: string; model: string };

type SortableRenderProps = {
  attributes: ReturnType<typeof useSortable>['attributes'];
  listeners: ReturnType<typeof useSortable>['listeners'];
  setActivatorNodeRef: ReturnType<typeof useSortable>['setActivatorNodeRef'];
  isDragging: boolean;
};

const SortableProviderCard: React.FC<{
  provider: IProvider;
  children: (props: SortableRenderProps) => React.ReactNode;
}> = ({ provider, children }) => {
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, transform, transition, isDragging } = useSortable({
    id: providerSortableId(provider.id),
    data: { type: 'provider', providerId: provider.id } satisfies SortableDragData,
  });

  return (
    <div
      ref={setNodeRef}
      className={isDragging ? 'model-provider-sortable-card is-dragging' : 'model-provider-sortable-card'}
      style={{
        transform: CSS.Transform.toString(transform),
        transition,
      }}
    >
      {children({ attributes, listeners, setActivatorNodeRef, isDragging })}
    </div>
  );
};

const SortableModelRow: React.FC<{
  providerId: string;
  model: string;
  children: (props: SortableRenderProps) => React.ReactNode;
}> = ({ providerId, model, children }) => {
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, transform, transition, isDragging } = useSortable({
    id: modelSortableId(providerId, model),
    data: { type: 'model', providerId, model } satisfies SortableDragData,
  });

  return (
    <div
      ref={setNodeRef}
      className={isDragging ? 'model-provider-sortable-row is-dragging' : 'model-provider-sortable-row'}
      style={{
        transform: CSS.Transform.toString(transform),
        transition,
      }}
    >
      {children({ attributes, listeners, setActivatorNodeRef, isDragging })}
    </div>
  );
};

const PriorityDragHandle: React.FC<SortableRenderProps & { label: string }> = ({
  attributes,
  listeners,
  setActivatorNodeRef,
  isDragging,
  label,
}) => (
  <Tooltip content={label}>
    <span
      ref={setActivatorNodeRef}
      {...attributes}
      {...listeners}
      aria-label={label}
      className={`model-provider-drag-handle inline-flex shrink-0 ${isDragging ? 'is-dragging' : ''}`}
      style={{ touchAction: 'none' }}
      onClick={(e) => e.stopPropagation()}
      onMouseDown={(e) => e.stopPropagation()}
    >
      <Button
        tabIndex={-1}
        size='mini'
        className='model-provider-action-btn !w-24px !h-24px !min-w-24px text-t-secondary hover:text-t-primary cursor-grab'
        icon={<Drag theme='outline' size='14' />}
      />
    </span>
  </Tooltip>
);

const ModelModalContent: React.FC = () => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const viewMode = useSettingsViewMode();
  const isPageMode = viewMode === 'page';
  // 以「内容面板实际宽度」而非视口宽度做分档：模型管理面板被一次 rail + 二级
  // ContentSider 占去宽度，视口断点(md:/lg:)会误判为宽屏。窄面板下用紧凑布局，
  // 避免 provider 头 hover 展开区(320px)挤占供应商名称。
  const { ref: paneRef, width: paneWidth } = useContainerWidth<HTMLDivElement>();
  const isWide = paneWidth >= 520;
  const [collapseKey, setCollapseKey] = useState<Record<string, boolean>>({});
  const [healthCheckLoading, setHealthCheckLoading] = useState<Record<string, boolean>>({});
  const { data, mutate } = useProvidersQuery();
  const { profileFor, mutate: mutateProfiles } = useModelProfiles();
  const [message, messageContext] = useArcoMessage();
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates })
  );
  const providerSortableItems = useMemo(() => (data || []).map((platform) => providerSortableId(platform.id)), [data]);

  /**
   * Create when the provider id is new, update otherwise.
   * The caller is expected to have mutated the id-bearing record already.
   */
  const persistPlatform = async (platform: IProvider): Promise<void> => {
    const existing = (data || []).some((item) => item.id === platform.id);
    if (existing) {
      const { id, ...body } = platform;
      await ipcBridge.mode.updateProvider.invoke({ id, ...body });
    } else {
      await ipcBridge.mode.createProvider.invoke(platform);
    }
  };

  const updatePlatform = (platform: IProvider, success: () => void, throwOnError = false): Promise<void> => {
    const existing = (data || []).find((item) => item.id === platform.id);
    const nextArray = existing
      ? (data || []).map((item) => (item.id === platform.id ? { ...item, ...platform } : item))
      : [...(data || []), platform];

    // Optimistic update
    void mutate(nextArray, false);

    return persistPlatform(platform)
      .then(() => {
        void mutate();
        success();
      })
      .catch((error) => {
        void mutate();
        console.error('Failed to save provider:', error);
        // 409 Conflict — duplicate id (rare pre-launch); different toast
        const msg = error instanceof Error ? error.message : String(error);
        if (msg.includes('409')) {
          message.error(t('settings.providerIdConflict', { defaultValue: 'Provider id already exists, retry.' }));
        } else {
          message.error(t('settings.saveModelConfigFailed'));
        }
        if (throwOnError) throw error;
      });
  };

  const removePlatform = (id: string) => {
    const nextArray = (data ?? []).filter((item: IProvider) => item.id !== id);
    void mutate(nextArray, false);
    ipcBridge.mode.deleteProvider
      .invoke({ id })
      .then(() => {
        void mutate();
      })
      .catch((error) => {
        void mutate();
        console.error('Failed to delete provider:', error);
        if (isBackendHttpError(error) && error.code === 'PROVIDER_IN_USE') {
          const groups = groupUsagesByFeature(parseProviderInUseDetails(error.details));
          const featureName: Record<ProviderUsageFeature, string> = {
            desktopCompanion: t('settings.providerInUse.desktopCompanion'),
            publicCompanion: t('settings.providerInUse.publicCompanion'),
            smartDecision: t('settings.providerInUse.smartDecision'),
            orchestrator: t('settings.providerInUse.orchestrator'),
          };
          Modal.confirm({
            title: t('settings.providerInUse.title'),
            content: (
              <div className='flex flex-col gap-8px'>
                <div>{t('settings.providerInUse.desc')}</div>
                {groups.map((g) => (
                  <div key={g.feature}>
                    <b>{featureName[g.feature]}</b>：{g.labels.join('、')}
                  </div>
                ))}
              </div>
            ),
            okText: t('settings.providerInUse.goto'),
            cancelText: t('common.cancel', { defaultValue: '取消' }),
            onOk: () => {
              const first = groups[0];
              if (first) navigate(featureRoute(first.feature, first.targetId));
            },
          });
          return;
        }
        message.error(t('settings.saveModelConfigFailed'));
      });
  };

  const persistProviderOrder = async (nextArray: IProvider[], previousArray: IProvider[]) => {
    const previousById = new Map(previousArray.map((item) => [item.id, item.sort_order]));
    const changed = nextArray.filter((item) => previousById.get(item.id) !== item.sort_order);

    if (changed.length === 0) return;

    await Promise.all(
      changed.map((platform) =>
        ipcBridge.mode.updateProvider.invoke({
          id: platform.id,
          sort_order: platform.sort_order,
        })
      )
    );
  };

  const handleProviderDragEnd = (activeData: SortableDragData, overData: SortableDragData) => {
    if (!data || activeData.type !== 'provider' || overData.type !== 'provider') return;

    const reordered = reorderById(data, activeData.providerId, overData.providerId);
    if (reordered === data) return;

    const nextArray = withDenseSortOrder(reordered);
    void mutate(nextArray, false);

    persistProviderOrder(nextArray, data)
      .then(() => {
        void mutate();
      })
      .catch((error) => {
        void mutate();
        console.error('Failed to save provider order:', error);
        message.error(t('settings.saveModelConfigFailed'));
      });
  };

  const handleModelDragEnd = (activeData: SortableDragData, overData: SortableDragData) => {
    if (activeData.type !== 'model' || overData.type !== 'model' || activeData.providerId !== overData.providerId) {
      return;
    }

    const platform = (data || []).find((item) => item.id === activeData.providerId);
    if (!platform) return;

    const nextModels = reorderStrings(platform.models ?? [], activeData.model, overData.model);
    if (nextModels === platform.models) return;

    updatePlatform(
      {
        ...platform,
        models: nextModels,
      },
      () => {}
    );
  };

  const handleDragEnd = ({ active, over }: DragEndEvent) => {
    if (!over || active.id === over.id) return;

    const activeData = active.data.current as SortableDragData | undefined;
    const overData = over.data.current as SortableDragData | undefined;
    if (!activeData || !overData || activeData.type !== overData.type) return;

    if (activeData.type === 'provider') {
      handleProviderDragEnd(activeData, overData);
    } else {
      handleModelDragEnd(activeData, overData);
    }
  };

  const duplicatePlatform = (platform: IProvider) => {
    const copied = cloneProviderConfig(
      platform,
      prefixedId('prov'),
      t('settings.providerCopySuffix', { defaultValue: '副本' })
    );
    updatePlatform(copied, () => {
      setCollapseKey((prev) => ({ ...prev, [copied.id]: true }));
      message.success(t('settings.providerConfigCopied', { name: copied.name }));
    });
  };

  // 切换供应商启用状态（全选 ↔ 全不选）
  const toggleProviderEnabled = (platform: IProvider) => {
    const { checked } = getProviderState(platform);
    const newState = !checked; // 切换状态

    // 批量更新所有模型状态
    const model_enabled: Record<string, boolean> = {};
    (platform.models ?? []).forEach((model) => {
      model_enabled[model] = newState;
    });

    const updated = {
      ...platform,
      model_enabled,
    };
    updatePlatform(updated, () => {});
  };

  // 切换模型启用状态
  const toggleModelEnabled = (platform: IProvider, model: string, enabled: boolean) => {
    const model_enabled = { ...platform.model_enabled };
    model_enabled[model] = enabled;

    const updated = {
      ...platform,
      model_enabled,
    };

    updatePlatform(updated, () => {});
  };

  // Execute provider/model health check without creating a conversation.
  const performHealthCheck = async (platform: IProvider, modelName: string) => {
    const loadingKey = `${platform.id}-${modelName}`;
    setHealthCheckLoading((prev) => ({ ...prev, [loadingKey]: true }));

    const startTime = Date.now();

    try {
      const result = await ipcBridge.acpConversation.checkProviderHealth.invoke({
        provider_id: platform.id,
        model: modelName,
      });
      const latency = result.elapsed_ms || Date.now() - startTime;
      const success = result.status === 'healthy';
      const errorMessage = result.message || t('common.unknownError');

      try {
        // 先获取最新的数据，确保不会覆盖其他并发的更新
        const latestData = await ipcBridge.mode.listProviders.invoke();
        const latestPlatform = (latestData || []).find((item) => item.id === platform.id);
        const model_health = { ...latestPlatform?.model_health };
        model_health[modelName] = {
          status: success ? 'healthy' : 'unhealthy',
          last_check: Date.now(),
          latency,
          error: success ? undefined : errorMessage,
        };

        await ipcBridge.mode.updateProvider.invoke({ id: platform.id, model_health });
        await mutate();
        if (success) {
          Message.success({
            content: `${platform.name} - ${modelName}: ${t('common.success')} (${latency}ms)`,
            duration: 3000,
          });
        } else {
          Message.error({
            content: `${platform.name} - ${modelName}: ${t('common.failed')} - ${errorMessage}`,
            duration: 5000,
          });
        }
      } catch (saveError) {
        console.error('Failed to save health check result:', saveError);
        Message.error({
          content: t('settings.saveModelConfigFailed'),
          duration: 3000,
        });
      }
    } catch (error: unknown) {
      const latency = Date.now() - startTime;
      const errorMessage = error instanceof Error ? error.message : String(error);
      Message.error({
        content: `${platform.name} - ${modelName}: ${t('common.failed')} - ${errorMessage}`,
        duration: 5000,
      });

      try {
        // 先获取最新的数据，确保不会覆盖其他并发的更新
        const latestData = await ipcBridge.mode.listProviders.invoke();
        const latestPlatform = (latestData || []).find((item) => item.id === platform.id);
        const model_health = { ...latestPlatform?.model_health };
        model_health[modelName] = {
          status: 'unhealthy',
          last_check: Date.now(),
          latency,
          error: errorMessage,
        };

        await ipcBridge.mode.updateProvider.invoke({ id: platform.id, model_health });
        await mutate();
      } catch (saveError) {
        console.error('Failed to save health check result:', saveError);
      }
    } finally {
      setHealthCheckLoading((prev) => ({ ...prev, [loadingKey]: false }));
    }
  };

  const clearAllHealthData = () => {
    if (!data) return;
    const nextArray: IProvider[] = data.map((platform: IProvider) => ({
      ...platform,
      model_health: undefined as IProvider['model_health'],
    }));
    void mutate(nextArray, false);

    Promise.all(
      (data || []).map((platform) => ipcBridge.mode.updateProvider.invoke({ id: platform.id, model_health: {} }))
    )
      .then(() => {
        void mutate();
        Message.success({
          content: t('settings.healthStatusCleared'),
          duration: 2000,
        });
      })
      .catch((error) => {
        void mutate();
        console.error('Failed to clear health status:', error);
        message.error(t('settings.saveModelConfigFailed'));
      });
  };

  const [addPlatformModalCtrl, addPlatformModalContext] = AddPlatformModal.useModal({
    async onSubmit(platform) {
      await updatePlatform(platform, () => {
        setCollapseKey((prev) => ({ ...prev, [platform.id]: true }));
      }, true);
    },
  });

  // Consume pending deep-link data on mount (set by useDeepLink hook before navigation)
  useEffect(() => {
    const pending = consumePendingDeepLink();
    if (pending) {
      addPlatformModalCtrl.open({ deepLinkData: pending });
    }
  }, [addPlatformModalCtrl]);

  const [addModelModalCtrl, addModelModalContext] = AddModelModal.useModal({
    onSubmit(platform) {
      updatePlatform(platform, () => {
        setCollapseKey((prev) => ({ ...prev, [platform.id]: true }));
        addModelModalCtrl.close();
      });
    },
  });

  const [editModalCtrl, editModalContext] = EditModeModal.useModal({
    onChange(platform) {
      updatePlatform(platform, () => editModalCtrl.close());
    },
  });

  return (
    <div
      ref={paneRef}
      className={`flex flex-col bg-2 rd-16px py-16px ${isWide ? 'px-24px' : 'px-16px'}`}
    >
      {messageContext}
      {addPlatformModalContext}
      {editModalContext}
      {addModelModalContext}

      {/* Header with Add Button */}
      <div className='flex-shrink-0 border-b border-[var(--color-border-2)] pb-12px mb-14px flex flex-col gap-10px'>
        <div className='flex items-center justify-between gap-8px flex-wrap'>
          <div className='text-20px font-600 text-t-primary leading-34px'>{t('settings.model')}</div>
          <div className='flex items-center gap-8px flex-wrap'>
            <Button
              type='outline'
              shape='round'
              size='small'
              onClick={clearAllHealthData}
              className='rd-100px border-1 border-solid border-[var(--color-border-2)] h-34px px-14px text-t-secondary hover:text-t-primary'
            >
              {t('settings.clearStatus')}
            </Button>
            <Button
              type='outline'
              shape='round'
              icon={<Plus size='16' />}
              onClick={() => addPlatformModalCtrl.open()}
              className='rd-100px border-1 border-solid border-[var(--color-border-2)] h-34px px-14px text-t-secondary hover:text-t-primary'
            >
              {t('settings.addModel')}
            </Button>
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
          {t('settings.customModelSupportNote')}
        </div>
      </div>

      {/* Content Area */}
      <NomiScrollArea className='flex-1 min-h-0' disableOverflow={isPageMode}>
        {!data || data.length === 0 ? (
          <div className='flex flex-col items-center justify-center py-40px'>
            <Info theme='outline' size='48' className='text-t-secondary mb-16px' />
            <h3 className='text-16px font-500 text-t-primary mb-8px'>{t('settings.noConfiguredModels')}</h3>
            <p className='text-14px text-t-secondary text-center max-w-400px'>
              {t('settings.needHelpConfigGuide')}
              <a
                href='https://github.com/nomifun/nomifun-app/wiki/LLM-Configuration'
                target='_blank'
                rel='noopener noreferrer'
                className='text-[rgb(var(--primary-6))] hover:text-[rgb(var(--primary-5))] underline ml-4px'
              >
                {t('settings.configGuide')}
              </a>
              {t('settings.configGuideSuffix')}
            </p>
          </div>
        ) : (
          <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={handleDragEnd}>
            <SortableContext items={providerSortableItems} strategy={verticalListSortingStrategy}>
              <div className='space-y-16px'>
            {(data || []).map((platform: IProvider) => {
              const key = platform.id;
              const isExpanded = collapseKey[platform.id] ?? false;
              return (
                <SortableProviderCard key={key} provider={platform}>
                  {({ attributes, listeners, setActivatorNodeRef, isDragging }) => (
                <Collapse
                  activeKey={isExpanded ? ['image-generation'] : []}
                  onChange={(_, activeKeys) => {
                    const expanded = activeKeys.includes('image-generation');
                    setCollapseKey((prev) => ({ ...prev, [platform.id]: expanded }));
                  }}
                  bordered
                  expandIconPosition='left'
                  className={`[&_.arco-collapse-item]:!border-0 [&_.arco-collapse-item]:!rounded-12px [&_.arco-collapse-item]:!overflow-hidden [&_.arco-collapse-item]:!bg-[var(--color-bg-2)] [&_.arco-collapse-item-header]:!bg-[var(--fill-0)] [&_.arco-collapse-item-header]:!pl-36px [&_.arco-collapse-item-header]:!pr-12px [&_.arco-collapse-item-header]:!py-8px [&_.arco-collapse-item-header]:transition-colors [&_.arco-collapse-item-header]:hover:!bg-[var(--color-bg-2)] [&_.arco-collapse-item-header]:!gap-8px [&_.arco-collapse-item-header-title]:!min-w-0 [&_.arco-collapse-item-header-icon]:!text-2 [&_.arco-collapse-item-header:hover_.arco-collapse-item-header-icon]:!text-1 [&_.arco-collapse-item-content]:!bg-fill-1 [&_.arco-collapse-item-content-box]:!px-10px [&_.arco-collapse-item-content-box]:!py-8px [&_.arco-collapse-item-content]:!border-t [&_.arco-collapse-item-content]:!border-[var(--color-border-2)] ${
                    isExpanded
                      ? '[&_.arco-collapse-item-header]:!rounded-t-12px [&_.arco-collapse-item-header]:!rounded-b-0 [&_.arco-collapse-item-content]:!rounded-b-12px'
                      : '[&_.arco-collapse-item-header]:!rounded-12px'
                  }`}
                >
                  <Collapse.Item
                    name='image-generation'
                    className='[&_.arco-collapse-item-header-title]:flex-1 group'
                    header={
                      <div className='group flex items-center justify-between w-full min-h-32px gap-8px min-w-0'>
                        <div className='flex items-center gap-8px min-w-0 flex-1'>
                          <PriorityDragHandle
                            attributes={attributes}
                            listeners={listeners}
                            setActivatorNodeRef={setActivatorNodeRef}
                            isDragging={isDragging}
                            label={t('settings.dragProviderPriority', { defaultValue: '拖拽调整供应商优先级' })}
                          />
                          <span
                            className={`text-14px font-500 truncate min-w-0 transition-colors ${isExpanded ? 'text-t-primary' : 'text-2 group-hover:text-1'}`}
                          >
                            {platform.name}
                          </span>
                        </div>
                        <div
                          className='flex items-center gap-8px shrink-0'
                          onClick={(e) => {
                            e.stopPropagation();
                          }}
                          onMouseDown={(e) => {
                            e.stopPropagation();
                          }}
                        >
                          <span className={`text-12px text-t-secondary whitespace-nowrap items-center ${isWide ? 'inline-flex' : 'hidden'}`}>
                            <span
                              className='cursor-pointer hover:text-t-primary transition-colors'
                              onClick={() => setCollapseKey((prev) => ({ ...prev, [platform.id]: !isExpanded }))}
                            >
                              {t('settings.modelCount')}（{(platform.models ?? []).length}）
                            </span>
                            <span className='mx-6px'>|</span>
                            <span
                              className='cursor-pointer hover:text-t-primary transition-colors'
                              onClick={() => editModalCtrl.open({ data: platform })}
                            >
                              {t('settings.apiKeyCount')}（{getApiKeyCount(platform.api_key)}）
                            </span>
                          </span>
                          <span className={`text-12px text-t-secondary whitespace-nowrap ${isWide ? 'hidden' : 'inline'}`}>
                            {(platform.models ?? []).length} / {getApiKeyCount(platform.api_key)}
                          </span>
                          {/* 供应商启用开关 / Provider enable switch */}
                          <Switch
                            size='small'
                            checked={getProviderState(platform).checked}
                            onChange={() => toggleProviderEnabled(platform)}
                          />
                          <div className='flex items-center gap-4px'>
                            <Button
                              size='mini'
                              className='model-provider-action-btn !w-28px !h-28px !min-w-28px text-t-secondary hover:text-t-primary'
                              icon={<Plus size='14' />}
                              onClick={() => addModelModalCtrl.open({ data: platform })}
                            />
                            <Popconfirm
                              title={t('settings.deleteAllModelConfirm')}
                              onOk={() => removePlatform(platform.id)}
                            >
                              <Button
                                size='mini'
                                className='model-provider-action-btn !w-28px !h-28px !min-w-28px text-t-secondary hover:text-t-primary'
                                icon={<Minus size='14' />}
                              />
                            </Popconfirm>
                            <Button
                              size='mini'
                              className='model-provider-action-btn !w-28px !h-28px !min-w-28px text-t-secondary hover:text-t-primary'
                              icon={<Write size='14' />}
                              onClick={() => editModalCtrl.open({ data: platform })}
                            />
                            <Tooltip content={t('settings.copyProviderConfig', { defaultValue: '复制整组配置' })}>
                              <Button
                                size='mini'
                                className='model-provider-action-btn !w-28px !h-28px !min-w-28px text-t-secondary hover:text-t-primary'
                                icon={<Copy theme='outline' size='14' />}
                                onClick={() => duplicatePlatform(platform)}
                              />
                            </Tooltip>
                          </div>
                        </div>
                      </div>
                    }
                  >
                    <SortableContext
                      items={(platform.models ?? []).map((model) => modelSortableId(platform.id, model))}
                      strategy={verticalListSortingStrategy}
                    >
                    {(platform.models ?? []).map((model: string, index: number, arr: string[]) => {
                      const isNewApiProvider = isNewApiPlatform(platform.platform);
                      const modelProtocol = platform.model_protocols?.[model] || 'openai';
                      const model_health = platform.model_health?.[model];
                      const healthStatus = model_health?.status || 'unknown';
                      const modelDescription = platform.model_descriptions?.[model] ?? '';
                      const modelContextLimit = platform.model_context_limits?.[model];
                      const inheritedContextLimit = modelContextLimit == null ? platform.context_limit : undefined;
                      const modelProfile = profileFor(platform.id, model);

                      return (
                        <SortableModelRow key={model} providerId={platform.id} model={model}>
                          {({
                            attributes: modelAttributes,
                            listeners: modelListeners,
                            setActivatorNodeRef: setModelActivatorNodeRef,
                            isDragging: modelIsDragging,
                          }) => (
                        <div>
                          <div className='flex items-center justify-between px-8px py-12px transition-colors hover:bg-[var(--fill-0)]'>
                            <div className='flex flex-col min-w-0 flex-1 gap-2px'>
                              <div className='flex items-center gap-8px min-w-0'>
                                <PriorityDragHandle
                                  attributes={modelAttributes}
                                  listeners={modelListeners}
                                  setActivatorNodeRef={setModelActivatorNodeRef}
                                  isDragging={modelIsDragging}
                                  label={t('settings.dragModelPriority', { defaultValue: '拖拽调整模型优先级' })}
                                />

                                {/* 健康状态指示器 / Health status indicator */}
                                {healthStatus !== 'unknown' && (
                                  <Tooltip
                                    content={
                                      <div>
                                        <div className='flex items-center gap-4px'>
                                          <span>{healthStatus === 'healthy' ? '✅' : '❌'}</span>
                                          <span>
                                            {healthStatus === 'healthy' ? t('common.success') : t('common.failed')}
                                          </span>
                                        </div>
                                        {model_health?.latency && (
                                          <div className='text-12px mt-4px'>
                                            {t('settings.latency')}: {model_health.latency}ms
                                          </div>
                                        )}
                                        {model_health?.error && (
                                          <div className='text-12px mt-4px'>{model_health.error}</div>
                                        )}
                                        {model_health?.last_check && (
                                          <div className='text-12px mt-4px'>
                                            {t('mcp.lastCheck')}: {new Date(model_health.last_check).toLocaleString()}
                                          </div>
                                        )}
                                      </div>
                                    }
                                  >
                                    <div
                                      className={`w-8px h-8px rounded-full shrink-0 ${healthStatus === 'healthy' ? 'bg-green-500' : 'bg-red-500'}`}
                                    />
                                  </Tooltip>
                                )}

                                <span className='text-14px text-t-primary min-w-0 truncate' title={model}>
                                  {model}
                                </span>

                                {/* 模态徽章 / Modality badges — non-chat tasks (image/tts/asr/...) surfaced. */}
                                {visibleModelTaskBadges(modelProfile).map((tk) => (
                                    <Tag
                                      key={tk}
                                      size='small'
                                      color='purple'
                                      bordered
                                      className='shrink-0 select-none'
                                    >
                                      {t(`settings.modelTask.${tk}`)}
                                    </Tag>
                                  ))}

                                {/* New API 协议标签（点击循环切换）/ New API protocol badge (click to cycle) */}
                                {isNewApiProvider && (
                                  <Tag
                                    size='small'
                                    color={getProtocolColor(modelProtocol)}
                                    className='cursor-pointer select-none shrink-0'
                                    onClick={() => {
                                      const nextProtocol = getNextProtocol(modelProtocol);
                                      const newProtocols = { ...platform.model_protocols };
                                      newProtocols[model] = nextProtocol;
                                      updatePlatform({ ...platform, model_protocols: newProtocols }, () => {});
                                    }}
                                  >
                                    {getProtocolLabel(modelProtocol)}
                                  </Tag>
                                )}

                                {/* 每模型上下文窗口 / Per-model context window */}
                                <ModelContextLimitEditor
                                  value={modelContextLimit}
                                  inheritedValue={inheritedContextLimit}
                                  onSave={(value) => {
                                    const next = { ...platform.model_context_limits };
                                    if (value && value > 0) {
                                      next[model] = value;
                                    } else {
                                      delete next[model];
                                    }
                                    updatePlatform(
                                      {
                                        ...platform,
                                        model_context_limits: next,
                                      },
                                      () => {}
                                    );
                                  }}
                                />

                                {/* 每模型类别编辑 / Per-model modality editor */}
                                <ModelModalityEditor
                                  profile={modelProfile}
                                  onSave={async (tasks, traits) => {
                                    try {
                                      await ipcBridge.modelProfile.upsert.invoke(
                                        buildModelProfileUpsertRequest(platform.id, model, tasks, traits)
                                      );
                                      await mutateProfiles();
                                    } catch (error) {
                                      console.error('model profile upsert failed', error);
                                      message.error(t('settings.saveModelConfigFailed'));
                                      throw error;
                                    }
                                  }}
                                />

                                {/* 模型启用开关 / Model enable switch */}
                                <Switch
                                  size='small'
                                  className='shrink-0'
                                  checked={isModelEnabled(platform, model)}
                                  onChange={(checked) => toggleModelEnabled(platform, model, checked)}
                                />

                                {/* 每模型描述编辑（驱动自动编排选用）/ Per-model description editor */}
                                <ModelDescriptionEditor
                                  description={modelDescription}
                                  onSave={(text) => {
                                    const next = { ...platform.model_descriptions };
                                    if (text) {
                                      next[model] = text;
                                    } else {
                                      delete next[model];
                                    }
                                    updatePlatform(
                                      {
                                        ...platform,
                                        model_descriptions: Object.keys(next).length > 0 ? next : undefined,
                                      },
                                      () => {}
                                    );
                                  }}
                                />
                              </div>

                              {/* 描述次级行（空态隐藏）/ Description secondary line (hidden when empty) */}
                              {modelDescription && (
                                <div
                                  className='text-12px text-t-secondary leading-snug line-clamp-2 break-words pr-8px'
                                  title={modelDescription}
                                >
                                  {modelDescription}
                                </div>
                              )}
                            </div>

                            <div className='flex items-center gap-6px shrink-0'>
                              {/* 心跳检测按钮 / Health check button */}
                              <Tooltip content={t('settings.healthCheck')}>
                                <Button
                                  size='mini'
                                  className='!w-28px !h-28px !min-w-28px !bg-[var(--color-bg-1)] text-t-secondary hover:text-t-primary hover:!bg-[var(--fill-0)]'
                                  icon={<Heartbeat theme='outline' size='16' />}
                                  loading={healthCheckLoading[`${platform.id}-${model}`]}
                                  onClick={() => performHealthCheck(platform, model)}
                                />
                              </Tooltip>

                              <Popconfirm
                                title={t('settings.deleteModelConfirm')}
                                onOk={() => {
                                  const newModels = platform.models.filter((item: string) => item !== model);
                                  // 同时清理模型相关状态，避免删除后重加模型时复用脏状态
                                  // Clean all per-model state to avoid stale state on re-add.
                                  const newProtocols = { ...platform.model_protocols };
                                  const newModelEnabled = { ...platform.model_enabled };
                                  const newModelHealth = { ...platform.model_health };
                                  const newModelDescriptions = { ...platform.model_descriptions };
                                  const newModelContextLimits = { ...platform.model_context_limits };
                                  delete newProtocols[model];
                                  delete newModelEnabled[model];
                                  delete newModelHealth[model];
                                  delete newModelDescriptions[model];
                                  delete newModelContextLimits[model];

                                  updatePlatform(
                                    {
                                      ...platform,
                                      models: newModels,
                                      model_protocols: Object.keys(newProtocols).length > 0 ? newProtocols : undefined,
                                      model_enabled:
                                        Object.keys(newModelEnabled).length > 0 ? newModelEnabled : undefined,
                                      model_health: Object.keys(newModelHealth).length > 0 ? newModelHealth : undefined,
                                      model_descriptions:
                                        Object.keys(newModelDescriptions).length > 0 ? newModelDescriptions : undefined,
                                      model_context_limits: newModelContextLimits,
                                    },
                                    () => {}
                                  );
                                }}
                              >
                                <Button
                                  size='mini'
                                  className='!w-28px !h-28px !min-w-28px !bg-[var(--color-bg-1)] text-t-secondary hover:text-t-primary hover:!bg-[var(--fill-0)]'
                                  icon={<DeleteFour theme='outline' size='18' strokeWidth={2} />}
                                />
                              </Popconfirm>
                            </div>
                          </div>
                          {index < arr.length - 1 && <Divider className='!my-0 !border-[var(--color-border-2)]/70' />}
                        </div>
                          )}
                        </SortableModelRow>
                      );
                    })}
                    </SortableContext>
                  </Collapse.Item>
                </Collapse>
                  )}
                </SortableProviderCard>
              );
            })}
              </div>
            </SortableContext>
          </DndContext>
        )}
      </NomiScrollArea>
    </div>
  );
};

export default ModelModalContent;
