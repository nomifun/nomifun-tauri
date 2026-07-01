/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, InputNumber, Select, Switch } from '@arco-design/web-react';
import { Close, Down, Plus, Up } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { IModelFailoverCandidate, IModelFailoverConfig } from '@/common/adapter/ipcBridge';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { useProvidersQuery } from '@renderer/hooks/agent/useModelProviderList';
import { buildModelFailoverConfigForSave } from './modelFailoverQueue';

const DEFAULT_CONFIG: IModelFailoverConfig = {
  enabled: false,
  queue: [],
  max_switches: 4,
  stamp_unhealthy: true,
};

/**
 * Global model failover queue editor (Phase-3 D8). An ordered list of
 * provider+model candidates the conversation send-loop falls back through when a
 * NOMI session hits a pre-response provider fault. Persisted as one JSON blob
 * under the `agent.model_failover` client preference, via the same
 * idmm-settings-style channel as the IDMM defaults tab.
 */
const ModelFailoverContent: React.FC = () => {
  const { t } = useTranslation();
  const [message, messageContext] = useArcoMessage();
  const { data: providers } = useProvidersQuery();
  const [config, setConfig] = useState<IModelFailoverConfig>(DEFAULT_CONFIG);
  const [saving, setSaving] = useState(false);

  // Pending "add candidate" row state.
  const [draftProvider, setDraftProvider] = useState<string | undefined>(undefined);
  const [draftModel, setDraftModel] = useState<string | undefined>(undefined);

  useEffect(() => {
    void ipcBridge.agentModelFailover.getSettings
      .invoke()
      .then((c) => setConfig({ ...DEFAULT_CONFIG, ...c, queue: c.queue ?? [] }))
      .catch(() => {});
  }, []);

  const providerOptions = useMemo(
    () => (providers ?? []).map((p) => ({ label: p.name, value: p.id })),
    [providers]
  );

  const draftModelOptions = useMemo(() => {
    const p = (providers ?? []).find((x) => x.id === draftProvider);
    return (p?.models ?? []).map((m) => ({ label: m, value: m }));
  }, [providers, draftProvider]);

  const providerName = (id: string) => (providers ?? []).find((p) => p.id === id)?.name ?? id;

  const moveCandidate = (index: number, delta: number) => {
    setConfig((c) => {
      const target = index + delta;
      if (target < 0 || target >= c.queue.length) return c;
      const queue = [...c.queue];
      const [picked] = queue.splice(index, 1);
      queue.splice(target, 0, picked);
      return { ...c, queue };
    });
  };

  const removeCandidate = (index: number) => {
    setConfig((c) => ({ ...c, queue: c.queue.filter((_, i) => i !== index) }));
  };

  const addCandidate = () => {
    if (!draftProvider || !draftModel) return;
    const next: IModelFailoverCandidate = { provider_id: draftProvider, model: draftModel };
    const dup = config.queue.some((q) => q.provider_id === next.provider_id && q.model === next.model);
    if (dup) {
      message.warning(t('modelFailover.duplicate'));
      return;
    }
    setConfig((c) => ({ ...c, queue: [...c.queue, next] }));
    setDraftModel(undefined);
  };

  const save = async () => {
    setSaving(true);
    try {
      const saveResult = buildModelFailoverConfigForSave(config, draftProvider, draftModel);
      const saved = await ipcBridge.agentModelFailover.updateSettings.invoke(saveResult.config);
      setConfig({ ...DEFAULT_CONFIG, ...saved, queue: saved.queue ?? [] });
      if (saveResult.hasCompleteDraft) {
        setDraftModel(undefined);
      }
      message.success(t('modelFailover.saved'));
    } catch (e) {
      message.error(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className='flex flex-col gap-16px max-w-640px'>
      {messageContext}
      <div>
        <div className='text-t-primary text-15px font-600'>{t('modelFailover.title')}</div>
        <div className='text-t-tertiary text-12px leading-18px mt-4px'>{t('modelFailover.desc')}</div>
      </div>

      <div className='flex items-center justify-between'>
        <span className='text-t-secondary text-13px'>{t('modelFailover.enable')}</span>
        <Switch checked={config.enabled} onChange={(v: boolean) => setConfig((c) => ({ ...c, enabled: v }))} />
      </div>

      <div className='flex flex-col gap-8px'>
        <span className='text-t-secondary text-13px'>{t('modelFailover.queue')}</span>
        <div className='text-t-tertiary text-12px leading-18px'>{t('modelFailover.queueHint')}</div>

        {config.queue.length === 0 ? (
          <div className='rounded-8px border border-dashed border-[rgba(var(--primary-6),0.25)] px-12px py-14px text-center text-12px text-t-tertiary'>
            {t('modelFailover.empty')}
          </div>
        ) : (
          <div className='flex flex-col gap-6px'>
            {config.queue.map((cand, index) => (
              <div
                key={`${cand.provider_id}::${cand.model}`}
                className='flex items-center gap-10px rounded-8px border border-solid border-[rgba(var(--primary-6),0.15)] bg-[rgba(var(--primary-6),0.04)] px-10px py-8px'
              >
                <span className='w-20px shrink-0 text-center text-12px text-t-tertiary'>{index + 1}</span>
                <div className='min-w-0 flex-1'>
                  <div className='truncate text-13px text-t-primary'>{cand.model}</div>
                  <div className='truncate text-11px text-t-tertiary'>{providerName(cand.provider_id)}</div>
                </div>
                <div
                  role='button'
                  aria-label={t('modelFailover.moveUp')}
                  className={`flex h-24px w-24px shrink-0 items-center justify-center rounded-6px ${index === 0 ? 'cursor-not-allowed text-t-disabled' : 'cursor-pointer text-t-secondary hover:bg-[rgba(var(--primary-6),0.1)]'}`}
                  onClick={() => index > 0 && moveCandidate(index, -1)}
                >
                  <Up theme='outline' size='14' fill='currentColor' />
                </div>
                <div
                  role='button'
                  aria-label={t('modelFailover.moveDown')}
                  className={`flex h-24px w-24px shrink-0 items-center justify-center rounded-6px ${index === config.queue.length - 1 ? 'cursor-not-allowed text-t-disabled' : 'cursor-pointer text-t-secondary hover:bg-[rgba(var(--primary-6),0.1)]'}`}
                  onClick={() => index < config.queue.length - 1 && moveCandidate(index, 1)}
                >
                  <Down theme='outline' size='14' fill='currentColor' />
                </div>
                <div
                  role='button'
                  aria-label={t('modelFailover.remove')}
                  className='flex h-24px w-24px shrink-0 cursor-pointer items-center justify-center rounded-6px text-t-secondary hover:bg-[rgba(var(--primary-6),0.1)] hover:text-danger'
                  onClick={() => removeCandidate(index)}
                >
                  <Close theme='outline' size='14' fill='currentColor' />
                </div>
              </div>
            ))}
          </div>
        )}

        {/* Add a candidate. */}
        <div className='flex items-end gap-8px'>
          <div className='min-w-0 flex-1'>
            <Select
              placeholder={t('modelFailover.selectProvider')}
              value={draftProvider}
              onChange={(v: string) => {
                setDraftProvider(v);
                setDraftModel(undefined);
              }}
              options={providerOptions}
              allowClear
            />
          </div>
          <div className='min-w-0 flex-1'>
            <Select
              placeholder={t('modelFailover.selectModel')}
              value={draftModel}
              onChange={(v: string) => setDraftModel(v)}
              options={draftModelOptions}
              disabled={!draftProvider}
              allowClear
            />
          </div>
          <div
            role='button'
            aria-label={t('modelFailover.add')}
            className={`flex h-32px shrink-0 items-center gap-4px rounded-6px px-12px text-13px ${
              draftProvider && draftModel
                ? 'cursor-pointer bg-[rgba(var(--primary-6),0.12)] text-primary-6 hover:bg-[rgba(var(--primary-6),0.2)]'
                : 'cursor-not-allowed bg-[rgba(var(--primary-6),0.06)] text-t-disabled'
            }`}
            onClick={addCandidate}
          >
            <Plus theme='outline' size='14' fill='currentColor' />
            <span>{t('modelFailover.add')}</span>
          </div>
        </div>
      </div>

      <div className='flex flex-col gap-6px'>
        <span className='text-t-secondary text-13px'>{t('modelFailover.maxSwitches')}</span>
        <div className='text-t-tertiary text-12px leading-18px'>{t('modelFailover.maxSwitchesHint')}</div>
        <InputNumber
          min={1}
          max={20}
          value={config.max_switches}
          onChange={(v) => setConfig((c) => ({ ...c, max_switches: typeof v === 'number' ? v : c.max_switches }))}
          style={{ width: 160 }}
        />
      </div>

      <div className='flex items-center justify-between'>
        <div className='flex flex-col'>
          <span className='text-t-secondary text-13px'>{t('modelFailover.stampUnhealthy')}</span>
          <span className='text-t-tertiary text-12px leading-18px'>{t('modelFailover.stampUnhealthyHint')}</span>
        </div>
        <Switch
          checked={config.stamp_unhealthy}
          onChange={(v: boolean) => setConfig((c) => ({ ...c, stamp_unhealthy: v }))}
        />
      </div>

      <div>
        <Button type='primary' loading={saving} onClick={save}>
          {t('common.save', { defaultValue: 'Save' })}
        </Button>
      </div>
    </div>
  );
};

export default ModelFailoverContent;
