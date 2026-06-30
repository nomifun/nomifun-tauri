/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { CharacterDeskSpec, CustomFigureMeta } from './types';

// Desktop figure heights (px) per size tier — the creation/library DEFAULTS.
// Kept small so a new DIY pet doesn't sprawl (built-in chibi is 150px). A
// per-companion `sizePx` override (the 总览 size slider) supersedes these.
// Read live at render time, so values flow through to existing companions with
// no migration.
export const FIGURE_HEIGHTS = { s: 150, m: 210, l: 280 } as const;
/** 总览 size-slider bounds (logical px figure height). SIZE_MIN stays above
 *  BUST_MAX_SIZE(130) so the desktop figure is always full-body, never the
 *  head-bust crop. SIZE_MAX is the user-chosen ceiling. */
export const SIZE_MIN = 140;
export const SIZE_MAX = 400;
// Window-width cap, kept in sync with SIZE_MAX: otherwise a tall figure at the top
// of the slider would be clamped DOWN by the width cap for many aspects. Still
// bounds pathologically wide/landscape cutouts from sprawling across the desktop.
export const MAX_WINDOW_WIDTH = 400;
/** Never narrower than the classic chibi window — chat bar and bubble must fit. */
export const MIN_WINDOW_WIDTH = 240;
const SIDE_MARGIN = 14; // px each side
// IDLE window vertical chrome AROUND the figure: just the hover quick-input bar
// reserve below (~48px) + a little headroom above for the hop/breath animation.
// The bubble's headroom is NOT reserved here anymore — that left a big always-
// transparent strip above the figure ("透明背景占住桌面空间"). The bubble grows the
// window on demand (enterChatSize) and shrinks back (exitChatSize) instead.
const CHROME_HEIGHT = 64;

/** Pure metadata → desk computation for DIY custom figures. */
export function customDeskSpec(meta: CustomFigureMeta): CharacterDeskSpec {
  // Defend against degenerate metadata (corrupt config, division by zero).
  const aspect = Number.isFinite(meta.aspect) && meta.aspect > 0 ? meta.aspect : 1;
  // A per-companion sizePx override (the 总览 slider) wins over the tier; clamp it
  // to [SIZE_MIN, SIZE_MAX]. Absent/degenerate ⇒ fall back to the tier height.
  let figureHeight: number =
    Number.isFinite(meta.sizePx) && (meta.sizePx as number) > 0
      ? Math.min(SIZE_MAX, Math.max(SIZE_MIN, meta.sizePx as number))
      : (FIGURE_HEIGHTS[meta.sizeTier] ?? FIGURE_HEIGHTS.m);
  let windowWidth = Math.ceil(figureHeight * aspect) + SIDE_MARGIN * 2;
  if (windowWidth > MAX_WINDOW_WIDTH) {
    windowWidth = MAX_WINDOW_WIDTH;
    figureHeight = Math.floor((MAX_WINDOW_WIDTH - SIDE_MARGIN * 2) / aspect);
  }
  windowWidth = Math.max(windowWidth, MIN_WINDOW_WIDTH);
  return { windowWidth, windowHeight: figureHeight + CHROME_HEIGHT, figureHeight };
}
