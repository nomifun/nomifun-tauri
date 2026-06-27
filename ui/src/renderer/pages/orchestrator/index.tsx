/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { Suspense, useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';
import classNames from 'classnames';
import AppLoader from '@/renderer/components/layout/AppLoader';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { useContainerWidth } from '@/renderer/hooks/ui/useContainerWidth';
import RunHistory from './RunHistory';
import WorkerTranscriptPanel from './RunDetail/WorkerTranscriptPanel';
import type { OpenTaskPayload } from './RunDetail/DagCanvas';
import MobileRunSummary from './RunDetail/MobileRunSummary';

// The DAG canvas pulls in react-flow (heavy) and is only mounted when a run is
// open, so it is split into its own chunk and loaded on demand.
const DagCanvas = React.lazy(() => import('./RunDetail/DagCanvas'));

/**
 * OrchestratorPage (/orchestrator) — 「智能编排」(orchestration). Runs are now
 * created from conversations (the DAG lives in the conversation rail), so this
 * tab is a READ-ONLY Run-history library: a single full-width list of the
 * current user's runs (across workspaces + ad-hoc), powered by
 * `runs.listMine`.
 *
 * Master-detail: clicking a run sets `?run=<id>`, and the pane is taken over
 * full-bleed by the interactive {@link DagCanvas} (lazy-loaded react-flow) for
 * a read-only replay; closing removes `?run=` (a non-replacing navigation, so
 * browser-back closes the canvas). On mobile the canvas is too awkward to use,
 * so a read-only {@link MobileRunSummary} is shown instead.
 */
const OrchestratorPage: React.FC = () => {
  const { t } = useTranslation();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const [searchParams, setSearchParams] = useSearchParams();

  // ── Master-detail: `?run=<id>` drives the full-bleed DAG canvas ────────────
  const runParam = searchParams.get('run');
  const selectedRunId = runParam && runParam !== '' ? runParam : undefined;

  // Opening a run sets `?run=`. replace:false so browser-back closes the canvas.
  const openRun = useCallback(
    (id: string) => {
      setSearchParams(
        (prev) => {
          const p = new URLSearchParams(prev);
          p.set('run', id);
          return p;
        },
        { replace: false }
      );
    },
    [setSearchParams]
  );

  const closeRun = useCallback(() => {
    setSearchParams(
      (prev) => {
        const p = new URLSearchParams(prev);
        p.delete('run');
        return p;
      },
      { replace: false }
    );
  }, [setSearchParams]);

  // The clicked DAG node's payload → opens the task inspector / transcript drawer.
  const [selectedTask, setSelectedTask] = useState<OpenTaskPayload | null>(null);

  // Closing the run also dismisses any open transcript drawer.
  useEffect(() => {
    if (!selectedRunId) setSelectedTask(null);
  }, [selectedRunId]);

  // Pad by the pane's real width (not the viewport breakpoint) so the narrow
  // content pane isn't robbed of horizontal space by a viewport-based class.
  const { ref: paneRef, width: paneWidth } = useContainerWidth<HTMLDivElement>();
  const panePadX = paneWidth === 0 ? 'px-24px' : paneWidth >= 600 ? 'px-40px' : paneWidth >= 420 ? 'px-24px' : 'px-16px';

  // Mobile: a single read-only run list. When a run is open we replace it with a
  // read-only run summary (the interactive DAG canvas is not mounted on mobile).
  if (isMobile) {
    return (
      <div className='w-full min-h-full box-border overflow-y-auto px-16px py-16px'>
        <div className='text-20px font-600 text-t-primary leading-tight'>{t('orchestrator.title')}</div>
        <div className='mt-4px mb-14px text-12px leading-16px text-t-tertiary'>{t('orchestrator.subtitle')}</div>
        {selectedRunId ? (
          <MobileRunSummary runId={selectedRunId} onBack={closeRun} />
        ) : (
          <RunHistory onOpenRun={openRun} />
        )}
      </div>
    );
  }

  return (
    <div className='relative flex size-full min-h-0'>
      {selectedRunId ? (
        // Full-bleed: the DAG canvas owns the entire pane (react-flow needs an
        // explicitly-sized, non-scrolling parent — every level keeps min-h-0).
        <div
          className='flex-1 min-w-0 min-h-0 overflow-hidden'
          role='tabpanel'
          aria-label={t('orchestrator.run.title')}
        >
          <Suspense fallback={<AppLoader />}>
            <DagCanvas runId={selectedRunId} onBack={closeRun} onOpenTask={setSelectedTask} />
          </Suspense>
        </div>
      ) : (
        <div className='flex-1 min-w-0 min-h-0 overflow-y-auto' role='tabpanel' aria-label={t('orchestrator.title')} ref={paneRef}>
          <div className={classNames('mx-auto w-full max-w-1100px box-border py-32px', panePadX)}>
            <RunHistory onOpenRun={openRun} />
          </div>
        </div>
      )}

      {/* Task inspector + worker transcript drawer — always mounted, visible
          when a task node is clicked in the canvas. */}
      <WorkerTranscriptPanel open={selectedTask} onClose={() => setSelectedTask(null)} />
    </div>
  );
};

export default OrchestratorPage;
