/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

export type CompanionPointerBackend = 'appkit' | 'win32' | 'x11';
export type CompanionUnsupportedPointerBackend = 'wayland' | 'other';

export type CompanionLocalPointerSample =
  | {
      kind: 'point';
      backend: CompanionPointerBackend;
      xRatio: number;
      yRatio: number;
    }
  | {
      kind: 'unsupported';
      backend: CompanionUnsupportedPointerBackend;
    };

export interface CompanionPointerViewport {
  width: number;
  height: number;
}

const POINT_BACKENDS = new Set<CompanionPointerBackend>(['appkit', 'win32', 'x11']);
const UNSUPPORTED_BACKENDS = new Set<CompanionUnsupportedPointerBackend>(['wayland', 'other']);

const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === 'object' && value !== null;

/** Validate the native IPC boundary instead of trusting a compile-time generic. */
export function parseCompanionLocalPointer(value: unknown): CompanionLocalPointerSample {
  if (!isRecord(value)) throw new Error('invalid companion pointer response');

  if (value.kind === 'point') {
    if (
      typeof value.backend !== 'string' ||
      !POINT_BACKENDS.has(value.backend as CompanionPointerBackend) ||
      typeof value.xRatio !== 'number' ||
      !Number.isFinite(value.xRatio) ||
      typeof value.yRatio !== 'number' ||
      !Number.isFinite(value.yRatio)
    ) {
      throw new Error('invalid companion pointer point');
    }
    return {
      kind: 'point',
      backend: value.backend as CompanionPointerBackend,
      xRatio: value.xRatio,
      yRatio: value.yRatio,
    };
  }

  if (
    value.kind === 'unsupported' &&
    typeof value.backend === 'string' &&
    UNSUPPORTED_BACKENDS.has(value.backend as CompanionUnsupportedPointerBackend)
  ) {
    return {
      kind: 'unsupported',
      backend: value.backend as CompanionUnsupportedPointerBackend,
    };
  }

  throw new Error('invalid companion pointer response');
}

/** Map native window-local ratios into the current CSS viewport (including page zoom). */
export function toCompanionClientPoint(
  sample: CompanionLocalPointerSample,
  viewport: CompanionPointerViewport
): { x: number; y: number } | null {
  if (
    sample.kind !== 'point' ||
    !Number.isFinite(viewport.width) ||
    !Number.isFinite(viewport.height) ||
    viewport.width <= 0 ||
    viewport.height <= 0
  ) {
    return null;
  }

  return {
    x: sample.xRatio * viewport.width,
    y: sample.yRatio * viewport.height,
  };
}

export async function getCompanionLocalPointer(): Promise<CompanionLocalPointerSample> {
  const { invoke } = await import('@tauri-apps/api/core');
  return parseCompanionLocalPointer(await invoke<unknown>('get_companion_local_pointer'));
}
