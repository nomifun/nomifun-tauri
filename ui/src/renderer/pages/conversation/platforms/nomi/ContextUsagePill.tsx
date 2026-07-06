/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { Tooltip } from '@arco-design/web-react';
import { useTranslation } from 'react-i18next';
import { formatTokenCount } from './turnMetrics';

export type ContextUsagePillProps = { used?: number; max?: number };

/** Compact context-usage gauge shown in the composer status row. Renders
 *  "used / max" with a progress-tinted dot; tone escalates as it nears the
 *  engine's ~83% compaction point. Returns null when there's no data yet.
 *
 *  Sized to match the pinned plan summary: 28px tall, fully rounded, 12px text.
 *  Colors are theme variables (`--color-bg-2`, `--color-border-2`,
 *  `--color-text-3`, `rgb(var(--warning-6|danger-6))`) so it tracks all four
 *  CSS themes rather than hardcoding hex. */
export function ContextUsagePill({ used, max }: ContextUsagePillProps) {
  const { t } = useTranslation();
  if (!max || max <= 0 || used == null) return null;
  const pct = Math.min(100, Math.round((used / max) * 100));
  const tone = pct >= 90 ? 'rgb(var(--danger-6))' : pct >= 70 ? 'rgb(var(--warning-6))' : 'var(--color-text-3)';
  return (
    <Tooltip
      mini
      content={t('conversation.contextUsage.tooltip', {
        used: used.toLocaleString(),
        max: max.toLocaleString(),
        pct,
      })}
    >
      <div
        data-testid='nomi-context-usage'
        className='inline-flex h-28px items-center gap-6px rd-999px b b-solid px-10px text-12px font-500 leading-none select-none cursor-default tabular-nums shrink-0'
        style={{
          background: 'color-mix(in srgb, rgb(var(--primary-6)) 5%, var(--color-bg-2))',
          borderColor: 'color-mix(in srgb, rgb(var(--primary-6)) 12%, var(--color-border-2))',
          boxShadow: '0 1px 0 color-mix(in srgb, #fff 42%, transparent) inset',
          color: pct >= 70 ? tone : 'var(--text-secondary)',
        }}
      >
        <span className='inline-block h-7px w-7px rd-999px transition-colors' style={{ background: tone }} />
        {formatTokenCount(used)}/{formatTokenCount(max)}
      </div>
    </Tooltip>
  );
}
