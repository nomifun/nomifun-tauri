/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ImageEditorResult } from './index';
import type { EditorImage } from './useEditorImage';

/** Imperative handle every tool exposes so the shared footer can trigger it. */
export interface ImageToolHandle {
  /** Produce the edit result, or `null` when the tool declines (e.g. empty mask). */
  apply: () => Promise<ImageEditorResult | null>;
}

/** Props shared by all four tool components. */
export interface ImageToolProps {
  image: EditorImage;
  /** Report whether the Apply button should be enabled. */
  onCanApplyChange: (canApply: boolean) => void;
}
