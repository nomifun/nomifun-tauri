/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Static parameter vocabularies + defaults for the generation card. Kept in one
 * place so the param panel, the run pipeline, and the node factory all agree.
 */

import type { GenMode } from './genTypes';

// ─── Image size presets ───────────────────────────────────────────────────────

export interface SizePreset {
  key: string;
  /** i18n suffix under `workshopGeneration.size.*`; falls back to `key`. */
  labelKey: string;
  width: number;
  height: number;
}

export const IMAGE_SIZE_PRESETS: SizePreset[] = [
  { key: '1:1', labelKey: 'square', width: 1024, height: 1024 },
  { key: '4:3', labelKey: 'landscape43', width: 1024, height: 768 },
  { key: '3:4', labelKey: 'portrait34', width: 768, height: 1024 },
  { key: '16:9', labelKey: 'wide169', width: 1280, height: 720 },
  { key: '9:16', labelKey: 'tall916', width: 720, height: 1280 },
  { key: '2k', labelKey: 'twoK', width: 2048, height: 2048 },
  { key: '4k', labelKey: 'fourK', width: 4096, height: 4096 },
];

export const IMAGE_QUALITIES = ['auto', 'high', 'medium', 'low'] as const;
export type ImageQuality = (typeof IMAGE_QUALITIES)[number];

export const IMAGE_COUNT_MIN = 1;
export const IMAGE_COUNT_MAX = 10;

// ─── Video presets ─────────────────────────────────────────────────────────────

export const VIDEO_RESOLUTIONS = ['480p', '720p', '1080p'] as const;
export type VideoResolution = (typeof VIDEO_RESOLUTIONS)[number];

export const VIDEO_ASPECTS = ['16:9', '9:16', '1:1', '4:3', '3:4'] as const;
export type VideoAspect = (typeof VIDEO_ASPECTS)[number];

export const VIDEO_SECONDS_MIN = 4;
export const VIDEO_SECONDS_MAX = 20;

/**
 * Explicit `resolution × aspect → width/height` table. The video card only lets
 * the user pick a resolution + aspect, but the openai_video adapter derives its
 * size from `params.width` / `params.height` (via `param_size`); it ignores
 * `resolution` / `aspect`. So we translate the selection into concrete pixels
 * here. Keyed `"<resolution>:<aspect>"`; any combination absent from the table
 * (e.g. an unknown/adaptive pairing) omits width/height so the provider falls
 * back to its own default rather than receiving a bogus size.
 *
 * Convention: the short side tracks the resolution (480/720/1080); 1:1 uses
 * resolution×4/3 so its pixel area matches the 16:9 variant.
 */
export const VIDEO_DIMENSIONS: Record<string, { width: number; height: number }> = {
  '480p:16:9': { width: 854, height: 480 },
  '480p:9:16': { width: 480, height: 854 },
  '480p:1:1': { width: 640, height: 640 },
  '480p:4:3': { width: 640, height: 480 },
  '480p:3:4': { width: 480, height: 640 },
  '720p:16:9': { width: 1280, height: 720 },
  '720p:9:16': { width: 720, height: 1280 },
  '720p:1:1': { width: 960, height: 960 },
  '720p:4:3': { width: 960, height: 720 },
  '720p:3:4': { width: 720, height: 960 },
  '1080p:16:9': { width: 1920, height: 1080 },
  '1080p:9:16': { width: 1080, height: 1920 },
  '1080p:1:1': { width: 1440, height: 1440 },
  '1080p:4:3': { width: 1440, height: 1080 },
  '1080p:3:4': { width: 1080, height: 1440 },
};

// ─── Per-mode defaults ───────────────────────────────────────────────────────

export interface ImageParams {
  preset: string;
  width: number;
  height: number;
  count: number;
  quality: ImageQuality;
}

export interface VideoParams {
  seconds: number;
  resolution: VideoResolution;
  aspect: VideoAspect;
  generate_audio: boolean;
  watermark: boolean;
}

export const DEFAULT_IMAGE_PARAMS: ImageParams = {
  preset: '1:1',
  width: 1024,
  height: 1024,
  count: 1,
  quality: 'auto',
};

export const DEFAULT_VIDEO_PARAMS: VideoParams = {
  seconds: 5,
  resolution: '720p',
  aspect: '16:9',
  generate_audio: false,
  watermark: false,
};

/** Comfortable default node box the card grows to on first mount, per mode. */
export const CARD_SIZE: Record<GenMode, { width: number; height: number }> = {
  image: { width: 344, height: 496 },
  video: { width: 344, height: 470 },
  text: { width: 340, height: 340 },
};

/** The factory-minted box (`model.ts` `KIND_META.generator`) we grow away from. */
export const FACTORY_BOX = { width: 300, height: 220 } as const;

// ─── Stored-param readers (tolerant of missing / stale fields) ──────────────────

function num(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

function oneOf<T extends string>(value: unknown, allowed: readonly T[], fallback: T): T {
  return typeof value === 'string' && (allowed as readonly string[]).includes(value) ? (value as T) : fallback;
}

export function readImageParams(p: Record<string, unknown>): ImageParams {
  return {
    preset: typeof p.preset === 'string' ? p.preset : DEFAULT_IMAGE_PARAMS.preset,
    width: Math.round(num(p.width, DEFAULT_IMAGE_PARAMS.width)),
    height: Math.round(num(p.height, DEFAULT_IMAGE_PARAMS.height)),
    count: Math.min(IMAGE_COUNT_MAX, Math.max(IMAGE_COUNT_MIN, Math.round(num(p.count, DEFAULT_IMAGE_PARAMS.count)))),
    quality: oneOf(p.quality, IMAGE_QUALITIES, DEFAULT_IMAGE_PARAMS.quality),
  };
}

export function readVideoParams(p: Record<string, unknown>): VideoParams {
  return {
    seconds: Math.min(VIDEO_SECONDS_MAX, Math.max(VIDEO_SECONDS_MIN, Math.round(num(p.seconds, DEFAULT_VIDEO_PARAMS.seconds)))),
    resolution: oneOf(p.resolution, VIDEO_RESOLUTIONS, DEFAULT_VIDEO_PARAMS.resolution),
    aspect: oneOf(p.aspect, VIDEO_ASPECTS, DEFAULT_VIDEO_PARAMS.aspect),
    generate_audio: p.generate_audio === true,
    watermark: p.watermark === true,
  };
}

/** Assemble the `params` object sent to `POST /api/creation/tasks`. */
export function buildTaskParams(mode: GenMode, stored: Record<string, unknown>, prompt: string): Record<string, unknown> {
  if (mode === 'image') {
    const p = readImageParams(stored);
    return { prompt, width: p.width, height: p.height, aspect: p.preset, count: p.count, quality: p.quality };
  }
  if (mode === 'video') {
    const p = readVideoParams(stored);
    const dims = VIDEO_DIMENSIONS[`${p.resolution}:${p.aspect}`];
    return {
      prompt,
      seconds: p.seconds,
      // resolution/aspect are retained for the (future) ark video adapter, which
      // consumes them directly along with generate_audio / watermark.
      resolution: p.resolution,
      aspect: p.aspect,
      generate_audio: p.generate_audio,
      watermark: p.watermark,
      // width/height feed the openai_video adapter's param_size(); omitted for
      // any combo missing from VIDEO_DIMENSIONS so the provider default applies.
      ...(dims ? { width: dims.width, height: dims.height } : {}),
    };
  }
  return { prompt };
}
