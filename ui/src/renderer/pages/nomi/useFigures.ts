/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { useMemo, useSyncExternalStore } from 'react';
import { ipcBridge } from '@/common';
import type { IFigureMeta, IFigureUpdatePatch } from '@/common/adapter/ipcBridge';
import { CUSTOM_CHARACTER_ID } from '@renderer/pages/companion/characters';
import { useCompanions } from './useNomi';

/**
 * Single module-level store for the custom-figure library, so every consumer —
 * the library page, the create-companion / settings character pickers, the overview
 * adjuster — reads one **live** list. Creating/deleting/renaming a figure
 * anywhere updates them all instantly (fixes the old "figure made in one place
 * doesn't show in another" staleness: each consumer used to fetch its own copy
 * once on mount).
 */

interface FiguresState {
  figures: IFigureMeta[];
  loading: boolean;
  loaded: boolean;
}

export type FigureUpdatePatch = Omit<IFigureUpdatePatch, 'figure_id'>;

let state: FiguresState = { figures: [], loading: false, loaded: false };
const listeners = new Set<() => void>();
let inFlight: Promise<void> | null = null;

const emit = (): void => listeners.forEach((l) => l());
const setState = (next: Partial<FiguresState>): void => {
  state = { ...state, ...next };
  emit();
};

async function refresh(): Promise<void> {
  if (!inFlight) {
    setState({ loading: true });
    inFlight = (async () => {
      try {
        const figures = await ipcBridge.companion.listFigures.invoke();
        setState({ figures, loaded: true });
      } finally {
        setState({ loading: false });
        inFlight = null;
      }
    })();
  }
  return inFlight;
}

async function remove(id: string): Promise<void> {
  await ipcBridge.companion.deleteFigure.invoke({ figure_id: id });
  // Optimistic local prune for instant UI, then reconcile with the backend.
  setState({ figures: state.figures.filter((f) => f.id !== id) });
  await refresh();
}

async function rename(id: string, name: string): Promise<IFigureMeta> {
  return update(id, { name });
}

async function update(id: string, patch: FigureUpdatePatch): Promise<IFigureMeta> {
  const updated = await ipcBridge.companion.updateFigure.invoke({ figure_id: id, ...patch });
  setState({ figures: state.figures.map((f) => (f.id === id ? updated : f)) });
  return updated;
}

/** Push a freshly-created figure into the store (callers get it from the wizard). */
function add(figure: IFigureMeta): void {
  setState({ figures: [figure, ...state.figures.filter((f) => f.id !== figure.id)] });
}

const subscribe = (cb: () => void): (() => void) => {
  listeners.add(cb);
  // First subscriber kicks off the initial load.
  if (!state.loaded && !inFlight) void refresh();
  return () => listeners.delete(cb);
};
const getSnapshot = (): FiguresState => state;

export interface FiguresApi {
  figures: IFigureMeta[];
  loading: boolean;
  loaded: boolean;
  refresh: () => Promise<void>;
  remove: (id: string) => Promise<void>;
  rename: (id: string, name: string) => Promise<IFigureMeta>;
  update: (id: string, patch: FigureUpdatePatch) => Promise<IFigureMeta>;
  add: (figure: IFigureMeta) => void;
}

/** Live figure-library state + mutators, shared across all consumers. */
export function useFigures(): FiguresApi {
  const snap = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  return { ...snap, refresh, remove, rename, update, add };
}

/**
 * The set of library figure ids currently in use by at least one companion
 * (`appearance.custom_figure.figure_id`). Derived from the live companion roster
 * (`useCompanions`, kept fresh via WS events), so it stays accurate as companions are
 * created / re-skinned / deleted. Consumers gate figure deletion on it: an
 * in-use figure must not be deleted — its image would 404 and the companion would
 * render blank. The backend enforces the same rule (409 Conflict); this hook
 * is the UI affordance so the user never reaches that error in normal use.
 */
export function useFiguresInUse(): Set<string> {
  const { companions } = useCompanions();
  return useMemo(() => {
    const ids = new Set<string>();
    for (const p of companions) {
      // Only a `custom` character actually renders its figure. A companion
      // switched to a built-in character can keep a stale `custom_figure.figure_id`
      // (orphan); that must NOT pin the figure as in-use, or it can never be
      // deleted. Mirrors the backend `figure_user_count` gate.
      if (p.character !== CUSTOM_CHARACTER_ID) continue;
      const fid = p.appearance?.custom_figure?.figure_id;
      if (fid) ids.add(fid);
    }
    return ids;
  }, [companions]);
}

/** `appearance.custom_figure` patch that links a companion to a library figure.
 *  `size_px: null` resets any prior per-companion size override (RFC 7396 delete),
 *  so a freshly (re)assigned figure starts at its tier's default height. */
export const figureToCustomPatch = (
  f: IFigureMeta
): { figure_id: string; aspect: number; head_box: { x: number; y: number; w: number; h?: number }; size_tier: 's' | 'm' | 'l'; size_px: number | null } => ({
  figure_id: f.id,
  aspect: f.aspect,
  head_box: f.head_box,
  size_tier: f.size_tier,
  size_px: null,
});
