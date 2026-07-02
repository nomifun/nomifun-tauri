/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, InputNumber, Message, Select, Spin, Switch, Table, Tag } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import type { ICompanionLearnRun } from '@/common/adapter/ipcBridge';
import { useModelProviderList } from '@renderer/hooks/agent/useModelProviderList';
import type { useCompanionShared } from '../useNomi';

const STATUS_COLOR: Record<string, string> = {
  ok: 'green',
  error: 'red',
  no_events: 'gray',
  model_unconfigured: 'orange',
};

interface Props {
  shared: ReturnType<typeof useCompanionShared>;
}

const LearnTab: React.FC<Props> = ({ shared }) => {
  const { t } = useTranslation();
  const { sharedConfig, patchSharedConfig } = shared;
  const { providers, getAvailableModels } = useModelProviderList();
  const [runs, setRuns] = useState<ICompanionLearnRun[]>([]);
  const [running, setRunning] = useState(false);

  const currentProvider = useMemo(
    () => providers.find((p) => p.id === sharedConfig?.learn.model.provider_id),
    [providers, sharedConfig?.learn.model.provider_id]
  );

  const refreshRuns = useCallback(() => {
    void ipcBridge.companion.listLearnRuns
      .invoke({ limit: 30 })
      .then(setRuns)
      .catch(() => {});
  }, []);

  useEffect(() => {
    refreshRuns();
    const unsub = ipcBridge.companion.onLearnFinished.on(refreshRuns);
    return unsub;
  }, [refreshRuns]);

  const runNow = useCallback(async () => {
    setRunning(true);
    try {
      const run = await ipcBridge.companion.runLearn.invoke();
      if (run.status === 'ok') {
        Message.success(t('nomi.learn.runOk', { memories: run.memories_added, suggestions: run.suggestions_added }));
      } else {
        Message.info(t(`nomi.learn.status.${run.status}`, run.status));
      }
      refreshRuns();
    } catch (e) {
      Message.error(String(e));
    } finally {
      setRunning(false);
    }
  }, [refreshRuns, t]);

  if (!sharedConfig) {
    return (
      <div className='flex justify-center py-40px'>
        <Spin />
      </div>
    );
  }

  return (
    <div className='flex flex-col gap-16px py-8px'>
      <div className='flex items-center gap-16px flex-wrap'>
        <div className='flex items-center gap-8px'>
          <span className='text-13px text-t-secondary'>{t('nomi.learn.enabled')}</span>
          <Switch
            checked={sharedConfig.learn.enabled}
            onChange={(checked) => void patchSharedConfig({ learn: { enabled: checked } })}
          />
        </div>
        <div className='flex items-center gap-8px'>
          <span className='text-13px text-t-secondary'>{t('nomi.learn.interval')}</span>
          <InputNumber
            style={{ width: 120 }}
            min={5}
            max={1440}
            value={sharedConfig.learn.interval_minutes}
            onChange={(v) =>
              void patchSharedConfig({ learn: { interval_minutes: Number(v) || 60 } })
            }
            suffix={t('nomi.learn.minutes')}
          />
        </div>
        <Button type='primary' loading={running} onClick={() => void runNow()}>
          {t('nomi.learn.runNow')}
        </Button>
      </div>

      {/* Shared learning model — all companions learn through this one. */}
      <div className='flex items-start gap-16px bg-fill-2 rd-10px px-14px py-12px'>
        <div className='w-200px shrink-0'>
          <div className='text-14px text-t-primary font-500'>{t('nomi.learn.model')}</div>
          <div className='text-12px text-t-tertiary mt-2px'>{t('nomi.learn.modelHint')}</div>
        </div>
        <div className='flex-1 min-w-0 flex gap-8px flex-wrap'>
          <Select
            style={{ width: 220 }}
            placeholder={t('nomi.settings.providerPlaceholder')}
            value={sharedConfig.learn.model.provider_id || undefined}
            onChange={(provider_id: string) =>
              void patchSharedConfig({ learn: { model: { provider_id, model: '' } } })
            }
          >
            {providers.map((p) => (
              <Select.Option key={p.id} value={p.id}>
                {p.name}
              </Select.Option>
            ))}
          </Select>
          <Select
            style={{ width: 260 }}
            placeholder={t('nomi.settings.modelPlaceholder')}
            value={sharedConfig.learn.model.model || undefined}
            disabled={!currentProvider}
            onChange={(model: string) =>
              void patchSharedConfig({
                learn: { model: { provider_id: sharedConfig.learn.model.provider_id, model } },
              })
            }
          >
            {(currentProvider ? getAvailableModels(currentProvider) : []).map((m) => (
              <Select.Option key={m} value={m}>
                {m}
              </Select.Option>
            ))}
          </Select>
        </div>
      </div>

      {/* Skill evolution — mines repeated work into reviewable skills. Uses the shared learning model. */}
      <div className='flex items-start gap-16px bg-fill-2 rd-10px px-14px py-12px'>
        <div className='w-200px shrink-0'>
          <div className='text-14px text-t-primary font-500'>{t('nomi.evolve.title', { defaultValue: '技能进化' })}</div>
          <div className='text-12px text-t-tertiary mt-2px'>
            {t('nomi.evolve.hint', { defaultValue: '从你重复的多步操作里自动沉淀技能，复用上面的学习模型。' })}
          </div>
        </div>
        <div className='flex-1 min-w-0 flex flex-col gap-10px'>
          <div className='flex items-center gap-8px'>
            <span className='text-13px text-t-secondary'>{t('nomi.evolve.enabled', { defaultValue: '开启技能进化' })}</span>
            <Switch
              checked={sharedConfig.evolve.enabled}
              onChange={(checked) => void patchSharedConfig({ evolve: { enabled: checked } })}
            />
          </div>
          <div className='flex items-center gap-8px'>
            <span className='text-13px text-t-secondary'>{t('nomi.evolve.reflect', { defaultValue: '任务后反思' })}</span>
            <Switch
              checked={sharedConfig.evolve.reflect_enabled}
              onChange={(checked) => void patchSharedConfig({ evolve: { reflect_enabled: checked } })}
            />
            <span className='text-11px text-t-tertiary'>
              {t('nomi.evolve.reflectHint', { defaultValue: '复杂任务完成后也反思是否值得固化' })}
            </span>
          </div>
          <div className='flex items-center gap-8px'>
            <span className='text-13px text-t-secondary'>{t('nomi.evolve.autoActivate', { defaultValue: '高置信自动生效' })}</span>
            <Switch
              checked={sharedConfig.evolve.auto_activate}
              onChange={(checked) => void patchSharedConfig({ evolve: { auto_activate: checked } })}
            />
            <span className='text-11px text-t-tertiary'>
              {t('nomi.evolve.autoActivateHint', { defaultValue: '多次重复的高置信技能直接启用（仍可在技能页撤销）' })}
            </span>
          </div>
          <div className='flex items-center gap-8px flex-wrap'>
            <span className='text-13px text-t-secondary'>{t('nomi.evolve.interval', { defaultValue: '挖掘间隔' })}</span>
            <InputNumber
              style={{ width: 120 }}
              min={5}
              max={1440}
              value={sharedConfig.evolve.interval_minutes}
              onChange={(v) => void patchSharedConfig({ evolve: { interval_minutes: Number(v) || 30 } })}
              suffix={t('nomi.learn.minutes')}
            />
            <span className='text-13px text-t-secondary ml-8px'>
              {t('nomi.evolve.minSessions', { defaultValue: '重复会话数阈值' })}
            </span>
            <InputNumber
              style={{ width: 90 }}
              min={2}
              max={20}
              value={sharedConfig.evolve.min_distinct_sessions}
              onChange={(v) => void patchSharedConfig({ evolve: { min_distinct_sessions: Number(v) || 2 } })}
            />
          </div>
        </div>
      </div>

      {/* Session archiving — compress idle chat windows into day-digests + reset live context. */}
      <div className='flex items-start gap-16px bg-fill-2 rd-10px px-14px py-12px'>
        <div className='w-200px shrink-0'>
          <div className='text-14px text-t-primary font-500'>{t('nomi.archive.title')}</div>
          <div className='text-12px text-t-tertiary mt-2px'>{t('nomi.archive.hint')}</div>
        </div>
        <div className='flex-1 min-w-0 flex flex-col gap-10px'>
          <div className='flex items-center gap-8px'>
            <span className='text-13px text-t-secondary'>{t('nomi.archive.enabled')}</span>
            <Switch
              checked={sharedConfig.archive?.enabled ?? false}
              onChange={(checked) => void patchSharedConfig({ archive: { enabled: checked } })}
            />
          </div>
          <div className='flex items-center gap-8px'>
            <span className='text-13px text-t-secondary'>{t('nomi.archive.idleMinutes')}</span>
            <InputNumber
              style={{ width: 120 }}
              min={5}
              max={1440}
              value={sharedConfig.archive?.idle_minutes ?? 30}
              onChange={(v) => void patchSharedConfig({ archive: { idle_minutes: Number(v) || 30 } })}
              suffix={t('nomi.learn.minutes')}
            />
          </div>
        </div>
      </div>

      {/* Smart orchestration — companion delegates complex tasks to isolated sub-agents. */}
      <div className='flex items-start gap-16px bg-fill-2 rd-10px px-14px py-12px'>
        <div className='w-200px shrink-0'>
          <div className='text-14px text-t-primary font-500'>{t('nomi.orchestration.title')}</div>
          <div className='text-12px text-t-tertiary mt-2px'>{t('nomi.orchestration.hint')}</div>
        </div>
        <div className='flex-1 min-w-0 flex flex-col gap-10px'>
          <div className='flex items-center gap-8px'>
            <span className='text-13px text-t-secondary'>{t('nomi.orchestration.enabled')}</span>
            <Switch
              checked={sharedConfig.smart_orchestration ?? false}
              onChange={(checked) => void patchSharedConfig({ smart_orchestration: checked })}
            />
          </div>
        </div>
      </div>

      <Table
        rowKey='id'
        data={runs}
        pagination={false}
        size='small'
        columns={[
          {
            title: t('nomi.learn.colTime'),
            dataIndex: 'started_at',
            render: (v: number) => new Date(v).toLocaleString(),
          },
          {
            title: t('nomi.learn.colStatus'),
            dataIndex: 'status',
            render: (s: string) => <Tag color={STATUS_COLOR[s] || 'gray'}>{t(`nomi.learn.status.${s}`, s)}</Tag>,
          },
          { title: t('nomi.learn.colEvents'), dataIndex: 'events_processed' },
          { title: t('nomi.learn.colMemories'), dataIndex: 'memories_added' },
          { title: t('nomi.learn.colSuggestions'), dataIndex: 'suggestions_added' },
          {
            title: t('nomi.learn.colNote'),
            dataIndex: 'summary',
            render: (_: unknown, run: ICompanionLearnRun) => (
              <span className='text-12px text-t-secondary'>{run.error || run.summary || '-'}</span>
            ),
          },
        ]}
      />
    </div>
  );
};

export default LearnTab;
