/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * RequirementListView — the workspace list surface. Renders a stack of
 * `RequirementListRow`s with an Arco `Pagination` (showTotal) footer. Handles
 * the three non-list states presentationally:
 *   - error            → Arco `Result` with a Retry action (`onRetry`)
 *   - empty (settled)  → `WorkspaceEmptyState` with the create CTA
 *   - loading skeleton  → light placeholder rows so the layout doesn't jump
 *
 * Pure/presentational: data, selection set, and the detail drawer all live in
 * the parent (WorkspacePage); this component only fans callbacks back out.
 */
import { Button, Pagination, Result } from '@arco-design/web-react';
import React from 'react';
import { useTranslation } from 'react-i18next';

import type { IRequirement, RequirementStatus } from '@/common/adapter/ipcBridge';
import RequirementListRow from './RequirementListRow';
import WorkspaceEmptyState from './WorkspaceEmptyState';

interface RequirementListViewProps {
  items: IRequirement[];
  total: number;
  page: number;
  pageSize: number;
  onPageChange: (page: number, pageSize: number) => void;
  loading?: boolean;
  error?: boolean;
  onRetry?: () => void;
  selectedIds: Set<number>;
  onToggleSelect: (id: number) => void;
  onOpenDetail: (id: number) => void;
  onStatusChange: (id: number, next: RequirementStatus) => void;
  onEdit: (id: number) => void;
  onDelete: (id: number) => void;
  onCreate: () => void; // for empty state CTA
}

const SKELETON_ROWS = 5;

const RequirementListView: React.FC<RequirementListViewProps> = ({
  items,
  total,
  page,
  pageSize,
  onPageChange,
  loading = false,
  error = false,
  onRetry,
  selectedIds,
  onToggleSelect,
  onOpenDetail,
  onStatusChange,
  onEdit,
  onDelete,
  onCreate,
}) => {
  const { t } = useTranslation();

  if (error) {
    return (
      <Result
        status='error'
        title={t('requirements.loadError')}
        extra={
          onRetry ? (
            <Button type='primary' onClick={onRetry}>
              {t('requirements.retry')}
            </Button>
          ) : undefined
        }
      />
    );
  }

  // Settled-and-empty → invitation, not a bare "no data" line.
  if (!loading && items.length === 0) {
    return <WorkspaceEmptyState onCreate={onCreate} />;
  }

  // First load with no rows yet → light skeleton so the surface doesn't pop.
  if (loading && items.length === 0) {
    return (
      <div className='flex flex-col gap-8px'>
        {Array.from({ length: SKELETON_ROWS }).map((_, i) => (
          <div
            key={i}
            className='h-58px rounded-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] opacity-60 animate-pulse'
            style={{ animationDelay: `${i * 0.08}s` }}
          />
        ))}
      </div>
    );
  }

  return (
    <div className='flex flex-col gap-12px'>
      <div
        className='flex flex-col gap-8px transition-opacity duration-150'
        style={{ opacity: loading ? 0.6 : 1 }}
      >
        {items.map((item) => (
          <RequirementListRow
            key={item.id}
            item={item}
            selected={selectedIds.has(item.id)}
            onToggleSelect={onToggleSelect}
            onOpenDetail={onOpenDetail}
            onStatusChange={onStatusChange}
            onEdit={onEdit}
            onDelete={onDelete}
          />
        ))}
      </div>

      <div className='flex justify-end'>
        <Pagination
          className='requirements-pagination'
          current={page}
          pageSize={pageSize}
          total={total}
          showTotal
          sizeCanChange
          showJumper={total > pageSize}
          onChange={(p, ps) => onPageChange(p, ps)}
        />
      </div>
    </div>
  );
};

export default RequirementListView;
