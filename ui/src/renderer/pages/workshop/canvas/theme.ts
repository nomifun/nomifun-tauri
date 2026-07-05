/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Theme mirror for react-flow internals.
 *
 * react-flow paints some chrome (MiniMap mask/bg, Background dots) via JS props
 * that cannot resolve CSS custom properties, so — exactly like `DagCanvas` — we
 * mirror the global `data-theme` attribute into resolved hex literals via a
 * `MutationObserver`.
 */

import { useEffect, useMemo, useState } from 'react';
import type { WorkshopNodeKind } from '../types';
import { KIND_META } from './model';

export type ThemeMode = 'light' | 'dark';

function readTheme(): ThemeMode {
  return (document.documentElement.getAttribute('data-theme') as ThemeMode) || 'light';
}

/** Track the app's light/dark theme, reacting to `data-theme` mutations. */
export function useThemeMode(): ThemeMode {
  const [theme, setTheme] = useState<ThemeMode>(() => readTheme());
  useEffect(() => {
    const update = (): void => setTheme(readTheme());
    const observer = new MutationObserver(update);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
    return () => observer.disconnect();
  }, []);
  return theme;
}

export interface FlowColors {
  dots: string;
  lines: string;
  minimapMask: string;
  minimapBg: string;
  minimapStroke: string;
}

/** Resolved (hex) colors for react-flow's JS-prop chrome, per theme. */
export function useFlowColors(theme: ThemeMode): FlowColors {
  return useMemo<FlowColors>(
    () =>
      theme === 'dark'
        ? {
            dots: '#3a3a3a',
            lines: '#2b2b2b',
            minimapMask: 'rgba(0,0,0,0.55)',
            minimapBg: '#1a1a1a',
            minimapStroke: '#333333',
          }
        : {
            dots: '#d1d5e5',
            lines: '#e9ebf2',
            minimapMask: 'rgba(255,255,255,0.6)',
            minimapBg: '#f9fafb',
            minimapStroke: '#e5e6eb',
          },
    [theme]
  );
}

/** Minimap fill for a node kind (theme-aware literal). */
export function minimapColorForKind(kind: string, theme: ThemeMode): string {
  const meta = KIND_META[kind as WorkshopNodeKind] ?? KIND_META.image;
  return meta.minimap[theme];
}
