/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Pure Canvas2D export helpers for the M5 image editor.
 *
 * Every function here operates on the **full-resolution** source image so the
 * exported binaries are pixel-exact regardless of the on-screen preview scale.
 * All outputs are PNG (alpha preserved).
 */

/** An axis-aligned rectangle in source-image pixel space. */
export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** A split divider: a line at `pos` (source px) whose `gap` px seam is removed. */
export interface Divider {
  /** Position along the axis, in source-image pixels. */
  pos: number;
  /** Seam width removed, centred on `pos` (gap/2 each side). */
  gap: number;
}

/** A kept band `[start, end)` along one axis, in source-image pixels. */
export interface Band {
  start: number;
  end: number;
}

/** Anything drawable by CanvasRenderingContext2D.drawImage as a source. */
export type DrawableSource = CanvasImageSource & { width: number; height: number };

// ─── Canvas → PNG ─────────────────────────────────────────────────────────────

/** Encode a canvas to a PNG {@link Blob}, rejecting when the browser returns null. */
export function canvasToPngBlob(canvas: HTMLCanvasElement): Promise<Blob> {
  return new Promise((resolve, reject) => {
    canvas.toBlob((blob) => {
      if (blob) resolve(blob);
      else reject(new Error('canvas.toBlob returned null'));
    }, 'image/png');
  });
}

function createCanvas(width: number, height: number): HTMLCanvasElement {
  const canvas = document.createElement('canvas');
  canvas.width = Math.max(1, Math.round(width));
  canvas.height = Math.max(1, Math.round(height));
  return canvas;
}

// ─── Crop ─────────────────────────────────────────────────────────────────────

/**
 * Crop the source to `rect` (source-pixel space) and encode PNG.
 * The rect is rounded to whole pixels and clamped to the image bounds.
 */
export async function exportCrop(source: DrawableSource, rect: Rect, naturalWidth: number, naturalHeight: number): Promise<Blob> {
  const x = Math.max(0, Math.min(naturalWidth, Math.round(rect.x)));
  const y = Math.max(0, Math.min(naturalHeight, Math.round(rect.y)));
  const w = Math.max(1, Math.min(naturalWidth - x, Math.round(rect.w)));
  const h = Math.max(1, Math.min(naturalHeight - y, Math.round(rect.h)));
  const canvas = createCanvas(w, h);
  const ctx = canvas.getContext('2d');
  if (!ctx) throw new Error('2D context unavailable');
  ctx.drawImage(source, x, y, w, h, 0, 0, w, h);
  return canvasToPngBlob(canvas);
}

// ─── Mask ───────────────────────────────────────────────────────────────────

/**
 * Build the inpaint mask PNG at the **original** image size.
 *
 * Contract (frozen in the M0 stub): unpainted area = opaque white, painted area
 * = fully transparent (alpha 0). We paint an opaque-white full frame, then use
 * `destination-out` compositing with the working paint canvas: wherever the
 * paint layer has alpha `a`, the output alpha becomes `1 - a`. Full-opacity
 * strokes (a = 1) therefore punch the pixel to alpha 0; anti-aliased stroke
 * edges yield a soft (partial-alpha) mask border, which is desirable for
 * inpainting. Untouched pixels (a = 0) stay opaque white.
 */
export async function exportMask(naturalWidth: number, naturalHeight: number, paintLayer: HTMLCanvasElement): Promise<Blob> {
  const canvas = createCanvas(naturalWidth, naturalHeight);
  const ctx = canvas.getContext('2d');
  if (!ctx) throw new Error('2D context unavailable');
  ctx.globalCompositeOperation = 'source-over';
  ctx.fillStyle = '#ffffff';
  ctx.fillRect(0, 0, canvas.width, canvas.height);
  ctx.globalCompositeOperation = 'destination-out';
  // The paint layer may be at a reduced working resolution; scale it up to the
  // full image size so the mask matches the original pixel-for-pixel.
  ctx.imageSmoothingEnabled = true;
  ctx.imageSmoothingQuality = 'high';
  ctx.drawImage(paintLayer, 0, 0, paintLayer.width, paintLayer.height, 0, 0, canvas.width, canvas.height);
  ctx.globalCompositeOperation = 'source-over';
  return canvasToPngBlob(canvas);
}

// ─── Split ───────────────────────────────────────────────────────────────────

/**
 * Reduce a set of dividers to the kept bands along a single axis of `length`.
 *
 * Each divider removes `[pos - gap/2, pos + gap/2]`; the surviving spans between
 * (and around) the removed seams become the bands. Zero/negative-width bands
 * (e.g. from overlapping gaps) are dropped, and bands are clamped to
 * `[0, length]`.
 */
export function computeBands(length: number, dividers: Divider[]): Band[] {
  const sorted = [...dividers].filter((d) => d.pos > 0 && d.pos < length).sort((a, b) => a.pos - b.pos);
  const bands: Band[] = [];
  let cursor = 0;
  for (const d of sorted) {
    const half = Math.max(0, d.gap) / 2;
    const end = d.pos - half;
    if (end - cursor > 0.5) bands.push({ start: cursor, end });
    cursor = Math.max(cursor, d.pos + half);
  }
  if (length - cursor > 0.5) bands.push({ start: cursor, end: length });
  return bands.map((b) => ({ start: Math.max(0, b.start), end: Math.min(length, b.end) }));
}

/** Even-split dividers: `count - 1` internal lines, each with the same `gap`. */
export function equalDividers(length: number, count: number, gap: number): Divider[] {
  const dividers: Divider[] = [];
  for (let i = 1; i < count; i += 1) dividers.push({ pos: (length * i) / count, gap });
  return dividers;
}

/** One exported grid cell. `row`/`col` are 0-based from the top-left. */
export interface SplitPiece {
  blob: Blob;
  row: number;
  col: number;
}

/**
 * Slice the source into the grid described by the X/Y dividers and encode each
 * cell to PNG. Cells are produced row-major; `row`/`col` index the surviving
 * bands (0-based), so seam strips are excluded entirely.
 */
export async function exportSplit(
  source: DrawableSource,
  naturalWidth: number,
  naturalHeight: number,
  xDividers: Divider[],
  yDividers: Divider[]
): Promise<SplitPiece[]> {
  const cols = computeBands(naturalWidth, xDividers);
  const rows = computeBands(naturalHeight, yDividers);
  const pieces: SplitPiece[] = [];
  for (let r = 0; r < rows.length; r += 1) {
    const row = rows[r];
    const sy = Math.round(row.start);
    const sh = Math.max(1, Math.round(row.end - row.start));
    for (let c = 0; c < cols.length; c += 1) {
      const col = cols[c];
      const sx = Math.round(col.start);
      const sw = Math.max(1, Math.round(col.end - col.start));
      const canvas = createCanvas(sw, sh);
      const ctx = canvas.getContext('2d');
      if (!ctx) throw new Error('2D context unavailable');
      ctx.drawImage(source, sx, sy, sw, sh, 0, 0, sw, sh);
      // Encode sequentially to bound peak memory on large grids.
      const blob = await canvasToPngBlob(canvas);
      pieces.push({ blob, row: r, col: c });
    }
  }
  return pieces;
}

// ─── Upscale ────────────────────────────────────────────────────────────────

export type UpscaleAlgo = 'progressive' | 'bilinear' | 'nearest';

/** Target dimensions for a longest-edge upscale, preserving aspect ratio. */
export function computeUpscaleTarget(
  naturalWidth: number,
  naturalHeight: number,
  longestEdge: number
): { width: number; height: number; scale: number } {
  const longest = Math.max(naturalWidth, naturalHeight);
  const scale = longest > 0 ? longestEdge / longest : 1;
  return {
    width: Math.max(1, Math.round(naturalWidth * scale)),
    height: Math.max(1, Math.round(naturalHeight * scale)),
    scale,
  };
}

/**
 * Local (interpolation-only) upscale.
 * - `nearest`   — smoothing off (hard pixels).
 * - `bilinear`  — single high-quality resample.
 * - `progressive` — repeatedly double with high-quality smoothing until the
 *   target is reached, then a final exact resize. Stepwise doubling gives a
 *   smoother enlargement than one big jump.
 */
export async function exportUpscale(
  source: DrawableSource,
  naturalWidth: number,
  naturalHeight: number,
  target: { width: number; height: number },
  algo: UpscaleAlgo
): Promise<Blob> {
  const draw = (dst: HTMLCanvasElement, src: DrawableSource, srcW: number, srcH: number, smooth: boolean) => {
    const ctx = dst.getContext('2d');
    if (!ctx) throw new Error('2D context unavailable');
    ctx.imageSmoothingEnabled = smooth;
    if (smooth) ctx.imageSmoothingQuality = 'high';
    ctx.clearRect(0, 0, dst.width, dst.height);
    ctx.drawImage(src, 0, 0, srcW, srcH, 0, 0, dst.width, dst.height);
  };

  if (algo === 'progressive' && (target.width > naturalWidth || target.height > naturalHeight)) {
    let current: DrawableSource = source;
    let curW = naturalWidth;
    let curH = naturalHeight;
    // Double each axis until we meet or exceed the target, capping at target.
    // Guard the loop count so a pathological target can never spin forever.
    for (let step = 0; step < 16 && (curW < target.width || curH < target.height); step += 1) {
      const nextW = Math.min(target.width, curW * 2);
      const nextH = Math.min(target.height, curH * 2);
      const step2x = createCanvas(nextW, nextH);
      draw(step2x, current, curW, curH, true);
      current = step2x;
      curW = step2x.width;
      curH = step2x.height;
    }
    if (curW === target.width && curH === target.height && current instanceof HTMLCanvasElement) {
      return canvasToPngBlob(current);
    }
    const out = createCanvas(target.width, target.height);
    draw(out, current, curW, curH, true);
    return canvasToPngBlob(out);
  }

  const out = createCanvas(target.width, target.height);
  draw(out, source, naturalWidth, naturalHeight, algo !== 'nearest');
  return canvasToPngBlob(out);
}

/**
 * Rough PNG byte estimate for a not-yet-encoded target. PNG size is content
 * dependent, so this is a deliberately coarse heuristic (bytes-per-pixel) used
 * only for a pre-apply "~size" hint.
 */
export function estimatePngBytes(width: number, height: number): number {
  const BYTES_PER_PIXEL = 2.2; // empirical middle ground for photographic PNGs
  return Math.round(width * height * BYTES_PER_PIXEL);
}

/** Human-readable byte size (e.g. "2.4 MB"). */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
