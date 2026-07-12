/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * RequirementFilters — compact icon-and-text filter controls for tag, status,
 * sort, and search. When one or more rows are selected, a stable batch-action
 * bar is rendered below the filters (its own surface — never squeezed into the
 * filter row) carrying a Popconfirm-guarded batch delete.
 *
 * Presentational: all state lives in the parent; this only emits callbacks.
 */
import { Button, Checkbox, Dropdown, Input, Menu, Popconfirm, Radio, Select } from '@arco-design/web-react';
import type { RefInputType } from '@arco-design/web-react/es/Input/interface';
import { ArrowDown, ArrowUp, Check, Filter, Search, SortTwo, Tag } from '@icon-park/react';
import React, { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import type { ITagSummary, RequirementOrderBy, RequirementStatus } from '@/common/adapter/ipcBridge';
import {
  isRequirementSearchExpanded,
  shouldCollapseRequirementSearch,
} from './requirementFilterToolbarState';

const STATUS_OPTIONS: RequirementStatus[] = [
  'pending',
  'in_progress',
  'needs_review',
  'done',
  'failed',
  'cancelled',
];

const DEFAULT_SORT = '__default__';
const ALL_TAGS = '__all_tags__';
const ALL_STATUSES = '__all_statuses__';

type FilterTriggerProps = Omit<React.ButtonHTMLAttributes<HTMLButtonElement>, 'value'> & {
  icon: React.ReactNode;
  label: string;
  value?: string;
  valueIcon?: React.ReactNode;
  valueIconLabel?: string;
  active?: boolean;
};

export const FilterTrigger = React.forwardRef<HTMLButtonElement, FilterTriggerProps>(function FilterTrigger(
  { icon, label, value, valueIcon, valueIconLabel, active = false, className, ...buttonProps },
  ref
) {
  return (
    <button
      {...buttonProps}
      ref={ref}
      type='button'
      aria-label={value ? `${label}: ${value}${valueIconLabel ? `, ${valueIconLabel}` : ''}` : label}
      aria-pressed={active || undefined}
      className={[
        'inline-flex h-32px max-w-full cursor-pointer items-center gap-6px rounded-6px border-0 px-8px text-13px transition-colors focus-visible:outline-2 focus-visible:outline-[rgb(var(--primary-6))]',
        active
          ? '!bg-primary-1 !text-primary-6'
          : 'bg-transparent text-[var(--color-text-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]',
        className,
      ]
        .filter(Boolean)
        .join(' ')}
    >
      <span aria-hidden='true' className='inline-flex shrink-0'>
        {icon}
      </span>
      <span className='shrink-0'>{label}</span>
      {value && (
        <span className='ml-2px inline-flex max-w-160px items-center gap-4px text-12px font-medium text-[var(--color-text-1)]'>
          <span className='min-w-0 truncate'>{value}</span>
          {valueIcon && (
            <span aria-hidden='true' className='inline-flex shrink-0 text-[rgb(var(--primary-6))]'>
              {valueIcon}
            </span>
          )}
        </span>
      )}
    </button>
  );
});

interface RequirementFiltersProps {
  tag?: string;
  status?: RequirementStatus;
  search: string;
  orderBy?: RequirementOrderBy;
  order: 'asc' | 'desc';
  onTagChange: (t?: string) => void;
  onStatusChange: (s?: RequirementStatus) => void;
  onSearchChange: (q: string) => void;
  onOrderByChange: (o?: RequirementOrderBy) => void;
  onOrderChange: (dir: 'asc' | 'desc') => void;
  tagOptions: ITagSummary[];
  selectedCount: number;
  onBatchDelete: () => void; // shown only when selectedCount>0, as a stable bar
  listSelection?: {
    total: number;
    pageIds: number[];
    selectedIds: Set<number>;
    onToggleSelectAll: (pageIds: number[], checked: boolean) => void;
    onClearSelection: () => void;
  };
}

// Same selected-surface principle as list rows: subtle feedback, readable text.
const SOFT_BATCH_BAR_STYLE: React.CSSProperties = {
  background:
    'linear-gradient(rgba(var(--primary-6), 0.06), rgba(var(--primary-6), 0.06)), var(--color-bg-2)',
  borderColor: 'rgba(var(--primary-6), 0.22)',
};

const RequirementFilters: React.FC<RequirementFiltersProps> = ({
  tag,
  status,
  search,
  orderBy,
  order,
  onTagChange,
  onStatusChange,
  onSearchChange,
  onOrderByChange,
  onOrderChange,
  tagOptions,
  selectedCount,
  onBatchDelete,
  listSelection,
}) => {
  const { t } = useTranslation();
  const [searchActive, setSearchActive] = useState(false);
  const [openFilter, setOpenFilter] = useState<'tag' | 'status' | 'sort' | null>(null);
  const searchInputRef = useRef<RefInputType | null>(null);
  const searchExpanded = isRequirementSearchExpanded(searchActive, search);

  useEffect(() => {
    if (searchActive) searchInputRef.current?.focus();
  }, [searchActive]);

  const filterLabel = t('requirements.columns.tag');
  const statusLabel = t('requirements.columns.status');
  const sortLabel = t('requirements.sort.label');
  const searchLabel = t('requirements.searchLabel');
  const selectedStatusLabel = status ? t(`requirements.status.${status}`) : undefined;
  const sortOptions: Array<{ label: string; value: RequirementOrderBy | typeof DEFAULT_SORT }> = [
    { label: t('requirements.sort.default'), value: DEFAULT_SORT },
    { label: t('requirements.sort.byId'), value: 'id' },
    { label: t('requirements.sort.byCreatedAt'), value: 'created_at' },
    { label: t('requirements.sort.byUpdatedAt'), value: 'updated_at' },
    { label: t('requirements.sort.byStatus'), value: 'status' },
  ];
  const selectedSortLabel = orderBy ? sortOptions.find((option) => option.value === orderBy)?.label : undefined;
  const selectionPageIds = listSelection?.pageIds ?? [];
  const selectionIds = listSelection?.selectedIds;
  const allOnPageSelected =
    selectionPageIds.length > 0 && selectionIds !== undefined && selectionPageIds.every((id) => selectionIds.has(id));
  const someOnPageSelected = listSelection?.pageIds.some((id) => listSelection.selectedIds.has(id)) ?? false;

  const optionContent = (label: React.ReactNode, selected: boolean) => (
    <span className='flex min-w-140px items-center gap-8px'>
      <span className='inline-flex w-14px shrink-0'>{selected && <Check theme='outline' size='14' />}</span>
      <span className='min-w-0 flex-1 truncate'>{label}</span>
    </span>
  );

  const tagMenu = (
    <Menu onClickMenuItem={(key) => onTagChange(key === ALL_TAGS ? undefined : String(key))}>
      <Menu.Item key={ALL_TAGS}>{optionContent(t('requirements.allTags'), !tag)}</Menu.Item>
      {tagOptions.map((item) => (
        <Menu.Item key={item.tag}>
          {optionContent(`${item.tag} (${item.done}/${item.total})`, tag === item.tag)}
        </Menu.Item>
      ))}
    </Menu>
  );

  const statusMenu = (
    <Menu
      onClickMenuItem={(key) =>
        onStatusChange(key === ALL_STATUSES ? undefined : (String(key) as RequirementStatus))
      }
    >
      <Menu.Item key={ALL_STATUSES}>{optionContent(t('requirements.allStatuses'), !status)}</Menu.Item>
      {STATUS_OPTIONS.map((item) => (
        <Menu.Item key={item}>
          {optionContent(t(`requirements.status.${item}`), status === item)}
        </Menu.Item>
      ))}
    </Menu>
  );

  const sortMenu = (
    <div className='min-w-390px rounded-8px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-white)] p-12px shadow-[0_8px_24px_rgba(0,0,0,0.18)]'>
      <div className='mb-10px text-13px font-medium text-[var(--color-text-1)]'>{sortLabel}</div>
      <div className='flex items-center gap-10px'>
        <Select
          size='small'
          className='w-168px shrink-0'
          aria-label={sortLabel}
          value={orderBy ?? DEFAULT_SORT}
          options={sortOptions}
          onChange={(value) =>
            onOrderByChange(value === DEFAULT_SORT ? undefined : (String(value) as RequirementOrderBy))
          }
        />
        <Radio.Group type='button' size='small' value={order} disabled={!orderBy} onChange={onOrderChange}>
          <Radio value='asc'>↑ {t('requirements.sort.asc')}</Radio>
          <Radio value='desc'>↓ {t('requirements.sort.desc')}</Radio>
        </Radio.Group>
      </div>
    </div>
  );

  return (
    <div className='flex flex-col'>
      <div className='flex flex-wrap items-center gap-8px'>
        <Dropdown
          droplist={tagMenu}
          trigger='click'
          position='bl'
          popupVisible={openFilter === 'tag'}
          onVisibleChange={(visible) => setOpenFilter(visible ? 'tag' : null)}
          getPopupContainer={() => document.body}
        >
          <FilterTrigger
            icon={<Tag theme='outline' size='15' fill='currentColor' />}
            label={filterLabel}
            value={tag}
            active={Boolean(tag) || openFilter === 'tag'}
          />
        </Dropdown>
        <Dropdown
          droplist={statusMenu}
          trigger='click'
          position='bl'
          popupVisible={openFilter === 'status'}
          onVisibleChange={(visible) => setOpenFilter(visible ? 'status' : null)}
          getPopupContainer={() => document.body}
        >
          <FilterTrigger
            icon={<Filter theme='outline' size='15' fill='currentColor' />}
            label={statusLabel}
            value={selectedStatusLabel}
            active={Boolean(status) || openFilter === 'status'}
          />
        </Dropdown>
        <Dropdown
          droplist={sortMenu}
          trigger='click'
          position='bl'
          popupVisible={openFilter === 'sort'}
          onVisibleChange={(visible) => setOpenFilter(visible ? 'sort' : null)}
          getPopupContainer={() => document.body}
        >
          <FilterTrigger
            icon={<SortTwo theme='outline' size='15' fill='currentColor' />}
            label={sortLabel}
            value={selectedSortLabel}
            valueIcon={
              orderBy &&
              (order === 'asc' ? (
                <ArrowUp theme='outline' size='12' fill='currentColor' />
              ) : (
                <ArrowDown theme='outline' size='12' fill='currentColor' />
              ))
            }
            valueIconLabel={orderBy ? t(`requirements.sort.${order}`) : undefined}
            active={Boolean(orderBy) || openFilter === 'sort'}
          />
        </Dropdown>
        {searchExpanded ? (
          <Input
            ref={searchInputRef}
            allowClear
            prefix={<Search theme='outline' size='15' fill='currentColor' />}
            placeholder={t('requirements.search')}
            className='max-w-full w-260px'
            value={search}
            onChange={onSearchChange}
            onBlur={() => {
              if (shouldCollapseRequirementSearch(search)) setSearchActive(false);
            }}
            onKeyDown={(event) => {
              if (event.key === 'Escape') {
                setSearchActive(false);
                event.currentTarget.blur();
              }
            }}
          />
        ) : (
          <FilterTrigger
            icon={<Search theme='outline' size='15' fill='currentColor' />}
            label={searchLabel}
            onClick={() => setSearchActive(true)}
          />
        )}
        {listSelection && (
          <div className='ml-auto flex flex-wrap items-center gap-12px text-13px text-[var(--color-text-3)]'>
            <Checkbox
              checked={allOnPageSelected}
              indeterminate={someOnPageSelected && !allOnPageSelected}
              onChange={(checked) => listSelection.onToggleSelectAll(listSelection.pageIds, checked)}
            >
              <span className='whitespace-nowrap text-13px text-[var(--color-text-2)]'>
                {t('requirements.selection.selectAllPage')}
              </span>
            </Checkbox>
            <span className='whitespace-nowrap tabular-nums'>
              {t('requirements.selection.totalCount', { count: listSelection.total })}
            </span>
            {selectedCount > 0 && (
              <>
                <span aria-hidden>·</span>
                <span className='whitespace-nowrap tabular-nums'>
                  {t('requirements.selection.selectedCount', { count: selectedCount })}
                </span>
                <Button type='text' size='mini' onClick={listSelection.onClearSelection}>
                  {t('requirements.selection.clear')}
                </Button>
              </>
            )}
          </div>
        )}
      </div>

      <div role='separator' className='mt-6px h-px bg-[var(--color-border-2)]' />

      {/* Stable batch-action bar — its own surface, only mounted when there is
          a selection. Kept out of the filter row so the filters never reflow. */}
      {selectedCount > 0 && (
        <div
          className='mt-10px flex items-center justify-between gap-12px rounded-10px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] px-12px py-8px'
          style={SOFT_BATCH_BAR_STYLE}
        >
          <span className='text-13px text-[var(--color-text-2)]'>
            {t('requirements.actions.batchDelete', { count: selectedCount })}
          </span>
          <Popconfirm
            title={t('requirements.actions.batchDeleteConfirm', { count: selectedCount })}
            onOk={onBatchDelete}
          >
            <Button status='danger' size='small' shape='round'>
              {t('requirements.actions.delete')}
            </Button>
          </Popconfirm>
        </div>
      )}
    </div>
  );
};

export default RequirementFilters;
