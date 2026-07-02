/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('API key save validation wiring', () => {
  test('validates every provider API key before create or edit saves', () => {
    const addSource = readSource(new URL('./AddPlatformModal.tsx', import.meta.url));
    const editSource = readSource(new URL('./EditModeModal.tsx', import.meta.url));
    const editorSource = readSource(new URL('./ApiKeyEditorModal.tsx', import.meta.url));

    for (const source of [addSource, editSource, editorSource]) {
      expect(source.includes('validateApiKeysForSave')).toBe(true);
      expect(source.includes('removeInvalidApiKeysBeforeSave')).toBe(true);
    }

    expect(addSource.includes('mode.detectProtocol.invoke')).toBe(true);
    expect(editSource.includes('mode.detectProtocol.invoke')).toBe(true);
    expect(editorSource.includes('onSave(validation.normalized)')).toBe(true);
  });
});
