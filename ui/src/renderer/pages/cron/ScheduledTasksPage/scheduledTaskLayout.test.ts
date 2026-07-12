/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import cronEn from '@renderer/services/i18n/locales/en-US/cron.json';
import cronZh from '@renderer/services/i18n/locales/zh-CN/cron.json';
import * as scheduledTaskLayout from './scheduledTaskLayout';

const pageSource = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');

test('keeps responsive utility classes in JSX instead of runtime exports', () => {
  const layout = scheduledTaskLayout as Record<string, unknown>;

  expect(layout.getScheduledTaskLayout).toBeUndefined();
  expect(layout.SCHEDULED_TASK_LIST_CLASS_NAMES).toBeUndefined();
  expect(layout.SCHEDULED_TASK_ROW_CLASS_NAMES).toBeUndefined();
});

test('defines five readable desktop columns', () => {
  expect((scheduledTaskLayout as Record<string, unknown>).DESKTOP_SCHEDULED_TASK_COLUMNS).toBe(
    'minmax(0,1.6fr) minmax(150px,1.1fr) minmax(84px,auto) minmax(120px,1fr) 44px'
  );
});

test('provides localized desktop-only column labels', () => {
  expect((cronZh.page as Record<string, unknown>).list).toEqual({
    task: '任务标题',
    status: '任务状态',
    action: '启停',
  });
  expect((cronEn.page as Record<string, unknown>).list).toEqual({
    task: 'Task',
    status: 'Status',
    action: 'On / off',
  });
});

test('uses compact desktop task rows', () => {
  expect(pageSource.includes('md:min-h-48px')).toBe(true);
  expect(pageSource.includes('md:py-8px')).toBe(true);
  expect(pageSource.includes('md:min-h-68px')).toBe(false);
  expect(pageSource.includes('md:py-14px')).toBe(false);
});

test('removes only the desktop perimeter and keeps internal dividers', () => {
  expect(pageSource.includes('rounded-t-12px')).toBe(false);
  expect(pageSource.includes('md:rounded-b-12px')).toBe(false);
  expect(pageSource.includes('md:divide-y')).toBe(true);
  expect(pageSource.includes('border-b-[var(--color-border-2)]')).toBe(true);
});

test('keeps desktop table surfaces transparent', () => {
  const desktopHeaderClass =
    pageSource.match(/className='hidden items-center gap-16px[^']*md:grid'/)?.[0] ?? '';
  const desktopListClass =
    pageSource.match(/className='grid w-full grid-cols-1 items-start gap-12px[^']*md:divide-\[var\(--color-border-2\)\]'/)?.[0] ?? '';
  const desktopRowClass =
    pageSource.match(/className='group flex cursor-pointer flex-col[^']*md:hover:shadow-none'/)?.[0] ?? '';

  expect(desktopHeaderClass.includes('bg-fill-2')).toBe(false);
  expect(desktopListClass.includes('md:bg-fill-1')).toBe(false);
  expect(desktopRowClass.includes('bg-fill-1')).toBe(true);
  expect(desktopRowClass.includes('md:bg-transparent')).toBe(true);
  expect(desktopHeaderClass.includes('border-b-[var(--color-border-2)]')).toBe(true);
  expect(desktopListClass.includes('md:divide-y')).toBe(true);
});

test('styles the scheduled task search as a bordered pill', () => {
  const searchClass =
    pageSource.match(/<Input\.Search[\s\S]*?className='([^']+)'[\s\S]*?\/>/)?.[1] ?? '';
  const searchClasses = searchClass.split(/\s+/);

  expect(searchClasses.includes('[&_.arco-input-inner-wrapper]:!rounded-full')).toBe(true);
  expect(searchClasses.includes('[&_.arco-input-inner-wrapper]:!border')).toBe(true);
  expect(searchClasses.includes('[&_.arco-input-inner-wrapper]:!border-solid')).toBe(true);
  expect(searchClasses.includes('[&_.arco-input-inner-wrapper]:!border-[var(--color-border-2)]')).toBe(true);
  expect(searchClasses.includes('[&_.arco-input-inner-wrapper:hover]:!border-[var(--color-border-3)]')).toBe(true);
  expect(searchClasses.includes('[&_.arco-input-inner-wrapper-focus]:!border-[rgb(var(--primary-6))]')).toBe(true);
});

test('uses a desktop more menu without changing the mobile switch contract', () => {
  expect(pageSource.includes("t('cron.page.list.action')")).toBe(false);
  expect(pageSource.includes("import ScheduledTaskActions from './ScheduledTaskActions'")).toBe(true);
  expect(pageSource.includes('deleteJob')).toBe(true);
  expect(pageSource.includes('<ScheduledTaskActions')).toBe(true);

  const mobileSwitchBlock =
    pageSource.match(/className='shrink-0 md:hidden'[\s\S]*?<Switch[\s\S]*?handleToggleEnabled\(job\)/)?.[0] ?? '';
  expect(Boolean(mobileSwitchBlock)).toBe(true);
});
