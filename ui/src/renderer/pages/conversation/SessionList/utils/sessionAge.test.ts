/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';

import { formatSessionAgeLabel, getSessionAgeBucket, getSessionAgeDays } from './sessionAge';

const DAY_MS = 24 * 60 * 60 * 1000;

describe('getSessionAgeDays', () => {
  test('returns 0 for sessions created less than one day ago', () => {
    expect(getSessionAgeDays(10_000, 10_000 + DAY_MS - 1)).toBe(0);
  });

  test('floors whole elapsed days since creation', () => {
    expect(getSessionAgeDays(10_000, 10_000 + DAY_MS * 3 + 123)).toBe(3);
  });

  test('clamps future timestamps to today', () => {
    expect(getSessionAgeDays(20_000, 10_000)).toBe(0);
  });

  test('returns null for missing timestamps', () => {
    expect(getSessionAgeDays(0, 10_000)).toBeNull();
    expect(getSessionAgeDays(Number.NaN, 10_000)).toBeNull();
  });
});

describe('getSessionAgeBucket', () => {
  const at = (day: number, hour: number, minute: number) => new Date(2026, 6, day, hour, minute).getTime();

  test('uses a just-now bucket for sessions created less than one minute ago today', () => {
    const now = new Date(2026, 6, 5, 12, 45, 30).getTime();
    const createdAt = new Date(2026, 6, 5, 12, 45, 0).getTime();

    expect(getSessionAgeBucket(createdAt, now)).toEqual({
      kind: 'now',
    });
  });

  test('groups sessions created in the same natural day by elapsed minutes', () => {
    expect(getSessionAgeBucket(at(5, 12, 10), at(5, 12, 45))).toEqual({
      kind: 'minutes',
      count: 35,
    });
  });

  test('groups sessions created in the same natural day by elapsed hours', () => {
    expect(getSessionAgeBucket(at(5, 9, 10), at(5, 12, 45))).toEqual({
      kind: 'hours',
      count: 3,
    });
  });

  test('uses natural day boundaries for older sessions', () => {
    expect(getSessionAgeBucket(at(4, 23, 50), at(5, 0, 10))).toEqual({
      kind: 'days',
      count: 1,
    });
  });
});

describe('formatSessionAgeLabel', () => {
  const t = (key: string, options?: Record<string, unknown>) =>
    key === 'sessionList.createdNow'
      ? '刚刚'
      : key === 'sessionList.createdMinutes'
        ? `${options?.count}分钟`
      : key === 'sessionList.createdHours'
        ? `${options?.count}小时`
        : `${options?.count}天`;

  test('formats today as minute/hour snapshots and older natural days as days', () => {
    const at = (day: number, hour: number, minute: number) => new Date(2026, 6, day, hour, minute).getTime();
    const now = new Date(2026, 6, 5, 12, 45, 30).getTime();
    const justNow = new Date(2026, 6, 5, 12, 45, 0).getTime();

    expect(formatSessionAgeLabel(t, justNow, now)).toBe('刚刚');
    expect(formatSessionAgeLabel(t, at(5, 12, 10), at(5, 12, 45))).toBe('35分钟');
    expect(formatSessionAgeLabel(t, at(5, 9, 10), at(5, 12, 45))).toBe('3小时');
    expect(formatSessionAgeLabel(t, at(4, 23, 50), at(5, 0, 10))).toBe('1天');
  });
});
