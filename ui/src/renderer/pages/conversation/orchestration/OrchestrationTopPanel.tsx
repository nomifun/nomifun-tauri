/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { Suspense, useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Left, Loading, Right } from '@icon-park/react';
import { Modal, Spin } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import type { TReplanRequest } from '@/common/types/orchestrator/orchestratorTypes';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import OrchestratorComposer, { type AutonomyLevel, type ComposerModelRange } from '@/renderer/pages/orchestrator/OrchestratorComposer';
import { useModelRange } from '@/renderer/pages/orchestrator/useModelRange';
import { RunControls } from '@/renderer/pages/orchestrator/RunDetail/RunControls';
import { STATUS_META } from '@/renderer/pages/orchestrator/RunDetail/runStatusMeta';
import { useOrchestration } from './OrchestrationContext';
import styles from './orchestrationTopPanel.module.css';

/**
 * Lazy-load the react-flow DAG canvas so its heavy graph deps (`@xyflow/react`)
 * aren't pulled into the conversation page bundle until a run actually exists.
 */
const DagCanvas = React.lazy(() => import('@/renderer/pages/orchestrator/RunDetail/DagCanvas'));

/** Fallback color for an unknown run status — neutral tertiary text var. */
const STATUS_FALLBACK_COLOR = 'var(--color-text-3)';

/** Resizable width of the canvas pane (px), persisted across sessions. */
const CANVAS_WIDTH_KEY = 'nomifun:orchestration-canvas-width';
const MIN_W = 320;
const MAX_W = 860;
const DEFAULT_W = 480;
/** Collapsed (hidden) preference, persisted so the pane stays where the user left it. */
const CANVAS_COLLAPSED_KEY = 'nomifun:orchestration-canvas-collapsed';

function readInitialWidth(): number {
  try {
    const n = Number(localStorage.getItem(CANVAS_WIDTH_KEY));
    if (Number.isFinite(n) && n >= MIN_W && n <= MAX_W) return n;
  } catch {
    /* ignore */
  }
  return DEFAULT_W;
}

function readInitialCollapsed(): boolean {
  try {
    return localStorage.getItem(CANVAS_COLLAPSED_KEY) === '1';
  } catch {
    return false;
  }
}

/**
 * OrchestrationTopPanel — the orchestration canvas as a 左右分屏 RIGHT pane (用户
 * 设计稿:左侧边栏 | 内容区(聊天) | 画布展开区 | 右侧边栏). The main agent chat is the
 * `flex-1` left column (rendered by the content switcher); this pane is the right
 * "画布展开区": a draggable-width, collapsible column hosting the DAG (with its
 * minimap). It sits between the chat and the 项目 right rail. No floating overlay,
 * no top split.
 *
 * Reads {@link useOrchestration} (always inside an `OrchestrationProvider`):
 *  - `runId == null` → renders nothing (no run linked → pane absent; the chat takes
 *    the full width, so a plain nomi conversation looks exactly as before).
 *  - collapsed → a thin vertical strip on the right edge (status dot + a 「‹」expand
 *    affordance) so the canvas can be hidden and the chat reclaim the width.
 *  - expanded → a width-resizable column: a left-edge drag handle to widen/narrow;
 *    a header (collapse 「›」 + 「编排画布」title + status pill {@link STATUS_META} +
 *    「规划中…」hint + compact {@link RunControls}); and a full-height body hosting
 *    the lazy {@link DagCanvas} (node → `projectTask`, main → `returnToMain`,
 *    `mainActive = projectedTaskId === null`; the completion RolePrecipitationPanel
 *    and the minimap come from the canvas itself).
 *  - replan: RunControls' `onReplan` opens a standard Arco Modal (not a floating
 *    window) hosting the {@link OrchestratorComposer} (fluid) prefilled with the
 *    run's goal → `runs.replan` → toast + refetch + close.
 */
const OrchestrationTopPanel: React.FC = () => {
  const { t } = useTranslation();
  const [message, msgCtx] = useArcoMessage();
  const orchestration = useOrchestration();
  const { buildModelRange } = useModelRange();

  // Collapsed (hidden) ⟷ expanded. Default expanded for discoverability.
  const [collapsed, setCollapsed] = useState<boolean>(readInitialCollapsed);
  // Resizable pane width (px). Persisted; drag the left edge to change it.
  const [width, setWidth] = useState<number>(readInitialWidth);
  const dragState = useRef<{ startX: number; startWidth: number } | null>(null);

  useEffect(() => {
    try {
      localStorage.setItem(CANVAS_WIDTH_KEY, String(width));
    } catch {
      /* ignore */
    }
  }, [width]);

  useEffect(() => {
    try {
      localStorage.setItem(CANVAS_COLLAPSED_KEY, collapsed ? '1' : '0');
    } catch {
      /* ignore */
    }
  }, [collapsed]);

  // ── Resize (drag the LEFT edge; pane is on the right, so dragging left widens) ──
  const onResizePointerDown = useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      if (e.button !== 0) return;
      e.preventDefault();
      dragState.current = { startX: e.clientX, startWidth: width };
      (e.currentTarget as HTMLDivElement).setPointerCapture(e.pointerId);
    },
    [width]
  );
  const onResizePointerMove = useCallback((e: React.PointerEvent<HTMLDivElement>) => {
    const ds = dragState.current;
    if (!ds) return;
    const next = ds.startWidth + (ds.startX - e.clientX);
    setWidth(Math.min(MAX_W, Math.max(MIN_W, next)));
  }, []);
  const onResizeEnd = useCallback((e: React.PointerEvent<HTMLDivElement>) => {
    if (!dragState.current) return;
    dragState.current = null;
    const el = e.currentTarget as HTMLDivElement;
    if (el.hasPointerCapture(e.pointerId)) el.releasePointerCapture(e.pointerId);
  }, []);

  // ── Replan modal state ──────────────────────────────────────────────────────
  // v1 simplification: the replan composer prefills the run's goal + autonomy,
  // but the model_range defaults to `auto` (every enabled pair) rather than being
  // reverse-rebuilt from the run's fleet_members snapshot. The user can narrow it.
  const [replanOpen, setReplanOpen] = useState(false);
  const [replanGoal, setReplanGoal] = useState('');
  const [replanModelRange, setReplanModelRange] = useState<ComposerModelRange>({ mode: 'auto', single: '', range: [] });
  const [replanAutonomy, setReplanAutonomy] = useState<AutonomyLevel>('interactive');
  const [replanSubmitting, setReplanSubmitting] = useState(false);

  const { runId, detail, leadThinking, refetch, projectTask, returnToMain, projectedTaskId } = orchestration;

  const openReplan = useCallback(() => {
    const goal = orchestration.detail?.run.goal ?? '';
    setReplanGoal(goal);
    setReplanModelRange({ mode: 'auto', single: '', range: [] });
    setReplanAutonomy(orchestration.detail?.run.autonomy === 'supervised' ? 'supervised' : 'interactive');
    setReplanOpen(true);
  }, [orchestration.detail?.run.goal, orchestration.detail?.run.autonomy]);

  const submitReplan = useCallback(
    async (goal: string) => {
      if (!runId) return;
      const trimmed = goal.trim();
      if (!trimmed) {
        message.warning(t('orchestrator.composer.goalRequired'));
        return;
      }
      const wireRange = buildModelRange({
        mode: replanModelRange.mode,
        single: replanModelRange.single,
        range: replanModelRange.range,
      });
      if (!wireRange) {
        message.warning(t('orchestrator.composer.modelRequired'));
        return;
      }
      setReplanSubmitting(true);
      try {
        const body: { id: string } & TReplanRequest = {
          id: runId,
          goal: trimmed,
          model_range: wireRange,
          autonomy: replanAutonomy,
        };
        await ipcBridge.orchestrator.runs.replan.invoke(body);
        message.success(t('orchestrator.run.detail.replanOk', { defaultValue: '已重新规划' }));
        await refetch();
        setReplanOpen(false);
      } catch (e) {
        message.error(t('orchestrator.composer.replanError', { error: String(e) }));
      } finally {
        setReplanSubmitting(false);
      }
    },
    [runId, buildModelRange, replanModelRange, replanAutonomy, refetch, message, t]
  );

  // No run linked to this conversation → the pane does not exist.
  if (runId == null) return null;

  const status = detail?.run.status ?? '';
  const statusMeta = STATUS_META[status];
  const statusColor = statusMeta?.color ?? STATUS_FALLBACK_COLOR;
  const statusLabel = statusMeta
    ? t(`orchestrator.run.status.${statusMeta.key}`, { defaultValue: status })
    : t('orchestrator.run.status.unknown', { defaultValue: status });
  const panelTitle = t('conversation.orchestration.panelTitle', { defaultValue: '编排画布' });

  // ── Collapsed: thin vertical strip on the right edge ──────────────────────────
  if (collapsed) {
    return (
      <div
        role='button'
        tabIndex={0}
        aria-label={t('conversation.orchestration.expandCanvas', { defaultValue: '展开编排画布' })}
        title={panelTitle}
        onClick={() => setCollapsed(false)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            setCollapsed(false);
          }
        }}
        className={styles.collapsedStrip}
      >
        {msgCtx}
        <Left theme='outline' size='14' strokeWidth={3} />
        <span className={styles.collapsedDot} style={{ background: statusColor }} />
        <span className={styles.collapsedLabel}>{panelTitle}</span>
      </div>
    );
  }

  // ── Expanded: width-resizable right column ────────────────────────────────────
  return (
    <div className={`${styles.panel} shrink-0 flex flex-col`} style={{ width }}>
      {msgCtx}

      {/* Left-edge drag handle — widen/narrow the pane. */}
      <div
        className={styles.resizeHandle}
        role='separator'
        aria-orientation='vertical'
        aria-label={t('conversation.orchestration.resizeCanvas', { defaultValue: '调整画布宽度' })}
        onPointerDown={onResizePointerDown}
        onPointerMove={onResizePointerMove}
        onPointerUp={onResizeEnd}
        onPointerCancel={onResizeEnd}
      />

      {/* Header — collapse toggle + title + status pill + planning hint + compact
          RunControls (allowed to wrap in the narrow column). */}
      <div className={`${styles.header} flex flex-wrap items-center gap-x-10px gap-y-6px`}>
        <div
          role='button'
          tabIndex={0}
          aria-label={t('conversation.orchestration.collapseCanvas', { defaultValue: '收起编排画布' })}
          onClick={() => setCollapsed(true)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              setCollapsed(true);
            }
          }}
          className={`${styles.toggle} inline-flex items-center gap-6px cursor-pointer select-none`}
        >
          <Right theme='outline' size='14' strokeWidth={3} />
          <span className='text-13px font-600 text-t-primary'>{panelTitle}</span>
        </div>

        <span
          className='inline-flex items-center gap-6px rd-full px-9px py-3px text-11px font-600 leading-none'
          style={{
            color: statusColor,
            background: 'color-mix(in srgb, currentColor 12%, transparent)',
          }}
        >
          <span className='size-6px rd-full shrink-0' style={{ background: statusColor }} />
          <span className='truncate'>{statusLabel}</span>
        </span>

        {leadThinking.active && (
          <span className='inline-flex items-center gap-5px text-11px text-primary-6 leading-none'>
            <Loading theme='outline' size='12' strokeWidth={3} className='animate-spin line-height-0' />
            <span>{t('conversation.orchestration.planning', { defaultValue: '规划中…' })}</span>
          </span>
        )}

        <div className='ml-auto'>
          <RunControls runId={runId} status={status} refetch={refetch} onReplan={openReplan} />
        </div>
      </div>

      {/* Body — fills the remaining column height; react-flow lays out + draws its
          own minimap. */}
      <div className={`${styles.body} flex-1 min-h-0`}>
        <Suspense
          fallback={
            <div className='size-full flex items-center justify-center'>
              <Spin />
            </div>
          }
        >
          <DagCanvas
            runId={runId}
            onOpenTask={projectTask}
            onOpenMain={returnToMain}
            mainActive={projectedTaskId === null}
          />
        </Suspense>
      </div>

      {/* Replan modal — a STANDARD Arco dialog (not a floating window). */}
      <Modal
        title={t('orchestrator.run.detail.replan')}
        visible={replanOpen}
        footer={null}
        onCancel={() => {
          if (!replanSubmitting) setReplanOpen(false);
        }}
        maskClosable={!replanSubmitting}
        autoFocus={false}
        unmountOnExit
        style={{ width: 'min(640px, calc(100vw - 32px))' }}
      >
        <OrchestratorComposer
          fluid
          value={replanGoal}
          onChange={setReplanGoal}
          onSubmit={submitReplan}
          submitting={replanSubmitting}
          placeholder={t('orchestrator.composer.goalPlaceholder', { defaultValue: '描述要重新规划的目标…' })}
          label={t('orchestrator.run.detail.replan')}
          showModelRange
          modelRange={replanModelRange}
          onModelRangeChange={setReplanModelRange}
          showAutonomy
          autonomy={replanAutonomy}
          onAutonomyChange={setReplanAutonomy}
        />
      </Modal>
    </div>
  );
};

export default OrchestrationTopPanel;
