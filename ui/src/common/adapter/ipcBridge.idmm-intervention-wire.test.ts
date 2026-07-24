/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./ipcBridge.ts', import.meta.url), 'utf8');

describe('IDMM intervention wire ID contract', () => {
  test('uses intervention_id without a generic id compatibility path', () => {
    expect(source.includes('intervention_id: IdmmInterventionId;')).toBe(true);
    expect(
      source.includes('intervention_id: parseIdmmInterventionId(record.intervention_id)'),
    ).toBe(true);
    expect(source.includes('id: parseIdmmInterventionId(record.id)')).toBe(false);
  });
});
