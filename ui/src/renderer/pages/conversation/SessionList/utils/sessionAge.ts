/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

const DAY_MS = 24 * 60 * 60 * 1000;
const HOUR_MS = 60 * 60 * 1000;
const MINUTE_MS = 60 * 1000;

export type SessionAgeBucket =
  | {
      kind: 'now';
    }
  | {
      kind: 'minutes';
      count: number;
    }
  | {
      kind: 'hours';
      count: number;
    }
  | {
      kind: 'days';
      count: number;
    };

export function getSessionAgeDays(createdAt: number | undefined | null, now = Date.now()): number | null {
  if (typeof createdAt !== 'number' || !Number.isFinite(createdAt) || createdAt <= 0) {
    return null;
  }

  return Math.max(0, Math.floor((now - createdAt) / DAY_MS));
}

const dayStartOf = (timestamp: number): number => {
  const date = new Date(timestamp);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
};

export function getSessionAgeBucket(createdAt: number | undefined | null, now = Date.now()): SessionAgeBucket | null {
  if (typeof createdAt !== 'number' || !Number.isFinite(createdAt) || createdAt <= 0) {
    return null;
  }

  const dayDiff = Math.max(0, Math.round((dayStartOf(now) - dayStartOf(createdAt)) / DAY_MS));
  if (dayDiff > 0) {
    return { kind: 'days', count: dayDiff };
  }

  const elapsedMs = Math.max(0, now - createdAt);
  const elapsedMinutes = Math.floor(elapsedMs / MINUTE_MS);
  if (elapsedMinutes <= 0) {
    return { kind: 'now' };
  }
  if (elapsedMinutes < 60) {
    return { kind: 'minutes', count: elapsedMinutes };
  }

  return { kind: 'hours', count: Math.floor(elapsedMs / HOUR_MS) };
}

export function formatSessionAgeLabel(
  t: (key: string, options?: Record<string, unknown>) => string,
  createdAt: number | undefined | null,
  now = Date.now()
): string | null {
  const bucket = getSessionAgeBucket(createdAt, now);
  if (!bucket) return null;
  if (bucket.kind === 'now') return t('sessionList.createdNow');
  if (bucket.kind === 'minutes') return t('sessionList.createdMinutes', { count: bucket.count });
  if (bucket.kind === 'hours') return t('sessionList.createdHours', { count: bucket.count });
  return t('sessionList.createdDays', { count: bucket.count });
}
