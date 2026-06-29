/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */
import { describe, expect, test } from 'bun:test';
import { isImeComposingKey, isSubmitGesture } from './useCompositionInput';

describe('isImeComposingKey', () => {
  const base = { key: 'Enter' as const };
  test('true when composing ref set', () => {
    expect(isImeComposingKey(base, { composing: true, justComposed: false })).toBe(true);
  });
  test('true right after compositionend (justComposed window)', () => {
    expect(isImeComposingKey(base, { composing: false, justComposed: true })).toBe(true);
  });
  test('true when native isComposing', () => {
    expect(isImeComposingKey({ ...base, nativeEvent: { isComposing: true } }, { composing: false, justComposed: false })).toBe(true);
  });
  test('true when keyCode 229', () => {
    expect(isImeComposingKey({ ...base, keyCode: 229 }, { composing: false, justComposed: false })).toBe(true);
  });
  test('false for a clean Enter', () => {
    expect(isImeComposingKey({ ...base, keyCode: 13, nativeEvent: { isComposing: false } }, { composing: false, justComposed: false })).toBe(false);
  });
});

describe('isSubmitGesture', () => {
  test('enter mode: plain Enter submits, Shift+Enter does not', () => {
    expect(isSubmitGesture({ key: 'Enter' }, 'enter')).toBe(true);
    expect(isSubmitGesture({ key: 'Enter', shiftKey: true }, 'enter')).toBe(false);
  });
  test('enter mode: Cmd+Enter still submits (legacy compatible)', () => {
    expect(isSubmitGesture({ key: 'Enter', metaKey: true }, 'enter')).toBe(true);
  });
  test('mod-enter mode: plain Enter does NOT submit', () => {
    expect(isSubmitGesture({ key: 'Enter' }, 'mod-enter')).toBe(false);
  });
  test('mod-enter mode: Cmd/Ctrl+Enter submits, Shift+Cmd+Enter does not', () => {
    expect(isSubmitGesture({ key: 'Enter', metaKey: true }, 'mod-enter')).toBe(true);
    expect(isSubmitGesture({ key: 'Enter', ctrlKey: true }, 'mod-enter')).toBe(true);
    expect(isSubmitGesture({ key: 'Enter', metaKey: true, shiftKey: true }, 'mod-enter')).toBe(false);
  });
  test('non-Enter never submits', () => {
    expect(isSubmitGesture({ key: 'a' }, 'enter')).toBe(false);
  });
});
