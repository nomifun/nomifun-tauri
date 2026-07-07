/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { Popover } from '@arco-design/web-react';
import { useTranslation } from 'react-i18next';
import { formatTokenCount } from './turnMetrics';

export type ContextUsageRingProps = { used?: number; max?: number };

/** Icon-only context gauge shown beside the active model. The ring carries the
 * compact status; click opens exact context-window details. */
export function ContextUsageRing({ used, max }: ContextUsageRingProps) {
  const { t } = useTranslation();
  if (!max || max <= 0 || used == null) return null;

  const pct = Math.min(100, Math.round((used / max) * 100));
  const remainingPct = Math.max(0, 100 - pct);
  const tone = pct >= 90 ? 'rgb(var(--danger-6))' : pct >= 70 ? 'rgb(var(--warning-6))' : 'var(--color-text-3)';
  const ringTrack = 'color-mix(in srgb, var(--color-fill-3) 76%, transparent)';
  const ringFill = `conic-gradient(${tone} ${pct * 3.6}deg, ${ringTrack} 0deg)`;
  const usedText = formatTokenCount(used);
  const maxText = formatTokenCount(max);
  const ariaLabel = t('conversation.contextUsage.tooltip', {
    used: used.toLocaleString(),
    max: max.toLocaleString(),
    pct,
  });

  const content = (
    <div data-testid='nomi-context-usage-popover' className='min-w-156px px-2px py-1px'>
      <div className='mb-6px text-12px font-600 leading-18px color-#86909C'>
        {t('conversation.contextUsage.title', { defaultValue: 'Context window' })}
      </div>
      <div className='text-15px font-600 leading-22px text-t-primary tabular-nums'>
        {t('conversation.contextUsage.percentLine', {
          pct,
          remaining: remainingPct,
          defaultValue: '{{pct}}% used ({{remaining}}% remaining)',
        })}
      </div>
      <div className='mt-4px text-14px leading-20px text-t-secondary tabular-nums'>
        {t('conversation.contextUsage.tokenLine', {
          used: usedText,
          max: maxText,
          defaultValue: '{{used}} tokens used, {{max}} total',
        })}
      </div>
    </div>
  );

  return (
    <Popover trigger='click' position='top' content={content} unmountOnExit>
      <button
        type='button'
        aria-label={ariaLabel}
        data-testid='nomi-context-usage-ring'
        className='relative h-22px w-22px shrink-0 rd-999px b-none bg-transparent p-0 cursor-pointer outline-none transition-transform hover:scale-105 active:scale-95 focus-visible:ring-2 focus-visible:ring-[rgb(var(--primary-6))] focus-visible:ring-offset-2 focus-visible:ring-offset-[var(--color-bg-2)]'
        style={{ color: tone }}
      >
        <span aria-hidden='true' className='absolute inset-0 rd-999px' style={{ background: ringFill }} />
        <span
          aria-hidden='true'
          className='absolute inset-3px rd-999px'
          style={{
            background: 'var(--color-bg-2)',
            boxShadow: '0 0 0 1px color-mix(in srgb, var(--color-border-2) 60%, transparent) inset',
          }}
        />
        <span
          aria-hidden='true'
          className='absolute left-1/2 top-1/2 h-6px w-6px -translate-x-1/2 -translate-y-1/2 rd-999px'
          style={{ background: tone }}
        />
      </button>
    </Popover>
  );
}
