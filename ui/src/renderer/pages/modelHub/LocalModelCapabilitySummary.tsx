/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import classNames from 'classnames';

export interface LocalModelCapabilitySummaryItem {
  label: React.ReactNode;
  value: React.ReactNode;
  tone?: 'neutral' | 'success' | 'warning' | 'danger';
}

export interface LocalModelCapabilitySummaryProps {
  items: LocalModelCapabilitySummaryItem[];
}

const toneClass: Record<NonNullable<LocalModelCapabilitySummaryItem['tone']>, string> = {
  neutral: 'bg-[var(--color-fill-2)] text-t-secondary',
  success: 'bg-[rgba(var(--success-6),0.1)] text-[rgb(var(--success-6))]',
  warning: 'bg-[rgba(var(--warning-6),0.1)] text-[rgb(var(--warning-6))]',
  danger: 'bg-[rgba(var(--danger-6),0.1)] text-[rgb(var(--danger-6))]',
};

const LocalModelCapabilitySummary: React.FC<LocalModelCapabilitySummaryProps> = ({ items }) => (
  <div
    className='grid gap-7px'
    style={{ gridTemplateColumns: 'repeat(auto-fit, minmax(min(180px, 100%), 1fr))' }}
  >
    {items.map((item, index) => (
      <div
        key={index}
        className='min-w-0 rd-10px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] px-11px py-9px'
      >
        <div className='text-11px leading-16px text-t-secondary'>{item.label}</div>
        <div className='mt-4px flex items-center gap-6px text-12px font-600 leading-17px text-t-primary'>
          <span
            aria-hidden='true'
            className={classNames('size-6px shrink-0 rd-full', toneClass[item.tone ?? 'neutral'])}
          />
          <span className='min-w-0'>{item.value}</span>
        </div>
      </div>
    ))}
  </div>
);

export default LocalModelCapabilitySummary;
