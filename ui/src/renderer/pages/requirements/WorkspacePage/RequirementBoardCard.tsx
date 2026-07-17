/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * RequirementBoardCard — a compact, draggable card for the workspace board.
 *
 * Mirrors PresetCard's surface language (compact rounded bordered surface on
 * bg-2, soft lift on hover) but stripped down for a Kanban column: a title,
 * an order-key chip, a tag chip, and — when the requirement is bound to an
 * executing session — a small session marker.
 *
 * Drag is native HTML5: the card is `draggable`, and `onDragStart` both seeds
 * the parent's dragged-id state and writes the id onto `dataTransfer` so the
 * column drop target can recover it either way. The whole card is clickable →
 * `onOpenDetail`. Theme tokens only; clickable surface uses `role="button"`
 * (no bare <button>).
 */
import type { IRequirement } from '@/common/adapter/ipcBridge';
import { Tag as ArcoTag } from '@arco-design/web-react';
import { Tag, User } from '@icon-park/react';
import React from 'react';
import { useTranslation } from 'react-i18next';
import type { RequirementId } from '@/common/types/ids';
import RequirementDisplayNumber from '../components/RequirementDisplayNumber';

interface RequirementBoardCardProps {
  item: IRequirement;
  onOpenDetail: (id: RequirementId) => void;
  /** Parent tracks the dragged id (mirrored onto dataTransfer for robustness). */
  onDragStart: (id: RequirementId) => void;
}

const RequirementBoardCard: React.FC<RequirementBoardCardProps> = ({ item, onOpenDetail, onDragStart }) => {
  const { t } = useTranslation();

  const open = () => onOpenDetail(item.id);

  return (
    <div
      role='button'
      tabIndex={0}
      draggable
      onClick={open}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          open();
        }
      }}
      onDragStart={(e) => {
        // Seed parent state AND dataTransfer so the column can read either one.
        e.dataTransfer.effectAllowed = 'move';
        e.dataTransfer.setData('text/plain', item.id);
        onDragStart(item.id);
      }}
      className={[
        'requirements-board-card group relative flex flex-col rounded-10px border border-solid p-12px cursor-grab active:cursor-grabbing select-none outline-none',
        'border-[var(--color-border-2)] bg-[var(--color-bg-2)] transition-all duration-180',
        'hover:border-[var(--color-primary-light-4)] hover:shadow-[0_4px_16px_rgba(0,0,0,0.06)]',
        'focus-visible:border-[rgb(var(--primary-5))] focus-visible:shadow-[0_0_0_3px_rgba(var(--primary-6),0.12)]',
      ].join(' ')}
    >
      <div className='flex items-start gap-8px'>
        <RequirementDisplayNumber displayNo={item.display_no} fullId={item.id} />
        {/* Title — two-line clamp keeps cards tidy when dragging across columns. */}
        <div
          className='min-w-0 flex-1 text-13px font-medium leading-18px text-[var(--color-text-1)] break-words'
          style={{
            display: '-webkit-box',
            WebkitLineClamp: 2,
            WebkitBoxOrient: 'vertical',
            overflow: 'hidden',
          }}
        >
          {item.title}
        </div>
      </div>

      {/* Meta row: order-key chip, tag chip, optional session marker. */}
      <div className='mt-10px flex flex-wrap items-center gap-6px'>
        <ArcoTag
          size='small'
          bordered={false}
          className='!flex-shrink-0 !text-11px !leading-16px !px-7px !py-0 !rounded-6px !bg-fill-2 !text-t-secondary'
        >
          {item.order_key || '-'}
        </ArcoTag>
        {item.tag ? (
          <span className='inline-flex items-center gap-3px rounded-[10px] px-7px py-1px text-11px leading-16px bg-[var(--color-fill-2)] text-[var(--color-text-2)] border border-solid border-[var(--color-border-2)] max-w-full'>
            <Tag theme='outline' size={11} strokeWidth={3} className='flex-shrink-0' />
            <span className='truncate'>{item.tag}</span>
          </span>
        ) : null}
        {item.owner_conversation_id != null || item.owner_terminal_id != null ? (
          <span className='inline-flex items-center gap-3px rounded-[10px] px-7px py-1px text-11px leading-16px bg-primary-1 text-primary-6'>
            <User theme='outline' size={11} strokeWidth={3} className='flex-shrink-0' />
            <span>{t('requirements.columns.session')}</span>
          </span>
        ) : null}
      </div>
    </div>
  );
};

export default RequirementBoardCard;
