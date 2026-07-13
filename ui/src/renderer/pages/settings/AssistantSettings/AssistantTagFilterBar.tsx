/**
 * AssistantTagFilterBar — Two labelled rows (Audience / Skill Scenario) of
 * multi-select toggle chips, each led by an「All」chip, with a trailing
 * "Manage Tags" chip-button. Hides a row when its tag list is empty.
 *
 * Chips mirror the `.presetAgentTag` pill language: idle = fill-2 surface with
 * a border-2 hairline; active = primary-light-1 surface, primary-6 text,
 * primary-light-3 border. Theme variables only; `<div onClick>` (no <button>).
 */
import type { AssistantTag, AssistantTagDimension } from '@/common/types/agent/assistantTypes';
import type { TagFilterState } from './assistantUtils';
import { SettingTwo } from '@icon-park/react';
import React from 'react';
import { useTranslation } from 'react-i18next';
import filterBarStyles from './AssistantTagFilterBar.module.css';

type AssistantTagFilterBarProps = {
  audienceTags: AssistantTag[];
  scenarioTags: AssistantTag[];
  value: TagFilterState;
  onChange: (next: TagFilterState) => void;
  localeKey: string;
  onManageTags: () => void;
  variant?: 'default' | 'drawer';
  className?: string;
  hideManageTags?: boolean;
};

const resolveTagLabel = (tag: AssistantTag, localeKey: string): string => tag.label_i18n?.[localeKey] || tag.label;

/** Idle/active pill. Active wraps the primary-light triad. */
const FilterChip: React.FC<{
  label: string;
  active: boolean;
  onClick: () => void;
  testId?: string;
  variant?: 'default' | 'drawer';
}> = ({ label, active, onClick, testId, variant = 'default' }) => (
  <div
    role='button'
    tabIndex={0}
    data-testid={testId}
    aria-pressed={active}
    onClick={onClick}
    onKeyDown={(e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        onClick();
      }
    }}
    className={
      variant === 'drawer'
        ? [
            filterBarStyles.drawerFilterChip,
            active ? filterBarStyles.drawerFilterChipActive : '',
          ].filter(Boolean).join(' ')
        : [
            'inline-flex items-center select-none cursor-pointer rounded-[16px] px-12px py-3px text-13px leading-20px',
            'border border-solid transition-all duration-150 whitespace-nowrap',
            active
              ? 'bg-[#151515] text-white border-white font-medium'
              : 'bg-[var(--color-fill-2)] text-[var(--color-text-2)] border-[var(--color-border-2)] hover:bg-[var(--color-fill-3)] hover:text-[var(--color-text-1)]',
          ].join(' ')
    }
  >
    {label}
  </div>
);

const AssistantTagFilterBar: React.FC<AssistantTagFilterBarProps> = ({
  audienceTags,
  scenarioTags,
  value,
  onChange,
  localeKey,
  onManageTags,
  variant = 'default',
  className,
  hideManageTags = false,
}) => {
  const { t } = useTranslation();
  const isDrawer = variant === 'drawer';

  const toggle = (dimension: AssistantTagDimension, key: string) => {
    const current = value[dimension];
    const next = current.includes(key) ? current.filter((k) => k !== key) : [...current, key];
    onChange({ ...value, [dimension]: next });
  };

  const clearDimension = (dimension: AssistantTagDimension) => {
    onChange({ ...value, [dimension]: [] });
  };

  const renderRow = (dimension: AssistantTagDimension, rowLabel: string, tags: AssistantTag[]) => {
    if (tags.length === 0) return null;
    const selected = value[dimension];

    return (
      <div className={isDrawer ? filterBarStyles.drawerFilterRow : 'flex items-start gap-12px'}>
        {/* Left dimension label with accent rail */}
        <div className={isDrawer ? filterBarStyles.drawerFilterLabel : 'flex items-center gap-7px flex-shrink-0 h-26px mt-1px'}>
          <span
            className={isDrawer ? filterBarStyles.drawerFilterRail : 'inline-block w-3px h-12px rounded-[2px] bg-[var(--color-primary-light-3)]'}
            aria-hidden='true'
          />
          <span className={isDrawer ? '' : 'text-12px font-medium text-[var(--color-text-3)] whitespace-nowrap'}>{rowLabel}</span>
        </div>
        <div className={isDrawer ? filterBarStyles.drawerFilterChips : 'flex flex-wrap items-center gap-8px min-w-0'}>
          <FilterChip
            label={t('settings.assistantTagAll', { defaultValue: 'All' })}
            active={selected.length === 0}
            onClick={() => clearDimension(dimension)}
            testId={`tag-chip-${dimension}-all`}
            variant={variant}
          />
          {tags.map((tag) => (
            <FilterChip
              key={tag.key}
              label={resolveTagLabel(tag, localeKey)}
              active={selected.includes(tag.key)}
              onClick={() => toggle(dimension, tag.key)}
              testId={`tag-chip-${dimension}-${tag.key}`}
              variant={variant}
            />
          ))}
        </div>
      </div>
    );
  };

  const hasAudience = audienceTags.length > 0;
  const hasScenario = scenarioTags.length > 0;

  return (
    <div
      className={
        isDrawer
          ? [filterBarStyles.drawerFilterBar, className].filter(Boolean).join(' ')
          : ['flex flex-col gap-12px rounded-16px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] px-16px py-14px', className].filter(Boolean).join(' ')
      }
    >
      <div className='flex items-start justify-between gap-12px'>
        <div className={isDrawer ? filterBarStyles.drawerFilterRows : 'flex flex-col gap-12px min-w-0 flex-1'}>
          {renderRow('audience', t('settings.assistantTagAudience', { defaultValue: 'Audience' }), audienceTags)}
          {renderRow('scenario', t('settings.assistantTagScenario', { defaultValue: 'Skill Scenario' }), scenarioTags)}
          {!hasAudience && !hasScenario && (
            <span className={isDrawer ? filterBarStyles.drawerEmpty : 'text-12px text-[var(--color-text-3)]'}>
              {t('settings.assistantTagEmpty', { defaultValue: 'No tags yet. Create some to organize your assistants.' })}
            </span>
          )}
        </div>
        {/* Manage tags — a quiet chip-button anchored to the top-right */}
        {!hideManageTags && (
          <div
            role='button'
            tabIndex={0}
            data-testid='btn-manage-tags'
            onClick={onManageTags}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                onManageTags();
              }
            }}
            className={
              isDrawer
                ? filterBarStyles.drawerManageChip
                : [
                    'inline-flex items-center gap-5px select-none cursor-pointer rounded-[16px] px-12px py-4px flex-shrink-0',
                    'text-12px font-medium border border-dashed transition-all duration-150',
                    'bg-transparent text-[var(--color-text-3)] border-[var(--color-border-3)]',
                    'hover:text-[rgb(var(--primary-6))] hover:border-[var(--color-primary-light-3)] hover:bg-[var(--color-primary-light-1)]',
                  ].join(' ')
            }
          >
            <SettingTwo theme='outline' size={13} strokeWidth={3} />
            {t('settings.assistantManageTags', { defaultValue: 'Manage Tags' })}
          </div>
        )}
      </div>
    </div>
  );
};

export default AssistantTagFilterBar;
