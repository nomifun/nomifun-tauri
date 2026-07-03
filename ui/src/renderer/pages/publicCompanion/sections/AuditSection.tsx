/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Empty, Input, InputNumber, Modal, Radio, Select, Spin } from '@arco-design/web-react';
import { Comment, Delete, History, Power, Refresh, Search } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type {
  IPublicAgent,
  IPublicAgentAuditEntry,
  IPublicAgentPatch,
  PublicAgentAuditKind,
} from '@/common/adapter/ipcBridge';
import type { ArcoMessageInstance } from '@renderer/utils/ui/useArcoMessage';
import { SectionCard, formatRelative, surfaceLabel } from '../components';

interface Props {
  agent: IPublicAgent;
  patch: (p: IPublicAgentPatch) => Promise<IPublicAgent | undefined>;
  message: ArcoMessageInstance;
}

type KindFilter = 'all' | PublicAgentAuditKind;
type DaysFilter = 'all' | 7 | 30 | 90;
const PAGE_SIZE = 50;

/** One audit-log row. */
const AuditRow: React.FC<{ entry: IPublicAgentAuditEntry; t: ReturnType<typeof useTranslation>['t'] }> = ({
  entry,
  t,
}) => {
  const isExposure = entry.kind === 'exposure_change';
  return (
    <div className='flex items-start gap-10px rd-10px border border-solid border-[var(--color-border-2)] bg-fill-1 px-12px py-10px'>
      <span
        className={[
          'mt-1px flex shrink-0 items-center justify-center w-24px h-24px rd-7px',
          isExposure
            ? 'text-[rgb(var(--warning-6))] bg-[rgba(var(--warning-6),0.12)]'
            : 'text-[rgb(var(--primary-6))] bg-[rgba(var(--primary-6),0.10)]',
        ].join(' ')}
      >
        {isExposure ? (
          <Power theme='outline' size='13' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
        ) : (
          <Comment theme='outline' size='13' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
        )}
      </span>
      <div className='min-w-0 flex-1'>
        <div className='flex items-center gap-6px flex-wrap'>
          <span className='inline-flex items-center rd-full px-7px py-1px text-10px font-600 leading-none text-t-secondary bg-fill-2 border border-solid border-[var(--color-border-2)]'>
            {surfaceLabel(t, entry.surface, entry.channel_platform)}
          </span>
          <span className='inline-flex items-center rd-full px-7px py-1px text-10px font-500 leading-none text-t-tertiary bg-fill-2'>
            {isExposure
              ? t('publicCompanion.audit.kindExposure', { defaultValue: '配置变更' })
              : t('publicCompanion.audit.kindTurn', { defaultValue: '对话' })}
          </span>
          <span className='text-11px text-t-tertiary'>{formatRelative(t, entry.at)}</span>
        </div>
        <div className='mt-3px text-13px leading-19px text-t-primary break-words'>{entry.detail}</div>
      </div>
    </div>
  );
};

/**
 * 审计 & 分析 —— 倒序展示对外伙伴的对外活动（对话 + 配置变更）。
 * 搜索 / 类型 / 最近 N 天 过滤 + 游标翻页；审计保留天数设置 + 清理历史。
 * 后端未就绪(404)时优雅降级为空态。
 */
const AuditSection: React.FC<Props> = ({ agent, patch, message }) => {
  const { t } = useTranslation();

  const [entries, setEntries] = useState<IPublicAgentAuditEntry[]>([]);
  const [cursor, setCursor] = useState<number | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);

  const [q, setQ] = useState('');
  const [kind, setKind] = useState<KindFilter>('all');
  const [days, setDays] = useState<DaysFilter>(30);

  const [retention, setRetention] = useState<number>(agent.audit_retention_days);
  const [retentionSaving, setRetentionSaving] = useState(false);
  const [cleanupDays, setCleanupDays] = useState<number>(agent.audit_retention_days);

  useEffect(() => {
    setRetention(agent.audit_retention_days);
    setCleanupDays(agent.audit_retention_days);
  }, [agent.id, agent.audit_retention_days]);

  const fetchPage = useCallback(
    async (opts: { reset: boolean; cursor?: number | null }) => {
      const res = await ipcBridge.publicAgent.listAudit.invoke({
        id: agent.id,
        limit: PAGE_SIZE,
        cursor: opts.reset ? undefined : opts.cursor,
        q: q.trim() || undefined,
        kind: kind === 'all' ? undefined : kind,
        days: days === 'all' ? undefined : days,
      });
      return res ?? { entries: [], next_cursor: null };
    },
    [agent.id, q, kind, days]
  );

  // Reload (reset) whenever the filters change. q is debounced.
  useEffect(() => {
    let alive = true;
    const timer = setTimeout(() => {
      setLoading(true);
      void (async () => {
        try {
          const res = await fetchPage({ reset: true });
          if (!alive) return;
          setEntries(res.entries);
          setCursor(res.next_cursor);
        } catch {
          if (!alive) return;
          setEntries([]);
          setCursor(null);
        } finally {
          if (alive) setLoading(false);
        }
      })();
    }, 300);
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [fetchPage]);

  const loadMore = async () => {
    if (cursor == null) return;
    setLoadingMore(true);
    try {
      const res = await fetchPage({ reset: false, cursor });
      setEntries((prev) => [...prev, ...res.entries]);
      setCursor(res.next_cursor);
    } catch (e) {
      message.error(e instanceof Error ? e.message : String(e));
    } finally {
      setLoadingMore(false);
    }
  };

  const reload = () => {
    // Bump by toggling q identity is unnecessary — just call fetchPage directly.
    setLoading(true);
    void (async () => {
      try {
        const res = await fetchPage({ reset: true });
        setEntries(res.entries);
        setCursor(res.next_cursor);
      } catch {
        setEntries([]);
        setCursor(null);
      } finally {
        setLoading(false);
      }
    })();
  };

  const saveRetention = async () => {
    setRetentionSaving(true);
    try {
      await patch({ audit_retention_days: retention });
      message.success(t('common.saveSuccess', { defaultValue: '已保存' }));
    } catch (e) {
      message.error(e instanceof Error ? e.message : String(e));
    } finally {
      setRetentionSaving(false);
    }
  };

  const confirmCleanup = () => {
    Modal.confirm({
      title: t('publicCompanion.audit.cleanupTitle', { defaultValue: '清理历史审计？' }),
      content: t('publicCompanion.audit.cleanupBody', {
        defaultValue: '将永久删除 {{n}} 天前的审计记录，此操作不可撤销。',
        n: cleanupDays,
      }),
      okButtonProps: { status: 'danger' },
      okText: t('publicCompanion.audit.cleanupConfirm', { defaultValue: '清理' }),
      cancelText: t('common.cancel', { defaultValue: '取消' }),
      onOk: async () => {
        try {
          const res = await ipcBridge.publicAgent.clearAudit.invoke({ id: agent.id, older_than_days: cleanupDays });
          message.success(
            t('publicCompanion.audit.cleanupOk', { defaultValue: '已清理 {{n}} 天前的记录', n: res?.deleted_days ?? cleanupDays })
          );
          reload();
        } catch (e) {
          message.error(e instanceof Error ? e.message : String(e));
        }
      },
    });
  };

  return (
    <div className='flex flex-col gap-16px'>
      {/* Log */}
      <SectionCard
        icon={<History theme='outline' size='16' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
        title={t('publicCompanion.audit.title', { defaultValue: '审计日志' })}
        desc={t('publicCompanion.audit.desc', { defaultValue: '对外伙伴的对外活动留痕（对话与配置变更），倒序展示。' })}
        action={
          <div
            role='button'
            tabIndex={0}
            title={t('common.refresh', { defaultValue: '刷新' })}
            onClick={reload}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                reload();
              }
            }}
            className='flex items-center justify-center w-30px h-30px rd-8px text-t-tertiary cursor-pointer hover:bg-fill-2 hover:text-t-primary transition-colors'
          >
            <Refresh theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
          </div>
        }
      >
        {/* Filters */}
        <div className='flex items-center gap-10px flex-wrap mb-12px'>
          <Input
            allowClear
            value={q}
            onChange={setQ}
            prefix={<Search theme='outline' size='14' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
            placeholder={t('publicCompanion.audit.searchPlaceholder', { defaultValue: '搜索内容…' })}
            style={{ width: 220 }}
          />
          <Radio.Group type='button' size='small' value={kind} onChange={(v: KindFilter) => setKind(v)}>
            <Radio value='all'>{t('publicCompanion.audit.kindAll', { defaultValue: '全部' })}</Radio>
            <Radio value='turn'>{t('publicCompanion.audit.kindTurn', { defaultValue: '对话' })}</Radio>
            <Radio value='exposure_change'>{t('publicCompanion.audit.kindExposure', { defaultValue: '配置变更' })}</Radio>
          </Radio.Group>
          <Select value={days} onChange={(v: DaysFilter) => setDays(v)} style={{ width: 128 }} size='small'>
            <Select.Option value={7}>{t('publicCompanion.audit.days7', { defaultValue: '最近 7 天' })}</Select.Option>
            <Select.Option value={30}>{t('publicCompanion.audit.days30', { defaultValue: '最近 30 天' })}</Select.Option>
            <Select.Option value={90}>{t('publicCompanion.audit.days90', { defaultValue: '最近 90 天' })}</Select.Option>
            <Select.Option value='all'>{t('publicCompanion.audit.daysAll', { defaultValue: '全部时间' })}</Select.Option>
          </Select>
        </div>

        {loading ? (
          <div className='flex justify-center py-32px'>
            <Spin />
          </div>
        ) : entries.length === 0 ? (
          <div className='flex justify-center py-28px'>
            <Empty description={t('publicCompanion.audit.empty', { defaultValue: '暂无记录' })} />
          </div>
        ) : (
          <div className='flex flex-col gap-6px'>
            {entries.map((e) => (
              <AuditRow key={e.id} entry={e} t={t} />
            ))}
            {cursor != null && (
              <div className='flex justify-center pt-6px'>
                <Button size='small' loading={loadingMore} onClick={() => void loadMore()}>
                  {t('publicCompanion.audit.loadMore', { defaultValue: '加载更多' })}
                </Button>
              </div>
            )}
          </div>
        )}
      </SectionCard>

      {/* Retention + cleanup */}
      <SectionCard
        icon={<Delete theme='outline' size='16' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
        title={t('publicCompanion.audit.retentionTitle', { defaultValue: '审计保留与清理' })}
        desc={t('publicCompanion.audit.retentionDesc', {
          defaultValue: '设置审计记录的保留天数，并可立即清理更早的历史记录。',
        })}
      >
        <div className='flex flex-col gap-14px'>
          <div className='flex items-center gap-10px flex-wrap'>
            <span className='text-13px text-t-primary w-160px shrink-0'>
              {t('publicCompanion.audit.retentionLabel', { defaultValue: '审计保留天数' })}
            </span>
            <InputNumber
              min={1}
              max={3650}
              value={retention}
              onChange={(v) => setRetention(typeof v === 'number' ? v : 1)}
              style={{ width: 140 }}
              suffix={t('publicCompanion.audit.daysSuffix', { defaultValue: '天' })}
            />
            <Button
              type='primary'
              size='small'
              loading={retentionSaving}
              disabled={retention === agent.audit_retention_days}
              onClick={() => void saveRetention()}
            >
              {t('common.save', { defaultValue: '保存' })}
            </Button>
          </div>

          <div className='flex items-center gap-10px flex-wrap'>
            <span className='text-13px text-t-primary w-160px shrink-0'>
              {t('publicCompanion.audit.cleanupLabel', { defaultValue: '清理更早记录' })}
            </span>
            <InputNumber
              min={1}
              max={3650}
              value={cleanupDays}
              onChange={(v) => setCleanupDays(typeof v === 'number' ? v : 1)}
              style={{ width: 140 }}
              prefix={t('publicCompanion.audit.cleanupPrefix', { defaultValue: '早于' })}
              suffix={t('publicCompanion.audit.daysSuffix', { defaultValue: '天' })}
            />
            <Button size='small' status='danger' onClick={confirmCleanup}>
              <span className='inline-flex items-center gap-5px'>
                <Delete theme='outline' size='13' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
                {t('publicCompanion.audit.cleanupConfirm', { defaultValue: '清理' })}
              </span>
            </Button>
          </div>
        </div>
      </SectionCard>
    </div>
  );
};

export default AuditSection;
