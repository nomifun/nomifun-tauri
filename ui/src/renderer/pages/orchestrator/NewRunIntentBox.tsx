/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Input } from '@arco-design/web-react';
import { Send, SettingTwo, Workbench } from '@icon-park/react';
import { mutate } from 'swr';
import { ipcBridge } from '@/common';
import { isBackendHttpError } from '@/common/adapter/httpBridge';
import type { TCreateAdhocRun, TModelRef } from '@/common/types/orchestrator/orchestratorTypes';
import NomiSelect from '@/renderer/components/base/NomiSelect';
import SegmentedTabs from '@/renderer/components/base/SegmentedTabs';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { type ModelMode, encodePair, useModelRange } from './useModelRange';
import { ORCH_MY_RUNS_SWR_KEY } from './useOrchestratorData';

/** The conversational new-run default autonomy — mirrors {@link NewRunComposer}'s
 * `create` default (`interactive`): the run parks at `awaiting_plan_approval`
 * after the lead agent plans, so the user reviews before anything executes. */
const DEFAULT_AUTONOMY = 'interactive';

export interface NewRunIntentBoxProps {
  /** Select + show the freshly-created run (drives `?run=<id>`). */
  onCreated: (runId: string) => void;
  /** Open the full structured composer for advanced control (work_dir / pinned
   * roles / explicit model range / autonomy). */
  onAdvanced: () => void;
}

/** Case-insensitive substring match against an option's text label (mirrors the
 * composer's filter — Arco types the option as a bare `ReactNode`). */
const filterByLabel = (input: string, option: React.ReactNode): boolean => {
  const children = (option as React.ReactElement<{ children?: React.ReactNode }>)?.props?.children;
  return String(children ?? '')
    .toLowerCase()
    .includes(input.toLowerCase());
};

/**
 * NewRunIntentBox — the conversational "start an orchestration" surface shown in
 * the orchestrator detail pane when no run is selected. The user types a natural-
 * language intent ("描述你想编排的任务，主管 agent 会自动规划…") and presses send;
 * we synthesize a fresh ad-hoc run via {@link ipcBridge.orchestrator.runs.createAdhoc}
 * and select it — the user then continues conversationally via the in-run
 * {@link RunIntentBox} (re-adjust). Together they form a unified conversational
 * lifecycle (no run → type to create; run open → type to re-adjust).
 *
 * Model selection DEFAULTS to "auto" (every enabled model), so an intent-only
 * submit just works — the shared {@link useModelRange} hook expands `auto`
 * client-side into the explicit range the REST endpoint requires (a bare `auto`
 * tag is rejected). A compact selector lets power users narrow to one / a few
 * models inline; the full structured composer (work_dir, pinned roles, explicit
 * range, autonomy) stays one click away via the 「结构化新建」link.
 *
 * Interaction mirrors {@link RunIntentBox}: Enter sends · Shift+Enter newline ·
 * IME `isComposing` guard · double-submit guard · empty intent no-op · errors via
 * {@link useArcoMessage}.
 */
const NewRunIntentBox: React.FC<NewRunIntentBoxProps> = ({ onCreated, onAdvanced }) => {
  const { t } = useTranslation();
  const [message, msgCtx] = useArcoMessage();
  const { providers, getAvailableModels, formatModelLabel, allPairs, hasModels, buildModelRange } = useModelRange();

  const [intent, setIntent] = useState('');
  const [submitting, setSubmitting] = useState(false);

  // Model range — defaults to "auto" so intent-only submit works out of the box.
  const [modelMode, setModelMode] = useState<ModelMode>('auto');
  const [singleModel, setSingleModel] = useState<string>(''); // encoded pair
  const [rangeModels, setRangeModels] = useState<string[]>([]); // encoded pairs

  const trimmed = intent.trim();
  const canSubmit = trimmed.length > 0 && !submitting && hasModels;

  const modelModeItems = useMemo(
    () => [
      { key: 'auto', label: t('orchestrator.composer.model.auto') },
      { key: 'single', label: t('orchestrator.composer.model.single') },
      { key: 'range', label: t('orchestrator.composer.model.range') },
    ],
    [t]
  );

  const handleSubmit = useCallback(async () => {
    const goal = intent.trim();
    if (!goal || submitting) return;
    if (!hasModels) {
      message.warning(t('orchestrator.composer.noModels'));
      return;
    }
    const modelRange = buildModelRange({ mode: modelMode, single: singleModel, range: rangeModels });
    if (!modelRange) {
      message.warning(t('orchestrator.composer.modelRequired'));
      return;
    }

    setSubmitting(true);
    try {
      const body: TCreateAdhocRun = { goal, model_range: modelRange, autonomy: DEFAULT_AUTONOMY };
      const run = await ipcBridge.orchestrator.runs.createAdhoc.invoke(body);
      void mutate(ORCH_MY_RUNS_SWR_KEY);
      setIntent('');
      onCreated(run.id);
    } catch (e) {
      const backendMsg = isBackendHttpError(e) && e.backendMessage ? e.backendMessage : '';
      message.error(t('orchestrator.composer.createError', { error: backendMsg || String(e) }));
    } finally {
      setSubmitting(false);
    }
  }, [intent, submitting, hasModels, buildModelRange, modelMode, singleModel, rangeModels, message, t, onCreated]);

  return (
    <div className='flex size-full min-h-0 flex-col items-center justify-center px-24px py-32px'>
      {msgCtx}
      <div className='w-full max-w-560px flex flex-col items-center gap-18px'>
        {/* Hero — a quiet emblem + headline framing the conversational entry. */}
        <span className='flex size-56px items-center justify-center rd-16px bg-fill-2 text-primary-6'>
          <Workbench theme='outline' size='28' strokeWidth={3} />
        </span>
        <div className='text-center'>
          <div className='text-18px font-600 leading-tight text-t-primary'>{t('orchestrator.start.title')}</div>
          <div className='mt-6px text-12px leading-18px text-t-tertiary'>{t('orchestrator.start.subtitle')}</div>
        </div>

        {/* Conversational input card — mirrors RunIntentBox: a tinted docked card
            with a multi-line textarea + a circular send affordance. */}
        <div
          className='w-full flex items-end gap-10px rd-14px px-12px py-10px transition-colors'
          style={{
            background: 'var(--bg-2)',
            border: `1px solid ${submitting ? 'rgb(var(--primary-6))' : 'var(--border-base)'}`,
            boxShadow: submitting ? '0 0 0 3px color-mix(in srgb, rgb(var(--primary-6)) 16%, transparent)' : undefined,
          }}
        >
          <div className='min-w-0 flex-1'>
            <div className='mb-4px flex items-center gap-5px text-11px font-600 leading-none text-primary-6'>
              <Send theme='outline' size='12' strokeWidth={3} />
              <span>{t('orchestrator.start.label')}</span>
            </div>
            <Input.TextArea
              value={intent}
              onChange={setIntent}
              disabled={submitting}
              autoSize={{ minRows: 2, maxRows: 8 }}
              placeholder={t('orchestrator.start.placeholder')}
              // Enter sends; Shift+Enter inserts a newline (mirrors RunIntentBox).
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  void handleSubmit();
                }
              }}
              style={{
                background: 'transparent',
                border: 'none',
                boxShadow: 'none',
                padding: 0,
                resize: 'none',
                fontSize: 13,
              }}
            />
          </div>

          <div
            role='button'
            tabIndex={0}
            aria-label={t('orchestrator.start.send')}
            aria-disabled={!canSubmit}
            title={t('orchestrator.start.send')}
            onClick={canSubmit ? () => void handleSubmit() : undefined}
            onKeyDown={(e) => {
              if ((e.key === 'Enter' || e.key === ' ') && canSubmit) {
                e.preventDefault();
                void handleSubmit();
              }
            }}
            className='flex size-32px shrink-0 items-center justify-center rd-10px transition-all'
            style={{
              background: canSubmit ? 'rgb(var(--primary-6))' : 'var(--bg-4)',
              color: canSubmit ? '#fff' : 'var(--color-text-3)',
              cursor: canSubmit ? 'pointer' : 'not-allowed',
              opacity: submitting ? 0.7 : 1,
            }}
          >
            <Send
              theme='outline'
              size='16'
              strokeWidth={3}
              style={submitting ? { animation: 'nomi-intent-pulse 1.1s ease-in-out infinite' } : undefined}
            />
          </div>
        </div>

        {/* Compact model selector — a slim mode switch defaulting to "auto". When
            "auto", an intent-only submit picks every enabled model (expanded by
            the shared hook). single/range reveal an inline picker. */}
        <div className='w-full flex flex-col gap-8px'>
          <div className='flex flex-wrap items-center gap-10px'>
            <span className='text-12px font-500 leading-none text-t-secondary'>{t('orchestrator.composer.modelLabel')}</span>
            <SegmentedTabs size='sm' items={modelModeItems} activeKey={modelMode} onChange={(k) => setModelMode(k as ModelMode)} />
          </div>

          {!hasModels ? (
            <div className='text-12px leading-18px text-warning-6'>{t('orchestrator.composer.noModels')}</div>
          ) : modelMode === 'auto' ? (
            <div className='text-12px leading-18px text-t-tertiary'>
              {t('orchestrator.composer.model.autoHint', { count: allPairs.length })}
            </div>
          ) : modelMode === 'single' ? (
            <NomiSelect
              value={singleModel || undefined}
              onChange={(v) => setSingleModel(v as string)}
              placeholder={t('orchestrator.composer.model.singlePlaceholder')}
              showSearch
              filterOption={filterByLabel}
              className='w-full'
            >
              {providers.map((p) => (
                <NomiSelect.OptGroup key={p.id} label={p.name || p.platform}>
                  {getAvailableModels(p).map((m) => {
                    const ref: TModelRef = { provider_id: p.id, model: m };
                    return (
                      <NomiSelect.Option key={encodePair(ref)} value={encodePair(ref)}>
                        {formatModelLabel(p, m)}
                      </NomiSelect.Option>
                    );
                  })}
                </NomiSelect.OptGroup>
              ))}
            </NomiSelect>
          ) : (
            <NomiSelect
              mode='multiple'
              value={rangeModels}
              onChange={(v) => setRangeModels(v as string[])}
              placeholder={t('orchestrator.composer.model.rangePlaceholder')}
              showSearch
              filterOption={filterByLabel}
              className='w-full'
            >
              {providers.map((p) => (
                <NomiSelect.OptGroup key={p.id} label={p.name || p.platform}>
                  {getAvailableModels(p).map((m) => {
                    const ref: TModelRef = { provider_id: p.id, model: m };
                    return (
                      <NomiSelect.Option key={encodePair(ref)} value={encodePair(ref)}>
                        {formatModelLabel(p, m)}
                      </NomiSelect.Option>
                    );
                  })}
                </NomiSelect.OptGroup>
              ))}
            </NomiSelect>
          )}
        </div>

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

      <style>{`@keyframes nomi-intent-pulse { 0%,100% { opacity: 1; } 50% { opacity: 0.45; } }`}</style>
    </div>
  );
};

export default NewRunIntentBox;
