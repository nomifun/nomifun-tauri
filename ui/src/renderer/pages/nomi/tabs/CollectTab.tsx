/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Message, Popconfirm, Spin, Switch, Tag } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import type { ICompanionCollectConfig, ICompanionCollectedEvent, ICompanionSourceStats } from '@/common/adapter/ipcBridge';
import type { useCompanionShared } from '../useNomi';

const SOURCES: { key: keyof ICompanionCollectConfig; sensitivity: 'low' | 'medium' | 'high' }[] = [
  { key: 'tool_calls', sensitivity: 'medium' },
  { key: 'chat_user_messages', sensitivity: 'high' },
  { key: 'chat_assistant_replies', sensitivity: 'high' },
  { key: 'requirements', sensitivity: 'medium' },
  { key: 'terminal_sessions', sensitivity: 'medium' },
  { key: 'cron_runs', sensitivity: 'low' },
  { key: 'conversation_lifecycle', sensitivity: 'low' },
];

const SENSITIVITY_COLOR = { low: 'green', medium: 'orange', high: 'red' } as const;

interface Props {
  shared: ReturnType<typeof useCompanionShared>;
}

const CollectTab: React.FC<Props> = ({ shared }) => {
  const { t } = useTranslation();
  const { sharedConfig, patchSharedConfig } = shared;
  const [stats, setStats] = useState<ICompanionSourceStats[]>([]);
  const [rawEvents, setRawEvents] = useState<ICompanionCollectedEvent[] | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);

  const refreshStats = () => {
    void ipcBridge.companion.eventStats
      .invoke()
      .then(setStats)
      .catch(() => {});
  };

  const loadRawEvents = () => {
    void ipcBridge.companion.recentEvents
      .invoke({ limit: 100 })
      .then(setRawEvents)
      .catch((e) => Message.error(String(e)));
  };

  useEffect(() => {
    refreshStats();
    // Counters move as events stream in; poll lightly while the tab is open
    // and refresh on learn completion (which consumes events). Arco keeps
    // inactive panes mounted (display:none), so skip polls while hidden —
    // offsetParent is null for display:none subtrees.
    const timer = setInterval(() => {
      if (rootRef.current?.offsetParent != null) refreshStats();
    }, 15_000);
    const unsubLearn = ipcBridge.companion.onLearnFinished.on(refreshStats);
    return () => {
      clearInterval(timer);
      unsubLearn();
    };
  }, []);

  if (!sharedConfig) {
    return (
      <div className='flex justify-center py-40px'>
        <Spin />
      </div>
    );
  }

  const statFor = (key: string) => stats.find((s) => s.source === key);

  return (
    <div ref={rootRef} className='flex flex-col gap-12px py-8px'>
      <p className='m-0 text-13px text-t-secondary'>{t('nomi.collect.intro')}</p>
      <div className='flex flex-col gap-8px'>
        {SOURCES.map(({ key, sensitivity }) => {
          const stat = statFor(key);
          return (
            <div key={key} className='flex items-center gap-12px bg-fill-2 rd-10px px-12px py-10px'>
              <Switch
                size='small'
                className='compact-dark-switch'
                checked={sharedConfig.collect[key]}
                onChange={(checked) => {
                  void patchSharedConfig({ collect: { [key]: checked } }).catch((e) =>
                    Message.error(String(e))
                  );
                }}
              />
              <div className='flex-1 min-w-0'>
                <div className='flex items-center gap-8px'>
                  <span className='text-14px text-t-primary font-500'>{t(`nomi.collect.sources.${key}.name`)}</span>
                  <Tag size='small' color={SENSITIVITY_COLOR[sensitivity]}>
                    {t(`nomi.collect.sensitivity.${sensitivity}`)}
                  </Tag>
                </div>
                <div className='text-12px text-t-tertiary mt-2px'>{t(`nomi.collect.sources.${key}.desc`)}</div>
              </div>
              <div className='text-12px text-t-secondary shrink-0 text-right'>
                <div>
                  {t('nomi.collect.today')}: {stat?.today ?? 0}
                </div>
                <div>
                  {t('nomi.collect.total')}: {stat?.total ?? 0}
                </div>
              </div>
            </div>
          );
        })}
      </div>
      <div className='flex items-center gap-12px flex-wrap'>
        <Popconfirm
          title={t('nomi.collect.disableAllConfirm', {
            defaultValue: '停止所有采集、学习与进化？已学到的技能和记忆会保留，模型配置不变，随时可再开启。',
          })}
          onOk={() => {
            void ipcBridge.companion.disableAll
              .invoke()
              .then(() => {
                void shared.refresh();
                refreshStats();
                Message.success(t('nomi.collect.disabledAll', { defaultValue: '已全部关闭' }));
              })
              .catch((e) => Message.error(String(e)));
          }}
        >
          <Button status='danger' type='primary'>
            {t('nomi.collect.disableAll', { defaultValue: '一键全关' })}
          </Button>
        </Popconfirm>
        <Popconfirm
          title={t('nomi.collect.clearConfirm')}
          onOk={() => {
            void ipcBridge.companion.clearEvents
              .invoke()
              .then(() => {
                Message.success(t('nomi.collect.cleared'));
                refreshStats();
                if (rawEvents) loadRawEvents();
              })
              .catch((e) => Message.error(String(e)));
          }}
        >
          <Button status='danger'>{t('nomi.collect.clear')}</Button>
        </Popconfirm>
        <span className='text-12px text-t-tertiary'>{t('nomi.collect.clearHint')}</span>
      </div>

      <div className='flex flex-col gap-8px'>
        <Button
          size='small'
          className='self-start'
          onClick={() => {
            if (rawEvents) {
              setRawEvents(null);
            } else {
              loadRawEvents();
            }
          }}
        >
          {rawEvents
            ? t('nomi.collect.hideRaw', { defaultValue: '收起采集内容' })
            : t('nomi.collect.viewRaw', { defaultValue: '查看采集到的内容' })}
        </Button>
        {rawEvents && (
          <div className='flex flex-col gap-4px max-h-360px overflow-y-auto bg-fill-1 rd-10px p-10px'>
            {rawEvents.length === 0 ? (
              <span className='text-12px text-t-tertiary'>
                {t('nomi.collect.rawEmpty', { defaultValue: '还没有采集到任何内容。' })}
              </span>
            ) : (
              rawEvents.map((ev) => (
                <div key={ev.event_id} className='flex items-start gap-8px text-12px'>
                  <Tag size='small' color='arcoblue'>
                    {ev.source}
                  </Tag>
                  <span className='text-t-tertiary shrink-0'>{new Date(ev.ts).toLocaleTimeString()}</span>
                  <span className='text-t-secondary break-all min-w-0'>{JSON.stringify(ev.data)}</span>
                </div>
              ))
            )}
          </div>
        )}
      </div>
    </div>
  );
};

export default CollectTab;
