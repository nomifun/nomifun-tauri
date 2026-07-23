/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { parseUserId, type UserId } from '@/common/types/ids';

export const QR_LOGIN_RESUME_KEY = 'nomifun:qr-login-resume';

const QR_LOGIN_RESUME_MAX_AGE_MS = 30_000;

export type QrLoginResumeUser = {
  id: UserId;
  username: string;
};

type QrLoginResumePayload = {
  at: number;
  user: { user_id: string; username: string };
};

const isResumePayload = (value: unknown): value is QrLoginResumePayload => {
  if (!value || typeof value !== 'object') return false;
  const payload = value as Partial<QrLoginResumePayload>;
  const user = payload.user as (Partial<QrLoginResumePayload['user']> & { id?: unknown }) | undefined;
  return (
    typeof payload.at === 'number' &&
    Boolean(user) &&
    !Object.prototype.hasOwnProperty.call(user, 'id') &&
    typeof user?.user_id === 'string' &&
    typeof user?.username === 'string'
  );
};

export function consumeQrLoginResume(now = Date.now()): QrLoginResumeUser | null {
  if (typeof window === 'undefined') return null;

  let raw: string | null = null;
  try {
    raw = window.sessionStorage.getItem(QR_LOGIN_RESUME_KEY);
  } catch {
    return null;
  }
  if (!raw) return null;

  try {
    const parsed = JSON.parse(raw) as unknown;
    if (!isResumePayload(parsed)) return null;
    if (now - parsed.at > QR_LOGIN_RESUME_MAX_AGE_MS) return null;
    return { id: parseUserId(parsed.user.user_id), username: parsed.user.username };
  } catch {
    return null;
  } finally {
    try {
      window.sessionStorage.removeItem(QR_LOGIN_RESUME_KEY);
    } catch {
      // Best effort only; failing to clear cannot grant backend access.
    }
  }
}
