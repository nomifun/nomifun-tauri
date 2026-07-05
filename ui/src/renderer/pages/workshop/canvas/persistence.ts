/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Debounced canvas-doc autosave.
 *
 * Contract (M1 spec): edits schedule a save 800 ms later; the resulting state
 * drives the toolbar pill (`saving` / `saved` / `error`). Saves are skipped
 * when the produced doc is byte-identical to what was last persisted (so pure
 * selection / viewport-noise doesn't spam the backend), and any pending save is
 * flushed on unmount.
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import { putCanvasDoc } from '../api';
import type { WorkshopCanvasDoc } from '../types';

export type SaveState = 'idle' | 'saving' | 'saved' | 'error';

const DEBOUNCE_MS = 800;

export interface DocPersistence {
  saveState: SaveState;
  /** Schedule a debounced save from the latest state. */
  schedule: () => void;
  /** Force an immediate save (awaitable). */
  flush: () => Promise<void>;
  /** Seed the "last persisted" signature (call after the initial load). */
  markLoaded: (doc: WorkshopCanvasDoc) => void;
}

export function useDocPersistence(
  canvasId: string,
  getDoc: () => WorkshopCanvasDoc,
  onSaveStateChange?: (state: SaveState) => void
): DocPersistence {
  const getRef = useRef(getDoc);
  getRef.current = getDoc;
  const onChangeRef = useRef(onSaveStateChange);
  onChangeRef.current = onSaveStateChange;

  const [saveState, setSaveStateRaw] = useState<SaveState>('idle');
  const timerRef = useRef<number | null>(null);
  const lastSavedSigRef = useRef<string>('');
  const inFlightRef = useRef<Promise<void> | null>(null);
  const savedResetRef = useRef<number | null>(null);

  const setSaveState = useCallback((s: SaveState) => {
    setSaveStateRaw(s);
    onChangeRef.current?.(s);
  }, []);

  const doSave = useCallback(async (): Promise<void> => {
    const doc = getRef.current();
    const sig = JSON.stringify(doc);
    if (sig === lastSavedSigRef.current) return;
    if (savedResetRef.current != null) {
      window.clearTimeout(savedResetRef.current);
      savedResetRef.current = null;
    }
    setSaveState('saving');
    const task = (async () => {
      try {
        await putCanvasDoc(canvasId, doc);
        lastSavedSigRef.current = sig;
        setSaveState('saved');
        // Fade the "saved" pill back to idle after a beat.
        savedResetRef.current = window.setTimeout(() => setSaveState('idle'), 1600);
      } catch (e) {
        console.error('[workshop] canvas autosave failed', e);
        setSaveState('error');
      } finally {
        inFlightRef.current = null;
      }
    })();
    inFlightRef.current = task;
    await task;
  }, [canvasId, setSaveState]);

  const schedule = useCallback(() => {
    if (timerRef.current != null) window.clearTimeout(timerRef.current);
    timerRef.current = window.setTimeout(() => {
      timerRef.current = null;
      void doSave();
    }, DEBOUNCE_MS);
  }, [doSave]);

  const flush = useCallback(async (): Promise<void> => {
    if (timerRef.current != null) {
      window.clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    if (inFlightRef.current) await inFlightRef.current;
    await doSave();
  }, [doSave]);

  const markLoaded = useCallback((doc: WorkshopCanvasDoc) => {
    lastSavedSigRef.current = JSON.stringify(doc);
  }, []);

  // Best-effort flush on unmount (navigating away).
  useEffect(() => {
    return () => {
      if (timerRef.current != null) {
        window.clearTimeout(timerRef.current);
        timerRef.current = null;
      }
      if (savedResetRef.current != null) window.clearTimeout(savedResetRef.current);
      void doSave();
    };
  }, [doSave]);

  return { saveState, schedule, flush, markLoaded };
}
