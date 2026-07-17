/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * WorkspacePage — the requirements workspace (index section of the platform
 * shell, rendered inside RequirementsLayout's right pane which already supplies
 * the centered, padded scroll wrapper).
 *
 * Assembles the workspace from its committed leaf surfaces:
 *   - a 看板|列表 view toggle (SegmentedTabs) synced to `?view=board|list`
 *   - a primary "+ 新建" button that opens the create drawer via `?new=1`
 *   - RequirementFilters (tag / status / search + batch-delete bar)
 *   - RequirementListView (paginated) OR RequirementBoardView (all matching)
 *   - the unified RequirementDrawer, whose open/mode/target derive purely from
 *     URL params (`new` / `req` / `edit`)
 *
 * Data flows through `useRequirements`, which already re-subscribes to the five
 * requirements live events and refetches — so this page never double-subscribes;
 * it only uses `refresh` for imperative post-mutation refetches.
 */

import React, { useCallback, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';
import { Button } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import type { ITagSummary, RequirementOrderBy, RequirementStatus } from '@/common/adapter/ipcBridge';
import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';
import SegmentedTabs, { type SegmentedTabItem } from '@/renderer/components/base/SegmentedTabs';
import { useRequirements } from '../useRequirements';
import { useWorkspaceTags } from './useWorkspaceTags';
import RequirementFilters from './RequirementFilters';
import RequirementListView from './RequirementListView';
import RequirementBoardView from './RequirementBoardView';
import RequirementDrawer from '../RequirementDrawer';
import type { RequirementId } from '@/common/types/ids';

type ViewMode = 'list' | 'board';

const DEFAULT_PAGE_SIZE = 20;
// Board groups ALL matching items by status client-side, so fetch a large page.
const BOARD_PAGE_SIZE = 500;

const WorkspacePage: React.FC = () => {
  const { t } = useTranslation();
  const [, messageCtx] = useArcoMessage();
  const [searchParams, setSearchParams] = useSearchParams();

  // ---- View mode (?view=board|list, default list) -------------------------
  const view: ViewMode = searchParams.get('view') === 'board' ? 'board' : 'list';
  const setView = useCallback(
    (next: ViewMode) => {
      setSearchParams(
        (prev) => {
          const p = new URLSearchParams(prev);
          if (next === 'list') p.delete('view');
          else p.set('view', next);
          return p;
        },
        { replace: true }
      );
    },
    [setSearchParams]
  );

  // ---- Filters + pagination (local state) ----------------------------------
  const [tag, setTag] = useState<string | undefined>();
  const [status, setStatus] = useState<RequirementStatus | undefined>();
  const [search, setSearch] = useState('');
  // Sort: `orderBy` undefined means the default queue order; direction applies
  // only once a field is chosen.
  const [orderBy, setOrderBy] = useState<RequirementOrderBy | undefined>(undefined);
  const [order, setOrder] = useState<'asc' | 'desc'>('desc');
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(DEFAULT_PAGE_SIZE);

  // ---- Selection (list view) -----------------------------------------------
  const [selectedIds, setSelectedIds] = useState<Set<RequirementId>>(new Set());

  const toggleSelect = useCallback((id: RequirementId) => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  // Select / deselect every row on the current page (used by the list header
  // checkbox). Selection is by id and persists across pages, so batch delete
  // still operates on the full accumulated set.
  const selectAllOnPage = useCallback((pageIds: RequirementId[], checked: boolean) => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (checked) pageIds.forEach((id) => next.add(id));
      else pageIds.forEach((id) => next.delete(id));
      return next;
    });
  }, []);

  const clearSelection = useCallback(() => setSelectedIds(new Set()), []);

  // ---- Data ----------------------------------------------------------------
  // Board groups all matching items client-side → fetch a large page and pin
  // page=1. List uses the paginated page/pageSize.
  const { items, total, loading, error, refresh } = useRequirements({
    tag,
    status,
    q: search || undefined,
    order_by: view === 'board' ? undefined : orderBy,
    order: view === 'board' ? undefined : orderBy ? order : undefined,
    page: view === 'board' ? 1 : page,
    page_size: view === 'board' ? BOARD_PAGE_SIZE : pageSize,
  });
  const { tags } = useWorkspaceTags();
  const tagOptions: ITagSummary[] = tags;

  // ---- Filter handlers (changing any filter resets to page 1) --------------
  const handleTagChange = useCallback((next?: string) => {
    setTag(next);
    setPage(1);
  }, []);
  const handleStatusChange = useCallback((next?: RequirementStatus) => {
    setStatus(next);
    setPage(1);
  }, []);
  const handleSearchChange = useCallback((q: string) => {
    setSearch(q);
    setPage(1);
  }, []);

  // Changing the sort field/direction resets to page 1 (same as filters).
  const handleOrderByChange = useCallback((next?: RequirementOrderBy) => {
    setOrderBy(next);
    setPage(1);
  }, []);
  const handleOrderChange = useCallback((next: 'asc' | 'desc') => {
    setOrder(next);
    setPage(1);
  }, []);

  const handlePageChange = useCallback((nextPage: number, nextPageSize: number) => {
    setPage(nextPage);
    setPageSize(nextPageSize);
  }, []);

  // ---- Drawer state derived from URL params --------------------------------
  // `new=1` → create; `req=<id>` + `edit=1` → edit; `req=<id>` → view; else closed.
  const reqParam = searchParams.get('req');
  const reqId =
    reqParam != null && reqParam !== '' && reqParam.startsWith('req_')
      ? (reqParam as RequirementId)
      : undefined;
  const isNew = searchParams.get('new') === '1';
  const isEdit = searchParams.get('edit') === '1';

  const drawerOpen = isNew || reqId !== undefined;
  const drawerMode: 'view' | 'edit' | 'create' = isNew ? 'create' : isEdit ? 'edit' : 'view';

  const openCreate = useCallback(() => {
    setSearchParams(
      (prev) => {
        const p = new URLSearchParams(prev);
        p.delete('req');
        p.delete('edit');
        p.set('new', '1');
        return p;
      },
      { replace: false }
    );
  }, [setSearchParams]);

  const openDetail = useCallback(
    (id: RequirementId) => {
      setSearchParams(
        (prev) => {
          const p = new URLSearchParams(prev);
          p.delete('new');
          p.delete('edit');
          p.set('req', String(id));
          return p;
        },
        { replace: false }
      );
    },
    [setSearchParams]
  );

  const openEdit = useCallback(
    (id: RequirementId) => {
      setSearchParams(
        (prev) => {
          const p = new URLSearchParams(prev);
          p.delete('new');
          p.set('req', String(id));
          p.set('edit', '1');
          return p;
        },
        { replace: false }
      );
    },
    [setSearchParams]
  );

  const closeDrawer = useCallback(() => {
    setSearchParams(
      (prev) => {
        const p = new URLSearchParams(prev);
        p.delete('new');
        p.delete('req');
        p.delete('edit');
        return p;
      },
      { replace: false }
    );
  }, [setSearchParams]);

  // ---- Mutations -----------------------------------------------------------
  const handleRowStatusChange = useCallback(
    async (id: RequirementId, next: RequirementStatus) => {
      try {
        await ipcBridge.requirements.update.invoke({ id, updates: { status: next } });
        void refresh();
      } catch (e) {
        // useArcoMessage is host-scoped; surface failures inline.
        console.error('Failed to update requirement status', e);
      }
    },
    [refresh]
  );

  const handleDelete = useCallback(
    async (id: RequirementId) => {
      try {
        await ipcBridge.requirements.remove.invoke({ id });
        setSelectedIds((prev) => {
          if (!prev.has(id)) return prev;
          const nextSet = new Set(prev);
          nextSet.delete(id);
          return nextSet;
        });
        void refresh();
      } catch (e) {
        console.error('Failed to delete requirement', e);
      }
    },
    [refresh]
  );

  const handleBatchDelete = useCallback(async () => {
    const ids = [...selectedIds];
    if (ids.length === 0) return;
    try {
      await ipcBridge.requirements.batchDelete.invoke({ ids });
      setSelectedIds(new Set());
      void refresh();
    } catch (e) {
      console.error('Failed to batch-delete requirements', e);
    }
  }, [selectedIds, refresh]);

  // ---- View toggle items ---------------------------------------------------
  const viewItems: SegmentedTabItem[] = useMemo(
    () => [
      { key: 'list', label: t('requirements.viewList') },
      { key: 'board', label: t('requirements.viewKanban') },
    ],
    [t]
  );

  return (
    <div className='flex h-full min-h-0 flex-col'>
      {messageCtx}

      {/* Header row: view toggle (left) + 新建 (right) */}
      <div className='flex flex-wrap items-center justify-between gap-x-16px gap-y-10px'>
        <SegmentedTabs
          items={viewItems}
          activeKey={view}
          onChange={(key) => setView(key === 'board' ? 'board' : 'list')}
          size='sm'
          className='requirements-view-toggle'
        />
        <Button type='primary' shape='round' onClick={openCreate}>
          {t('requirements.newRequirement')}
        </Button>
      </div>

      {/* Filters + list selection controls + batch bar */}
      <div className='mt-8px'>
        <RequirementFilters
          tag={tag}
          status={status}
          search={search}
          orderBy={orderBy}
          order={order}
          onTagChange={handleTagChange}
          onStatusChange={handleStatusChange}
          onSearchChange={handleSearchChange}
          onOrderByChange={handleOrderByChange}
          onOrderChange={handleOrderChange}
          tagOptions={tagOptions}
          selectedCount={selectedIds.size}
          onBatchDelete={handleBatchDelete}
          listSelection={
            view === 'list' && !error && items.length > 0
              ? {
                  total,
                  pageIds: items.map((item) => item.id),
                  selectedIds,
                  onToggleSelectAll: selectAllOnPage,
                  onClearSelection: clearSelection,
                }
              : undefined
          }
        />
      </div>

      {/* The view */}
      <div className={view === 'board' ? 'mt-10px flex flex-1 min-h-0' : 'mt-10px'}>
        {view === 'board' ? (
          <RequirementBoardView items={items} onOpenDetail={openDetail} onStatusChange={handleRowStatusChange} />
        ) : (
          <RequirementListView
            items={items}
            total={total}
            page={page}
            pageSize={pageSize}
            onPageChange={handlePageChange}
            loading={loading}
            error={!!error}
            onRetry={() => void refresh()}
            selectedIds={selectedIds}
            onToggleSelect={toggleSelect}
            onOpenDetail={openDetail}
            onStatusChange={handleRowStatusChange}
            onEdit={openEdit}
            onDelete={handleDelete}
            onCreate={openCreate}
          />
        )}
      </div>

      {/* Unified drawer — open/mode/target derived from URL params. */}
      <RequirementDrawer
        open={drawerOpen}
        mode={drawerMode}
        requirementId={reqId}
        onClose={closeDrawer}
        onSaved={() => void refresh()}
      />
    </div>
  );
};

export default WorkspacePage;
