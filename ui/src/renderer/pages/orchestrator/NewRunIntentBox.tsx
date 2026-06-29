/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { SettingTwo, Workbench } from '@icon-park/react';
import { mutate } from 'swr';
import { ipcBridge } from '@/common';
import { isBackendHttpError } from '@/common/adapter/httpBridge';
import type { TCreateAdhocRun } from '@/common/types/orchestrator/orchestratorTypes';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import OrchestratorComposer, { type AutonomyLevel, type ComposerModelRange } from './OrchestratorComposer';
import { useModelRange } from './useModelRange';
import { ORCH_MY_RUNS_SWR_KEY } from './useOrchestratorData';

export interface NewRunIntentBoxProps {
  /** Select + show the freshly-created run (drives `?run=<id>`). Called as soon
   * as `createAdhoc` returns — the backend returns immediately in `planning`
   * state and streams the lead-agent's planning over WS, so jumping straight
   * into the run view shows the orchestration思考 forming (no submit空挡). */
  onCreated: (runId: string) => void;
  /** Open the full structured composer for advanced control (work_dir / pinned
   * roles / explicit model range / autonomy). */
  onAdvanced: () => void;
}

/**
 * NewRunIntentBox — the conversational "start an orchestration" surface shown in
 * the orchestrator detail pane when no run is selected. The user types a natural-
 * language intent and presses send; we synthesize a fresh ad-hoc run via
 * {@link ipcBridge.orchestrator.runs.createAdhoc} and **immediately** navigate
 * into it (`?run=<id>`) — the backend returns straight away in `planning` state
 * and streams the lead agent's planning thoughts over WS, so the run view shows
 * the orchestration forming instead of a blank submit gap.
 *
 * The input is the shared {@link OrchestratorComposer} (chat-style rd-24 card +
 * model-range / autonomy pills + circular send), so it matches the conversation
 * page's composer exactly. Model selection DEFAULTS to "auto" (every enabled
 * model) — the shared {@link useModelRange} hook expands `auto` client-side into
 * the explicit range the REST endpoint requires. The full structured composer
 * (work_dir, pinned roles, explicit range, autonomy) stays one click away via
 * the 「结构化新建」link.
 */
const NewRunIntentBox: React.FC<NewRunIntentBoxProps> = ({ onCreated, onAdvanced }) => {
  const { t } = useTranslation();
  const [message, msgCtx] = useArcoMessage();
  const { hasModels, buildModelRange } = useModelRange();

  const [intent, setIntent] = useState('');
  const [submitting, setSubmitting] = useState(false);

  // Model range — defaults to "auto" so an intent-only submit works out of the
  // box; single/range are edited inside the composer's model pill popover.
  const [modelRange, setModelRange] = useState<ComposerModelRange>({ mode: 'auto', single: '', range: [] });
  // Autonomy — defaults to `interactive` (review the plan first), mirroring
  // NewRunComposer's create default: the run parks at `awaiting_plan_approval`.
  const [autonomy, setAutonomy] = useState<AutonomyLevel>('interactive');

  const handleSubmit = useCallback(
    async (goal: string) => {
      if (!goal || submitting) return;
      if (!hasModels) {
        message.warning(t('orchestrator.composer.noModels'));
        return;
      }
      const wireRange = buildModelRange({ mode: modelRange.mode, single: modelRange.single, range: modelRange.range });
      if (!wireRange) {
        message.warning(t('orchestrator.composer.modelRequired'));
        return;
      }

      setSubmitting(true);
      try {
        const body: TCreateAdhocRun = { goal, model_range: wireRange, autonomy };
        const run = await ipcBridge.orchestrator.runs.createAdhoc.invoke(body);
        void mutate(ORCH_MY_RUNS_SWR_KEY);
        setIntent('');
        // Optimistic jump — the backend already persisted the run (planning state)
        // and is streaming planning over WS; land in the run view immediately.
        onCreated(run.id);
      } catch (e) {
        const backendMsg = isBackendHttpError(e) && e.backendMessage ? e.backendMessage : '';
        message.error(t('orchestrator.composer.createError', { error: backendMsg || String(e) }));
      } finally {
        setSubmitting(false);
      }
    },
    [submitting, hasModels, buildModelRange, modelRange, autonomy, message, t, onCreated]
  );

  return (
    <div className='flex size-full min-h-0 flex-col items-center justify-center px-24px py-32px'>
      {msgCtx}
      <div className='w-full flex flex-col items-center gap-18px'>
        {/* Hero — a quiet emblem + headline framing the conversational entry. */}
        <span className='flex size-56px items-center justify-center rd-16px bg-fill-2 text-primary-6'>
          <Workbench theme='outline' size='28' strokeWidth={3} />
        </span>
        <div className='text-center'>
          <div className='text-18px font-600 leading-tight text-t-primary'>{t('orchestrator.start.title')}</div>
          <div className='mt-6px text-12px leading-18px text-t-tertiary'>{t('orchestrator.start.subtitle')}</div>
        </div>

        {/* Shared chat-style composer — rd-24 card, lilac focus glow, model-range
            + autonomy pills, circular send (mirrors the conversation page). */}
        <OrchestratorComposer
          value={intent}
          onChange={setIntent}
          onSubmit={handleSubmit}
          submitting={submitting}
          placeholder={t('orchestrator.start.placeholder')}
          label={t('orchestrator.start.label')}
          showModelRange
          modelRange={modelRange}
          onModelRangeChange={setModelRange}
          showAutonomy
          autonomy={autonomy}
          onAutonomyChange={setAutonomy}
        />

        {/* Advanced: open the full structured composer (work_dir / pinned roles /
            explicit model range / autonomy). The composer is never removed — the
            intent box is the quick conversational path layered on top of it. */}
        <div
          role='button'
          tabIndex={0}
          onClick={onAdvanced}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              onAdvanced();
            }
          }}
          className='inline-flex cursor-pointer select-none items-center gap-6px rd-8px px-10px py-6px text-12px text-t-tertiary transition-colors hover:bg-fill-2 hover:text-t-secondary'
        >
          <SettingTwo theme='outline' size='13' strokeWidth={3} />
          <span>{t('orchestrator.start.advanced')}</span>
        </div>
      </div>
    </div>
  );
};

export default NewRunIntentBox;
