import { describe, expect, test } from 'bun:test';
import { getAutoWorkTagPickerMode, isAutoWorkEnableBlocked } from './AutoWorkControl.model';

describe('AutoWork tag picker state', () => {
  test('distinguishes loading, ready, failure, and empty results', () => {
    expect(getAutoWorkTagPickerMode(0, true, null)).toBe('loading');
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
});
