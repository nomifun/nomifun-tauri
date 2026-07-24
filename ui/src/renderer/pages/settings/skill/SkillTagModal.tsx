/**
 * SkillTagModal — Assigns tags to a single skill. An Arco Modal titled with the
 * skill name, holding two PresetTagPicker rows (Audience / Skill Scenario)
 * over the SHARED preset tag vocabulary. Selections are prefilled from the
 * skill's currently-resolved tags and held in local state until Save, which
 * PUTs to /api/skills/{name}/tags and then calls onSaved so the parent reloads.
 *
 * Inline tag creation flows through onCreateTag (usePresetTags().createTag),
 * keeping the skill and preset pages on one vocabulary.
 *
 * Theme variables only; `<div onClick>`/Arco controls (no <button>).
 */
import { ipcBridge } from '@/common';
import type { PresetTag, CreatePresetTagRequest } from '@/common/types/agent/presetTypes';
import type { SkillInfo } from '@/renderer/pages/settings/PresetSettings/types';
import type { ArcoMessageInstance } from '@/renderer/utils/ui/useArcoMessage';
// Shared tag UI — reused verbatim from the preset page so both surfaces
// share one chip language and one vocabulary.
import PresetTagPicker, {
  type PresetTagPickerHandle,
} from '@/renderer/pages/settings/PresetSettings/PresetTagPicker';
import { Button, Modal } from '@arco-design/web-react';
import React, { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { resolveSkillDisplay } from './skillDisplay';

type SkillTagModalProps = {
  visible: boolean;
  skill: SkillInfo | null;
  onClose: () => void;
  audienceTags: PresetTag[];
  scenarioTags: PresetTag[];
  onCreateTag: (req: CreatePresetTagRequest) => Promise<PresetTag>;
  localeKey: string;
  /** Called after a successful save so the parent can reload the skill list. */
  onSaved: () => void;
  message: ArcoMessageInstance;
};

const SkillTagModal: React.FC<SkillTagModalProps> = ({
  visible,
  skill,
  onClose,
  audienceTags,
  scenarioTags,
  onCreateTag,
  localeKey,
  onSaved,
  message,
}) => {
  const { t } = useTranslation();
  const [audience, setAudience] = useState<string[]>([]);
  const [scenario, setScenario] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);
  const audiencePickerRef = useRef<PresetTagPickerHandle>(null);
  const scenarioPickerRef = useRef<PresetTagPickerHandle>(null);
  const display = skill ? resolveSkillDisplay(skill, localeKey) : null;

  // Re-seed local selection whenever a new skill opens.
  useEffect(() => {
    if (visible && skill) {
      setAudience(skill.audience_tags ?? []);
      setScenario(skill.scenario_tags ?? []);
      audiencePickerRef.current?.resetPendingTag();
      scenarioPickerRef.current?.resetPendingTag();
    }
  }, [visible, skill]);

  const handleClose = () => {
    audiencePickerRef.current?.resetPendingTag();
    scenarioPickerRef.current?.resetPendingTag();
    onClose();
  };

  const handleSave = async () => {
    if (!skill || saving) return;
    setSaving(true);
    try {
      const [nextAudience, nextScenario] = await Promise.all([
        audiencePickerRef.current?.flushPendingTag() ?? Promise.resolve(audience),
        scenarioPickerRef.current?.flushPendingTag() ?? Promise.resolve(scenario),
      ]);
      setAudience(nextAudience);
      setScenario(nextScenario);
      await ipcBridge.fs.setSkillTags.invoke({
        skill_name: skill.name,
        audience_tags: nextAudience,
        scenario_tags: nextScenario,
      });
      message.success(t('settings.skillsHub.tagsSaved', { defaultValue: 'Tags saved' }));
      onSaved();
      handleClose();
    } catch (error) {
      console.error('Failed to save skill tags:', error);
      message.error(t('settings.skillsHub.tagsSaveFailed', { defaultValue: 'Failed to save tags' }));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Modal
      visible={visible}
      onCancel={handleClose}
      title={
        <div className='flex items-center gap-8px min-w-0'>
          <span className='text-12px font-normal text-[var(--color-text-3)] flex-shrink-0'>
            {t('settings.skillsHub.editTagsTitle', { defaultValue: 'Tags' })}
          </span>
          <span className='truncate text-14px font-medium text-[var(--color-text-1)]' title={display?.name}>
            {display?.name}
          </span>
        </div>
      }
      style={{ width: 560, maxWidth: '92vw', borderRadius: 16 }}
      maskClosable={!saving}
      footer={
        <div className='flex items-center justify-end gap-10px'>
          <Button onClick={handleClose} disabled={saving}>
            {t('common.cancel', { defaultValue: 'Cancel' })}
          </Button>
          <Button type='primary' loading={saving} onClick={() => void handleSave()} data-testid='btn-save-skill-tags'>
            {t('common.save', { defaultValue: 'Save' })}
          </Button>
        </div>
      }
      data-testid='skill-tag-modal'
    >
      <p className='mt-0 mb-16px text-12px leading-18px text-[var(--color-text-3)]'>
        {t('settings.skillsHub.editTagsDesc', {
          defaultValue: 'Tag this skill so it surfaces under the right audience and scenario filters.',
        })}
      </p>
      <div className='flex flex-col gap-18px'>
        <PresetTagPicker
          ref={audiencePickerRef}
          dimension='audience'
          label={t('settings.presetTagAudience', { defaultValue: 'Audience' })}
          tags={audienceTags}
          value={audience}
          onChange={setAudience}
          onCreateTag={onCreateTag}
          localeKey={localeKey}
          readOnly={false}
          commitOnBlur
        />
        <PresetTagPicker
          ref={scenarioPickerRef}
          dimension='scenario'
          label={t('settings.presetTagScenario', { defaultValue: 'Skill Scenario' })}
          tags={scenarioTags}
          value={scenario}
          onChange={setScenario}
          onCreateTag={onCreateTag}
          localeKey={localeKey}
          readOnly={false}
          commitOnBlur
        />
      </div>
    </Modal>
  );
};

export default SkillTagModal;
