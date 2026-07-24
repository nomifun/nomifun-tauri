/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Empty, Message, Popconfirm, Spin, Tag } from '@arco-design/web-react';
import { Check, CloseSmall, FullScreen } from '@icon-park/react';
import Diff2Html from '@renderer/components/media/Diff2Html';
import { ipcBridge } from '@/common';
import type { IKnowledgeInboxDiff, IKnowledgeInboxEntry } from '@/common/adapter/ipcBridge';
import { knowledgeErrorText } from './useKnowledge';
import type { KnowledgeBaseId } from '@/common/types/ids';

interface InboxReviewPanelProps {
  baseId: KnowledgeBaseId;
  items: IKnowledgeInboxEntry[];
  loading: boolean;
  /** Refetch inbox + base after a merge/discard. */
  onChanged: () => void;
}

/**
 * Render a human-friendly source label from the raw scope id.
 * The scope is an opaque id (session/terminal/companion). We prefix it with a
 * generic label and show a truncated id. No fake name mapping.
 */
function renderScopeLabel(scope: string): string {
  const short = scope.length > 16 ? `${scope.slice(0, 16)}...` : scope;
  return short;
}

/**
 * Review panel for staged write-back proposals (`_inbox/{scope}/{rel}`).
 * Displays a top info banner with batch actions, then a flat card list where
 * each card shows source, diff, and per-item accept/discard actions.
 */
const InboxReviewPanel: React.FC<InboxReviewPanelProps> = ({ baseId, items, loading, onChanged }) => {
  const { t } = useTranslation();
  const [batchActing, setBatchActing] = useState(false);
  const [actingKey, setActingKey] = useState<string | null>(null);
  const [expandedKey, setExpandedKey] = useState<string | null>(null);
  const [diffs, setDiffs] = useState<Record<string, IKnowledgeInboxDiff>>({});
  const [diffLoading, setDiffLoading] = useState<Record<string, boolean>>({});

  // Auto-expand first item
  useEffect(() => {
    if (items.length > 0 && !expandedKey) {
      setExpandedKey(`${items[0].scope}/${items[0].rel_path}`);
    }
    // Clean up expandedKey if item was removed
    if (expandedKey && !items.some((i) => `${i.scope}/${i.rel_path}` === expandedKey)) {
      setExpandedKey(items.length > 0 ? `${items[0].scope}/${items[0].rel_path}` : null);
    }
  }, [items]); // eslint-disable-line react-hooks/exhaustive-deps

  // Fetch diff for expanded item
  useEffect(() => {
    if (!expandedKey) return;
    if (diffs[expandedKey]) return; // already fetched
    const entry = items.find((i) => `${i.scope}/${i.rel_path}` === expandedKey);
    if (!entry) return;

    let cancelled = false;
    setDiffLoading((prev) => ({ ...prev, [expandedKey]: true }));
    ipcBridge.knowledge.getInboxDiff
      .invoke({ knowledge_base_id: baseId, scope: entry.scope, path: entry.rel_path })
      .then((res) => {
        if (!cancelled) setDiffs((prev) => ({ ...prev, [expandedKey]: res }));
      })
      .catch((e) => {
        if (!cancelled) Message.error(knowledgeErrorText(e));
      })
      .finally(() => {
        if (!cancelled) setDiffLoading((prev) => ({ ...prev, [expandedKey]: false }));
      });
    return () => {
      cancelled = true;
    };
  }, [expandedKey, baseId, items]); // eslint-disable-line react-hooks/exhaustive-deps

  // ─── Single-item actions (reuse existing logic) ─────────────────────────────
  const handleMerge = async (entry: IKnowledgeInboxEntry) => {
    const key = `${entry.scope}/${entry.rel_path}`;
    if (actingKey) return;
    setActingKey(key);
    try {
      await ipcBridge.knowledge.mergeInbox.invoke({ knowledge_base_id: baseId, scope: entry.scope, path: entry.rel_path });
      Message.success(t('knowledge.inbox.mergeOk', { defaultValue: '已接受' }));
      onChanged();
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    } finally {
      setActingKey(null);
    }
  };

  const handleDiscard = async (entry: IKnowledgeInboxEntry) => {
    const key = `${entry.scope}/${entry.rel_path}`;
    if (actingKey) return;
    setActingKey(key);
    try {
      await ipcBridge.knowledge.discardInbox.invoke({ knowledge_base_id: baseId, scope: entry.scope, path: entry.rel_path });
      Message.success(t('knowledge.inbox.discardOk', { defaultValue: '已丢弃' }));
      onChanged();
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    } finally {
      setActingKey(null);
    }
  };

  // ─── Batch actions ──────────────────────────────────────────────────────────
  const handleMergeAll = async () => {
    if (batchActing) return;
    setBatchActing(true);
    try {
      await ipcBridge.knowledge.mergeAllInbox.invoke({ kbId: baseId });
      Message.success(t('knowledge.inbox.mergeAllOk', { defaultValue: '已全部接受' }));
      onChanged();
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    } finally {
      setBatchActing(false);
    }
  };

  const handleDiscardAll = async () => {
    if (batchActing) return;
    setBatchActing(true);
    try {
      await ipcBridge.knowledge.discardAllInbox.invoke({ kbId: baseId });
      Message.success(t('knowledge.inbox.discardAllOk', { defaultValue: '已全部丢弃' }));
      onChanged();
    } catch (e) {
      Message.error(knowledgeErrorText(e));
    } finally {
      setBatchActing(false);
    }
  };

  // ─── Empty state ────────────────────────────────────────────────────────────
  if (!loading && items.length === 0) {
    return (
      <div className='flex w-full items-center justify-center py-48px'>
        <Empty description={t('knowledge.detail.inboxEmpty', { defaultValue: '暂无待审内容' })} />
      </div>
    );
  }

  return (
    <Spin loading={loading} className='w-full'>
      <div className='flex w-full flex-col gap-16px'>
        {/* ─── Top info banner with batch actions ─── */}
        <div className='flex flex-wrap items-center justify-between gap-12px rd-12px border border-solid border-[var(--color-warning-light-4)] bg-[var(--color-warning-light-1)] py-14px px-16px'>
          <div className='min-w-0 flex-1'>
            <div className='text-13px font-600 text-[var(--color-text-1)]'>
              {t('knowledge.detail.inbox.bannerTitle', {
                count: items.length,
                defaultValue: `AI 在会话中沉淀了 ${items.length} 条新知识，待你确认`,
              })}
            </div>
            <div className='mt-2px text-12px text-[var(--color-text-3)]'>
              {t('knowledge.detail.inbox.bannerDesc', {
                defaultValue: '开启「回血 · 暂存审阅」后，模型学到的新东西会先进这里，由你决定是否并入知识库。',
              })}
            </div>
          </div>
          <div className='flex shrink-0 gap-8px'>
            <Popconfirm
              title={t('knowledge.detail.inbox.discardAllConfirm', { defaultValue: '确认丢弃全部待审内容？' })}
              onOk={() => void handleDiscardAll()}
            >
              <Button size='small' status='danger' loading={batchActing}>
                {t('knowledge.detail.inbox.discardAll', { defaultValue: '全部丢弃' })}
              </Button>
            </Popconfirm>
            <Popconfirm
              title={t('knowledge.detail.inbox.mergeAllConfirm', { defaultValue: '确认接受全部待审内容？' })}
              onOk={() => void handleMergeAll()}
            >
              <Button size='small' type='primary' loading={batchActing}>
                {t('knowledge.detail.inbox.mergeAll', { defaultValue: '全部接受' })}
              </Button>
            </Popconfirm>
          </div>
        </div>

        {/* ─── Proposal cards ─── */}
        {items.map((entry) => {
          const key = `${entry.scope}/${entry.rel_path}`;
          const isExpanded = expandedKey === key;
          const diff = diffs[key];
          const isDiffLoading = diffLoading[key] ?? false;
          const isActing = actingKey === key;

          return (
            <div
              key={key}
              className='overflow-hidden rd-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)]'
            >
              {/* Card header */}
              <div className='flex flex-wrap items-center justify-between gap-10px border-b border-solid border-[var(--color-border-2)] px-16px py-12px'>
                <div className='min-w-0 flex-1'>
                  <div className='flex items-center gap-8px text-13px text-[var(--color-text-1)]'>
                    <span className='truncate font-500' title={entry.rel_path}>
                      {entry.rel_path}
                    </span>
                    {diff?.is_new && (
                      <Tag size='small' color='green'>
                        {t('knowledge.inbox.newDoc', { defaultValue: '新增' })}
                      </Tag>
                    )}
                  </div>
                  <div className='mt-2px text-11px text-[var(--color-text-3)]'>
                    {t('knowledge.detail.inbox.fromScope', {
                      scope: renderScopeLabel(entry.scope),
                      defaultValue: `来自 ${renderScopeLabel(entry.scope)}`,
                    })}
                  </div>
                </div>
                <div className='flex shrink-0 gap-7px'>
                  <Button
                    size='mini'
                    type='primary'
                    loading={isActing}
                    icon={<Check theme='outline' size='12' />}
                    onClick={() => void handleMerge(entry)}
                  >
                    {t('knowledge.inbox.merge', { defaultValue: '接受' })}
                  </Button>
                  <Button
                    size='mini'
                    status='danger'
                    loading={isActing}
                    icon={<CloseSmall theme='outline' size='12' />}
                    onClick={() => void handleDiscard(entry)}
                  >
                    {t('knowledge.inbox.discard', { defaultValue: '丢弃' })}
                  </Button>
                  <Button
                    size='mini'
                    icon={<FullScreen theme='outline' size='12' />}
                    onClick={() => setExpandedKey(isExpanded ? null : key)}
                  >
                    {isExpanded
                      ? t('knowledge.detail.inbox.collapse', { defaultValue: '收起' })
                      : t('knowledge.detail.inbox.viewFull', { defaultValue: '查看全文' })}
                  </Button>
                </div>
              </div>

              {/* Diff / expanded content */}
              {isExpanded && (
                <div className='px-16px py-10px'>
                  {isDiffLoading ? (
                    <Spin className='w-full py-16px' />
                  ) : diff ? (
                    <div className='max-h-400px overflow-y-auto'>
                      <Diff2Html diff={diff.unified_diff} title={entry.rel_path} />
                    </div>
                  ) : null}
                </div>
              )}
            </div>
          );
        })}
      </div>
    </Spin>
  );
};

export default InboxReviewPanel;
