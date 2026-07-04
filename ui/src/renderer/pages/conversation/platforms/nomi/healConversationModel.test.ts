/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { resolveHealModel } from './healConversationModel';

const getAvailable = (p: any) => (p.models ?? []) as string[];
const provs = [
  { id: 'prov_a', models: ['m1', 'm2'] },
  { id: 'prov_b', models: ['m3'] },
] as any[];

describe('resolveHealModel', () => {
  test('returns null when bound provider still available', () => {
    expect(resolveHealModel({ id: 'prov_a', use_model: 'm1' } as any, provs, getAvailable, undefined)).toBeNull();
  });
  test('heals to saved default when bound provider gone', () => {
    const r = resolveHealModel({ id: 'prov_dead', use_model: 'x' } as any, provs, getAvailable, { id: 'prov_b', use_model: 'm3' });
    expect(r?.provider.id).toBe('prov_b');
    expect(r?.use_model).toBe('m3');
  });
  test('heals to first available when no valid default', () => {
    const r = resolveHealModel({ id: 'prov_dead', use_model: 'x' } as any, provs, getAvailable, undefined);
    expect(r?.provider.id).toBe('prov_a');
    expect(r?.use_model).toBe('m1');
  });
  test('returns null when there are no providers at all', () => {
    expect(resolveHealModel({ id: 'prov_dead', use_model: 'x' } as any, [], getAvailable, undefined)).toBeNull();
  });
  test('returns null when the conversation has no bound provider', () => {
    expect(resolveHealModel({ id: '', use_model: '' } as any, provs, getAvailable, undefined)).toBeNull();
    expect(resolveHealModel(undefined, provs, getAvailable, undefined)).toBeNull();
  });
  test('falls back to first available when saved default model is unavailable', () => {
    // saved default provider exists but its stored model is no longer offered
    const r = resolveHealModel({ id: 'prov_dead', use_model: 'x' } as any, provs, getAvailable, { id: 'prov_a', use_model: 'zzz' });
    expect(r?.provider.id).toBe('prov_a');
    expect(r?.use_model).toBe('m1');
  });
});
