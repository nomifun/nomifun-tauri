/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import {
  LOCAL_MODEL_CAPABILITIES,
  capabilityActivity,
  detailsForcedOpen,
} from './localModelCapabilityView';

describe('local model capability center view state', () => {
  test('keeps implemented and planned capabilities in product order', () => {
    expect(LOCAL_MODEL_CAPABILITIES.map(({ key, implemented }) => [key, implemented])).toEqual([
      ['text', true],
      ['image', true],
      ['speech_recognition', true],
      ['speech_synthesis', false],
    ]);
  });

  test('forces details open only for actionable transfer states', () => {
    expect(detailsForcedOpen('not_installed', false)).toBe(false);
    expect(detailsForcedOpen('installed', false)).toBe(false);
    expect(detailsForcedOpen('downloading', false)).toBe(true);
    expect(detailsForcedOpen('verifying', false)).toBe(true);
    expect(detailsForcedOpen('extracting', false)).toBe(true);
    expect(detailsForcedOpen('paused', false)).toBe(true);
    expect(detailsForcedOpen('failed', false)).toBe(true);
    expect(detailsForcedOpen('installed', true)).toBe(true);
  });

  test('summarizes hidden tab activity with errors taking precedence', () => {
    expect(capabilityActivity(['installed'], false)).toBe('idle');
    expect(capabilityActivity(['downloading'], false)).toBe('running');
    expect(capabilityActivity(['installed'], true)).toBe('error');
    expect(capabilityActivity(['downloading'], true)).toBe('error');
  });
});
