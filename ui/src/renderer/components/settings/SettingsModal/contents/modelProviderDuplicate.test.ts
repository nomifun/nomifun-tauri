/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('model provider duplicate action', () => {
  test('exposes a row action that clones an entire provider configuration', () => {
    const source = readSource(new URL('./ModelModalContent.tsx', import.meta.url));

    expect(source.includes('cloneProviderConfig')).toBe(true);
    expect(source.includes('duplicatePlatform')).toBe(true);
    expect(source.includes('settings.copyProviderConfig')).toBe(true);
    expect(source.includes('<Copy theme')).toBe(true);
    expect(source.indexOf('icon={<Write size')).toBeLessThan(source.indexOf('<Copy theme'));
  });
});
