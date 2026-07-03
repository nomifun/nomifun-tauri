/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import type { TFunction } from 'i18next';
import { Headset } from '@icon-park/react';
import type { PublicAgentAuditSurface } from '@/common/adapter/ipcBridge';

/**
 * Enterprise-console shared primitives for the 对外伙伴 (Public Companion) domain.
 * Deliberately NON-cute: a professional headset seal, quiet status pills, and
 * structured section cards that speak the app's Arco + theme-token language.
 */

/** A public companion's identity tile — a headset seal over a soft brand wash. */
export const AgentSeal: React.FC<{ size?: number; enabled?: boolean }> = ({ size = 48, enabled = true }) => {
  const icon = Math.round(size * 0.46);
  return (
    <span
      className='relative flex items-center justify-center rd-14px shrink-0'
      style={{
        width: size,
        height: size,
        background: enabled
          ? 'linear-gradient(150deg, rgba(var(--primary-5),0.16) 0%, rgba(var(--primary-6),0.28) 100%)'
          : 'var(--color-fill-2)',
        color: enabled ? 'rgb(var(--primary-6))' : 'var(--color-text-3)',
        border: '1px solid',
        borderColor: enabled ? 'rgba(var(--primary-6),0.22)' : 'var(--color-border-2)',
      }}
    >
      <Headset theme='outline' size={icon} fill='currentColor' className='block' style={{ lineHeight: 0 }} />
    </span>
  );
};

/** 启用 / 停用 status pill. Enabled reads as a live service (success); paused reads quiet. */
export const StatusPill: React.FC<{ enabled: boolean; t: TFunction }> = ({ enabled, t }) =>
  enabled ? (
    <span className='inline-flex items-center gap-5px rd-full px-9px py-2px text-11px font-600 leading-none text-[rgb(var(--success-6))] bg-[rgba(var(--success-6),0.12)] border border-solid border-[rgba(var(--success-6),0.26)]'>
      <span className='w-6px h-6px rd-full' style={{ background: 'rgb(var(--success-6))' }} />
      {t('publicCompanion.status.enabled', { defaultValue: '已启用' })}
    </span>
  ) : (
    <span className='inline-flex items-center gap-5px rd-full px-9px py-2px text-11px font-600 leading-none text-t-tertiary bg-fill-2 border border-solid border-[var(--color-border-2)]'>
      <span className='w-6px h-6px rd-full bg-[var(--color-text-4)]' />
      {t('publicCompanion.status.disabled', { defaultValue: '已停用' })}
    </span>
  );

/** A titled section card used across the detail page. */
export const SectionCard: React.FC<{
  icon: React.ReactNode;
  title: string;
  desc?: string;
  action?: React.ReactNode;
  children: React.ReactNode;
}> = ({ icon, title, desc, action, children }) => (
  <div className='rd-14px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] p-18px'>
    <div className='flex items-start justify-between gap-12px'>
      <div className='flex items-start gap-10px min-w-0'>
        <span className='mt-1px flex shrink-0 items-center justify-center w-28px h-28px rd-8px text-[rgb(var(--primary-6))] bg-[rgba(var(--primary-6),0.10)]'>
          {icon}
        </span>
        <div className='min-w-0'>
          <div className='text-15px font-600 text-t-primary leading-22px'>{title}</div>
          {desc && <div className='mt-2px text-12px text-t-tertiary leading-18px'>{desc}</div>}
        </div>
      </div>
      {action && <div className='shrink-0'>{action}</div>}
    </div>
    <div className='mt-14px'>{children}</div>
  </div>
);

/** Labelled field row: a fixed left caption column + a flexible control column. */
export const FieldRow: React.FC<{ label: string; hint?: string; children: React.ReactNode }> = ({
  label,
  hint,
  children,
}) => (
  <div className='flex flex-col gap-6px sm:flex-row sm:items-start sm:gap-16px py-4px'>
    <div className='sm:w-160px shrink-0 pt-6px'>
      <div className='text-13px font-500 text-t-primary'>{label}</div>
      {hint && <div className='mt-2px text-12px text-t-tertiary leading-17px'>{hint}</div>}
    </div>
    <div className='flex-1 min-w-0'>{children}</div>
  </div>
);

/** Relative "N 分钟前" formatter; falls back to a locale date past a week. */
export const formatRelative = (t: TFunction, at: number): string => {
  const diff = Date.now() - at;
  const MIN = 60_000;
  const HOUR = 3_600_000;
  const DAY = 86_400_000;
  if (diff < 0 || diff < MIN) return t('publicCompanion.audit.justNow', { defaultValue: '刚刚' });
  if (diff < HOUR) return t('publicCompanion.audit.minutesAgo', { defaultValue: '{{n}} 分钟前', n: Math.floor(diff / MIN) });
  if (diff < DAY) return t('publicCompanion.audit.hoursAgo', { defaultValue: '{{n}} 小时前', n: Math.floor(diff / HOUR) });
  if (diff < 7 * DAY) return t('publicCompanion.audit.daysAgo', { defaultValue: '{{n}} 天前', n: Math.floor(diff / DAY) });
  return new Date(at).toLocaleDateString();
};

/** Human label for an audit entry's surface (IM platform / desktop / remote). */
export const surfaceLabel = (t: TFunction, surface: PublicAgentAuditSurface, platform: string | null): string => {
  if (surface === 'channel') return platform || t('publicCompanion.audit.surfaceChannel', { defaultValue: '社交渠道' });
  if (surface === 'remote') return t('publicCompanion.audit.surfaceRemote', { defaultValue: '远程' });
  return t('publicCompanion.audit.surfaceDesktop', { defaultValue: '桌面' });
};
