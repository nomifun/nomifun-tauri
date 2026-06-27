/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { detectFamily, isTerminalAutoworkCapable } from './detectFamily';

describe('detectFamily', () => {
  test('detects direct and path-qualified invocations', () => {
    expect(detectFamily('claude')).toBe('claude');
    expect(detectFamily('/usr/local/bin/codex')).toBe('codex');
    expect(detectFamily('gemini --yolo')).toBe('gemini');
  });

  test('detects wrapped invocations via any token', () => {
    expect(detectFamily('stepcode claude')).toBe('claude');
    expect(detectFamily('npx codex')).toBe('codex');
  });

  test('returns null for unknown CLIs / shells', () => {
    expect(detectFamily('')).toBeNull();
    expect(detectFamily('/bin/bash -l')).toBeNull();
    expect(detectFamily('stepcode frobnicate')).toBeNull();
  });
});

describe('isTerminalAutoworkCapable', () => {
  test('bare / direct agent CLIs are capable', () => {
    expect(isTerminalAutoworkCapable('claude')).toBe(true);
    expect(isTerminalAutoworkCapable('codex')).toBe(true);
  });

  test('wrappers (command + args, no declared backend) are capable', () => {
    expect(isTerminalAutoworkCapable('stepcode', ['claude'])).toBe(true);
    expect(isTerminalAutoworkCapable('npx', ['codex'])).toBe(true);
    expect(isTerminalAutoworkCapable('claude', ['--dangerously-skip-permissions'])).toBe(true);
  });

  test('declared backend wins', () => {
    // Preset launch: command is the bare program, backend declared explicitly.
    expect(isTerminalAutoworkCapable('claude', [], 'claude')).toBe(true);
    expect(isTerminalAutoworkCapable('codex', [], 'codex')).toBe(true);
  });

  test('gemini resolves to a family but is NOT autowork-capable (no lifecycle renderer)', () => {
    expect(isTerminalAutoworkCapable('gemini')).toBe(false);
    expect(isTerminalAutoworkCapable('gemini', [], 'gemini')).toBe(false);
    expect(isTerminalAutoworkCapable('stepcode', ['gemini'])).toBe(false);
  });

  test('plain shell / unknown CLI is not capable', () => {
    expect(isTerminalAutoworkCapable('$SHELL')).toBe(false);
    expect(isTerminalAutoworkCapable('/bin/bash', ['-l'])).toBe(false);
    expect(isTerminalAutoworkCapable('stepcode', ['frobnicate'])).toBe(false);
  });
});
