/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Input, Message, Modal, Radio, Spin, TimePicker } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import { CUSTOM_CHARACTER_ID } from '@renderer/pages/companion/characters';
import { customFigureMetaOf } from '@renderer/pages/companion/characters/customMeta';
import CharacterPicker from '../CharacterPicker';
import { figureToCustomPatch } from '../useFigures';
import type { useCompanion } from '../useNomi';
import PresetApplyControl from '@/renderer/components/preset/PresetApplyControl';
import type { CompanionId } from '@/common/types/ids';

interface Props {
  companion: ReturnType<typeof useCompanion>;
  /** Called after this companion was deleted so the page can switch selection. */
  onDeleted: (companionId: CompanionId) => void;
}

/**
 * Debounced text editing over an optimistically-patched source value: local
 * draft follows keystrokes, the commit fires after `delay` ms of quiet.
 */
const useDebouncedText = (source: string, commit: (value: string) => void, delay = 500) => {
  const [draft, setDraft] = useState(source);
  const timerRef = useRef<number | undefined>(undefined);
  const commitRef = useRef(commit);
  commitRef.current = commit;

  useEffect(() => {
    setDraft(source);
  }, [source]);
  useEffect(() => () => window.clearTimeout(timerRef.current), []);

  const onChange = useCallback(
    (value: string) => {
      setDraft(value);
      window.clearTimeout(timerRef.current);
      timerRef.current = window.setTimeout(() => commitRef.current(value), delay);
    },
    [delay]
  );

  return [draft, onChange] as const;
};

const SettingsTab: React.FC<Props> = ({ companion, onDeleted }) => {
  const { t } = useTranslation();
  const { profile, patchCompanion } = companion;

  const [nameDraft, onNameChange] = useDebouncedText(profile?.name ?? '', (value) => {
    const name = value.trim();
    if (!name || name === profile?.name) return;
    void patchCompanion({ name }).catch((e) => Message.error(String(e)));
  });

  const [customDraft, onCustomChange] = useDebouncedText(profile?.persona.custom ?? '', (custom) => {
    if (custom === profile?.persona.custom) return;
    void patchCompanion({ persona: { custom } }).catch((e) => Message.error(String(e)));
  });

  const confirmDelete = useCallback(() => {
    if (!profile) return;
    const companionName = profile.name;
    Modal.confirm({
      title: t('nomi.settings.deleteConfirmTitle'),
      content: t('nomi.settings.deleteConfirmBody', { companionName }),
      okButtonProps: { status: 'danger' },
      onOk: async () => {
        try {
          await ipcBridge.companion.deleteCompanion.invoke({ companion_id: profile.companion_id });
          Message.success(t('nomi.settings.deleted', { companionName }));
          onDeleted(profile.companion_id);
        } catch (e) {
          Message.error(String(e));
        }
      },
    });
  }, [profile, onDeleted, t]);

  if (!profile) {
    return (
      <div className='flex justify-center py-40px'>
        <Spin />
      </div>
    );
  }

  const companionName = profile.name;

  const row = (label: string, hint: string | null, control: React.ReactNode) => (
    <div className='flex items-start gap-16px bg-fill-2 rd-10px px-14px py-12px'>
      <div className='w-200px shrink-0'>
        <div className='text-14px text-t-primary font-500'>{label}</div>
        {hint && <div className='text-12px text-t-tertiary mt-2px'>{hint}</div>}
      </div>
      <div className='flex-1 min-w-0'>{control}</div>
    </div>
  );

  return (
    <div className='flex flex-col gap-10px py-8px'>
      {row(
        t('nomi.settings.name'),
        t('nomi.settings.nameHint'),
        <Input style={{ width: 260 }} value={nameDraft} onChange={onNameChange} maxLength={30} />
      )}
      {row(
        t('nomi.settings.character'),
        t('nomi.settings.characterHint'),
        <CharacterPicker
          value={profile.character || 'mochi'}
          figureId={customFigureMetaOf(profile)?.figureId}
          onSelectCharacter={(character) => void patchCompanion({ character, appearance: { custom_figure: null } })}
          onSelectFigure={(fig) =>
            void patchCompanion({
              character: CUSTOM_CHARACTER_ID,
              appearance: { custom_figure: figureToCustomPatch(fig) },
            })
          }
        />
      )}
      {row(
        t('nomi.settings.preset'),
        t('nomi.settings.presetHint'),
        <PresetApplyControl
          target='companion'
          appliedPreset={profile.applied_preset}
          onApply={async (presetId, locale) => {
            await ipcBridge.companion.applyPreset.invoke({
              companion_id: profile.companion_id,
              preset_id: presetId,
              locale,
            });
            await companion.refresh();
          }}
        />
      )}
      {row(
        t('nomi.settings.persona'),
        t('nomi.settings.personaHint', { companionName }),
        <div className='flex flex-col gap-8px'>
          <Radio.Group
            type='button'
            value={profile.persona.preset}
            onChange={(preset: string) => void patchCompanion({ persona: { preset } })}
          >
            <Radio value='lively'>{t('nomi.settings.personaLively')}</Radio>
            <Radio value='calm'>{t('nomi.settings.personaCalm')}</Radio>
            <Radio value='sassy'>{t('nomi.settings.personaSassy')}</Radio>
          </Radio.Group>
          <Input.TextArea
            rows={2}
            placeholder={t('nomi.settings.personaCustomPlaceholder')}
            value={customDraft}
            onChange={onCustomChange}
          />
        </div>
      )}
      {row(
        t('nomi.settings.quietHours'),
        t('nomi.settings.quietHoursHint'),
        <TimePicker.RangePicker
          format='HH:mm'
          allowClear
          value={
            profile.appearance.quiet_start && profile.appearance.quiet_end
              ? [profile.appearance.quiet_start, profile.appearance.quiet_end]
              : undefined
          }
          onChange={(value) => {
            const [quiet_start, quiet_end] = (value as string[] | undefined) ?? ['', ''];
            void patchCompanion({
              appearance: { quiet_start: quiet_start || '', quiet_end: quiet_end || '' },
            });
          }}
        />
      )}

      <div className='mt-8px text-13px font-600 text-[rgb(var(--danger-6))]'>{t('nomi.settings.danger')}</div>
      {row(
        t('nomi.settings.deleteCompanion'),
        t('nomi.settings.deleteCompanionHint', { companionName }),
        <Button status='danger' onClick={confirmDelete}>
          {t('nomi.settings.deleteCompanion')}
        </Button>
      )}
    </div>
  );
};

export default SettingsTab;
