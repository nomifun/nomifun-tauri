/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('preset catalog cache consistency', () => {
  test('settings and Guid consume the same SWR catalog identity', () => {
    const listHook = readSource(new URL('./usePresetList.ts', import.meta.url));
    const guidLoader = readSource(
      new URL('../../pages/guid/hooks/useCustomAgentsLoader.ts', import.meta.url),
    );
    const conversationLoader = readSource(
      new URL('../../pages/conversation/hooks/useConversationAgents.ts', import.meta.url),
    );

    for (const source of [listHook, guidLoader, conversationLoader]) {
      expect(source.includes('PRESET_CATALOG_SWR_KEY')).toBe(true);
      expect(source.includes('fetchPresetCatalog')).toBe(true);
    }
  });

  test('every preset editor mutation refreshes the shared catalog', () => {
    const editor = readSource(new URL('./usePresetEditor.ts', import.meta.url));
    const mutationCalls =
      editor.match(/ipcBridge\.presets\.(?:create|update|delete|setState)\.invoke/g) ?? [];
    const refreshCalls = editor.match(/await loadPresets\(\)/g) ?? [];

    // create may also set state, and update always sets state, so the editor
    // refreshes once per completed mutation group rather than once per HTTP call.
    expect(mutationCalls.length).toBeGreaterThanOrEqual(6);
    expect(refreshCalls.length).toBe(4);
  });
});
