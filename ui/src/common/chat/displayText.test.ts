/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { compactDisplayText, extractResponseTextChunk, optionalDisplayText, toDisplayText } from './displayText';

describe('display text normalization', () => {
  test('keeps strings unchanged and serializes structured runtime values', () => {
    expect(toDisplayText('plain')).toBe('plain');
    expect(toDisplayText({ command: 'codex --version' })).toBe('{\n  "command": "codex --version"\n}');
  });

  test('compacts structured values for receipt labels', () => {
    expect(compactDisplayText({ command: 'codex --version' })).toBe('{ "command": "codex --version" }');
  });

  test('omits nullish optional values but keeps serializable values', () => {
    expect(optionalDisplayText(null)).toBeUndefined();
    expect(optionalDisplayText({ description: 'needs confirmation' })).toBe('{\n  "description": "needs confirmation"\n}');
  });

  test('extracts text chunks from rich stream payloads without returning objects', () => {
    expect(extractResponseTextChunk('hello')).toBe('hello');
    expect(extractResponseTextChunk({ content: { command: 'npm test' } })).toBe('{\n  "command": "npm test"\n}');
    expect(extractResponseTextChunk({ delta: 'ignored' })).toBe('');
  });
});
