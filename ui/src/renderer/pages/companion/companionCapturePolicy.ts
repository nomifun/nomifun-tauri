/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

export interface CompanionCapturePolicyState {
  composerOpen: boolean;
  barRevealed: boolean;
  hasInput: boolean;
  sending: boolean;
  dragOver: boolean;
}

/**
 * Whole-window cursor capture is only for interactions that genuinely need the
 * entire native transparent window as a target. Normal visible chrome is covered
 * by `[data-companion-hit]` area tests; capturing the full window there turns the
 * invisible chat-sized surface into a desktop click shield.
 */
export function shouldCaptureWholeCompanionWindow(state: CompanionCapturePolicyState): boolean {
  return state.dragOver;
}
