/**
 * PresetCard — A grid item for the preset list. Mirrors the AgentCard
 * visual language (rounded-16px bordered surface on bg-2, soft hover) but is
 * richer: avatar + name + source badge + enable Switch in the header, a 2-line
 * description clamp, a resolved tag-chip row, and a hover-revealed action
 * footer (Duplicate / Edit). The whole card is clickable → onEdit.
 *
 * Theme variables only; `<div onClick>` for clickables (no <button>).
 */
import type { PresetTag } from '@/common/types/agent/presetTypes';
import type { PresetListItem } from './types';
import PresetAvatar from './PresetAvatar';
import { Switch, Tag } from '@arco-design/web-react';
import { Copy, SettingOne } from '@icon-park/react';
import React from 'react';
import { useTranslation } from 'react-i18next';

type PresetCardProps = {
  preset: PresetListItem;
  localeKey: string;
  avatarImageMap: Record<string, string>;
  tagById: Map<string, PresetTag>;
  isExtensionPreset: (preset: PresetListItem | null | undefined) => boolean;
  onEdit: (preset: PresetListItem) => void;
  onDuplicate: (preset: PresetListItem) => void;
  onToggleEnabled: (preset: PresetListItem, checked: boolean) => void;
  highlighted?: boolean;
  cardRef?: (el: HTMLDivElement | null) => void;
};

const MAX_VISIBLE_TAGS = 4;

const PresetCard: React.FC<PresetCardProps> = ({
  preset,
  localeKey,
  avatarImageMap,
  tagById,
  isExtensionPreset,
  onEdit,
  onDuplicate,
  onToggleEnabled,
  highlighted = false,
  cardRef,
}) => {
  const { t } = useTranslation();
  const presetIsExtension = isExtensionPreset(preset);
  const name = preset.name_i18n?.[localeKey] || preset.name;
  const description = preset.description_i18n?.[localeKey] || preset.description || '';

  // Resolve persisted UUIDv7 tag IDs to readable catalog labels.
  const resolvedTags = [...(preset.audience_tag_ids ?? []), ...(preset.scenario_tag_ids ?? [])]
    .map((presetTagId) => tagById.get(presetTagId))
    .filter((tag): tag is PresetTag => Boolean(tag));
  const visibleTags = resolvedTags.slice(0, MAX_VISIBLE_TAGS);
  const overflowCount = resolvedTags.length - visibleTags.length;

  const isCustom = preset.source === 'user';

  return (
    <div
      ref={cardRef}
      data-testid={`preset-card-${preset.preset_id}`}
      onClick={() => onEdit(preset)}
      className={[
        'group relative flex flex-col rounded-16px p-14px cursor-pointer',
        'transition-all duration-180',
        highlighted
          ? 'bg-[var(--color-fill-3)] shadow-[0_8px_22px_rgba(0,0,0,0.14)]'
          : 'bg-[var(--color-bg-1)] hover:bg-[var(--color-fill-2)] hover:shadow-[0_8px_22px_rgba(0,0,0,0.12)]',
      ].join(' ')}
    >
      {/* Header: avatar + name/badge, enable Switch pinned top-right */}
      <div className='flex items-start gap-10px'>
        <PresetAvatar preset={preset} size={36} avatarImageMap={avatarImageMap} />
        <div className='min-w-0 flex-1 pt-1px'>
          <div className='flex items-center gap-6px min-w-0'>
            <span className='truncate text-14px font-medium leading-20px text-[var(--color-text-1)]'>{name}</span>
            {isCustom && (
              <Tag
                size='small'
                bordered={false}
                className='!flex-shrink-0 !text-10px !leading-14px !px-6px !py-0 !rounded-6px !bg-primary-1 !text-primary-6'
              >
                {t('settings.presetSourceCustom', { defaultValue: 'Custom' })}
              </Tag>
            )}
            {presetIsExtension && (
              <Tag
                size='small'
                bordered={false}
                className='!flex-shrink-0 !text-10px !leading-14px !px-6px !py-0 !rounded-6px !bg-fill-2 !text-t-secondary'
              >
                {t('settings.presetSourceExtension', { defaultValue: 'Extension' })}
              </Tag>
            )}
          </div>
        </div>
        <div className='flex-shrink-0 -mt-1px' onClick={(e) => e.stopPropagation()}>
          <Switch
            size='small'
            className='compact-dark-switch'
            data-testid={`switch-enabled-${preset.preset_id}`}
            checked={presetIsExtension ? true : preset.enabled !== false}
            disabled={presetIsExtension}
            onChange={(checked) => onToggleEnabled(preset, checked)}
          />
        </div>
      </div>

      {/* Compact configuration fingerprint: the preset reads as a reusable
          launch configuration, not as another person/companion card. */}
      <div className='mt-10px flex flex-wrap items-center gap-6px text-11px text-[var(--color-text-3)]'>
        <span className='rounded-8px bg-[var(--color-fill-2)] px-7px py-2px'>
          {preset.agent_preferences.length} {t('settings.presetAgentsShort', { defaultValue: 'Agents' })}
        </span>
        <span className='rounded-8px bg-[var(--color-fill-2)] px-7px py-2px'>
          {preset.included_skills.length} Skills
        </span>
        {preset.knowledge_policy.enabled && (
          <span className='rounded-8px bg-[rgba(var(--primary-6),0.08)] text-primary-6 px-7px py-2px'>
            {preset.knowledge_bases.length} {t('settings.presetKnowledgeShort', { defaultValue: 'Knowledge' })}
          </span>
        )}
      </div>

      {/* Description — fixed 2-line clamp so cards stay even-height */}
      <div
        className='mt-10px text-12px leading-18px text-[var(--color-text-3)]'
        style={{
          display: '-webkit-box',
          WebkitLineClamp: 2,
          WebkitBoxOrient: 'vertical',
          overflow: 'hidden',
        }}
      >
        {description || t('settings.presetNoDescription', { defaultValue: 'No description provided.' })}
      </div>

      {/* Tag chips — static pills resolved from the vocabulary */}
      {visibleTags.length > 0 && (
        <div className='mt-12px flex flex-wrap items-center gap-6px'>
          {visibleTags.map((tag) => (
            <span
              key={tag.preset_tag_id}
              className={[
                'inline-flex items-center rounded-[12px] px-8px py-1px text-11px leading-16px',
                'bg-[var(--color-fill-3)] text-[var(--color-text-2)]',
              ].join(' ')}
            >
              {tag.label_i18n?.[localeKey] || tag.label}
            </span>
          ))}
          {overflowCount > 0 && (
            <span className='inline-flex items-center rounded-[12px] px-7px py-1px text-11px leading-16px text-[var(--color-text-3)]'>
              +{overflowCount}
            </span>
          )}
        </div>
      )}

      {/* Hover footer — quiet action links, revealed on card hover */}
      <div
        className='mt-auto pt-12px flex min-h-36px items-center justify-end gap-12px opacity-0 group-hover:opacity-100 transition-opacity duration-180'
        onClick={(e) => e.stopPropagation()}
      >
        <span
          role='button'
          tabIndex={0}
          data-testid={`btn-duplicate-${preset.preset_id}`}
          onClick={() => onDuplicate(preset)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              onDuplicate(preset);
            }
          }}
          className='inline-flex items-center gap-4px leading-none text-12px text-[var(--color-text-3)] cursor-pointer hover:text-[var(--color-text-1)] transition-colors'
        >
          <Copy theme='outline' size={13} strokeWidth={3} />
          {t('settings.duplicatePreset', { defaultValue: 'Duplicate' })}
        </span>
        <span
          role='button'
          tabIndex={0}
          data-testid={`btn-edit-${preset.preset_id}`}
          onClick={() => onEdit(preset)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              onEdit(preset);
            }
          }}
          className='inline-flex items-center gap-4px leading-none text-12px text-[var(--color-text-2)] cursor-pointer hover:text-[var(--color-text-1)] transition-colors'
        >
          <SettingOne theme='outline' size={13} strokeWidth={3} />
          {t('settings.editPreset', { defaultValue: 'Preset Details' })}
        </span>
      </div>
    </div>
  );
};

export default PresetCard;
