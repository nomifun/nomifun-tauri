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
import { Calendar, SortTwo, Tag } from '@icon-park/react';
import React from 'react';
import { useTranslation } from 'react-i18next';
import type { RequirementId } from '@/common/types/ids';
import CopyIconButton from '@/renderer/components/base/CopyIconButton';

interface RequirementBoardCardProps {
  item: IRequirement;
  onOpenDetail: (id: RequirementId) => void;
  /** Parent tracks the dragged id (mirrored onto dataTransfer for robustness). */
  onDragStart: (id: RequirementId) => void;
}

const formatCreatedDate = (timestamp: number): string =>
  new Date(timestamp).toLocaleDateString(undefined, { year: 'numeric', month: '2-digit', day: '2-digit' });

const RequirementBoardCard: React.FC<RequirementBoardCardProps> = ({ item, onOpenDetail, onDragStart }) => {
  const { t } = useTranslation();

  const open = () => onOpenDetail(item.requirement_id);

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
        e.dataTransfer.setData('text/plain', item.requirement_id);
        onDragStart(item.requirement_id);
      }}
      className={[
        'requirements-board-card group relative flex flex-col rounded-10px border border-solid p-12px cursor-grab active:cursor-grabbing select-none outline-none',
        'border-[var(--color-border-2)] bg-[var(--color-bg-2)] transition-all duration-180',
        'hover:border-[var(--color-primary-light-4)] hover:shadow-[0_4px_16px_rgba(0,0,0,0.06)]',
        'focus-visible:border-[rgb(var(--primary-5))] focus-visible:shadow-[0_0_0_3px_rgba(var(--primary-6),0.12)]',
      ].join(' ')}
    >
      <div className='flex items-start gap-6px'>
        {/* Title — two-line clamp keeps cards tidy when dragging across columns. */}
        <div
          className='min-w-0 flex-1 text-14px font-400 leading-20px text-[var(--color-text-1)] break-words'
          style={{
            display: '-webkit-box',
            WebkitLineClamp: 2,
            WebkitBoxOrient: 'vertical',
            overflow: 'hidden',
          }}
        >
          {item.title}
        </div>
        <CopyIconButton text={item.requirement_id} tooltip={t('common.copyFullId')} size={13} className='mt-2px shrink-0' />
      </div>

      {/* Labelled metadata keeps the card scannable without competing with the title. */}
      <div className='mt-10px flex flex-col gap-5px text-11px leading-16px text-[var(--color-text-2)]'>
        <div className='grid min-w-0 grid-cols-[16px_66px_minmax(0,1fr)] items-center gap-x-6px'>
          <span className='relative top-2px inline-flex h-16px w-16px flex-shrink-0 items-center justify-center text-[var(--color-text-3)]'>
            <Tag theme='outline' size={13} strokeWidth={3} className='block' />
          </span>
          <span className='text-[var(--color-text-3)] leading-16px'>{t('requirements.columns.tag')}:</span>
          <span className='min-w-0 truncate font-500 leading-16px'>{item.tag || '-'}</span>
        </div>
        <div className='grid min-w-0 grid-cols-[16px_66px_minmax(0,1fr)] items-center gap-x-6px'>
          <span className='relative top-2px inline-flex h-16px w-16px flex-shrink-0 items-center justify-center text-[var(--color-text-3)]'>
            <SortTwo theme='outline' size={13} strokeWidth={3} className='block' />
          </span>
          <span className='text-[var(--color-text-3)] leading-16px'>{t('requirements.sort.label')}:</span>
          <span className='min-w-0 truncate font-500 leading-16px'>{item.order_key || '-'}</span>
        </div>
        <div className='grid min-w-0 grid-cols-[16px_66px_minmax(0,1fr)] items-center gap-x-6px'>
          <span className='relative top-2px inline-flex h-16px w-16px flex-shrink-0 items-center justify-center text-[var(--color-text-3)]'>
            <Calendar theme='outline' size={13} strokeWidth={3} className='block' />
          </span>
          <span className='text-[var(--color-text-3)] leading-16px'>{t('requirements.columns.createdAt')}:</span>
          <span className='min-w-0 truncate font-500 leading-16px'>{formatCreatedDate(item.created_at)}</span>
        </div>
      </div>
    </div>
  );
};

export default RequirementBoardCard;
