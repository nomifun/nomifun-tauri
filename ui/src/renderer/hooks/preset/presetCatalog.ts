/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { Preset } from '@/common/types/agent/presetTypes';

/** One cache identity for every preset catalog consumer. */
export const PRESET_CATALOG_SWR_KEY = 'presets.list';

/** Backend-merged builtin + user + extension preset catalog. */
export const fetchPresetCatalog = async (): Promise<Preset[]> => {
  return await ipcBridge.presets.list.invoke();
};
