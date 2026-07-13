/**
 * AssistantCard — A grid item for the assistant list. Mirrors the AgentCard
 * visual language (rounded-16px bordered surface on bg-2, soft hover) but is
 * richer: avatar + name + source badge + enable Switch in the header, a 2-line
 * description clamp, a resolved tag-chip row, and a hover-revealed action
 * footer (Duplicate / Edit). The whole card is clickable → onEdit.
 *
 * Theme variables only; `<div onClick>` for clickables (no <button>).
 */
import type { AssistantTag } from '@/common/types/agent/assistantTypes';
import type { AssistantListItem } from './types';
import AssistantAvatar from './AssistantAvatar';
import { Switch, Tag } from '@arco-design/web-react';
import { Copy, SettingOne } from '@icon-park/react';
import React from 'react';
import { useTranslation } from 'react-i18next';

type AssistantCardProps = {
  assistant: AssistantListItem;
  localeKey: string;
  avatarImageMap: Record<string, string>;
  tagByKey: Map<string, AssistantTag>;
  isExtensionAssistant: (assistant: AssistantListItem | null | undefined) => boolean;
  onEdit: (assistant: AssistantListItem) => void;
  onDuplicate: (assistant: AssistantListItem) => void;
  onToggleEnabled: (assistant: AssistantListItem, checked: boolean) => void;
  highlighted?: boolean;
  cardRef?: (el: HTMLDivElement | null) => void;
};

const MAX_VISIBLE_TAGS = 4;

const AssistantCard: React.FC<AssistantCardProps> = ({
  assistant,
  localeKey,
  avatarImageMap,
  tagByKey,
  isExtensionAssistant,
  onEdit,
  onDuplicate,
  onToggleEnabled,
  highlighted = false,
  cardRef,
}) => {
  const { t } = useTranslation();
  const assistantIsExtension = isExtensionAssistant(assistant);
  const name = assistant.name_i18n?.[localeKey] || assistant.name;
  const description = assistant.description_i18n?.[localeKey] || assistant.description || '';

  // Resolve tag keys → labels via the merged vocabulary. Unknown keys (e.g. a
  // deleted tag still lingering on a row) are silently dropped.
  const resolvedTags = [...(assistant.audience_tags ?? []), ...(assistant.scenario_tags ?? [])]
    .map((key) => tagByKey.get(key))
    .filter((tag): tag is AssistantTag => Boolean(tag));
  const visibleTags = resolvedTags.slice(0, MAX_VISIBLE_TAGS);
  const overflowCount = resolvedTags.length - visibleTags.length;

  const isCustom = assistant.source === 'user';

  return (
    <div
      ref={cardRef}
      data-testid={`assistant-card-${assistant.id}`}
      onClick={() => onEdit(assistant)}
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
        <AssistantAvatar assistant={assistant} size={36} avatarImageMap={avatarImageMap} />
        <div className='min-w-0 flex-1 pt-1px'>
          <div className='flex items-center gap-6px min-w-0'>
            <span className='truncate text-14px font-medium leading-20px text-[var(--color-text-1)]'>{name}</span>
            {isCustom && (
              <Tag
                size='small'
                bordered={false}
                className='!flex-shrink-0 !text-10px !leading-14px !px-6px !py-0 !rounded-6px !bg-primary-1 !text-primary-6'
              >
                {t('settings.assistantSourceCustom', { defaultValue: 'Custom' })}
              </Tag>
            )}
            {assistantIsExtension && (
              <Tag
                size='small'
                bordered={false}
                className='!flex-shrink-0 !text-10px !leading-14px !px-6px !py-0 !rounded-6px !bg-fill-2 !text-t-secondary'
              >
                {t('settings.assistantSourceExtension', { defaultValue: 'Extension' })}
              </Tag>
            )}
          </div>
        </div>
        <div className='flex-shrink-0 -mt-1px' onClick={(e) => e.stopPropagation()}>
          <Switch
            size='small'
            data-testid={`switch-enabled-${assistant.id}`}
            checked={assistantIsExtension ? true : assistant.enabled !== false}
            disabled={assistantIsExtension}
            onChange={(checked) => onToggleEnabled(assistant, checked)}
          />
        </div>
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
        {description || t('settings.assistantNoDescription', { defaultValue: 'No description provided.' })}
      </div>

      {/* Tag chips — static pills resolved from the vocabulary */}
      {visibleTags.length > 0 && (
        <div className='mt-12px flex flex-wrap items-center gap-6px'>
          {visibleTags.map((tag) => (
            <span
              key={tag.key}
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
          data-testid={`btn-duplicate-${assistant.id}`}
          onClick={() => onDuplicate(assistant)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              onDuplicate(assistant);
            }
          }}
          className='inline-flex items-center gap-4px leading-none text-12px text-[var(--color-text-3)] cursor-pointer hover:text-[var(--color-text-1)] transition-colors'
        >
          <Copy theme='outline' size={13} strokeWidth={3} />
          {t('settings.duplicateAssistant', { defaultValue: 'Duplicate' })}
        </span>
        <span
          role='button'
          tabIndex={0}
          data-testid={`btn-edit-${assistant.id}`}
          onClick={() => onEdit(assistant)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              onEdit(assistant);
            }
          }}
          className='inline-flex items-center gap-4px leading-none text-12px text-[var(--color-text-2)] cursor-pointer hover:text-[var(--color-text-1)] transition-colors'
        >
          <SettingOne theme='outline' size={13} strokeWidth={3} />
          {t('settings.editAssistant', { defaultValue: 'Assistant Details' })}
        </span>
      </div>
    </div>
  );
};

export default AssistantCard;
