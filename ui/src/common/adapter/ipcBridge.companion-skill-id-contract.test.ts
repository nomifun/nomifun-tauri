/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { companion } from './ipcBridge';
import {
  parseCompanionEventId,
  parseCompanionId,
  parseCompanionSkillId,
  parseSkillPatternId,
} from '../types/ids';

const COMPANION_ID = parseCompanionId('0190f5fe-7c00-7a00-8000-000000000001');
const COMPANION_SKILL_ID = parseCompanionSkillId('0190f5fe-7c00-7a00-8000-000000000002');
const COMPANION_EVENT_ID = parseCompanionEventId('0190f5fe-7c00-7a00-8000-000000000003');
const SKILL_PATTERN_ID = parseSkillPatternId('0190f5fe-7c00-7a00-8000-000000000004');
const TO_COMPANION_ID = parseCompanionId('0190f5fe-7c00-7a00-8000-000000000005');

const realFetch = globalThis.fetch;

const rawSkill = (overrides: Record<string, unknown> = {}) => ({
  companion_skill_id: COMPANION_SKILL_ID,
  skill_name: 'research',
  scope_kind: 'companion',
  scope_companion_id: COMPANION_ID,
  status: 'draft',
  source: 'evolution',
  confidence: 0.9,
  provenance_event_ids: [COMPANION_EVENT_ID],
  strength: 0.7,
  version: 1,
  skill_pattern_id: SKILL_PATTERN_ID,
  usage_count: 2,
  last_used_at: null,
  created_at: 1,
  updated_at: 2,
  description: 'A durable research workflow.',
  ...overrides,
});

const jsonResponse = (data: unknown): Response =>
  new Response(JSON.stringify({ success: true, data }), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  });

const installFetch = (
  handler: (input: RequestInfo | URL, init?: RequestInit) => Response | Promise<Response>,
): void => {
  globalThis.fetch = handler as typeof fetch;
};

const expectRejected = async (action: () => Promise<unknown>): Promise<void> => {
  let error: unknown;
  try {
    await action();
  } catch (caught) {
    error = caught;
  }
  expect(error instanceof TypeError).toBe(true);
};

describe('companion skill v3 wire contract', () => {
  test('maps the explicit skill identity and nested provenance IDs at the HTTP boundary', async () => {
    const calls: Array<{ method: string; url: string; body?: unknown }> = [];
    try {
      installFetch(async (input, init) => {
        calls.push({
          method: init?.method ?? 'GET',
          url: String(input),
          body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
        });
        return jsonResponse({
          items: [rawSkill()],
          total: 1,
        });
      });

      const page = await companion.listSkills.invoke({
        companion_id: COMPANION_ID,
        include_shared: false,
        status: 'draft',
        limit: 10,
        offset: 20,
      });

      expect(page.total).toBe(1);
      expect(page.items[0]?.companion_skill_id).toBe(COMPANION_SKILL_ID);
      expect(page.items[0]?.provenance_event_ids).toEqual([COMPANION_EVENT_ID]);
      expect(page.items[0]?.skill_pattern_id).toBe(SKILL_PATTERN_ID);
      expect(Object.prototype.hasOwnProperty.call(page.items[0], 'provenance')).toBe(false);
      expect(Object.prototype.hasOwnProperty.call(page.items[0], 'superseded_by')).toBe(false);
      expect(calls).toEqual([
        {
          method: 'GET',
          url:
            `http://127.0.0.1:13400/api/companion/companions/${COMPANION_ID}/skills` +
            '?include_shared=false&status=draft&limit=10&offset=20',
        },
      ]);
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  test('addresses every mutable skill operation by companion_skill_id and strips locator fields from bodies', async () => {
    const calls: Array<{ method: string; url: string; body?: unknown }> = [];
    try {
      installFetch(async (input, init) => {
        calls.push({
          method: init?.method ?? 'GET',
          url: String(input),
          body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
        });
        if (init?.method === 'PUT') return new Response(null, { status: 204 });
        if (init?.method === 'GET') {
          return jsonResponse({ skill: rawSkill(), content: '# research\n' });
        }
        return jsonResponse(rawSkill());
      });

      const content = await companion.getSkillContent.invoke({
        companion_id: COMPANION_ID,
        companion_skill_id: COMPANION_SKILL_ID,
      });
      expect(content.skill.companion_skill_id).toBe(COMPANION_SKILL_ID);
      expect(content.content).toBe('# research\n');

      await companion.writeSkillContent.invoke({
        companion_id: COMPANION_ID,
        companion_skill_id: COMPANION_SKILL_ID,
        content: '# updated\n',
      });
      const decided = await companion.decideSkill.invoke({
        companion_id: COMPANION_ID,
        companion_skill_id: COMPANION_SKILL_ID,
        accept: true,
        reason: 'useful',
      });
      expect(decided.companion_skill_id).toBe(COMPANION_SKILL_ID);

      const gifted = await companion.giftSkill.invoke({
        companion_id: COMPANION_ID,
        companion_skill_id: COMPANION_SKILL_ID,
        to_companion_id: TO_COMPANION_ID,
      });
      expect(gifted.companion_skill_id).toBe(COMPANION_SKILL_ID);

      expect(calls).toEqual([
        {
          method: 'GET',
          url: `http://127.0.0.1:13400/api/companion/companions/${COMPANION_ID}/skills/${COMPANION_SKILL_ID}`,
        },
        {
          method: 'PUT',
          url: `http://127.0.0.1:13400/api/companion/companions/${COMPANION_ID}/skills/${COMPANION_SKILL_ID}`,
          body: { content: '# updated\n' },
        },
        {
          method: 'POST',
          url:
            `http://127.0.0.1:13400/api/companion/companions/${COMPANION_ID}/skills/` +
            `${COMPANION_SKILL_ID}/decide`,
          body: { accept: true, reason: 'useful' },
        },
        {
          method: 'POST',
          url:
            `http://127.0.0.1:13400/api/companion/companions/${COMPANION_ID}/skills/` +
            `${COMPANION_SKILL_ID}/gift`,
          body: { to_companion_id: TO_COMPANION_ID },
        },
      ]);
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  test('rejects legacy fields and every non-canonical v3 skill ID at the response boundary', async () => {
    const invalidSkills = [
      rawSkill({ companion_skill_id: `skill_${COMPANION_SKILL_ID}` }),
      rawSkill({ companion_skill_id: COMPANION_SKILL_ID.toUpperCase() }),
      rawSkill({ companion_skill_id: '550e8400-e29b-41d4-a716-446655440000' }),
      rawSkill({ provenance_event_ids: [`event_${COMPANION_EVENT_ID}`] }),
      rawSkill({ skill_pattern_id: `pattern_${SKILL_PATTERN_ID}` }),
      rawSkill({ provenance: [] }),
      rawSkill({ superseded_by: null }),
      rawSkill({ provenance_event_ids: '[]' }),
    ];

    try {
      for (const invalidSkill of invalidSkills) {
        installFetch(async () => jsonResponse({ items: [invalidSkill], total: 1 }));
        await expectRejected(() => companion.listSkills.invoke({ companion_id: COMPANION_ID }));
      }
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  test('maps skill lifecycle events with companion_skill_id instead of a name-only identity', () => {
    const originalWindow = (globalThis as { window?: unknown }).window;
    const originalWebSocket = globalThis.WebSocket;

    class FakeWebSocket {
      static readonly CONNECTING = 0;
      static readonly OPEN = 1;
      static readonly CLOSING = 2;
      static readonly CLOSED = 3;
      static instance: FakeWebSocket | undefined;

      readyState = FakeWebSocket.OPEN;
      private readonly listeners = new Map<string, Array<(event: unknown) => void>>();

      constructor(..._args: unknown[]) {
        FakeWebSocket.instance = this;
      }

      addEventListener(type: string, listener: (event: unknown) => void): void {
        const listeners = this.listeners.get(type) ?? [];
        listeners.push(listener);
        this.listeners.set(type, listeners);
      }

      send(_data: string): void {}

      close(): void {
        this.readyState = FakeWebSocket.CLOSED;
      }

      dispatch(type: string, event: unknown): void {
        for (const listener of this.listeners.get(type) ?? []) listener(event);
      }
    }

    (globalThis as { window?: unknown }).window = {
      location: { protocol: 'http:', host: 'localhost:13400' },
    };
    globalThis.WebSocket = FakeWebSocket as unknown as typeof WebSocket;

    let unsubscribe = () => {};
    try {
      const received: Array<{
        companion_id: string;
        companion_skill_id: string;
        skill_name: string;
      }> = [];
      unsubscribe = companion.onSkillDrafted.on((event) => {
        received.push(event);
      });

      const socket = FakeWebSocket.instance;
      if (!socket) throw new Error('skill lifecycle subscription did not create a WebSocket');
      socket.dispatch('message', {
        data: JSON.stringify({
          name: 'companion.skill-drafted',
          data: {
            companion_id: COMPANION_ID,
            companion_skill_id: COMPANION_SKILL_ID,
            skill_name: 'research',
          },
        }),
      });

      expect(received).toEqual([
        {
          companion_id: COMPANION_ID,
          companion_skill_id: COMPANION_SKILL_ID,
          skill_name: 'research',
        },
      ]);
    } finally {
      unsubscribe();
      FakeWebSocket.instance?.close();
      (globalThis as { window?: unknown }).window = originalWindow;
      globalThis.WebSocket = originalWebSocket;
    }
  });
});
