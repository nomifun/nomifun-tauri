/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(
  new URL('./KnowledgeModelSelector.tsx', import.meta.url),
  'utf8',
);

describe('knowledge explicit model preference', () => {
  test('keeps a stale explicit pair visible and marks it unavailable', () => {
    expect(
      source.includes(
        'return { provider_id: stored.provider_id, model: stored.model };',
      ),
    ).toBe(true);
    expect(source.includes("t('knowledge.form.modelUnavailable')")).toBe(true);
    expect(source.includes("status={choiceUnavailable ? 'warning' : undefined}")).toBe(true);
    expect(source.includes("t('knowledge.form.modelUnavailableHint')")).toBe(true);
  });

  test('only an absent or malformed setting selects backend default', () => {
    expect(
      source.includes('if (!stored?.provider_id || !stored.model) return null;'),
    ).toBe(true);
    expect(source.includes('if (!provider) return null')).toBe(false);
    expect(
      source.includes(
        'if (!getAvailableModels(provider).includes(stored.model)) return null',
      ),
    ).toBe(false);
  });
});
