/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { presets } from './ipcBridge';
import { parsePresetReference, type PresetSource } from '../types/agent/presetTypes';

const USER_PRESET_ID = '0190f5fe-7c00-7a00-8000-000000000001';
const PROVIDER_ID = '0190f5fe-7c00-7a00-8000-000000000002';
const KNOWLEDGE_BASE_ID = '0190f5fe-7c00-7a00-8000-000000000003';
const PRESET_TAG_ID = '0190f5fe-7c00-7a00-8000-000000000005';
const realFetch = globalThis.fetch;

async function expectTypeError(action: () => Promise<unknown>): Promise<void> {
  let error: unknown;
  try {
    await action();
  } catch (caught) {
    error = caught;
  }
  expect(error instanceof TypeError).toBe(true);
}

const rawPreset = (
  preset_id: unknown,
  source: PresetSource,
  source_key?: string,
) => ({
  preset_id,
  revision: 1,
  source,
  source_key,
  name: 'Boundary preset',
  name_i18n: {},
  description_i18n: {},
  instructions: '',
  instructions_i18n: {},
  fallback_allowed: false,
  targets: ['conversation'],
  agent_preferences: [],
  model_preferences: [{ provider_id: PROVIDER_ID, model: 'gpt-5', required: false }],
  included_skills: [],
  excluded_auto_skills: [],
  knowledge_policy: {
    enabled: false,
    mode: 'inherit',
    writeback: false,
    grounded: false,
  },
  knowledge_bases: [{ knowledge_base_id: KNOWLEDGE_BASE_ID, required: false }],
  examples: [],
  examples_i18n: {},
  audience_tag_ids: [],
  scenario_tag_ids: [],
  enabled: true,
  auto_selectable: false,
  sort_order: 0,
});

const respondWith = (data: unknown) => {
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response(JSON.stringify({ success: true, data }), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      }),
    )) as unknown as typeof fetch;
};

describe('preset wire ID contract', () => {
  test('keeps preset_id on responses and does not synthesize generic id', async () => {
    try {
      respondWith([
        rawPreset(USER_PRESET_ID, 'user', USER_PRESET_ID),
        rawPreset('0190f5fe-7c00-7a00-8000-000000000004', 'builtin', 'builtin:office'),
      ]);

      const [userPreset, builtinPreset] = await presets.list.invoke();

      expect(userPreset?.preset_id).toBe(USER_PRESET_ID);
      expect(builtinPreset?.preset_id).toBe('0190f5fe-7c00-7a00-8000-000000000004');
      expect(builtinPreset?.source_key).toBe('builtin:office');
      expect(Object.prototype.hasOwnProperty.call(userPreset, 'id')).toBe(false);
      expect(Object.prototype.hasOwnProperty.call(builtinPreset, 'id')).toBe(false);
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  test('rejects legacy generic-id responses and non-UUID preset IDs', async () => {
    try {
      respondWith({
        ...rawPreset(USER_PRESET_ID, 'user', USER_PRESET_ID),
        id: USER_PRESET_ID,
      });
      await expectTypeError(() =>
        presets.get.invoke({ preset_id: parsePresetReference(USER_PRESET_ID, 'user') }),
      );

      respondWith(rawPreset('office', 'builtin', 'builtin:office'));
      await expectTypeError(() =>
        presets.get.invoke({ preset_id: parsePresetReference(USER_PRESET_ID, 'user') }),
      );
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  test('uses preset_id only as the locator and strips it from path-routed bodies', async () => {
    try {
      const calls: Array<{ method: string; url: string; body?: unknown }> = [];
      globalThis.fetch = (async (input, init) => {
        calls.push({
          method: init?.method ?? 'GET',
          url: String(input),
          body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
        });
        return new Response(JSON.stringify({
          success: true,
          data: rawPreset(USER_PRESET_ID, 'user', USER_PRESET_ID),
        }), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        });
      }) as typeof fetch;

      const preset_id = parsePresetReference(USER_PRESET_ID, 'user');
      await presets.get.invoke({ preset_id });
      await presets.update.invoke({ preset_id, name: 'Updated' });
      await presets.setState.invoke({ preset_id, enabled: false });

      expect(calls.map(({ method, url }) => `${method} ${url}`)).toEqual([
        `GET http://127.0.0.1:13400/api/presets/${USER_PRESET_ID}`,
        `PUT http://127.0.0.1:13400/api/presets/${USER_PRESET_ID}`,
        `PATCH http://127.0.0.1:13400/api/presets/${USER_PRESET_ID}/state`,
      ]);
      expect(calls[1]?.body).toEqual({ name: 'Updated' });
      expect(calls[2]?.body).toEqual({ enabled: false });
      expect(Object.prototype.hasOwnProperty.call(calls[1]?.body ?? {}, 'id')).toBe(false);
      expect(Object.prototype.hasOwnProperty.call(calls[1]?.body ?? {}, 'preset_id')).toBe(false);
      expect(Object.prototype.hasOwnProperty.call(calls[2]?.body ?? {}, 'id')).toBe(false);
      expect(Object.prototype.hasOwnProperty.call(calls[2]?.body ?? {}, 'preset_id')).toBe(false);
    } finally {
      globalThis.fetch = realFetch;
    }
  });
});
