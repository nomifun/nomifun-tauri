/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Empty, Radio, Spin, Tag } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import type { ICompanionDayDigest } from '@/common/adapter/ipcBridge';

interface Props {
  companionId: string;
}

/** `YYYYMMDD` → `YYYY-MM-DD` (defensive on unexpected shapes). */
const formatDay = (day: string): string =>
  day.length === 8 ? `${day.slice(0, 4)}-${day.slice(4, 6)}-${day.slice(6, 8)}` : day;

/** Today's `MMDD` (local), the key for the "去年今日" query. */
const todayMmdd = (): string => {
  const now = new Date();
  return `${String(now.getMonth() + 1).padStart(2, '0')}${String(now.getDate()).padStart(2, '0')}`;
};

const parseHighlights = (raw: string | null): { topics: string[]; mood?: string } => {
  if (!raw) return { topics: [] };
  try {
    const h = JSON.parse(raw) as { topics?: unknown; mood?: unknown };
    return {
      topics: Array.isArray(h.topics) ? (h.topics.filter((x) => typeof x === 'string') as string[]) : [],
      mood: typeof h.mood === 'string' ? h.mood : undefined,
    };
  } catch {
    return { topics: [] };
  }
};

/**
 * 会话回顾（伙伴会话窗口归档）：把伙伴的每段空闲会话压缩成的日记，按天倒序展示；
 * 「去年今日」筛出往年同月同日的记录。数据源 GET /api/companion/companions/{id}/digests。
 */
const ReviewTab: React.FC<Props> = ({ companionId }) => {
  const { t } = useTranslation();
  const [mode, setMode] = useState<'all' | 'onThisDay'>('all');
  const [digests, setDigests] = useState<ICompanionDayDigest[]>([]);
  const [loading, setLoading] = useState(true);

  const load = useCallback(() => {
    setLoading(true);
    const params =
      mode === 'onThisDay'
        ? { companion_id: companionId, on_day: todayMmdd(), limit: 60 }
        : { companion_id: companionId, limit: 120 };
    void ipcBridge.companion.listDayDigests
      .invoke(params)
      .then(setDigests)
      .catch(() => setDigests([]))
      .finally(() => setLoading(false));
  }, [companionId, mode]);

  useEffect(() => {
    load();
  }, [load]);

  // Group by local start day, most-recent day first.
  const groups = useMemo(() => {
    const byDay = new Map<string, ICompanionDayDigest[]>();
    for (const d of digests) {
      const arr = byDay.get(d.session_day) ?? [];
      arr.push(d);
      byDay.set(d.session_day, arr);
    }
    return Array.from(byDay.entries()).sort((a, b) => b[0].localeCompare(a[0]));
  }, [digests]);

  return (
    <div className='flex flex-col gap-12px py-8px'>
      <Radio.Group type='button' value={mode} onChange={(v) => setMode(v as 'all' | 'onThisDay')}>
        <Radio value='all'>{t('nomi.archive.reviewTitle')}</Radio>
        <Radio value='onThisDay'>{t('nomi.archive.onThisDay')}</Radio>
      </Radio.Group>

      {loading ? (
        <div className='flex justify-center py-40px'>
          <Spin />
        </div>
      ) : groups.length === 0 ? (
        <Empty description={t(mode === 'onThisDay' ? 'nomi.archive.onThisDayEmpty' : 'nomi.archive.reviewEmpty')} />
      ) : (
        <div className='flex flex-col gap-16px'>
          {groups.map(([day, items]) => (
            <div key={day} className='flex flex-col gap-8px'>
              <div className='text-13px text-t-secondary font-500 sticky top-0 bg-bg-1 py-2px'>{formatDay(day)}</div>
              {items.map((d) => {
                const { topics, mood } = parseHighlights(d.highlights);
                return (
                  <div key={d.id} className='bg-fill-2 rd-10px px-14px py-12px flex flex-col gap-8px'>
                    <div className='text-14px text-t-primary leading-relaxed whitespace-pre-wrap'>
                      {d.digest || '—'}
                    </div>
                    <div className='flex items-center gap-6px flex-wrap'>
                      {mood && <Tag color='arcoblue'>{mood}</Tag>}
                      {topics.map((topic) => (
                        <Tag key={topic}>{topic}</Tag>
                      ))}
                      <span className='text-11px text-t-tertiary ml-auto'>
                        {new Date(d.started_at).toLocaleTimeString()} · {d.message_count}
                      </span>
                    </div>
                  </div>
                );
              })}
            </div>
          ))}
        </div>
      )}
    </div>
  );
};

export default ReviewTab;
