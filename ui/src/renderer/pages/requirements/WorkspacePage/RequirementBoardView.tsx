/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * RequirementBoardView — the workspace board (Kanban) surface.
 *
 * Purely presentational: the parent fetches `items` and owns mutations via
 * callbacks. Unlike the legacy kanban, this view DEFAULTS to showing ALL
 * requirements — there is no tag gating and no "select a tag" placeholder. The
 * six status columns are always rendered, even when empty.
 *
 * Items are grouped into the six status columns client-side. Each column is a
 * native HTML5 drop target: dropping a card whose status differs from the
 * column fires `onStatusChange(id, columnStatus)`. The dragged id is recovered
 * from internal state (seeded by the card's `onDragStart`) with a dataTransfer
 * fallback, so drops work regardless of which path delivered the id.
 */
import type { IRequirement, RequirementStatus } from '@/common/adapter/ipcBridge';
import { Tag } from '@arco-design/web-react';
import React, { useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import RequirementBoardCard from './RequirementBoardCard';
import type { RequirementId } from '@/common/types/ids';

interface RequirementBoardViewProps {
  items: IRequirement[];
  onOpenDetail: (id: RequirementId) => void;
  onStatusChange: (id: RequirementId, next: RequirementStatus) => void;
}

/** The six statuses, in display order — always rendered as columns. */
const COLUMNS: RequirementStatus[] = ['pending', 'in_progress', 'needs_review', 'done', 'failed', 'cancelled'];

/** Per-status accent, expressed as theme palette tokens (never hex). Mirrors StatusPill. */
const STATUS_ACCENT: Record<RequirementStatus, string> = {
  pending: 'rgb(var(--gray-6))',
  in_progress: 'rgb(var(--primary-6))',
  needs_review: 'rgb(var(--purple-6))',
  done: 'rgb(var(--green-6))',
  failed: 'rgb(var(--red-6))',
  cancelled: 'rgb(var(--orange-6))',
};

const RequirementBoardView: React.FC<RequirementBoardViewProps> = ({ items, onOpenDetail, onStatusChange }) => {
  const { t } = useTranslation();

  // The id currently being dragged. Seeded by a card's onDragStart, read on drop.
  const draggedIdRef = useRef<RequirementId | null>(null);
  // Column the pointer is hovering over — drives a soft drop-affordance highlight.
  const [dropTarget, setDropTarget] = useState<RequirementStatus | null>(null);

  // Group items into the six columns; statuses outside COLUMNS are dropped.
  const grouped = useMemo(() => {
    const map: Record<RequirementStatus, IRequirement[]> = {
      pending: [],
      in_progress: [],
      needs_review: [],
      done: [],
      failed: [],
      cancelled: [],
    };
    for (const item of items) {
      const bucket = map[item.status];
      if (bucket) bucket.push(item);
    }
    return map;
  }, [items]);

  /** Resolve the dragged id from internal state, falling back to dataTransfer. */
  const resolveDraggedId = (e: React.DragEvent<HTMLDivElement>): RequirementId | null => {
    if (draggedIdRef.current != null) return draggedIdRef.current;
    const raw = e.dataTransfer.getData('text/plain');
    return raw !== '' && raw.startsWith('req_') ? (raw as RequirementId) : null;
  };

  const handleDrop = (e: React.DragEvent<HTMLDivElement>, columnStatus: RequirementStatus) => {
    e.preventDefault();
    setDropTarget(null);
    const id = resolveDraggedId(e);
    draggedIdRef.current = null;
    if (id == null) return;
    const dragged = items.find((it) => it.id === id);
    // Only fire when the card is actually moving to a different column.
    if (dragged && dragged.status !== columnStatus) {
      onStatusChange(id, columnStatus);
    }
  };

  return (
    <div className='flex w-full flex-1 min-h-0 items-start gap-12px overflow-x-auto pb-4px'>
      {COLUMNS.map((status) => {
        const colItems = grouped[status];
        const hasItems = colItems.length > 0;
        const isDropTarget = dropTarget === status;
        const accent = STATUS_ACCENT[status];
        return (
          <div
            key={status}
            onDragOver={(e) => {
              e.preventDefault();
              e.dataTransfer.dropEffect = 'move';
              if (dropTarget !== status) setDropTarget(status);
            }}
            onDragLeave={(e) => {
              // Ignore leaves into descendant nodes; only clear when leaving the column.
              if (!e.currentTarget.contains(e.relatedTarget as Node | null)) {
                setDropTarget((cur) => (cur === status ? null : cur));
              }
            }}
            onDrop={(e) => handleDrop(e, status)}
            className={[
              'requirements-board-column flex min-w-260px flex-1 flex-col gap-8px rounded-12px p-8px box-border transition-colors duration-180',
              hasItems ? 'self-stretch' : '',
              isDropTarget
                ? 'bg-[var(--color-fill-2)] border border-dashed border-[var(--color-primary-light-4)]'
                : 'bg-[var(--color-fill-1)] border border-solid border-transparent',
            ].join(' ')}
          >
            {/* Column header: status accent dot + localized label + count. */}
            <div className='flex items-center gap-6px px-4px py-2px text-13px font-500 text-[var(--color-text-2)]'>
              <span
                className='inline-block w-7px h-7px rounded-full flex-shrink-0'
                style={{ backgroundColor: accent }}
              />
              <span className='truncate'>{t(`requirements.status.${status}`)}</span>
              <Tag
                size='small'
                bordered={false}
                className='!ml-auto !flex-shrink-0 !text-11px !leading-16px !px-6px !py-0 !rounded-6px !bg-fill-3 !text-t-tertiary'
              >
                {colItems.length}
              </Tag>
            </div>

            {/* Card column — scrolls independently when tall. */}
            <div
              className={[
                'requirements-board-card-list flex flex-col gap-4px overflow-y-auto min-h-60px',
                hasItems ? 'flex-1 min-h-0' : '',
              ].join(' ')}
            >
              {!hasItems ? (
                <div className='flex flex-1 items-center justify-center py-16px text-12px text-[var(--color-text-3)] select-none'>
                  {t('requirements.kanban.emptyColumn')}
                </div>
              ) : (
                colItems.map((item) => (
                  <RequirementBoardCard
                    key={item.id}
                    item={item}
                    onOpenDetail={onOpenDetail}
                    onDragStart={(id) => {
                      draggedIdRef.current = id;
                    }}
                  />
                ))
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
};

export default RequirementBoardView;
