/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./TerminalSessionPage.tsx', import.meta.url), 'utf8');
const xtermSource = readFileSync(new URL('./XtermView.tsx', import.meta.url), 'utf8');
const sendBoxSource = readFileSync(new URL('./TerminalSendBox.tsx', import.meta.url), 'utf8');

describe('TerminalSessionPage workspace rail collapse wiring', () => {
  test('keeps terminal file auto-expand scoped to the current terminal session', () => {
    expect(source.includes('autoExpandOnFiles: true')).toBe(true);
    expect(source.includes('target: workspaceTarget')).toBe(true);
  });

  test('keeps the workspace tool rail at the far right of the expanded panel', () => {
    const panelIndex = source.indexOf("className='!bg-1 relative layout-sider'");
    const railIndex = source.indexOf('<WorkspaceToolRail');

    expect(panelIndex >= 0).toBe(true);
    expect(railIndex >= 0).toBe(true);
    expect(panelIndex < railIndex).toBe(true);
  });

  test('remounts stateful terminal content when the route id changes', () => {
    expect(source.includes('const TerminalSessionContent: React.FC<{ sessionId: TerminalId }>')).toBe(true);
    expect(source.includes('<TerminalSessionContent key={sessionId} sessionId={sessionId} />')).toBe(true);
    expect(source.includes('persistNamespace={`terminal:${sessionId}`}')).toBe(true);
  });

  test('shows an explicit recoverable state when session loading fails', () => {
    expect(source.includes('const [loadError, setLoadError] = useState<TerminalLoadError | null>(null)')).toBe(true);
    expect(source.includes("<div role='alert'")).toBe(true);
    expect(source.includes('setLoadAttempt((attempt) => attempt + 1)')).toBe(true);
    expect(source.includes('}, [loadAttempt, sessionId]);')).toBe(true);
  });

  test('acknowledges the activation resize before caching its dimensions', () => {
    const requestIndex = xtermSource.indexOf('await ipcBridge.terminal.resize.invoke');
    const cacheIndex = xtermSource.indexOf('lastCols = next.cols');

    expect(requestIndex >= 0).toBe(true);
    expect(cacheIndex > requestIndex).toBe(true);
    expect(xtermSource.includes('resizeFailureCount <= 2')).toBe(true);
    expect(xtermSource.includes('if (resizeRetryTimer) clearTimeout(resizeRetryTimer)')).toBe(true);
    expect(xtermSource.includes('desiredResize = { cols, rows }')).toBe(true);
    expect(xtermSource.includes('markActivationReady()')).toBe(true);
    expect(xtermSource.includes('inputQueue.push({ text, byteLength, resolve, reject })')).toBe(true);
    expect(xtermSource.includes('coalescePreActivationInput()')).toBe(true);
    expect(xtermSource.includes('onResizeFailureRef.current?.(error)')).toBe(true);
    expect(source.includes('setTerminalError(')).toBe(true);
    expect(source.includes('setXtermAttempt((attempt) => attempt + 1)')).toBe(true);
    expect(source.includes('disabled={isExited || terminalError !== null}')).toBe(true);
    expect(sendBoxSource.includes('await api.writeToPty(text)')).toBe(true);
    expect(sendBoxSource.includes('ipcBridge.terminal.input')).toBe(false);
  });

  test('renders exited terminal scrollback without resizing a missing PTY', () => {
    expect(source.includes('isRunning={!isExited}')).toBe(true);
    expect(xtermSource.includes('if (!isRunning) return;')).toBe(true);
    expect(xtermSource.includes("new Error('Terminal process is not running')")).toBe(true);
  });
});
