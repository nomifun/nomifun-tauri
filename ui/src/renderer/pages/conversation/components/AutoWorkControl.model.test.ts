import { describe, expect, test } from 'bun:test';
import {
  getAutoWorkTagPickerMode,
  isAutoWorkTagPickerActionKey,
  isAutoWorkEnableBlocked,
  shouldFocusAutoWorkTagPickerAction,
} from './AutoWorkControl.model';

describe('AutoWork tag picker state', () => {
  test('distinguishes loading, ready, failure, and empty results', () => {
    expect(getAutoWorkTagPickerMode(0, true, null)).toBe('loading');
    expect(getAutoWorkTagPickerMode(2, true, null)).toBe('loading');
    expect(getAutoWorkTagPickerMode(2, false, null)).toBe('ready');
    expect(getAutoWorkTagPickerMode(0, false, 'offline')).toBe('error');
    expect(getAutoWorkTagPickerMode(0, false, null)).toBe('empty');
  });

  test('keeps an existing binding switchable off in every state', () => {
    for (const mode of ['loading', 'error', 'empty', 'ready'] as const) {
      expect(isAutoWorkEnableBlocked(true, mode)).toBe(false);
    }
  });

  test('only allows a disabled binding to turn on when tags are ready', () => {
    expect(isAutoWorkEnableBlocked(false, 'loading')).toBe(true);
    expect(isAutoWorkEnableBlocked(false, 'error')).toBe(true);
    expect(isAutoWorkEnableBlocked(false, 'empty')).toBe(true);
    expect(isAutoWorkEnableBlocked(false, 'ready')).toBe(false);
  });

  test('focuses the actionable feedback on forward Tab', () => {
    expect(shouldFocusAutoWorkTagPickerAction('empty', 'Tab', false)).toBe(true);
    expect(shouldFocusAutoWorkTagPickerAction('error', 'Tab', false)).toBe(true);
  });

  test('leaves forward Tab unchanged when no action is available', () => {
    expect(shouldFocusAutoWorkTagPickerAction('loading', 'Tab', false)).toBe(false);
    expect(shouldFocusAutoWorkTagPickerAction('ready', 'Tab', false)).toBe(false);
  });

  test('leaves non-Tab keys unchanged', () => {
    expect(shouldFocusAutoWorkTagPickerAction('empty', 'Enter', false)).toBe(false);
    expect(shouldFocusAutoWorkTagPickerAction('error', 'Escape', false)).toBe(false);
  });

  test('leaves Shift+Tab unchanged in every mode', () => {
    for (const mode of ['loading', 'error', 'empty', 'ready'] as const) {
      expect(shouldFocusAutoWorkTagPickerAction(mode, 'Tab', true)).toBe(false);
    }
  });

  test('recognizes Enter and Space as action keys', () => {
    expect(isAutoWorkTagPickerActionKey('Enter')).toBe(true);
    expect(isAutoWorkTagPickerActionKey(' ')).toBe(true);
  });

  test('leaves other keys unchanged for action activation', () => {
    expect(isAutoWorkTagPickerActionKey('Tab')).toBe(false);
    expect(isAutoWorkTagPickerActionKey('Escape')).toBe(false);
    expect(isAutoWorkTagPickerActionKey('a')).toBe(false);
  });
});
