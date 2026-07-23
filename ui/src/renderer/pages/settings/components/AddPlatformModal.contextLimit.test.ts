import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('AddPlatformModal context window control', () => {
  test('uses common preset choices instead of a free-form token input', () => {
    const addSource = readSource(new URL('./AddPlatformModal.tsx', import.meta.url));
    const addModelSource = readSource(new URL('./AddModelModal.tsx', import.meta.url));
    const editSource = readSource(new URL('./EditModeModal.tsx', import.meta.url));
    const modelListSource = readSource(
      new URL('../../../components/settings/SettingsModal/contents/ModelModalContent.tsx', import.meta.url)
    );
    const selectSource = readSource(new URL('./ContextLimitSelect.tsx', import.meta.url));

    expect(selectSource.includes('CONTEXT_WINDOW_OPTIONS')).toBe(true);
    expect(selectSource.includes('value: 32_000')).toBe(true);
    expect(selectSource.includes('value: 64_000')).toBe(true);
    expect(selectSource.includes('value: 128_000')).toBe(true);
    expect(selectSource.includes('value: 200_000')).toBe(true);
    expect(selectSource.includes('value: 1_000_000')).toBe(true);
    expect(selectSource.includes('getPopupContainer={() => document.body}')).toBe(true);
    expect(selectSource.includes('node.parentElement')).toBe(false);
    expect(addSource.includes('<ContextLimitSelect')).toBe(true);
    expect(addSource.includes('model_context_limits')).toBe(true);
    expect(addSource.includes('context_limit: values.context_limit')).toBe(false);
    expect(addModelSource.includes('<ContextLimitSelect')).toBe(true);
    expect(addModelSource.includes('model_context_limits')).toBe(true);
    expect(modelListSource.includes('ModelContextLimitEditor')).toBe(true);
    expect(modelListSource.includes('model_context_limits')).toBe(true);
    expect(modelListSource.includes('newModelContextLimits')).toBe(true);
    expect(modelListSource.includes('model_context_limits: next')).toBe(true);
    expect(modelListSource.includes('model_context_limits: newModelContextLimits')).toBe(true);
    expect(modelListSource.includes('model_context_limits: Object.keys(next).length > 0 ? next : undefined')).toBe(
      false
    );
    expect(editSource.includes('<ContextLimitSelect')).toBe(false);
    expect(/\bcontext_limit\b/.test(editSource)).toBe(false);
    expect(modelListSource.includes('platform.context_limit')).toBe(false);
    expect(modelListSource.includes('inheritedContextLimit')).toBe(false);
    expect(addSource.includes('<InputNumber')).toBe(false);
    expect(editSource.includes('<InputNumber')).toBe(false);
  });
});
