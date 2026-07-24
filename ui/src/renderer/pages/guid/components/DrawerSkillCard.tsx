/**
 * DrawerSkillCard — Multi-select skill card for PresetPickerDrawer.
 * Displays initials avatar, name, source/auto-inject badge, description,
 * tag chips, and a right-side checkbox.
 */
import type { PresetTag } from '@/common/types/agent/presetTypes';
import type { SkillInfo } from '@/renderer/pages/settings/PresetSettings/types';
import { resolveSkillDisplay } from '@/renderer/pages/settings/skill/skillDisplay';
import React from 'react';
import { useTranslation } from 'react-i18next';
import { CheckSmall } from '@icon-park/react';
import styles from '../index.module.css';

export type DrawerSkillCardProps = {
  skill: SkillInfo;
  checked: boolean;
  isAuto: boolean;
  localeKey: string;
  tagByKey: Map<string, PresetTag>;
  onToggle: (name: string, isAuto: boolean) => void;
};

const DrawerSkillCard: React.FC<DrawerSkillCardProps> = ({
  skill,
  checked,
  isAuto,
  localeKey,
  tagByKey,
  onToggle,
}) => {
  const { t } = useTranslation();
  const display = resolveSkillDisplay(skill, localeKey);

  // Generate initials from skill name (first 2 uppercase chars)
  const initials = display.name
    .replace(/[^a-zA-Z]/g, '')
    .slice(0, 2)
    .toUpperCase() || display.name.slice(0, 2).toUpperCase();

  const sourceLabel =
    skill.source === 'custom' || skill.is_custom
      ? t('guid.drawer.sourceCustom', { defaultValue: '自定义' })
      : skill.source === 'extension'
        ? t('settings.skillsHub.sourceExtension', { defaultValue: 'Extension' })
        : t('guid.drawer.sourceBuiltin', { defaultValue: '内置' });
  const resolvedTags = [...(skill.audience_tags ?? []), ...(skill.scenario_tags ?? [])]
    .map((key) => tagByKey.get(key))
    .filter((tag): tag is PresetTag => Boolean(tag))
    .slice(0, 4);
  const handleKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      onToggle(skill.name, isAuto);
    }
  };

  return (
    <div
      role='checkbox'
      tabIndex={0}
      aria-checked={checked}
      className={[
        styles.drawerCard,
        checked ? styles.drawerCardSelected : '',
      ].filter(Boolean).join(' ')}
      onClick={() => onToggle(skill.name, isAuto)}
      onKeyDown={handleKeyDown}
    >
      {/* Checkbox indicator */}
      <span
        className={[
          styles.drawerCardStatus,
          checked ? styles.drawerCardStatusSelected : '',
        ].filter(Boolean).join(' ')}
        aria-hidden='true'
      >
        {checked && <CheckSmall theme='filled' size={13} fill='currentColor' />}
      </span>

      {/* Avatar initials */}
      <div className={styles.drawerIconTile}>
        {initials}
      </div>

      {/* Body */}
      <div className={styles.drawerCardBody}>
        <div className={styles.drawerCardTitleRow}>
          <h4 className={styles.drawerCardTitle}>{display.name}</h4>
          <span className={[styles.drawerBadge, styles.drawerBadgeMuted].join(' ')}>
            {sourceLabel}
          </span>
          {isAuto && (
            <span className={[styles.drawerBadge, styles.drawerBadgeSuccess].join(' ')}>
              {t('guid.drawer.autoInject', { defaultValue: '自动注入' })}
            </span>
          )}
        </div>

        <p className={styles.drawerDescription}>{display.description}</p>

        {/* Tag chips */}
        {resolvedTags.length > 0 ? (
          <div className={styles.drawerMetaRow}>
            {resolvedTags.map((tag) => (
              <span key={tag.key} className={styles.drawerTagChip}>
                {tag.label_i18n?.[localeKey] || tag.label}
              </span>
            ))}
          </div>
        ) : null}
      </div>
    </div>
  );
};

export default DrawerSkillCard;
