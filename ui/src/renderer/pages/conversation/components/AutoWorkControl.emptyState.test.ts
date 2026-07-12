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

  test('activates either focused feedback button before the Select popup handles the key', () => {
    const source = readSource(new URL('./AutoWorkControl.tsx', import.meta.url));

    expect(source.includes('const handleTagPickerActionKeyDown = (')).toBe(true);
    expect(source.includes('if (!isAutoWorkTagPickerActionKey(event.key)) return;')).toBe(true);
    expect(source.includes('action();')).toBe(true);
    expect(source.includes('handleTagPickerActionKeyDown(event, retryTags)')).toBe(true);
    expect(source.includes('handleTagPickerActionKeyDown(event, openNewRequirement)')).toBe(true);
    expect(source.split('onKeyDown={(event) => handleTagPickerActionKeyDown').length - 1).toBe(2);
  });

  test('focuses the Select before retrying through one shared callback', () => {
    const source = readSource(new URL('./AutoWorkControl.tsx', import.meta.url));
    const retryStart = source.indexOf('const retryTags = () => {');
    const retryEnd = source.indexOf('\n  };', retryStart);
    const retrySource = source.slice(retryStart, retryEnd);

    expect(
      source.includes("import type { SelectHandle } from '@arco-design/web-react/es/Select/interface';")
    ).toBe(true);
    expect(source.includes('const tagSelectRef = useRef<SelectHandle>(null);')).toBe(true);
    expect(source.includes('ref={tagSelectRef}')).toBe(true);
    expect(retryStart).toBeGreaterThan(-1);
    expect(retrySource.indexOf('tagSelectRef.current?.focus();')).toBeGreaterThan(-1);
    expect(retrySource.indexOf('void refreshTags();')).toBeGreaterThan(
      retrySource.indexOf('tagSelectRef.current?.focus();')
    );
    expect(source.includes('onClick={retryTags}')).toBe(true);
    expect(source.includes('handleTagPickerActionKeyDown(event, retryTags)')).toBe(true);
  });

  test('announces both loading and error feedback as polite atomic status regions', () => {
    const source = readSource(new URL('./AutoWorkControl.tsx', import.meta.url));

    expect(source.split("role='status'").length - 1).toBe(2);
    expect(source.split("aria-live='polite'").length - 1).toBe(2);
    expect(source.split("aria-atomic='true'").length - 1).toBe(2);
  });

  test('hides cached options outside ready mode without clearing the selected binding', () => {
    const source = readSource(new URL('./AutoWorkControl.tsx', import.meta.url));

    expect(source.includes("const tagOptions = tagPickerMode === 'ready'")).toBe(true);
    expect(source.includes('options={tagOptions}')).toBe(true);
    expect(source.includes('value={tag}')).toBe(true);
    expect(source.includes('disabled={enabled}')).toBe(true);
  });
});
