import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('AutoWork tag picker empty state', () => {
  test('renders actionable feedback and opens the canonical requirement form', () => {
    const source = readSource(new URL('./AutoWorkControl.tsx', import.meta.url));

    expect(source.includes('notFoundContent={tagPickerFeedback}')).toBe(true);
    expect(source.includes("navigate('/requirements?new=1')")).toBe(true);
    expect(source.includes("t('requirements.autowork.emptyTitle')")).toBe(true);
    expect(source.includes("t('requirements.autowork.emptyDescription')")).toBe(true);
    expect(source.includes("t('requirements.autowork.emptyCta')")).toBe(true);
    expect(source.includes("t('requirements.autowork.loadingTags')")).toBe(true);
    expect(source.includes("t('requirements.autowork.loadErrorTitle')")).toBe(true);
    expect(source.includes("t('requirements.autowork.retry')")).toBe(true);
    expect(source.includes('isAutoWorkEnableBlocked(enabled, tagPickerMode)')).toBe(true);
  });

  test('keeps Chinese and English copy keys aligned', () => {
    const zh = JSON.parse(readSource(new URL('../../../services/i18n/locales/zh-CN/requirements.json', import.meta.url)));
    const en = JSON.parse(readSource(new URL('../../../services/i18n/locales/en-US/requirements.json', import.meta.url)));
    const keys = [
      'emptyTitle',
      'emptyDescription',
      'emptyCta',
      'loadingTags',
      'loadErrorTitle',
      'loadErrorDescription',
      'retry',
    ];

    expect(keys.map((key) => zh.autowork[key] && key)).toEqual(keys);
    expect(keys.map((key) => en.autowork[key] && key)).toEqual(keys);
  });

  test('captures forward Tab and focuses either actionable feedback button', () => {
    const source = readSource(new URL('./AutoWorkControl.tsx', import.meta.url));

    expect(source.includes('onKeyDownCapture={handleTagPickerKeyDownCapture}')).toBe(true);
    expect(source.includes('event.preventDefault()')).toBe(true);
    expect(source.includes('event.stopPropagation()')).toBe(true);
    expect(source.includes('tagPickerActionRef.current?.focus()')).toBe(true);
    expect(source.includes('!tagPickerActionRef.current')).toBe(true);
    expect(source.includes('tagPickerActionRef.current.contains(event.target as Node)')).toBe(true);
    expect(source.split('ref={setTagPickerActionRef}').length - 1).toBe(2);
  });
});
