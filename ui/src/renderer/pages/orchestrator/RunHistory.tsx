/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import { Spin } from '@arco-design/web-react';
import { History, Right, Comment } from '@icon-park/react';
import type { TRun } from '@/common/types/orchestrator/orchestratorTypes';
import { useMyRuns } from './useOrchestratorData';

/** Map a run status string to a theme-var color + i18n label key suffix. */
const STATUS_META: Record<string, { color: string; key: string }> = {
  planning: { color: 'var(--warning)', key: 'planning' },
  running: { color: 'rgb(var(--primary-6))', key: 'running' },
  completed: { color: 'var(--success)', key: 'completed' },
  failed: { color: 'var(--danger)', key: 'failed' },
  cancelled: { color: 'var(--color-text-3)', key: 'cancelled' },
  paused: { color: 'var(--color-text-3)', key: 'paused' },
  awaiting_plan_approval: { color: 'var(--warning)', key: 'awaiting_plan_approval' },
};

const formatTime = (ms: number): string => new Date(ms).toLocaleString();

/**
 * A single run row/card. Clicking the body opens the read-only DAG replay
 * (via `onOpen`); when the run has an owning conversation an extra "open
 * conversation" link jumps straight to it without opening the canvas.
 */
const RunCard: React.FC<{ run: TRun; onOpen: () => void; onOpenConversation?: () => void }> = ({
  run,
  onOpen,
  onOpenConversation,
}) => {
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
      {onOpenConversation && (
        <div
          role='button'
          tabIndex={0}
          aria-label={t('orchestrator.run.openConversation')}
          title={t('orchestrator.run.openConversation')}
          onClick={(e) => {
            e.stopPropagation();
            onOpenConversation();
          }}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              e.stopPropagation();
              onOpenConversation();
            }
          }}
          className='shrink-0 size-30px rd-8px flex items-center justify-center text-t-tertiary transition-colors hover:bg-fill-2 hover:text-primary-6'
        >
          <Comment theme='outline' size='16' strokeWidth={3} />
        </div>
      )}
      <Right theme='outline' size='16' strokeWidth={3} className='shrink-0 text-t-tertiary' />
    </div>
  );
};

/**
 * RunHistory — the read-only Run-history library. Runs are created from
 * conversations (the DAG lives in the conversation rail), so this section no
 * longer offers a create affordance or a workspace selector; it simply lists
 * the current user's runs (all workspaces + ad-hoc) via `useMyRuns`. Clicking
 * a run opens its read-only DAG replay (parent's `onOpenRun` → `?run=`); runs
 * tied to a conversation also expose an "open conversation" jump.
 */
const RunHistory: React.FC<{ onOpenRun?: (runId: string) => void }> = ({ onOpenRun }) => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { runs, isLoading, error } = useMyRuns();

  return (
    <div className='w-full'>
      <div className='mb-16px min-w-0'>
        <div className='text-18px font-600 text-t-primary leading-tight'>{t('orchestrator.run.title')}</div>
        <div className='mt-4px text-12px leading-16px text-t-tertiary'>{t('orchestrator.run.subtitle')}</div>
      </div>

      {isLoading ? (
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
            <RunCard
              key={run.id}
              run={run}
              onOpen={() => onOpenRun?.(run.id)}
              onOpenConversation={
                run.lead_conv_id != null ? () => void navigate(`/conversation/${run.lead_conv_id}`) : undefined
              }
            />
          ))}
        </div>
      )}
    </div>
  );
};

export default RunHistory;
