/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { resolveHealModel } from './healConversationModel';

const getAvailable = (p: any) => (p.models ?? []) as string[];
const PROVIDER_A = '0190f5fe-7c00-7a00-8000-00000000000a';
const PROVIDER_B = '0190f5fe-7c00-7a00-8000-00000000000b';
const PROVIDER_DEAD = '0190f5fe-7c00-7a00-8000-00000000dead';
const provs = [
  { id: PROVIDER_A, models: ['m1', 'm2'] },
  { id: PROVIDER_B, models: ['m3'] },
] as any[];

describe('resolveHealModel', () => {
  test('returns null when bound provider still available', () => {
    expect(resolveHealModel({ id: PROVIDER_A, use_model: 'm1' } as any, provs, getAvailable, undefined)).toBeNull();
  });
  test('heals to saved default when bound provider gone', () => {
    const r = resolveHealModel(
      { id: PROVIDER_DEAD, use_model: 'x' } as any,
      provs,
      getAvailable,
      { provider_id: PROVIDER_B, model: 'm3' } as any,
    );
    expect(r?.provider.id).toBe(PROVIDER_B);
    expect(r?.use_model).toBe('m3');
  });
  test('heals to first available when no valid default', () => {
    const r = resolveHealModel({ id: PROVIDER_DEAD, use_model: 'x' } as any, provs, getAvailable, undefined);
    expect(r?.provider.id).toBe(PROVIDER_A);
    expect(r?.use_model).toBe('m1');
  });
  test('returns null when there are no providers at all', () => {
    expect(resolveHealModel({ id: PROVIDER_DEAD, use_model: 'x' } as any, [], getAvailable, undefined)).toBeNull();
  });
  test('returns null when the conversation has no bound provider', () => {
    expect(resolveHealModel({ id: '', use_model: '' } as any, provs, getAvailable, undefined)).toBeNull();
    expect(resolveHealModel(undefined, provs, getAvailable, undefined)).toBeNull();
  });
  test('falls back to first available when saved default model is unavailable', () => {
    // saved default provider exists but its stored model is no longer offered
    const r = resolveHealModel(
      { id: PROVIDER_DEAD, use_model: 'x' } as any,
      provs,
      getAvailable,
      { provider_id: PROVIDER_A, model: 'zzz' } as any,
    );
    expect(r?.provider.id).toBe(PROVIDER_A);
    expect(r?.use_model).toBe('m1');
  });
});
