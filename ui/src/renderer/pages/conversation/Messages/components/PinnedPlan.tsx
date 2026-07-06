/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { Badge } from '@arco-design/web-react';
import { IconCheckCircle, IconDown, IconRight } from '@arco-design/web-react/icon';
import React, { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useMessageList } from '@renderer/pages/conversation/Messages/hooks';
import { derivePinnedPlan, type PinnedPlanData } from './pinnedPlanModel';

/**
 * Pinned plan bar: docked just above the composer, it surfaces the conversation's
 * current plan (the latest `plan` message) so it never scrolls out of view.
 * Collapsed by default to a one-line progress summary; click to expand the full
 * checklist. Renders nothing when there is no active plan.
 */
const PinnedPlan: React.FC<{ plan?: PinnedPlanData | null; className?: string }> = ({
  plan: suppliedPlan,
  className = 'w-full sm:w-[56%] max-w-[520px] min-w-0',
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

  return (
    <div
      data-testid='pinned-plan-bar'
      className={`${className} mx-auto rd-12px overflow-hidden`}
      style={{
        background: 'var(--color-bg-2)',
        border: '1px solid var(--color-border-2)',
        boxShadow: '0 12px 32px rgba(15, 23, 42, 0.12)',
      }}
    >
      {/* Summary row — toggles expand/collapse */}
      <div
        className='flex items-center gap-10px px-12px py-8px cursor-pointer select-none'
        onClick={() => setExpanded((v) => !v)}
      >
        <Badge
          status='default'
          text={t('messages.planTodoList', { defaultValue: 'To do list' })}
          className={'![&_span.arco-badge-status-text]:color-#86909C'}
        ></Badge>
        <span className='ml-auto text-13px color-#86909C'>
          {t('messages.planProgress', { done, total, defaultValue: '{{done}}/{{total}} done' })}
        </span>
        {expanded ? <IconDown className='color-#86909C' /> : <IconRight className='color-#86909C' />}
      </div>

      {/* Progress bar */}
      <div className='h-3px w-full' style={{ background: 'var(--color-fill-3)' }}>
        <div className='h-full transition-all' style={{ width: `${pct}%`, background: 'var(--primary-6)' }} />
      </div>

      {/* Full checklist — expanded only */}
      {expanded && (
        <div className='flex flex-col gap-8px px-12px py-10px max-h-[30vh] overflow-y-auto'>
          {entries.map((item, index) => (
            <div key={index} className='flex flex-row items-center gap-8px color-#86909C'>
              {item.status === 'completed' ? (
                <IconCheckCircle fontSize={20} strokeWidth={4} className='flex color-#00B42A' />
              ) : item.status === 'in_progress' ? (
                <div className='size-20px flex items-center justify-center'>
                  <div className='size-12px rd-full b-2px b-solid' style={{ borderColor: 'var(--primary-6)' }}></div>
                </div>
              ) : (
                <div className='size-20px flex items-center justify-center'>
                  <div className='size-12px rd-full b-2px b-solid b-[rgba(201,205,212,1)]'></div>
                </div>
              )}
              <span
                className='min-w-0 flex-1 break-words'
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
