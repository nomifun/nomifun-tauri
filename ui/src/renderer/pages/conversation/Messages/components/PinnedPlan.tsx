/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { IconCheckCircle } from '@arco-design/web-react/icon';
import { Loading } from '@icon-park/react';
import React, { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useLayoutContext } from '@renderer/hooks/context/LayoutContext';
import { useMessageList } from '@renderer/pages/conversation/Messages/hooks';
import { derivePinnedPlan, type PinnedPlanData } from './pinnedPlanModel';

/**
 * Pinned plan bar: centered above the composer, it surfaces the conversation's
 * current plan (the latest `plan` message) without competing with the command
 * queue. It shows the checklist on desktop hover or keyboard focus, and after
 * explicit activation on mobile. Renders nothing when there is no active plan.
 */
const PinnedPlan: React.FC<{ plan?: PinnedPlanData | null; className?: string }> = ({
  plan: suppliedPlan,
  className = 'w-fit max-w-[calc(100vw-32px)]',
}) => {
  const { t } = useTranslation();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const list = useMessageList();
  const derivedPlan = useMemo(
    () => (suppliedPlan === undefined ? derivePinnedPlan(list) : null),
    [list, suppliedPlan]
  );
  const plan = suppliedPlan === undefined ? derivedPlan : suppliedPlan;
  const [expanded, setExpanded] = useState(false);

  if (!plan) return null;

  const { entries, done, total } = plan;
  const handleSummaryClick = () => {
    if (!isMobile) return;
    setExpanded((value) => !value);
  };
  const handleDesktopOpen = () => {
    if (isMobile) return;
    setExpanded(true);
  };
  const handleDesktopClose = () => {
    if (isMobile) return;
    setExpanded(false);
  };
  const handleSummaryKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (event.key !== 'Enter' && event.key !== ' ') return;
    event.preventDefault();
    setExpanded((value) => !value);
  };

  return (
    <div
      data-testid='pinned-plan-bar'
      className={`relative ${className}`}
      onMouseEnter={handleDesktopOpen}
      onMouseLeave={handleDesktopClose}
      onFocus={handleDesktopOpen}
      onBlur={handleDesktopClose}
    >
      {/* Summary row — toggles expand/collapse */}
      <div
        role='button'
        tabIndex={0}
        aria-expanded={expanded}
        data-testid='pinned-plan-summary'
        className='flex h-28px items-center gap-6px rd-999px px-10px cursor-pointer select-none'
        style={{
          background: 'var(--color-bg-1)',
          border: '1px solid color-mix(in srgb, rgb(var(--primary-6)) 14%, var(--color-border-2))',
          boxShadow: 'none',
          color: 'var(--text-secondary)',
        }}
        onClick={handleSummaryClick}
        onKeyDown={handleSummaryKeyDown}
      >
        {done < total && (
          <Loading
            aria-hidden='true'
            data-testid='pinned-plan-progress-indicator'
            theme='outline'
            size='14'
            className='shrink-0 animate-spin'
            style={{ color: 'var(--color-text-3)' }}
          />
        )}
        <span className='min-w-0 truncate text-12px font-600 leading-none'>
          {t('messages.planTodoList', { defaultValue: 'Task queue' })}
        </span>
        <span className='ml-18px whitespace-nowrap text-12px leading-none tabular-nums'>
          {t('messages.planProgress', { done, total, defaultValue: '{{done}}/{{total}}' })}
        </span>
      </div>

      {/* Full checklist — expanded only */}
      {expanded && (
        <div
          data-testid='pinned-plan-popover'
          className='absolute left-1/2 w-[min(320px,calc(100vw-32px))] -translate-x-1/2 bottom-[calc(100%+8px)] z-10'
        >
          <div
            data-testid='pinned-plan-list'
            className='flex max-h-[180px] flex-col gap-6px overflow-y-auto rd-12px px-12px py-10px'
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
                  className='min-w-0 flex-1 line-clamp-2'
                  style={item.status === 'in_progress' ? { color: 'var(--text-primary)' } : undefined}
                >
                  {item.content}
                </span>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
};

export default PinnedPlan;
