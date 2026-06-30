/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Rapid consecutive Ctrl+C detection: when a claude/codex TUI wedges the
 * terminal, mashing Ctrl+C is the user's instinct to escape. We treat a burst of
 * Ctrl+C within a short window as a request to hard-fall-back to a clean shell.
 */
export interface CtrlCState {
  /** Timestamps (ms) of recent Ctrl+C presses still inside the window. */
  hits: number[];
}

export function createCtrlCState(): CtrlCState {
  return { hits: [] };
}

/** Whether a PTY input chunk is a single Ctrl+C (ETX, 0x03). */
export function isCtrlC(data: string): boolean {
  return data === '\x03';
}

/**
 * Record a Ctrl+C at `nowMs`, dropping presses older than `windowMs`. Returns
 * the next state and whether the count within the window reached `threshold`
 * (the user is mashing Ctrl+C → offer/trigger the shell escape). When it fires,
 * the window is cleared so the next escalation needs a fresh burst.
 */
export function bumpCtrlC(
  state: CtrlCState,
  nowMs: number,
  windowMs: number,
  threshold: number
): { state: CtrlCState; escalate: boolean } {
  const hits = state.hits.filter((t) => nowMs - t < windowMs);
  hits.push(nowMs);
  if (hits.length >= threshold) {
    return { state: { hits: [] }, escalate: true };
  }
  return { state: { hits }, escalate: false };
}
