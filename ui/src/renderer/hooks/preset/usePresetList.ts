import { ipcBridge } from '@/common';
import { resolveLocaleKey } from '@/common/utils';
import type { Preset, PresetReference } from '@/common/types/agent/presetTypes';
import { sortPresets as sortPresetsUtil } from '@/renderer/pages/settings/PresetSettings/presetUtils';
import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

/**
 * Pure predicate: an preset is extension-sourced.
 */
export const isExtensionPreset = (preset: Preset | null | undefined): boolean =>
  preset?.source === 'extension';

/**
 * Manages the preset list: loading from backend, sorting, and tracking the
 * active selection. The backend merges builtin + user + extension into a single
 * ordered list, so no client-side merge logic is needed.
 */
export const usePresetList = () => {
  const { i18n } = useTranslation();
  const [presets, setPresets] = useState<Preset[]>([]);
  const [activePresetId, setActivePresetId] = useState<PresetReference | null>(null);
  const localeKey = resolveLocaleKey(i18n.language);

  const loadPresets = useCallback(async () => {
    try {
      const list = await ipcBridge.presets.list.invoke();
      const sorted = sortPresetsUtil(list);
      setPresets(sorted);
      setActivePresetId((prev) => {
        if (prev && sorted.some((preset) => preset.preset_id === prev)) return prev;
        return sorted[0]?.preset_id ?? null;
      });
    } catch (error) {
      console.error('Failed to load presets:', error);
    }
  }, []);

  useEffect(() => {
    void loadPresets();
  }, [loadPresets]);

  const activePreset = presets.find((preset) => preset.preset_id === activePresetId) ?? null;

  return {
    presets,
    setPresets,
    activePresetId,
    setActivePresetId,
    activePreset,
    isExtensionPreset,
    loadPresets,
    localeKey,
  };
};
