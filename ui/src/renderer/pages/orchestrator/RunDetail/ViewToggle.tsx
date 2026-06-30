/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import { Branch, Comment } from '@icon-park/react';

export type RunViewMode = 'conversation' | 'canvas';

/** The 对话 ⟷ 编排画布 segmented control — a clean two-segment pill matching the
 * orchestrator visual language (primary-tinted active segment, theme tokens). */
export const ViewToggle: React.FC<{ mode: RunViewMode; onChange: (mode: RunViewMode) => void }> = ({ mode, onChange }) => {
  const { t } = useTranslation();
  const segments: { key: RunViewMode; label: string; hint: string; Glyph: typeof Comment }[] = [
    {
      key: 'conversation',
      label: t('orchestrator.run.view.conversation'),
      hint: t('orchestrator.run.view.conversationHint'),
      Glyph: Comment,
    },
    { key: 'canvas', label: t('orchestrator.run.view.canvas'), hint: t('orchestrator.run.view.canvasHint'), Glyph: Branch },
  ];
  return (
    <div
      role='tablist'
      aria-label={t('orchestrator.title')}
      className='inline-flex shrink-0 items-center gap-2px rd-10px p-3px'
      style={{ background: 'var(--bg-2)', border: '1px solid var(--border-base)' }}
    >
      {segments.map(({ key, label, hint, Glyph }) => {
        const active = mode === key;
        return (
          <div
            key={key}
            role='tab'
            tabIndex={0}
            aria-selected={active}
            title={hint}
            onClick={() => onChange(key)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                onChange(key);
              }
            }}
            className='flex h-26px cursor-pointer select-none items-center gap-5px rd-8px px-12px text-12px font-600 leading-none outline-none transition-all duration-150'
            style={{
              background: active ? 'rgb(var(--primary-6))' : 'transparent',
              color: active ? '#fff' : 'var(--text-secondary)',
              boxShadow: active ? '0 1px 4px color-mix(in srgb, rgb(var(--primary-6)) 40%, transparent)' : undefined,
            }}
          >
            <Glyph theme='outline' size='13' strokeWidth={3} className='line-height-0' />
            <span>{label}</span>
          </div>
        );
      })}
    </div>
  );
};
