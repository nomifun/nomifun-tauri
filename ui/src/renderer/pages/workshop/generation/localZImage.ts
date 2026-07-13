/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/** Frontend contract for the lightweight local Z-Image integration. */

import type { CreateTaskBody, CreationInput, MediaCapability } from '../types';
import { IMAGE_SIZE_PRESETS, readImageParams } from './genConstants';
import type { GenMode, ModelOption } from './genTypes';

export const LOCAL_Z_IMAGE_PROVIDER_ID = 'nomifun-local-model';
export const LOCAL_Z_IMAGE_MODEL_ID = 'z-image-turbo-q3-k';

export const LOCAL_Z_IMAGE_DIMENSION_MIN = 256;
export const LOCAL_Z_IMAGE_DIMENSION_MAX = 2048;
export const LOCAL_Z_IMAGE_DIMENSION_STEP = 8;

type ModelIdentity = Pick<ModelOption, 'providerId' | 'model'>;

export function isLocalZImageModel(model: ModelIdentity | null | undefined): boolean {
  return model?.providerId === LOCAL_Z_IMAGE_PROVIDER_ID && model.model === LOCAL_Z_IMAGE_MODEL_ID;
}

export function normalizeLocalZImageDimension(value: unknown): number {
  const numeric = typeof value === 'number' && Number.isFinite(value) ? value : 1024;
  const aligned = Math.round(numeric / LOCAL_Z_IMAGE_DIMENSION_STEP) * LOCAL_Z_IMAGE_DIMENSION_STEP;
  return Math.min(LOCAL_Z_IMAGE_DIMENSION_MAX, Math.max(LOCAL_Z_IMAGE_DIMENSION_MIN, aligned));
}

/**
 * Make stale canvas params safe for Z-Image without mutating the saved object.
 * In particular, old 4K/count>1 selections can survive model switches.
 */
export function normalizeLocalZImageParams(stored: Record<string, unknown>): Record<string, unknown> {
  const params = readImageParams(stored);
  const width = normalizeLocalZImageDimension(params.width);
  const height = normalizeLocalZImageDimension(params.height);
  const preset = params.preset === '4k' ? '2k' : params.preset;
  return { ...stored, preset, width, height, count: 1 };
}

/** Preserve the exact object and behavior for every non-local model. */
export function normalizeImageParamsForModel(
  model: ModelIdentity | null | undefined,
  stored: Record<string, unknown>
): Record<string, unknown> {
  return isLocalZImageModel(model) ? normalizeLocalZImageParams(stored) : stored;
}

export function localZImageSizePresets() {
  return IMAGE_SIZE_PRESETS.filter(
    (preset) => preset.width <= LOCAL_Z_IMAGE_DIMENSION_MAX && preset.height <= LOCAL_Z_IMAGE_DIMENSION_MAX
  );
}

export type LocalZImageRunIssue = 'text_to_image_only';

/**
 * The packaged runtime currently supports text-to-image only. Any resolved
 * media input must be rejected before POST /api/creation/tasks.
 */
export function validateLocalZImageRun(
  model: ModelIdentity | null | undefined,
  capability: MediaCapability,
  inputs: readonly CreationInput[]
): LocalZImageRunIssue | null {
  if (!isLocalZImageModel(model)) return null;
  return capability === 't2i' && inputs.length === 0 ? null : 'text_to_image_only';
}

export type LocalZImageTaskIssue =
  | LocalZImageRunIssue
  | 'invalid_dimensions'
  | 'single_image_only';

/**
 * Last-line guard for every caller of POST /api/creation/tasks. UI controls
 * normalize ordinary input, but saved canvases and future call sites may still
 * carry stale parameters.
 */
export function validateLocalZImageTask(body: CreateTaskBody): LocalZImageTaskIssue | null {
  if (!isLocalZImageModel({ providerId: body.provider_id, model: body.model })) return null;
  if (body.capability !== 't2i' || body.inputs.length !== 0) return 'text_to_image_only';

  const width = body.params.width ?? 1024;
  const height = body.params.height ?? 1024;
  const validDimension = (value: unknown): boolean =>
    typeof value === 'number' &&
    Number.isInteger(value) &&
    value >= LOCAL_Z_IMAGE_DIMENSION_MIN &&
    value <= LOCAL_Z_IMAGE_DIMENSION_MAX &&
    value % LOCAL_Z_IMAGE_DIMENSION_STEP === 0;
  if (!validDimension(width) || !validDimension(height)) return 'invalid_dimensions';

  const count = body.params.count ?? 1;
  return count === 1 ? null : 'single_image_only';
}

export function modelHubPathForMode(mode: GenMode): string {
  switch (mode) {
    case 'image':
      return '/models?section=local';
    case 'video':
      return '/models?section=creation';
    case 'text':
      return '/models?section=models';
  }
}
