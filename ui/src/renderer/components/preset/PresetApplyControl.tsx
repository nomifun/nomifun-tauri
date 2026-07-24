/**
 * One-click application of a reusable preset to a durable target.
 * Resolution, validation and snapshot persistence are exclusively backend-owned.
 */
import React, { useEffect, useMemo, useState } from 'react';
import { Button, Message } from '@arco-design/web-react';
import { useTranslation } from 'react-i18next';
import type { Preset, PresetReference, PresetTarget, ResolvedPresetSnapshot } from '@/common/types/agent/presetTypes';
import NomiSelect from '@/renderer/components/base/NomiSelect';
import useSWR from 'swr';
import {
  fetchPresetCatalog,
  PRESET_CATALOG_SWR_KEY,
} from '@/renderer/hooks/preset/presetCatalog';

type Props = {
  target: PresetTarget;
  appliedPreset?: ResolvedPresetSnapshot;
  onApply: (presetId: PresetReference, locale: string) => Promise<void>;
  disabled?: boolean;
};

const PresetApplyControl: React.FC<Props> = ({ target, appliedPreset, onApply, disabled = false }) => {
  const { t, i18n } = useTranslation();
  const [applying, setApplying] = useState(false);
  const [selectedId, setSelectedId] = useState<PresetReference | undefined>(appliedPreset?.preset_id);
  const { data: presets = [], isLoading: loading } = useSWR<Preset[]>(
    PRESET_CATALOG_SWR_KEY,
    fetchPresetCatalog,
  );

  useEffect(() => {
    setSelectedId(appliedPreset?.preset_id);
  }, [appliedPreset?.preset_id]);

  const availablePresets = useMemo(
    () => presets.filter((preset) => preset.enabled && preset.targets.includes(target)),
    [presets, target]
  );
  const selectedPresetExists = useMemo(
    () => Boolean(selectedId && availablePresets.some((preset) => preset.preset_id === selectedId)),
    [availablePresets, selectedId]
  );

  useEffect(() => {
    if (!loading && selectedId && !selectedPresetExists) {
      setSelectedId(undefined);
    }
  }, [loading, selectedId, selectedPresetExists]);

  const apply = async () => {
    if (!selectedId || !selectedPresetExists) return;
    setApplying(true);
    try {
      await onApply(selectedId, i18n.language);
      Message.success(t('settings.presetApplySuccess', { defaultValue: 'Preset applied' }));
    } catch (error) {
      Message.error(error instanceof Error ? error.message : String(error));
    } finally {
      setApplying(false);
    }
  };

  return (
    <div className='flex flex-col gap-8px'>
      <div className='flex items-center gap-8px flex-wrap'>
        <NomiSelect
          value={selectedId}
          onChange={(value) => setSelectedId(value as PresetReference | undefined)}
          disabled={disabled || loading || applying}
          loading={loading}
          showSearch
          allowClear
          className='min-w-240px flex-1'
          placeholder={t('settings.presetApplyPlaceholder', { defaultValue: 'Choose a preset' })}
          notFoundContent={t('settings.presetApplyEmpty', { defaultValue: 'No preset supports this target yet' })}
        >
          {availablePresets.map((preset) => (
            <NomiSelect.Option key={preset.preset_id} value={preset.preset_id}>
              {preset.name_i18n?.[i18n.language] || preset.name}
            </NomiSelect.Option>
          ))}
        </NomiSelect>
        <Button
          type='primary'
          loading={applying}
          disabled={disabled || !selectedPresetExists || loading}
          onClick={() => void apply()}
        >
          {t('settings.presetApplyAction', { defaultValue: 'Apply preset' })}
        </Button>
      </div>
      {appliedPreset && (
        <div className='text-12px text-t-tertiary'>
          {t('settings.presetAppliedSnapshot', {
            defaultValue: 'Applied: {{name}} · revision {{revision}}',
            name: appliedPreset.preset_name,
            revision: appliedPreset.preset_revision,
          })}
        </div>
      )}
    </div>
  );
};

export default PresetApplyControl;
