/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * CanvasEntryPill — 会话头部「agent 画布」快捷入口 pill(「会话原生编排 v2」F8).
 *
 * Lives in {@link NomiConversationPanel}'s `headerExtra`, rendered by ChatLayout
 * INSIDE the conversation's {@link OrchestrationProvider} subtree, so it consumes
 * the run state via {@link useOrchestrationSafe} without prop-drilling. When the
 * right rail (carrying the 编排 tab) is collapsed, this pill is the discoverable
 * entry point to open the floating agent canvas (F6, `openCanvas()`).
 *
 * Render gates (header stays clean unless there's something to show):
 *  • outside a provider (`ctx == null`, e.g. companion chat) → `null`;
 *  • no linked run (`runId == null`) → `null`;
 *  • otherwise → a compact status pill mirroring the sibling header capability
 *    controls (自动工作 / 智能决策 / 知识库): icon + 画布 label + status dot, status
 *    text/color from {@link STATUS_META}; `leadThinking.active` swaps the dot for a
 *    spinner + 规划中; `canvasOpen` lifts it to a primary-tinted active state.
 *
 * It is a `<div role="button">` (project convention — bare `<button>` shows a
 * WebView2 black box; see memory「No UnoCSS button reset」) and clicks open the
 * floating canvas. All colors flow through CSS variables so it honors every theme.
 */

import React, { useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { Workbench, Loading } from '@icon-park/react';
import { STATUS_META } from '@/renderer/pages/orchestrator/RunDetail/runStatusMeta';
import { useOrchestrationSafe } from './OrchestrationContext';

const CanvasEntryPill: React.FC = () => {
  const { t } = useTranslation();
  const ctx = useOrchestrationSafe();

  const openCanvas = ctx?.openCanvas;
  const handleOpen = useCallback(() => {
    openCanvas?.();
  }, [openCanvas]);
  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLDivElement>) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        openCanvas?.();
      }
    },
    [openCanvas]
  );

  // Header stays clean when there is no run to surface: outside a provider OR
  // not linked to a run → render nothing at all.
  if (ctx == null || ctx.runId == null) return null;

  const { detail, leadThinking, canvasOpen } = ctx;
  const status = detail?.run.status ?? '';
  const statusMeta = STATUS_META[status];
  // Fall back to the muted text token for an unknown/absent status (mirrors the
  // overlay's `dotColor` fallback), so the dot/label never go un-themed.
  const dotColor = statusMeta?.color ?? 'var(--color-text-3)';
  const statusLabel = t(`orchestrator.run.status.${statusMeta?.key ?? 'unknown'}`);
  const planning = leadThinking.active;

  const label = t('orchestrator.canvas.entryLabel', { defaultValue: '画布' });
  const planningLabel = t('orchestrator.run.header.planning', { defaultValue: '规划中' });

  return (
    <div
      role='button'
      tabIndex={0}
      aria-label={t('orchestrator.canvas.openCanvas', { defaultValue: '展开 agent 画布' })}
      title={detail?.run.goal?.trim() || label}
      onClick={handleOpen}
      onKeyDown={handleKeyDown}
      className='inline-flex h-26px shrink-0 cursor-pointer select-none items-center gap-6px rd-full border border-solid px-10px leading-none outline-none transition-all duration-150'
      style={{
        // Active (canvas open) → primary-tinted, matching a `type='primary'`
        // sibling control; idle → neutral surface like the secondary controls.
        borderColor: canvasOpen
          ? 'color-mix(in srgb, rgb(var(--primary-6)) 45%, var(--color-border-2))'
          : 'var(--color-border-2)',
        background: canvasOpen
          ? 'color-mix(in srgb, rgb(var(--primary-6)) 12%, transparent)'
          : 'var(--color-bg-1)',
        color: canvasOpen ? 'rgb(var(--primary-6))' : 'var(--color-text-1)',
      }}
    >
      <Workbench
        theme='outline'
        size='14'
        fill='currentColor'
        strokeWidth={3}
        className='block'
        style={{ lineHeight: 0 }}
      />
      <span className='text-12px font-500'>{label}</span>
      {planning ? (
        <span
          className='inline-flex shrink-0 items-center gap-3px text-11px font-500 leading-none'
          style={{ color: 'rgb(var(--primary-6))' }}
        >
          <Loading
            theme='outline'
            size='12'
            strokeWidth={3}
            className='block animate-spin line-height-0'
            fill='currentColor'
          />
          {planningLabel}
        </span>
      ) : (
        <>
          <span className='inline-block size-6px shrink-0 rd-full' style={{ backgroundColor: dotColor }} />
          <span className='text-11px font-500 leading-none' style={{ color: dotColor }}>
            {statusLabel}
          </span>
        </>
      )}
    </div>
  );
};

export default CanvasEntryPill;
