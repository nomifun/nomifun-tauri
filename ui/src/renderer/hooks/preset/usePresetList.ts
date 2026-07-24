import { resolveLocaleKey } from '@/common/utils';
import type { Preset, PresetReference } from '@/common/types/agent/presetTypes';
import { sortPresets as sortPresetsUtil } from '@/renderer/pages/settings/PresetSettings/presetUtils';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import useSWR from 'swr';
import { fetchPresetCatalog, PRESET_CATALOG_SWR_KEY } from './presetCatalog';

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
  const [activePresetId, setActivePresetId] = useState<PresetReference | null>(null);
  const localeKey = resolveLocaleKey(i18n.language);
  const { data: catalog = [], mutate } = useSWR<Preset[]>(
    PRESET_CATALOG_SWR_KEY,
    fetchPresetCatalog,
  );
  const presets = useMemo(() => sortPresetsUtil(catalog), [catalog]);

  const loadPresets = useCallback(async () => {
    try {
      await mutate();
    } catch (error) {
      console.error('Failed to load presets:', error);
    }
  }, [mutate]);

  useEffect(() => {
    setActivePresetId((prev) => {
      if (prev && presets.some((preset) => preset.preset_id === prev)) return prev;
      return presets[0]?.preset_id ?? null;
    });
  }, [presets]);

  const activePreset = presets.find((preset) => preset.preset_id === activePresetId) ?? null;

  return {
    presets,
    activePresetId,
    setActivePresetId,
    activePreset,
    isExtensionPreset,
    loadPresets,
    localeKey,
  };
};
