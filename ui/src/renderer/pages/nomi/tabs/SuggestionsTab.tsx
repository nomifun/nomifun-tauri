/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import { Button, Empty, Message, Pagination, Radio, Spin, Tag } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import type { ICompanionSuggestion } from '@/common/adapter/ipcBridge';

const KIND_EMOJI: Record<string, string> = {
  guess_question: '💡',
  create_skill: '🛠️',
  create_cron: '⏰',
  unfinished_task: '📌',
  insight: '🔍',
  wellness: '🌙',
  risk: '⚠️',
};

const SuggestionsTab: React.FC = () => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [filter, setFilter] = useState('new');
  const [items, setItems] = useState<ICompanionSuggestion[]>([]);
  const [loading, setLoading] = useState(true);
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(10);
  const [total, setTotal] = useState(0);

  const refreshSeq = useRef(0);

  const refresh = useCallback(async () => {
    const seq = ++refreshSeq.current;
    setLoading(true);
    try {
      const result = await ipcBridge.companion.listSuggestions.invoke({
        status: filter === 'all' ? undefined : filter,
        limit: pageSize,
        offset: (page - 1) * pageSize,
      });
      if (seq === refreshSeq.current) {
        const maxPage = Math.max(1, Math.ceil(result.total / pageSize));
        setTotal(result.total);
        if (page > maxPage) {
          setPage(maxPage);
          return;
        }
        setItems(result.items);
      }
    } catch (e) {
      if (seq === refreshSeq.current) Message.error(String(e));
    } finally {
      if (seq === refreshSeq.current) setLoading(false);
    }
  }, [filter, page, pageSize]);

  useEffect(() => {
    setPage(1);
  }, [filter, pageSize]);

  useEffect(() => {
    void refresh();
    const unsubCreated = ipcBridge.companion.onSuggestionCreated.on(() => void refresh());
    // Another surface (desktop bubble, console) decided a suggestion — refresh
    // so we don't keep a stale `new` card that 404s when clicked.
    const unsubDecided = ipcBridge.companion.onSuggestionDecided.on(() => void refresh());
    return () => {
      unsubCreated();
      unsubDecided();
    };
  }, [refresh]);

  const decide = useCallback(
    async (s: ICompanionSuggestion, accept: boolean) => {
      try {
        await ipcBridge.companion.decideSuggestion.invoke({ id: s.id, accept });
        void refresh();
        if (accept && s.action?.type === 'navigate' && s.action.to) {
          void navigate(s.action.to);
        }
      } catch (e) {
        // Refresh too — drops a stale card instead of leaving it clickable to
        // re-fail. (Backend decide is idempotent, so "already decided" no
        // longer throws; this only fires on genuine errors.)
        void refresh();
        Message.error(String(e));
      }
    },
    [navigate, refresh]
  );

  const handlePageChange = useCallback(
    (nextPage: number, nextPageSize: number) => {
      const pageSizeChanged = nextPageSize !== pageSize;
      if (pageSizeChanged) setPageSize(nextPageSize);
      setPage(pageSizeChanged ? 1 : nextPage);
    },
    [pageSize]
  );

  const initialLoading = loading && items.length === 0 && total === 0;

  return (
    <div className='flex flex-col gap-12px py-8px'>
      <Radio.Group type='button' value={filter} onChange={setFilter}>
        <Radio value='new'>{t('nomi.suggestions.filterNew')}</Radio>
        <Radio value='accepted'>{t('nomi.suggestions.filterAccepted')}</Radio>
        <Radio value='dismissed'>{t('nomi.suggestions.filterDismissed')}</Radio>
        <Radio value='all'>{t('nomi.suggestions.filterAll')}</Radio>
      </Radio.Group>
      {initialLoading ? (
        <div className='flex justify-center py-40px'>
          <Spin />
        </div>
      ) : items.length === 0 ? (
        <Empty description={t('nomi.suggestions.empty')} />
      ) : (
        <div className='flex flex-col gap-8px transition-opacity duration-150' style={{ opacity: loading ? 0.6 : 1 }}>
          {items.map((s) => (
            <div key={s.id} className='flex items-start gap-10px bg-fill-2 rd-10px px-12px py-10px'>
              <span className='text-18px leading-none mt-2px'>{KIND_EMOJI[s.kind] || '🐰'}</span>
              <div className='flex-1 min-w-0'>
                <div className='flex items-center gap-8px'>
                  <span className='text-14px font-600 text-t-primary'>{s.title}</span>
                  {s.status !== 'new' && (
                    <Tag size='small' color={s.status === 'accepted' ? 'green' : 'gray'}>
                      {t(`nomi.suggestions.status_${s.status}`)}
                    </Tag>
                  )}
                </div>
                <div className='text-13px text-t-secondary mt-2px break-words'>{s.body}</div>
                <div className='text-11px text-t-tertiary mt-4px'>{new Date(s.created_at).toLocaleString()}</div>
              </div>
              {s.status === 'new' && (
                <div className='flex items-center gap-6px shrink-0'>
                  <Button size='mini' type='primary' onClick={() => void decide(s, true)}>
                    {t('nomi.suggestions.accept')}
                  </Button>
                  <Button size='mini' onClick={() => void decide(s, false)}>
                    {t('nomi.suggestions.dismiss')}
                  </Button>
                </div>
              )}
            </div>
          ))}
        </div>
      )}

      {total > 0 && (
        <div className='flex justify-end pt-2px'>
          <Pagination
            current={page}
            pageSize={pageSize}
            total={total}
            showTotal
            sizeCanChange
            sizeOptions={[10, 20, 50]}
            showJumper={total > pageSize}
            onChange={handlePageChange}
          />
        </div>
      )}
    </div>
  );
};

export default SuggestionsTab;
