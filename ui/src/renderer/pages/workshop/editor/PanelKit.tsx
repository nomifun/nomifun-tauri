/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/** Small presentational building blocks shared by the four tool panels. */
import React from 'react';

/** The working-area layout: dark stage on the left, parameter rail on the right. */
export const WorkArea: React.FC<{ stage: React.ReactNode; panel: React.ReactNode }> = ({ stage, panel }) => (
  <div className='flex h-full min-h-0 w-full'>
    <div className='relative min-w-0 flex-1'>{stage}</div>
    <aside
      className='flex w-300px shrink-0 flex-col gap-20px overflow-y-auto p-18px'
      style={{ background: 'var(--nfe-panel-bg)', borderLeft: '1px solid var(--nfe-panel-border)' }}
    >
      {panel}
    </aside>
  </div>
);

/** A titled group of controls in the right-hand parameter rail. */
export const PanelSection: React.FC<React.PropsWithChildren<{ title: string; extra?: React.ReactNode }>> = ({ title, extra, children }) => (
  <section className='flex flex-col gap-10px'>
    <div className='flex items-center justify-between'>
      <span className='text-12px font-600 uppercase tracking-wide' style={{ color: 'var(--nfe-text-3)', letterSpacing: '0.04em' }}>
        {title}
      </span>
      {extra}
    </div>
    {children}
  </section>
);

/** A label + control row (label above control, stacked). */
export const Field: React.FC<React.PropsWithChildren<{ label?: string; value?: React.ReactNode }>> = ({ label, value, children }) => (
  <div className='flex flex-col gap-6px'>
    {label !== undefined && (
      <div className='flex items-center justify-between'>
        <span className='text-13px' style={{ color: 'var(--nfe-text-2)' }}>
          {label}
        </span>
        {value !== undefined && (
          <span className='text-13px tabular-nums font-600' style={{ color: 'var(--nfe-text-1)' }}>
            {value}
          </span>
        )}
      </div>
    )}
    {children}
  </div>
);

/** A read-only stat pill (e.g. "1024 × 768"). */
export const StatPill: React.FC<{ label: string; value: string }> = ({ label, value }) => (
  <div
    className='flex flex-col gap-3px rounded-9px px-11px py-9px'
    style={{ background: 'var(--nfe-inset-bg)', border: '1px solid var(--nfe-panel-border)' }}
  >
    <span className='text-11px' style={{ color: 'var(--nfe-text-3)' }}>
      {label}
    </span>
    <span className='text-14px font-700 tabular-nums' style={{ color: 'var(--nfe-text-1)' }}>
      {value}
    </span>
  </div>
);

/** Bottom-of-panel helper hint. */
export const PanelHint: React.FC<React.PropsWithChildren> = ({ children }) => (
  <p className='m-0 mt-auto pt-8px text-12px leading-18px' style={{ color: 'var(--nfe-text-3)' }}>
    {children}
  </p>
);

/** A segmented pill toggle (used for tool / mode choices inside a panel). */
export interface SegmentOption<T extends string> {
  value: T;
  label: string;
  icon?: React.ReactNode;
}

export function SegmentedToggle<T extends string>({
  options,
  value,
  onChange,
}: {
  options: SegmentOption<T>[];
  value: T;
  onChange: (value: T) => void;
}): React.ReactElement {
  return (
    <div
      className='grid gap-3px rounded-10px p-3px'
      style={{ gridTemplateColumns: `repeat(${options.length}, 1fr)`, background: 'var(--nfe-inset-bg)', border: '1px solid var(--nfe-panel-border)' }}
    >
      {options.map((opt) => {
        const active = opt.value === value;
        return (
          <div
            key={opt.value}
            role='button'
            tabIndex={0}
            onClick={() => onChange(opt.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                onChange(opt.value);
              }
            }}
            className='flex h-30px cursor-pointer items-center justify-center gap-6px rounded-8px text-13px transition-all'
            style={{
              background: active ? 'var(--nfe-accent-soft)' : 'transparent',
              color: active ? 'var(--nfe-accent)' : 'var(--nfe-text-2)',
              fontWeight: active ? 600 : 400,
              boxShadow: active ? '0 1px 3px rgba(0,0,0,0.12)' : 'none',
            }}
          >
            {opt.icon}
            {opt.label}
          </div>
        );
      })}
    </div>
  );
}
