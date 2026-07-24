/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * RequirementListRow — a single row in the requirements workspace list.
 * It deliberately reads like a headerless table: each item occupies a stable
 * set of columns, with a simple bottom divider instead of a card surface.
 * Layout, left→right: checkbox · title + display number (with a second-line
 * description) · created time · status + tag · edit/delete actions.
 *
 * The whole row is a `<div role="button">` whose background click opens the
 * detail drawer (`onOpenDetail`). Interactive children — checkbox, status pill,
 * edit, delete — stopPropagation so they never bubble into a drawer-open.
 * Theme tokens only; `<div onClick>` / Arco controls, never a bare <button>.
 */
import { Checkbox, Popconfirm } from '@arco-design/web-react';
import { Delete, Edit, Tag } from '@icon-park/react';
import React from 'react';
import { useTranslation } from 'react-i18next';

import type { IRequirement, RequirementStatus } from '@/common/adapter/ipcBridge';
import StatusPill from '../components/StatusPill';
import RequirementDisplayNumber from '../components/RequirementDisplayNumber';
import type { RequirementId } from '@/common/types/ids';

interface RequirementListRowProps {
  item: IRequirement;
  selected: boolean;
  onToggleSelect: (id: RequirementId) => void;
  onOpenDetail: (id: RequirementId) => void; // row click
  onStatusChange: (id: RequirementId, next: RequirementStatus) => void;
  onEdit: (id: RequirementId) => void;
  onDelete: (id: RequirementId) => void;
}

const stop = (e: React.SyntheticEvent) => e.stopPropagation();

// Keep selected rows readable even when a theme defines primary-light-1 as
// a saturated brand fill. The checkbox remains the strongest selected cue.
const SOFT_SELECTED_ROW_STYLE: React.CSSProperties = {
  background: 'linear-gradient(rgba(var(--primary-6), 0.055), rgba(var(--primary-6), 0.055))',
};

// The row is intentionally not a card. Make the single horizontal divider
// explicit so no inherited border shorthand can create a boxed grid.
const ROW_DIVIDER_STYLE: React.CSSProperties = {
  borderTopWidth: 0,
  borderRightWidth: 0,
  borderBottomWidth: 1,
  borderLeftWidth: 0,
  borderBottomStyle: 'solid',
  borderBottomColor: 'var(--color-border-2)',
};

// Keep the title block at 65% of the available row width. Time, status/tag,
// and actions share the remaining space without relying on generated utilities.
const ROW_LAYOUT_STYLE: React.CSSProperties = {
  gridTemplateColumns: '24px minmax(0, 65%) minmax(90px, 0.8fr) minmax(180px, 1.2fr) 56px',
};

const formatCreatedAt = (timestamp: number): string =>
  new Date(timestamp).toLocaleString(undefined, {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
  });

const RequirementListRow: React.FC<RequirementListRowProps> = ({
  item,
  selected,
  onToggleSelect,
  onOpenDetail,
  onStatusChange,
  onEdit,
  onDelete,
}) => {
  const { t } = useTranslation();

  return (
    <div
      role='button'
      tabIndex={0}
      onClick={() => onOpenDetail(item.requirement_id)}
      onKeyDown={(e) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          onOpenDetail(item.requirement_id);
        }
      }}
      className={[
        'requirements-list-row group grid min-h-52px min-w-0 items-center gap-x-8px py-4px cursor-pointer',
        'transition-colors duration-150 hover:bg-[var(--color-fill-1)]',
        selected
          ? 'bg-[var(--color-fill-1)]'
          : '',
      ].join(' ')}
      style={{ ...ROW_DIVIDER_STYLE, ...ROW_LAYOUT_STYLE, ...(selected ? SOFT_SELECTED_ROW_STYLE : {}) }}
    >
      {/* Checkbox — selection, never opens the drawer */}
      <div className='flex-shrink-0' onClick={stop}>
        <Checkbox
          className='requirements-selection-checkbox'
          checked={selected}
          onChange={() => onToggleSelect(item.requirement_id)}
        />
      </div>

      {/* Primary column takes half the list width: title + number, then description. */}
      <div className='flex min-w-0 flex-col justify-center gap-3px'>
        <div className='flex min-w-0 items-center gap-8px'>
          <span className='min-w-0 truncate text-14px font-medium leading-20px text-[var(--color-text-1)]'>
            {item.title}
          </span>
          <RequirementDisplayNumber displayNo={item.display_no} fullId={item.requirement_id} className='!h-auto !min-w-0 !rounded-0 !border-0 !bg-transparent !px-0 !py-0 !font-sans !text-12px !font-normal !text-[var(--color-text-3)] hover:!bg-transparent' />
        </div>
        {item.content && (
          <span
            className='text-12px leading-18px text-[var(--color-text-3)]'
            style={{
              display: '-webkit-box',
              WebkitLineClamp: 1,
              WebkitBoxOrient: 'vertical',
              overflow: 'hidden',
            }}
          >
            {item.content}
          </span>
        )}
      </div>

      {/* Secondary columns use quiet text so the title remains the visual anchor. */}
      <span
        className='hidden min-w-0 truncate text-13px leading-20px text-[var(--color-text-2)] lg:block'
        style={{ paddingLeft: 12 }}
      >
        {formatCreatedAt(item.created_at)}
      </span>

      {/* Status and tag share one compact metadata column. */}
      <div className='flex min-w-0 items-center gap-12px' onClick={stop} style={{ paddingLeft: 16 }}>
        <div className='flex-shrink-0'>
          <StatusPill status={item.status} size='sm' onChange={(next) => onStatusChange(item.requirement_id, next)} />
        </div>
        <span className='inline-flex h-22px min-w-0 items-center gap-4px rounded-full bg-[var(--color-fill-2)] px-7px text-11px leading-none text-[var(--color-text-2)]'>
          <Tag theme='outline' size={12} strokeWidth={3} className='flex-shrink-0 text-[var(--color-text-3)]' />
          <span className='truncate'>{item.tag || '-'}</span>
        </span>
      </div>

      {/* Hover-revealed actions — quiet icon links, kept off the keyboard tab
          flow until visible to avoid surprising focus jumps on the row. */}
      <div
        className='flex flex-shrink-0 items-center gap-10px opacity-0 group-hover:opacity-100 transition-opacity duration-150'
        onClick={stop}
      >
        <span
          role='button'
          tabIndex={0}
          aria-label={t('requirements.actions.edit')}
          title={t('requirements.actions.edit')}
          onClick={() => onEdit(item.requirement_id)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              onEdit(item.requirement_id);
            }
          }}
          className='inline-flex items-center text-[var(--color-text-3)] cursor-pointer hover:text-[rgb(var(--primary-6))] transition-colors'
        >
          <Edit theme='outline' size={15} strokeWidth={3} />
        </span>
        <Popconfirm
          title={t('requirements.actions.deleteConfirm')}
          onOk={() => onDelete(item.requirement_id)}
        >
          <span
            role='button'
            tabIndex={0}
            aria-label={t('requirements.actions.delete')}
            title={t('requirements.actions.delete')}
            onKeyDown={(e) => {
              if (e.key === ' ') {
                e.preventDefault();
                (e.currentTarget as HTMLElement).click();
              }
            }}
            className='inline-flex items-center text-[var(--color-text-3)] cursor-pointer hover:text-[rgb(var(--danger-6))] transition-colors'
          >
            <Delete theme='outline' size={15} strokeWidth={3} />
          </span>
        </Popconfirm>
      </div>
    </div>
  );
};

export default RequirementListRow;
