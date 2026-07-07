import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./MessageThinking.tsx', import.meta.url), 'utf8');
const cssSource = readFileSync(new URL('./MessageThinking.module.css', import.meta.url), 'utf8');

describe('MessageThinking expansion', () => {
  test('collapses completed process thinking by default while keeping live thinking open', () => {
    expect(source.includes("const defaultExpanded = expanded ?? (isProcessVariant ? !isDone : true);")).toBe(true);
    expect(source.includes('useState(() => defaultExpanded)')).toBe(true);
    expect(source.includes('onExpandedChange?.(nextExpanded)')).toBe(true);
    expect(source.includes('useState(!isDone)')).toBe(false);
    expect(source.includes('setExpanded(false)')).toBe(false);
  });

  test('supports a neutral process timeline variant', () => {
    expect(source.includes("variant = 'standalone'")).toBe(true);
    expect(source.includes('styles.containerProcess')).toBe(true);
    expect(source.includes('styles.bodyProcess')).toBe(true);
    expect(cssSource.includes('.containerProcess')).toBe(true);
    expect(cssSource.includes('.bodyProcess')).toBe(true);
    expect(cssSource.includes('background: transparent')).toBe(true);
    expect(cssSource.includes('font-size: var(--conversation-message-font-size')).toBe(true);
  });

  test('frames thinking content with a light thin border', () => {
    expect(cssSource.includes('border: 1px solid var(--color-border-2')).toBe(true);
    expect(cssSource.includes('border-radius: 6px')).toBe(true);
  });
});
