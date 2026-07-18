import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const studioSource = readFileSync(new URL('./CreateStudio/index.tsx', import.meta.url), 'utf8');

describe('CreateStudio responsive scroll layout', () => {
  test('bounds the modal to a definite dynamic viewport height', () => {
    expect(studioSource.includes('const studioViewportHeight = isMobile')).toBe(true);
    expect(studioSource.includes("'calc(100dvh - 48px)'")).toBe(true);
    expect(studioSource.includes("'min(760px, calc(100dvh - 80px))'")).toBe(true);
    expect(studioSource.includes('height: studioViewportHeight')).toBe(true);
    expect(
      studioSource.includes("maxHeight: isMobile ? '100vh' : 'calc(100vh - 80px)'"),
    ).toBe(false);
  });

  test('keeps the desktop grid row shrinkable so the config panel owns scrolling', () => {
    expect(
      studioSource.includes("gridTemplateColumns: '236px minmax(0, 1fr)'"),
    ).toBe(true);
    expect(studioSource.includes("gridTemplateRows: 'minmax(0, 1fr)'")).toBe(true);
    expect(
      studioSource.includes('knowledge-studio-config-panel min-h-0 flex-1 overflow-y-auto'),
    ).toBe(true);
  });
});
