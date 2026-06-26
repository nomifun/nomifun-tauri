/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Select, Spin } from '@arco-design/web-react';
import { History, Plus, Right } from '@icon-park/react';
import classNames from 'classnames';
import type { TRun } from '@/common/types/orchestrator/orchestratorTypes';
import CreateRunModal from './CreateRunModal';
import { useFleets, useRuns, useWorkspaces } from './useOrchestratorData';

/** Map a run status string to a theme-var color + i18n label key suffix. */
const STATUS_META: Record<string, { color: string; key: string }> = {
  planning: { color: 'var(--warning)', key: 'planning' },
  running: { color: 'rgb(var(--primary-6))', key: 'running' },
  completed: { color: 'var(--success)', key: 'completed' },
  failed: { color: 'var(--danger)', key: 'failed' },
  cancelled: { color: 'var(--color-text-3)', key: 'cancelled' },
};

const formatTime = (ms: number): string => new Date(ms).toLocaleString();

/** A single run row/card. */
const RunCard: React.FC<{ run: TRun; onOpen: () => void }> = ({ run, onOpen }) => {
  const { t } = useTranslation();
  const meta = STATUS_META[run.status];
  const dotColor = meta?.color ?? 'var(--color-text-3)';
  const statusLabel = t(`orchestrator.run.status.${meta?.key ?? 'unknown'}`);
  const goalText = run.goal.trim() || t('orchestrator.run.untitledGoal');

  return (
    <div
      role='button'
      tabIndex={0}
      onClick={onOpen}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          onOpen();
        }
      }}
      className='rd-12px bg-1 px-16px py-14px flex items-center gap-12px cursor-pointer select-none transition-colors hover:bg-fill-1'
    >
      <div className='min-w-0 flex-1'>
        <div className='text-14px font-600 text-t-primary truncate leading-tight'>{goalText}</div>
        <div className='mt-4px text-12px text-t-tertiary flex items-center gap-8px truncate'>
          <span className='inline-flex items-center gap-4px shrink-0'>
            <span className='size-7px rd-full shrink-0' style={{ backgroundColor: dotColor }} />
            <span style={{ color: dotColor }}>{statusLabel}</span>
          </span>
          <span className='text-t-tertiary shrink-0'>·</span>
          <span className='truncate'>{formatTime(run.created_at)}</span>
        </div>
      </div>
      <Right theme='outline' size='16' strokeWidth={3} className='shrink-0 text-t-tertiary' />
    </div>
  );
};

/**
 * RunHistory — the 「Run 历史」section of the orchestration page. Runs are
 * persisted per-workspace, so the section offers a workspace selector (default
 * = first workspace) and lists that workspace's runs via SWR `useRuns`. A
 * 「新建 Run」action opens {@link CreateRunModal}; on creation the parent is
 * notified via `onOpenRun` to navigate to the new run.
 */
const RunHistory: React.FC<{ onOpenRun?: (runId: string) => void }> = ({ onOpenRun }) => {
  const { t } = useTranslation();
  const { data: workspaces } = useWorkspaces();
  const { data: fleets } = useFleets();
  const workspaceList = useMemo(() => workspaces ?? [], [workspaces]);
  const fleetList = fleets ?? [];

  const [selectedWorkspaceId, setSelectedWorkspaceId] = useState<string | undefined>(undefined);
  const [createOpen, setCreateOpen] = useState(false);

  // Default to the first workspace once the list loads (and recover if the
  // current selection disappears, e.g. after a workspace is deleted elsewhere).
  useEffect(() => {
    if (workspaceList.length === 0) {
      setSelectedWorkspaceId(undefined);
      return;
    }
    setSelectedWorkspaceId((prev) =>
      prev && workspaceList.some((w) => w.id === prev) ? prev : workspaceList[0].id
    );
  }, [workspaceList]);

  const { runs, isLoading, error, mutate } = useRuns(selectedWorkspaceId);

  const openRun = (runId: string) => {
    onOpenRun?.(runId);
  };

  const hasWorkspace = workspaceList.length > 0;

  return (
    <div className='w-full'>
      <div className='flex items-center justify-between gap-12px mb-16px'>
        <div className='min-w-0'>
          <div className='text-18px font-600 text-t-primary leading-tight'>{t('orchestrator.run.title')}</div>
          <div className='mt-4px text-12px leading-16px text-t-tertiary'>{t('orchestrator.run.subtitle')}</div>
        </div>
        <div
          role='button'
          tabIndex={hasWorkspace ? 0 : -1}
          aria-disabled={!hasWorkspace}
          onClick={() => {
            if (hasWorkspace) setCreateOpen(true);
          }}
          onKeyDown={(e) => {
            if (hasWorkspace && (e.key === 'Enter' || e.key === ' ')) {
              e.preventDefault();
              setCreateOpen(true);
            }
          }}
          className={classNames(
            'shrink-0 h-34px px-14px rd-8px flex items-center gap-6px select-none transition-opacity',
            hasWorkspace
              ? 'bg-primary-6 text-white cursor-pointer hover:opacity-90 active:opacity-80'
              : 'bg-fill-2 text-t-tertiary cursor-not-allowed'
          )}
        >
          <Plus theme='outline' size='15' strokeWidth={4} />
          <span className='text-13px font-500'>{t('orchestrator.run.newRun')}</span>
        </div>
      </div>

      {hasWorkspace && (
        <div className='mb-12px flex items-center gap-8px'>
          <span className='text-12px text-t-tertiary shrink-0'>{t('orchestrator.run.selectWorkspace')}</span>
          <Select
            value={selectedWorkspaceId}
            onChange={(v: string) => setSelectedWorkspaceId(v)}
            options={workspaceList.map((w) => ({ label: w.name, value: w.id }))}
            style={{ width: 220 }}
            size='small'
          />
        </div>
      )}

      {!hasWorkspace ? (
        <div className='rd-12px bg-1 px-24px py-48px flex flex-col items-center justify-center text-center'>
          <span className='size-48px rd-14px bg-fill-2 text-t-tertiary flex items-center justify-center mb-14px'>
            <History theme='outline' size='24' strokeWidth={3} />
          </span>
          <div className='text-15px font-600 text-t-primary'>{t('orchestrator.run.emptyTitle')}</div>
          <div className='mt-6px text-12px leading-18px text-t-tertiary max-w-320px'>
            {t('orchestrator.run.noWorkspace')}
          </div>
        </div>
      ) : isLoading ? (
        <div className='py-48px flex items-center justify-center'>
          <Spin />
        </div>
      ) : error ? (
        <div className='py-48px text-center text-13px text-t-tertiary'>{t('orchestrator.run.loadError')}</div>
      ) : runs.length === 0 ? (
        <div className='rd-12px bg-1 px-24px py-48px flex flex-col items-center justify-center text-center'>
          <span className='size-48px rd-14px bg-fill-2 text-t-tertiary flex items-center justify-center mb-14px'>
            <History theme='outline' size='24' strokeWidth={3} />
          </span>
          <div className='text-15px font-600 text-t-primary'>{t('orchestrator.run.emptyTitle')}</div>
          <div className='mt-6px text-12px leading-18px text-t-tertiary max-w-320px'>
            {t('orchestrator.run.emptyDesc')}
          </div>
        </div>
      ) : (
        <div className='flex flex-col gap-10px'>
          {runs.map((run) => (
            <RunCard key={run.id} run={run} onOpen={() => openRun(run.id)} />
          ))}
        </div>
      )}

      <CreateRunModal
        visible={createOpen}
        workspaces={workspaceList}
        fleets={fleetList}
        defaultWorkspaceId={selectedWorkspaceId}
        onClose={() => setCreateOpen(false)}
        onCreated={(runId) => {
          mutate();
          openRun(runId);
        }}
      />
    </div>
  );
};

export default RunHistory;
