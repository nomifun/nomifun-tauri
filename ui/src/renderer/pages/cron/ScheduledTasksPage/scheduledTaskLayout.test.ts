/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { expect, test } from 'bun:test';
import cronEn from '@renderer/services/i18n/locales/en-US/cron.json';
import cronZh from '@renderer/services/i18n/locales/zh-CN/cron.json';
import * as scheduledTaskLayout from './scheduledTaskLayout';

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
