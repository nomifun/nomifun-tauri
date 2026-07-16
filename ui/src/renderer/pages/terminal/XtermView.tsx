/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useRef } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import { WebglAddon } from '@xterm/addon-webgl';
import '@xterm/xterm/css/xterm.css';
import { ipcBridge } from '@/common';
import type { TerminalId } from '@/common/types/ids';
import { createStreamingDecoder, encodeStringToBase64 } from './terminalEncoding';
import { bumpCtrlC, createCtrlCState, isCtrlC, type CtrlCState } from './ctrlCEscalation';
import { TERMINAL_THEME, TERMINAL_TYPOGRAPHY } from './terminalTheme';
import styles from './XtermView.module.css';

/**
 * Interactive xterm.js view bound to a backend PTY session over the IPC bridge.
 *
 * - Replays scrollback on mount (reconnect), then live-streams `terminal.output`.
 * - Forwards raw keystrokes (`onData`) to the PTY input endpoint, so full TUIs
 *   (claude, vim, arrow keys, Ctrl-C) work.
 * - Reports size changes via the resize endpoint so the PTY and emulator agree.
 *
 * Sizing is the tricky part: `fit()` must run AFTER the flex container has its
 * real measured size and AFTER the webfont loads, otherwise xterm renders at the
 * wrong cols/rows and looks garbled until a manual window resize. We therefore
 * fit on: a double rAF after open, `document.fonts.ready`, every ResizeObserver
 * callback with a non-zero box, and tab visibility changes — and only ever fit
 * when the container actually has a measurable size.
 *
 * Unmount disposes the terminal + unsubscribes but does NOT kill the PTY —
 * reopening reconnects to the live session (reconnect-while-backend-lives).
 */

const TERMINAL_FONT = '"SFMono-Regular", "JetBrains Mono", Menlo, Consolas, "Liberation Mono", monospace';
const MAX_QUEUED_INPUT_BYTES = 1024 * 1024;

export interface XtermViewHandle {
  /** Write text to the PTY (used by the composer SendBox). */
  writeToPty: (text: string) => Promise<void>;
  /**
   * Whether the running program currently has bracketed paste mode enabled
   * (DECSET 2004). The composer uses this to decide whether a multi-line submit
   * can be wrapped as a single paste instead of executed line by line.
   */
  isBracketedPaste: () => boolean;
  /** Clear the visible terminal. */
  clear: () => void;
  /**
   * Full soft reset: exit the alternate screen buffer, reset modes, and clear.
   * Unlike `clear` (which only clears the normal-buffer scrollback) this recovers
   * a grid left garbled by a wedged full-screen TUI (claude/codex). Used by the
   * "fall back to shell" affordance and after a WS reconnect.
   */
  reset: () => void;
  focus: () => void;
}

interface XtermViewProps {
  sessionId: TerminalId;
  /**
   * Exited sessions still mount xterm to replay persisted scrollback, but must
   * not call the live PTY resize/input endpoints (which correctly return 404).
   */
  isRunning?: boolean;
  className?: string;
  apiRef?: React.MutableRefObject<XtermViewHandle | null>;
  /**
   * Called when the user mashes Ctrl+C (a burst within ~1.5s) — the instinct to
   * escape a wedged TUI. The page wires this to the shell-fallback action.
   */
  onEscalateShell?: () => void;
  /** Called after bounded resize/activation retries are exhausted. */
  onResizeFailure?: (error: unknown) => void;
}

const XtermView: React.FC<XtermViewProps> = ({
  sessionId,
  isRunning = true,
  className,
  apiRef,
  onEscalateShell,
  onResizeFailure,
}) => {
  const containerRef = useRef<HTMLDivElement>(null);
  // Keep the latest escalation callback without re-running the mount effect.
  const onEscalateShellRef = useRef(onEscalateShell);
  onEscalateShellRef.current = onEscalateShell;
  const onResizeFailureRef = useRef(onResizeFailure);
  onResizeFailureRef.current = onResizeFailure;

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const term = new Terminal({
      fontFamily: TERMINAL_FONT,
      ...TERMINAL_TYPOGRAPHY,
      cursorBlink: true,
      cursorStyle: 'bar',
      cursorWidth: 2,
      convertEol: false,
      scrollback: 10000,
      allowProposedApi: true,
      smoothScrollDuration: 0,
      theme: TERMINAL_THEME,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.loadAddon(new WebLinksAddon());
    term.open(container);

    // GPU renderer. The default DOM renderer reflows the DOM per redraw and
    // cannot keep up with rapid TUI redraws on Retina (the macOS "scramble").
    // WebGL renders the grid via a texture atlas in one draw call. If the
    // browser drops the WebGL context (OOM, GPU reset, sleep), dispose the addon
    // and let xterm fall back to the DOM renderer automatically. Must attach
    // AFTER term.open().
    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => webgl.dispose());
      term.loadAddon(webgl);
    } catch {
      // WebGL2 unavailable — silently keep the DOM renderer.
    }

    let disposed = false;
    let lastCols = 0;
    let lastRows = 0;
    let desiredResize: { cols: number; rows: number } | null = null;
    let resizeInFlight = false;
    let resizeFailureCount = 0;
    let resizeRetryTimer: ReturnType<typeof setTimeout> | null = null;
    let activationReady = false;
    let inputDrainRunning = false;
    let queuedInputBytes = 0;
    let inputPipelineError: Error | null = null;
    type QueuedInput = {
      text: string;
      byteLength: number;
      resolve: () => void;
      reject: (error: Error) => void;
    };
    let inputQueue: QueuedInput[] = [];
    let activeInputItem: QueuedInput | null = null;

    const failInputPipeline = (error: Error, notify = true): void => {
      if (inputPipelineError) return;
      inputPipelineError = error;
      const queued = inputQueue;
      inputQueue = [];
      queuedInputBytes = 0;
      for (const item of queued) item.reject(error);
      if (notify && !disposed) onResizeFailureRef.current?.(error);
    };

    // Users can type as soon as xterm receives focus, while the first resize is
    // still activating the deferred PTY. Preserve that input until activation
    // is acknowledged instead of racing the backend and losing it to a 404.
    const drainInputQueue = async (): Promise<void> => {
      if (disposed || !activationReady || inputDrainRunning || inputPipelineError) return;
      inputDrainRunning = true;
      while (!disposed && !inputPipelineError && inputQueue.length > 0) {
        const item = inputQueue.shift()!;
        activeInputItem = item;
        queuedInputBytes -= item.byteLength;
        try {
          await ipcBridge.terminal.input.invoke({
            id: sessionId,
            data_b64: encodeStringToBase64(item.text),
          });
          item.resolve();
        } catch (caught: unknown) {
          const error = caught instanceof Error ? caught : new Error(String(caught));
          item.reject(error);
          console.error('[XtermView] Failed to write terminal input:', error);
          failInputPipeline(error);
        } finally {
          if (activeInputItem === item) activeInputItem = null;
        }
      }
      inputDrainRunning = false;
    };

    const sendInput = (text: string): Promise<void> => {
      if (disposed) return Promise.reject(new Error('Terminal view has been disposed'));
      if (!isRunning) return Promise.reject(new Error('Terminal process is not running'));
      if (inputPipelineError) return Promise.reject(inputPipelineError);

      const byteLength = new TextEncoder().encode(text).byteLength;
      if (queuedInputBytes + byteLength > MAX_QUEUED_INPUT_BYTES) {
        const error = new Error('Terminal input queue limit exceeded before the backend acknowledged it');
        console.error('[XtermView] Refusing oversized queued terminal input:', error);
        // Reject and clear the entire prefix: executing only part of a command
        // would be more dangerous than dropping the oversized submission.
        failInputPipeline(error);
        return Promise.reject(error);
      }

      return new Promise<void>((resolve, reject) => {
        inputQueue.push({ text, byteLength, resolve, reject });
        queuedInputBytes += byteLength;
        if (activationReady) void drainInputQueue();
      });
    };

    const coalescePreActivationInput = (): void => {
      if (inputQueue.length < 2) return;
      const queued = inputQueue;
      inputQueue = [
        {
          text: queued.map((item) => item.text).join(''),
          byteLength: queued.reduce((total, item) => total + item.byteLength, 0),
          resolve: () => {
            for (const item of queued) item.resolve();
          },
          reject: (error) => {
            for (const item of queued) item.reject(error);
          },
        },
      ];
    };

    const markActivationReady = (): void => {
      if (activationReady || inputPipelineError) return;
      // Raw keys collected during a slow activation are one ordered byte stream;
      // flush them in a single POST instead of issuing one HTTP request per key.
      coalescePreActivationInput();
      activationReady = true;
      void drainInputQueue();
    };

    // The first resize also activates a deferred PTY. Treat it as an acknowledged
    // state transition rather than fire-and-forget: only cache dimensions after
    // the backend accepts them, coalesce concurrent layout notifications, and
    // retry short transient failures without creating unhandled rejections.
    const flushResize = async (): Promise<void> => {
      if (disposed || resizeInFlight || !desiredResize) return;

      const next = desiredResize;
      if (next.cols === lastCols && next.rows === lastRows) return;

      resizeInFlight = true;
      try {
        await ipcBridge.terminal.resize.invoke({ id: sessionId, cols: next.cols, rows: next.rows });
        if (!disposed) {
          markActivationReady();
          lastCols = next.cols;
          lastRows = next.rows;
          resizeFailureCount = 0;
        }
      } catch (error) {
        if (!disposed) {
          const failedSizeIsStillDesired =
            desiredResize?.cols === next.cols && desiredResize?.rows === next.rows;
          if (!failedSizeIsStillDesired) {
            // A newer measurement superseded this failed request. Flush that
            // value immediately instead of backing off an obsolete size.
            resizeFailureCount = 0;
          } else {
            resizeFailureCount += 1;
            if (resizeFailureCount <= 2) {
              const delayMs = Math.min(1000, 100 * 2 ** (resizeFailureCount - 1));
              resizeRetryTimer = setTimeout(() => {
                resizeRetryTimer = null;
                void flushResize();
              }, delayMs);
            } else {
              // A persistent failure is surfaced in diagnostics but cannot create
              // an infinite request/log loop. A later real layout change retries.
              console.error('[XtermView] Failed to resize terminal after retries:', error);
              desiredResize = null;
              resizeFailureCount = 0;
              onResizeFailureRef.current?.(error);
            }
          }
        }
      } finally {
        resizeInFlight = false;
        const desiredIsAcknowledged =
          desiredResize?.cols === lastCols && desiredResize?.rows === lastRows;
        if (!disposed && desiredResize && !desiredIsAcknowledged && !resizeRetryTimer) {
          void flushResize();
        }
      }
    };

    const requestResize = (cols: number, rows: number): void => {
      // Always record the newest measurement, including a return to the last
      // acknowledged size while an older failed size is waiting in backoff.
      desiredResize = { cols, rows };
      if (cols === lastCols && rows === lastRows) {
        if (resizeRetryTimer) {
          clearTimeout(resizeRetryTimer);
          resizeRetryTimer = null;
        }
        resizeFailureCount = 0;
        return;
      }
      if (!resizeRetryTimer) void flushResize();
    };

    // Fit only when the container is actually laid out, and only push a resize
    // to the PTY when the dimensions actually changed (avoids resize spam).
    const doFit = () => {
      if (disposed) return;
      const { clientWidth, clientHeight } = container;
      if (clientWidth === 0 || clientHeight === 0) return;
      try {
        fit.fit();
      } catch {
        return;
      }
      // An exited session is a read-only scrollback viewer. Resizing a missing
      // PTY returns 404 and used to be misreported as an activation failure.
      if (!isRunning) return;
      if (term.cols !== lastCols || term.rows !== lastRows) {
        requestResize(term.cols, term.rows);
      }
    };

    // Initial fit: defer past layout with a double rAF, and refit once the
    // monospace webfont is ready (font metrics change the cell size).
    let initialFitRaf = requestAnimationFrame(() => {
      if (disposed) return;
      initialFitRaf = requestAnimationFrame(doFit);
    });
    if (typeof document !== 'undefined' && 'fonts' in document) {
      void (document as Document & { fonts: FontFaceSet }).fonts.ready.then(() => doFit());
    }

    // Raw keystrokes → PTY. Also watch for a Ctrl+C burst: when a TUI wedges the
    // terminal, mashing Ctrl+C is the user's escape instinct, so a rapid burst
    // triggers the shell-fallback affordance (the single Ctrl+C is still sent).
    let ctrlCState: CtrlCState = createCtrlCState();
    const queueRawInput = (data: string): void => {
      // Pipeline failures already report once and switch the page to its visible
      // recovery state; consume the per-keystroke rejection here.
      void sendInput(data).catch(() => {});
    };
    const dataDisposable = term.onData((data) => {
      if (!isRunning) return;
      queueRawInput(data);
      if (isCtrlC(data)) {
        const r = bumpCtrlC(ctrlCState, Date.now(), 1500, 3);
        ctrlCState = r.state;
        if (r.escalate) onEscalateShellRef.current?.();
      }
    });

    // Shift+Enter inserts a newline instead of submitting. xterm sends CR (\r)
    // for BOTH Enter and Shift+Enter by default, so raw-mode agents (claude,
    // codex, gemini, REPLs) can't tell them apart and submit on Shift+Enter too.
    // Send LF (\n) instead: those programs treat CR as "submit" and LF as
    // "insert newline" — the same byte `claude /terminal-setup` maps Shift+Enter
    // to. Returning false stops xterm from also sending its default CR.
    term.attachCustomKeyEventHandler((e) => {
      if (
        e.type === 'keydown' &&
        e.key === 'Enter' &&
        e.shiftKey &&
        !e.isComposing &&
        !e.ctrlKey &&
        !e.altKey &&
        !e.metaKey
      ) {
        queueRawInput('\n');
        return false;
      }
      return true;
    });

    // Focus the terminal so it captures keystrokes immediately, and on click.
    const focusTerminal = () => {
      if (!disposed) term.focus();
    };
    container.addEventListener('mousedown', focusTerminal);
    let initialFocusRaf = requestAnimationFrame(() => {
      if (disposed) return;
      initialFocusRaf = requestAnimationFrame(focusTerminal);
    });

    if (apiRef) {
      apiRef.current = {
        writeToPty: sendInput,
        isBracketedPaste: () => term.modes.bracketedPasteMode,
        clear: () => term.clear(),
        reset: () => term.reset(),
        focus: focusTerminal,
      };
    }

    // One decoder for this session: scrollback replay first (a complete buffer),
    // then live chunks continue on the same decoder so a multibyte char split
    // across WS messages is buffered, not corrupted into U+FFFD. Reassignable so
    // a reconnect can start a fresh decoder for the re-replay (see below).
    let decodeStream = createStreamingDecoder();

    // Fetch the current scrollback snapshot and write it through `decodeStream`.
    const replayScrollback = () => {
      void ipcBridge.terminal.get
        .invoke({ id: sessionId })
        .then((session) => {
          if (disposed || !session?.scrollback_b64) return;
          term.write(decodeStream(session.scrollback_b64));
        })
        .catch(() => {
          /* session may have been removed; ignore */
        });
    };

    // Replay scrollback, then subscribe to live output.
    replayScrollback();

    const unsubscribeOutput = ipcBridge.terminal.onOutput.on((evt) => {
      if (disposed || evt.id !== sessionId) return;
      term.write(decodeStream(evt.data_b64));
    });

    // On WS reconnect the server does NOT replay the frames emitted while the
    // socket was down, so a TUI's redraws are lost and the grid is left garbled.
    // Full-reset the emulator (exit alt-screen / clear) and re-replay the current
    // scrollback through a fresh decoder to resync.
    const unsubscribeReconnected = ipcBridge.terminal.onReconnected.on(() => {
      if (disposed) return;
      term.reset();
      decodeStream = createStreamingDecoder();
      replayScrollback();
    });

    const unsubscribeExit = ipcBridge.terminal.onExit.on((evt) => {
      if (disposed || evt.id !== sessionId) return;
      const code = evt.exit_code ?? 0;
      term.write(`\r\n\x1b[2m[process exited with code ${code}]\x1b[0m\r\n`);
    });

    // Debounce reflow-heavy resizes to the next frame.
    let rafId = 0;
    const scheduleFit = () => {
      if (rafId) cancelAnimationFrame(rafId);
      rafId = requestAnimationFrame(doFit);
    };
    const resizeObserver = new ResizeObserver(scheduleFit);
    resizeObserver.observe(container);

    // Re-fit when the tab/window becomes visible again (it may have resized
    // while hidden, which xterm cannot observe).
    const onVisibility = () => {
      if (!document.hidden) scheduleFit();
    };
    document.addEventListener('visibilitychange', onVisibility);

    return () => {
      disposed = true;
      const disposeError = new Error('Terminal view was disposed before queued input was written');
      activeInputItem?.reject(disposeError);
      activeInputItem = null;
      failInputPipeline(disposeError, false);
      if (rafId) cancelAnimationFrame(rafId);
      cancelAnimationFrame(initialFitRaf);
      cancelAnimationFrame(initialFocusRaf);
      if (resizeRetryTimer) clearTimeout(resizeRetryTimer);
      container.removeEventListener('mousedown', focusTerminal);
      document.removeEventListener('visibilitychange', onVisibility);
      dataDisposable.dispose();
      unsubscribeOutput();
      unsubscribeReconnected();
      unsubscribeExit();
      resizeObserver.disconnect();
      term.dispose();
      if (apiRef) apiRef.current = null;
    };
  }, [sessionId, apiRef, isRunning]);

  return <div ref={containerRef} className={`${styles.card} ${className ?? ''}`} style={{ overflow: 'hidden' }} />;
};

export default XtermView;
