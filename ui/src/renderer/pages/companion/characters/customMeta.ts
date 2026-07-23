/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { CUSTOM_CHARACTER_ID } from './index';
import type { CustomFigureMeta } from './types';
import { parseFigureId, type CompanionId, type FigureId } from '@/common/types/ids';

/** Wire shape of `appearance.custom_figure` (snake_case, fields may be missing). */
interface WireCustomFigure {
  aspect?: number;
  head_box?: { x?: number; y?: number; w?: number; h?: number };
  size_tier?: string;
  /** Per-companion continuous figure-height override (logical px); supersedes size_tier. */
  size_px?: number | null;
  /** Stable library figure UUIDv7; when present the image is served from the library. */
  figure_id?: string | null;
}

/** Minimal profile slice this helper reads — accepts ICompanionProfile / ICompanionWithStatus alike. */
interface ProfileLike {
  character?: string | null;
  appearance?: { custom_figure?: WireCustomFigure | null } | null;
}

const isFiniteNumber = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v);

/**
 * snake→camel conversion of a companion profile's `appearance.custom_figure` with
 * field-missing defense. Returns null unless the profile is a `custom`
 * character with structurally valid figure metadata — callers can pass the
 * result straight into `CompanionAvatar`/`getDeskSpecFor`.
 */
export function customFigureMetaOf(profile?: ProfileLike | null): CustomFigureMeta | null {
  if (!profile || profile.character !== CUSTOM_CHARACTER_ID) return null;
  const cf = profile.appearance?.custom_figure;
  if (!cf) return null;
  const hb = cf.head_box;
  if (!isFiniteNumber(cf.aspect) || cf.aspect <= 0) return null;
  if (!hb || !isFiniteNumber(hb.x) || !isFiniteNumber(hb.y) || !isFiniteNumber(hb.w) || hb.w <= 0) return null;
  // Legacy figures (pre free-rectangle framing) have no `h` ⇒ a square box,
  // whose height as an image-height fraction is `w * aspect`.
  const h = isFiniteNumber(hb.h) && hb.h > 0 ? hb.h : hb.w * cf.aspect;
  const sizeTier = cf.size_tier === 's' || cf.size_tier === 'l' ? cf.size_tier : 'm';
  const sizePx = isFiniteNumber(cf.size_px) && cf.size_px > 0 ? cf.size_px : undefined;
  const figureId = cf.figure_id == null ? undefined : parseFigureId(cf.figure_id);
  return { aspect: cf.aspect, headBox: { x: hb.x, y: hb.y, w: hb.w, h }, sizeTier, sizePx, figureId };
}

/**
 * Figure image URL for a custom companion. A library-backed figure resolves to the
 * shared `/api/companion/figures/{id}/image`; a legacy per-companion figure keeps serving
 * from `/api/companion/companions/{companionId}/figure`. The `?v=` derives from the metadata —
 * every wizard confirmation changes it, so re-DIY busts mounted <img>/mesh
 * caches (the backend's ETag only helps once the browser re-requests).
 */
export function customFigureUrlOf(baseUrl: string, companionId: CompanionId, meta: CustomFigureMeta): string {
  const v = encodeURIComponent(`${meta.aspect}-${meta.headBox.x}-${meta.headBox.y}-${meta.headBox.w}-${meta.headBox.h}`);
  if (meta.figureId) {
    return `${baseUrl}/api/companion/figures/${meta.figureId}/image?v=${v}`;
  }
  return `${baseUrl}/api/companion/companions/${companionId}/figure?v=${v}`;
}

/** Figure image URL straight from a library figure id (picker thumbnails). */
export function figureImageUrlOf(baseUrl: string, figureId: FigureId, version?: string | number): string {
  const v = version != null ? `?v=${encodeURIComponent(String(version))}` : '';
  return `${baseUrl}/api/companion/figures/${figureId}/image${v}`;
}

/**
 * CSS box (px) to render the head-box crop **contain**-fit and centered inside a
 * square `side`×`side` avatar slot — no distortion. The framed rectangle is
 * scaled so its longer edge spans the slot; the shorter edge letterboxes.
 * Shared by `CustomFigure` (bust mode) and the wizard's frame-step preview so
 * what the user frames is exactly what the avatar shows.
 *
 * A square box (`h === w * aspect`) yields `cropAspect === 1` → fills the slot
 * with zero padding, pixel-identical to the pre-rectangle behaviour. `h ≤ 0`
 * (an unresolved legacy box) is treated as square defensively.
 */
export function bustCropStyle(
  headBox: { x: number; y: number; w: number; h: number },
  aspect: number,
  side: number
): { width: number; height: number; left: number; top: number } {
  const h = headBox.h > 0 ? headBox.h : headBox.w * aspect;
  const cropAspect = (headBox.w * aspect) / h; // framed box px-width / px-height
  const boxW = cropAspect >= 1 ? side : side * cropAspect;
  const boxH = cropAspect >= 1 ? side / cropAspect : side;
  const imgW = boxW / headBox.w; // displayed full-image size (imgW/imgH === aspect)
  const imgH = boxH / h;
  return {
    width: imgW,
    height: imgH,
    left: (side - boxW) / 2 - headBox.x * imgW,
    top: (side - boxH) / 2 - headBox.y * imgH,
  };
}
