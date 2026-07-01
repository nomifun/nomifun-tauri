/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { ICompanionWithStatus } from '@/common/adapter/ipcBridge';
import CompanionAvatar from '@renderer/pages/companion/CompanionAvatar';
import type { CompanionMood } from '@renderer/pages/companion/characters';
import { customFigureMetaOf } from '@renderer/pages/companion/characters/customMeta';
import { useCompanions } from '@renderer/pages/nomi/useNomi';
import { cleanupSiderTooltips } from '@renderer/utils/ui/siderTooltip';
import { Message, Tooltip } from '@arco-design/web-react';
import classNames from 'classnames';
import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';

import {
  COMPANION_COLLAPSED_LIST_LIMIT,
  getVisibleCompanionEntries,
} from './utils/companionVisibleEntries';

interface Props {
  /** Active conversation id parsed from the `/conversation/:id` route, for row highlight. */
  activeConversationId: number | null;
  /** Icon-only rail variant (parent sider collapsed). */
  collapsed?: boolean;
  /** Closes the mobile drawer / clears tooltips after navigating, mirrors the workpath list. */
  onSessionClick?: () => void;
  /** Fold state of the group (persisted in useWorkpathUiState). Ignored in the collapsed rail. */
  expanded?: boolean;
  /** Toggles the persisted fold state. */
  onToggleExpanded?: () => void;
}

const modelReadyOf = (c: ICompanionWithStatus) => Boolean(c.model?.provider_id && c.model?.model);

/**
 * 会话侧边栏顶部的「桌面伙伴」专属工作空间分组（roster-driven）。
 *
 * 把伙伴聊天迁进「会话」：数据源是伙伴花名册（useCompanions），每个伙伴 = 一行 =
 * 其唯一专属会话（单会话契约）。点击行解析（幂等 ensure）该伙伴的会话并跳转标准
 * `/conversation/:id`（由 ChatConversation 识别 extra.companionSession 渲染受限聊天）。
 *
 * 与项目/工作路径分组的区别：仅交互式会话（无终端子组）、不在此新建（创建仍在管理中心
 * /nomi）。未配置模型的伙伴点击跳转管理中心引导配置，而非创建会话（后端会 400）。
 *
 * 不触碰工作会话过滤器：伙伴会话仍被 useConversationListSync 过滤出项目分组，故不会重复列出。
 */
const CompanionSessionGroup: React.FC<Props> = ({
  activeConversationId,
  collapsed = false,
  onSessionClick,
  expanded = true,
  onToggleExpanded,
}) => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { companions } = useCompanions();
  const [showAllCompanions, setShowAllCompanions] = useState(false);

  // companionId → 其唯一会话 id（只读解析，用于活动行高亮 + 点击直达，避免无谓 ensure）。
  // 随花名册变化重解析；getCompanionSession 对未建会话返回 null（不入表）。
  const [sessionMap, setSessionMap] = useState<Map<string, number>>(new Map());
  const rosterKey = companions.map((c) => c.id).join(',');
  useEffect(() => {
    if (companions.length === 0) {
      setSessionMap((prev) => (prev.size === 0 ? prev : new Map()));
      return;
    }
    let cancelled = false;
    void Promise.all(
      companions.map(async (c) => {
        try {
          const r = await ipcBridge.companion.getCompanionSession.invoke({ companion_id: c.id });
          return [c.id, r.conversation_id] as const;
        } catch {
          return [c.id, null] as const;
        }
      })
    ).then((entries) => {
      if (cancelled) return;
      const next = new Map<string, number>();
      for (const [id, cid] of entries) if (cid != null) next.set(id, cid);
      setSessionMap(next);
    });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rosterKey]);

  const handleOpen = useCallback(
    async (c: ICompanionWithStatus) => {
      cleanupSiderTooltips();
      onSessionClick?.();
      const cached = sessionMap.get(c.id);
      if (cached != null) {
        void navigate(`/conversation/${cached}`);
        return;
      }
      // 未配置模型：无法 ensure（后端 400）→ 跳管理中心引导配置。
      if (!modelReadyOf(c)) {
        void navigate(`/nomi?companion=${encodeURIComponent(c.id)}&tab=overview`);
        return;
      }
      try {
        const thread = await ipcBridge.companion.ensureCompanionSession.invoke({ companion_id: c.id });
        setSessionMap((prev) => new Map(prev).set(c.id, thread.conversation_id));
        void navigate(`/conversation/${thread.conversation_id}`);
      } catch (e) {
        Message.error(String(e));
      }
    },
    [navigate, onSessionClick, sessionMap]
  );

  // 无伙伴时不渲染分组（避免对不使用伙伴的用户造成噪音；创建后经 WS 刷新即出现）。
  if (companions.length === 0) return null;

  if (collapsed) {
    return (
      <div className='min-w-0 flex flex-col items-center gap-4px mb-4px'>
        {companions.map((c) => {
          const active = activeConversationId != null && sessionMap.get(c.id) === activeConversationId;
          return (
            <Tooltip key={c.id} content={c.name} position='right' mini>
              <div
                role='button'
                aria-label={c.name}
                onClick={() => void handleOpen(c)}
                className={classNames(
                  'flex items-center justify-center w-36px h-36px rd-10px cursor-pointer transition-colors',
                  active ? '!bg-primary-1' : 'hover:bg-fill-2 active:bg-fill-3'
                )}
              >
                <CompanionAvatar
                  character={c.character}
                  companionId={c.id}
                  customFigure={customFigureMetaOf(c)}
                  mood={(c.status.mood as CompanionMood) || 'content'}
                  activity='idle'
                  size={28}
                />
              </div>
            </Tooltip>
          );
        })}
      </div>
    );
  }

  const activeCompanionIndex =
    activeConversationId == null
      ? -1
      : companions.findIndex((c) => sessionMap.get(c.id) === activeConversationId);
  const forceShowActiveCompanion =
    activeCompanionIndex >= COMPANION_COLLAPSED_LIST_LIMIT;
  const visibleCompanions = getVisibleCompanionEntries(
    companions,
    showAllCompanions || forceShowActiveCompanion
  );

  return (
    <div className='min-w-0 mb-2px'>
      {/* 与「项目/工作路径」完全同款的纯 section 标题（无边框/盒子/箭头，只有标签 + 数字）。
          点击整行切换持久化折叠态（默认展开）。 */}
      <div className='px-2px'>
        <div
          className='h-22px px-2px flex items-center justify-between gap-8px select-none cursor-pointer min-w-0'
          onClick={() => onToggleExpanded?.()}
        >
          <span className='text-13px text-t-tertiary font-[500] leading-none tracking-wide truncate min-w-0'>
            {t('sessionList.companionGroup')}
          </span>
          <span className='text-12px text-t-tertiary leading-none shrink-0'>{companions.length}</span>
        </div>
      </div>

      {expanded && (
        <div className='flex flex-col gap-2px mt-2px'>
          {visibleCompanions.entries.map((c) => {
            const active = activeConversationId != null && sessionMap.get(c.id) === activeConversationId;
            const modelReady = modelReadyOf(c);
            return (
              <div
                key={c.id}
                onClick={() => void handleOpen(c)}
                className={classNames(
                  'group flex items-center gap-8px shrink-0 rd-10px px-8px py-6px cursor-pointer transition-colors box-border',
                  active ? '!bg-primary-1 !text-primary-6' : 'hover:bg-fill-2 active:bg-fill-3'
                )}
              >
                <div className='relative shrink-0'>
                  <CompanionAvatar
                    character={c.character}
                    companionId={c.id}
                    customFigure={customFigureMetaOf(c)}
                    mood={(c.status.mood as CompanionMood) || 'content'}
                    activity='idle'
                    size={32}
                  />
                  <span
                    className='absolute -right-1px -bottom-1px w-9px h-9px rd-full border-2 border-[var(--color-bg-1)]'
                    style={{ background: modelReady ? 'rgb(var(--success-6))' : 'rgb(var(--warning-6))' }}
                    title={modelReady ? undefined : t('nomi.chat.modelUnset')}
                  />
                </div>
                <div className='flex flex-col gap-1px min-w-0 flex-1'>
                  <span
                    className={classNames(
                      'text-13px font-600 truncate min-w-0',
                      active ? '!text-primary-6' : 'text-t-primary'
                    )}
                  >
                    {c.name}
                  </span>
                  <span className={classNames('text-11px', active ? 'text-primary-6 opacity-70' : 'text-t-tertiary')}>
                    Lv{c.status.level}
                  </span>
                </div>
              </div>
            );
          })}
          {visibleCompanions.hasOverflow && !forceShowActiveCompanion && (
            <button
              type='button'
              aria-expanded={showAllCompanions}
              className='ml-48px mt-1px mb-2px inline-flex h-20px w-fit max-w-full appearance-none items-center border-none bg-transparent p-0 text-left text-12px leading-20px text-t-quaternary transition-colors cursor-pointer select-none hover:text-t-secondary focus:outline-none focus-visible:text-t-primary'
              onClick={(e) => {
                e.stopPropagation();
                setShowAllCompanions((value) => !value);
              }}
            >
              {showAllCompanions
                ? t('sessionList.collapseDisplay')
                : t('sessionList.expandDisplay', { count: visibleCompanions.hiddenCount })}
            </button>
          )}
        </div>
      )}
    </div>
  );
};

export default CompanionSessionGroup;
