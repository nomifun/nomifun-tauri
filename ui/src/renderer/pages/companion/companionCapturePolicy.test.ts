import { describe, expect, test } from 'bun:test';
import { shouldCaptureWholeCompanionWindow } from './companionCapturePolicy';

describe('shouldCaptureWholeCompanionWindow', () => {
  test('keeps normal companion chrome area-scoped instead of capturing the transparent window', () => {
    expect(
      shouldCaptureWholeCompanionWindow({
        composerOpen: true,
        barRevealed: true,
        hasInput: true,
        sending: true,
        dragOver: false,
      })
    ).toBe(false);
  });

  test('captures the whole window only for native file drag hover', () => {
    expect(
      shouldCaptureWholeCompanionWindow({
        composerOpen: false,
        barRevealed: false,
        hasInput: false,
        sending: false,
        dragOver: true,
      })
    ).toBe(true);
  });
});
