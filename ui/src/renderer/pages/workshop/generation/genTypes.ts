/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Local types for the generation-card module (M7). Kept separate from the
 * canvas doc contract (`../types.ts`) — these describe UI-layer constructs
 * (resolved model options, `@`-mention candidates) that never persist.
 */

import type { WorkshopAssetKind, WorkshopGeneratorMode } from '../types';

/** A single generation-capable model resolved against a configured provider. */
export interface ModelOption {
  providerId: string;
  providerName: string;
  platform: string;
  model: string;
}

/** Models grouped under one provider, for the picker's grouped list. */
export interface ModelGroup {
  providerId: string;
  providerName: string;
  platform: string;
  models: ModelOption[];
}

/**
 * An `@`-mention candidate: either a canvas node (auto-numbered 图1 / 视频2 /
 * 文1 …) or a library asset (labelled by title). The stable `ref` is what gets
 * written into `data.mentions`; the human label is inserted inline into the
 * prompt text.
 */
export interface MentionCandidate {
  /** Stable reference: `node:<nodeId>` or `asset:<kind>:<assetId>`. */
  ref: string;
  /** Human label shown in the picker + inserted inline (e.g. `图2`). */
  label: string;
  kind: WorkshopAssetKind;
  source: 'node' | 'asset';
}

/** A resolved mention, ready to feed the run pipeline. */
export interface ResolvedMention {
  ref: string;
  label: string;
  kind: WorkshopAssetKind;
  /** Asset id for image / video references; null for text (folded into prompt). */
  assetId: string | null;
  /** Text body for text nodes / assets; null otherwise. */
  text: string | null;
}

/** The generation mode a card is currently in. */
export type GenMode = WorkshopGeneratorMode;
