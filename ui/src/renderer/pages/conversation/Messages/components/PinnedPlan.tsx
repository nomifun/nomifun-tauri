/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { IconCheckCircle, IconDown, IconRight } from '@arco-design/web-react/icon';
import React, { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useMessageList } from '@renderer/pages/conversation/Messages/hooks';
import { derivePinnedPlan, type PinnedPlanData } from './pinnedPlanModel';

/**
 * Pinned plan bar: docked inside the composer status row, it surfaces the
 * conversation's current plan (the latest `plan` message) without competing with
 * the command queue above the composer. Collapsed by default to a one-line
 * progress summary; click to expand the full checklist. Renders nothing when
 * there is no active plan.
 */
const PinnedPlan: React.FC<{ plan?: PinnedPlanData | null; className?: string }> = ({
  plan: suppliedPlan,
  className = 'w-full min-w-0 max-w-[420px]',
}) => {
  const { t } = useTranslation();
  const list = useMessageList();
  const derivedPlan = useMemo(
    () => (suppliedPlan === undefined ? derivePinnedPlan(list) : null),
    [list, suppliedPlan]
  );
  const plan = suppliedPlan === undefined ? derivedPlan : suppliedPlan;
  const [expanded, setExpanded] = useState(false);

  if (!plan) return null;

  const { entries, done, total } = plan;
  const pct = total > 0 ? Math.round((done / total) * 100) : 0;
  const toggleExpanded = () => setExpanded((v) => !v);
  const handleSummaryKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (event.key !== 'Enter' && event.key !== ' ') return;
    event.preventDefault();
    toggleExpanded();
  };

  return (
    <div
      data-testid='pinned-plan-bar'
      className={`${className} rd-12px`}
    >
      {/* Summary row — toggles expand/collapse */}
      <div
        role='button'
        tabIndex={0}
        aria-expanded={expanded}
        data-testid='pinned-plan-summary'
        className='relative flex h-28px items-center gap-7px rd-999px px-10px cursor-pointer select-none overflow-hidden'
        style={{
          background: 'color-mix(in srgb, rgb(var(--primary-6)) 5%, var(--color-bg-2))',
          border: '1px solid color-mix(in srgb, rgb(var(--primary-6)) 14%, var(--color-border-2))',
          boxShadow: '0 1px 0 color-mix(in srgb, #fff 42%, transparent) inset',
          color: 'var(--text-secondary)',
        }}
        onClick={toggleExpanded}
        onKeyDown={handleSummaryKeyDown}
      >
        <span
          aria-hidden='true'
          className='inline-block h-6px w-6px rd-999px shrink-0'
          style={{ background: 'color-mix(in srgb, rgb(var(--primary-6)) 42%, var(--color-text-4))' }}
        />
        <span className='min-w-0 truncate text-12px font-600 leading-none'>
          {t('messages.planTodoList', { defaultValue: 'To do list' })}
        </span>
        <span className='ml-auto whitespace-nowrap text-12px leading-none tabular-nums'>
          {t('messages.planProgress', { done, total, defaultValue: '{{done}}/{{total}} done' })}
        </span>
        <span className='inline-flex h-16px w-16px items-center justify-center shrink-0' style={{ color: 'var(--color-text-3)' }}>
          {expanded ? <IconDown /> : <IconRight />}
        </span>
        <span
          aria-hidden='true'
          data-testid='pinned-plan-progress'
          className='absolute left-10px right-10px bottom-2px h-1px rd-999px overflow-hidden'
          style={{ background: 'color-mix(in srgb, var(--color-fill-3) 68%, transparent)' }}
        >
          <span
            className='block h-full rd-999px transition-all'
            style={{
              width: `${pct}%`,
              background: 'color-mix(in srgb, rgb(var(--primary-6)) 70%, var(--color-text-4))',
            }}
          />
        </span>
      </div>

      {/* Full checklist — expanded only */}
      {expanded && (
        <div
          data-testid='pinned-plan-list'
          className='mt-6px flex max-h-[148px] max-w-[420px] flex-col gap-6px overflow-y-auto rd-12px px-10px py-8px'
          style={{
            background: 'color-mix(in srgb, var(--color-bg-2) 92%, rgb(var(--primary-6)))',
            border: '1px solid color-mix(in srgb, var(--color-border-2) 76%, transparent)',
            boxShadow: '0 8px 22px rgba(15, 23, 42, 0.08)',
          }}
        >
          {entries.map((item, index) => (
            <div key={index} className='flex min-h-22px flex-row items-center gap-8px text-12px leading-18px color-#86909C'>
              {item.status === 'completed' ? (
                <IconCheckCircle fontSize={18} strokeWidth={4} className='flex shrink-0 color-#00B42A' />
              ) : item.status === 'in_progress' ? (
                <div className='size-18px flex shrink-0 items-center justify-center'>
                  <div className='size-11px rd-full b-2px b-solid' style={{ borderColor: 'var(--primary-6)' }}></div>
                </div>
              ) : (
                <div className='size-18px flex shrink-0 items-center justify-center'>
                  <div className='size-11px rd-full b-2px b-solid b-[rgba(201,205,212,1)]'></div>
                </div>
              )}
              <span
                className='min-w-0 flex-1 truncate'
                style={item.status === 'in_progress' ? { color: 'var(--text-primary)' } : undefined}
              >
                {item.content}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
};

export default PinnedPlan;
