/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type React from 'react';
import type { FigureId } from '@/common/types/ids';

/** Moods every character must express (driven by the learner / chat). */
export type CompanionMood = 'happy' | 'content' | 'sleepy' | 'worried' | 'excited';

/** Activities: idle breathing vs. a learn run in flight. */
export type CompanionActivity = 'idle' | 'thinking';

export interface CharacterProps {
  mood: CompanionMood;
  activity: CompanionActivity;
  size?: number;
}

/** Per-character desktop window/render spec. Characters without one use DEFAULT_DESK. */
export interface CharacterDeskSpec {
  /** Companion window logical size (px). */
  windowWidth: number;
  windowHeight: number;
  /** Figure height passed to the component as `size` inside the companion window. */
  figureHeight: number;
}

/** Metadata for a user-supplied single-image figure (DIY custom character). */
export interface CustomFigureMeta {
  /** width / height of the cutout image. */
  aspect: number;
  /** Head-and-shoulders crop in image-fraction coords: left x + width w (of image
   *  width), top y + height h (of image height). Free rectangle; a legacy square
   *  box has h = w·aspect. */
  headBox: { x: number; y: number; w: number; h: number };
  /** Desk size tier. */
  sizeTier: 's' | 'm' | 'l';
  /** Per-companion continuous figure-height override (logical px). When set it
   *  supersedes `sizeTier` in `customDeskSpec` (the 总览 size slider writes it);
   *  absent ⇒ fall back to the tier height. Clamped to [SIZE_MIN, SIZE_MAX]. */
  sizePx?: number;
  /** Library figure backing this companion (canonical lowercase bare UUIDv7).
   *  When set, the image comes from the shared figure library. */
  figureId?: FigureId;
}

export interface CharacterMeta {
  /** Stable id persisted in companion config (appearance.character). */
  id: string;
  /** i18n key suffix: nomi.characters.<id>.name / .style */
  nameKey: string;
  /** Two swatch colors shown in the picker chip. */
  palette: [string, string];
  /** Optional desktop spec — full-figure characters need a taller window. */
  desk?: CharacterDeskSpec;
  Component: React.FC<CharacterProps>;
}
