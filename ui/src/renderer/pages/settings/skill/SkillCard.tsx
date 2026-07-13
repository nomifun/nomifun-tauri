/**
 * SkillCard — A grid item for the Skills Hub. Mirrors the PresetCard visual
 * language (rounded-16px bordered surface on bg-2, soft hover lift, fixed 2-line
 * description clamp, resolved tag-chip row capped at MAX_VISIBLE_TAGS + "+N", and
 * a hover-revealed action footer) but is tuned for skills:
 *   - a deterministic letter avatar (shared getAvatarColorClass), or a Lightning
 *     glyph for auto-injected skills
 *   - a source badge: Built-in / Custom / Extension / Auto-injected
 *   - NO enable switch (skills aren't toggled here)
 *   - hover footer: Edit Tags (every source) + Delete (custom only)
 * The whole card is clickable → onOpenDetails. Tag editing stays an explicit
 * footer action so inspecting a skill never mutates its organization.
 *
 * Theme variables only (the avatar hex palette is the documented exception);
 * `<div onClick>` for clickables (no <button>, to dodge the WebView2 black box).
 */
import type { PresetTag } from '@/common/types/agent/presetTypes';
import type { SkillInfo } from '@/renderer/pages/settings/PresetSettings/types';
import { resolveSkillDisplay } from './skillDisplay';
import { getAvatarColorClass, normalizeTestId } from './skillPresentation';
import { Tag } from '@arco-design/web-react';
import { Delete, Lightning, SettingOne } from '@icon-park/react';
import React from 'react';
import { useTranslation } from 'react-i18next';

type SkillCardProps = {
  skill: SkillInfo;
  /** Shared preset tag vocabulary, for resolving tag keys → labels. */
  tagByKey: Map<string, PresetTag>;
  localeKey: string;
  /** True when the skill name is in the built-in auto-inject set (parent-supplied). */
  isAutoInjected: boolean;
  onOpenDetails: (skill: SkillInfo) => void;
  onEditTags: (skill: SkillInfo) => void;
  onDelete: (skill: SkillInfo) => void;
  highlighted?: boolean;
  cardRef?: (el: HTMLDivElement | null) => void;
};

const MAX_VISIBLE_TAGS = 4;

/** Source badge — one quiet pill per source, color-coded by semantic. */
const SourceBadge: React.FC<{ skill: SkillInfo; isAutoInjected: boolean }> = ({ skill, isAutoInjected }) => {
  const { t } = useTranslation();

  if (isAutoInjected) {
    return (
      <Tag
        size='small'
        bordered={false}
        className='!flex-shrink-0 !text-10px !leading-14px !px-6px !py-0 !rounded-6px !bg-[rgba(var(--success-6),0.1)] !text-[rgb(var(--success-6))]'
      >
        {t('settings.skillsHub.sourceAuto', { defaultValue: 'Auto' })}
      </Tag>
    );
  }
  if (skill.source === 'custom') {
    return (
      <Tag
        size='small'
        bordered={false}
        className='!flex-shrink-0 !text-10px !leading-14px !px-6px !py-0 !rounded-6px !bg-[rgba(var(--orange-6),0.1)] !text-[rgb(var(--orange-6))]'
      >
        {t('settings.skillsHub.custom', { defaultValue: 'Custom' })}
      </Tag>
    );
  }
  if (skill.source === 'extension') {
    return (
      <Tag
        size='small'
        bordered={false}
        className='!flex-shrink-0 !text-10px !leading-14px !px-6px !py-0 !rounded-6px !bg-fill-2 !text-t-secondary'
      >
        {t('settings.skillsHub.sourceExtension', { defaultValue: 'Extension' })}
      </Tag>
    );
  }
  return (
    <Tag
      size='small'
      bordered={false}
      className='!flex-shrink-0 !text-10px !leading-14px !px-6px !py-0 !rounded-6px !bg-primary-1 !text-primary-6'
    >
      {t('settings.skillsHub.builtin', { defaultValue: 'Built-in' })}
    </Tag>
  );
};

const SkillCard: React.FC<SkillCardProps> = ({
  skill,
  tagByKey,
  localeKey,
  isAutoInjected,
  onOpenDetails,
  onEditTags,
  onDelete,
  highlighted = false,
  cardRef,
}) => {
  const { t } = useTranslation();
  const testId = normalizeTestId(skill.name);
  const display = resolveSkillDisplay(skill, localeKey);

  // Resolve tag keys → labels via the shared vocabulary; drop unknown keys.
  const resolvedTags = [...(skill.audience_tags ?? []), ...(skill.scenario_tags ?? [])]
    .map((key) => tagByKey.get(key))
    .filter((tag): tag is PresetTag => Boolean(tag));
  const visibleTags = resolvedTags.slice(0, MAX_VISIBLE_TAGS);
  const overflowCount = resolvedTags.length - visibleTags.length;

  const canDelete = skill.source === 'custom';

  return (
    <div
      ref={cardRef}
      data-testid={`skill-card-${testId}`}
      onClick={() => onOpenDetails(skill)}
      role='button'
      tabIndex={0}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          onOpenDetails(skill);
        }
      }}
      className={[
        'group relative flex flex-col rounded-16px border border-solid p-14px pb-42px cursor-pointer outline-none',
        'transition-all duration-180',
        highlighted
          ? 'border-[rgb(var(--primary-5))] bg-[var(--color-primary-light-1)] shadow-[0_0_0_3px_rgba(var(--primary-6),0.12)]'
          : 'border-[var(--color-border-2)] bg-[var(--color-bg-2)] hover:border-[var(--color-primary-light-4)] hover:shadow-[0_4px_16px_rgba(0,0,0,0.06)]',
      ].join(' ')}
    >
      {/* Header: avatar + name/badge */}
      <div className='flex items-start gap-10px'>
        {isAutoInjected ? (
          <div className='flex-shrink-0 w-36px h-36px rounded-10px flex items-center justify-center bg-[rgba(var(--success-6),0.1)] shadow-sm'>
            <Lightning theme='filled' size={18} fill='rgb(var(--success-6))' />
          </div>
        ) : (
          <div
            className={`flex-shrink-0 w-36px h-36px rounded-10px flex items-center justify-center font-bold text-15px shadow-sm uppercase ${getAvatarColorClass(skill.name)}`}
          >
            {skill.name.charAt(0).toUpperCase()}
          </div>
        )}
        <div className='min-w-0 flex-1 pt-2px'>
          <div className='flex items-center gap-6px min-w-0 flex-wrap'>
            <span
              className='truncate max-w-full text-14px font-medium leading-20px text-[var(--color-text-1)]'
              title={display.name}
            >
              {display.name}
            </span>
            <SourceBadge skill={skill} isAutoInjected={isAutoInjected} />
          </div>
        </div>
      </div>

      {/* Description — fixed 2-line clamp so cards stay even-height */}
      <div
        className='mt-10px text-12px leading-18px text-[var(--color-text-3)] min-h-[36px]'
        title={display.description || undefined}
        style={{
          display: '-webkit-box',
          WebkitLineClamp: 2,
          WebkitBoxOrient: 'vertical',
          overflow: 'hidden',
        }}
      >
        {display.description || t('settings.skillsHub.noDescription', { defaultValue: 'No description provided.' })}
      </div>

      {/* Tag chips — static pills resolved from the shared vocabulary */}
      {visibleTags.length > 0 && (
        <div className='mt-12px flex flex-wrap items-center gap-6px'>
          {visibleTags.map((tag) => (
            <span
              key={tag.key}
              className={[
                'inline-flex items-center rounded-[12px] px-8px py-1px text-11px leading-16px',
                'bg-[var(--color-fill-2)] text-[var(--color-text-2)] border border-solid border-[var(--color-border-2)]',
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
        className='absolute bottom-14px right-14px flex items-center justify-end gap-14px opacity-0 group-hover:opacity-100 transition-opacity duration-180'
        onClick={(e) => e.stopPropagation()}
      >
        {canDelete && (
          <span
            role='button'
            tabIndex={0}
            data-testid={`btn-delete-${testId}`}
            onClick={() => onDelete(skill)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                e.stopPropagation();
                onDelete(skill);
              }
            }}
            className='inline-flex items-center gap-4px text-12px text-[var(--color-text-3)] cursor-pointer hover:text-[rgb(var(--danger-6))] transition-colors'
          >
            <Delete theme='outline' size={13} strokeWidth={3} />
            {t('common.delete', { defaultValue: 'Delete' })}
          </span>
        )}
        <span
          role='button'
          tabIndex={0}
          data-testid={`btn-edit-tags-${testId}`}
          onClick={() => onEditTags(skill)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              e.stopPropagation();
              onEditTags(skill);
            }
          }}
          className='inline-flex items-center gap-4px text-12px leading-none text-[var(--color-text-3)] cursor-pointer hover:text-[var(--color-text-2)] transition-colors'
        >
          <SettingOne theme='outline' size={13} strokeWidth={3} fill='currentColor' className='relative top-px shrink-0' />
          <span className='leading-none'>{t('settings.skillsHub.editTags', { defaultValue: 'Edit Tags' })}</span>
        </span>
      </div>
    </div>
  );
};

export default SkillCard;
