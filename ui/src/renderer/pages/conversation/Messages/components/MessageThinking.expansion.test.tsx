import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./MessageThinking.tsx', import.meta.url), 'utf8');
const cssSource = readFileSync(new URL('./MessageThinking.module.css', import.meta.url), 'utf8');

describe('MessageThinking expansion', () => {
  test('keeps completed thinking expanded by default', () => {
    expect(source.includes('useState(!isDone)')).toBe(false);
    expect(source.includes('setExpanded(false)')).toBe(false);
    expect(source.includes('useState(true)')).toBe(true);
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
});
