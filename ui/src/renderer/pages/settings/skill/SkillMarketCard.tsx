import type { AssistantTag } from '@/common/types/agent/assistantTypes';
import type { ISkillMarketItem } from '@/common/adapter/ipcBridge';
import { getAvatarColorClass, normalizeTestId } from './skillPresentation';
import { translateMarketDescription } from './skillMarket';
import { Button, Tag } from '@arco-design/web-react';
import { Plus } from '@icon-park/react';
import React from 'react';
import { useTranslation } from 'react-i18next';

type SkillMarketCardProps = {
  item: ISkillMarketItem;
  tagByKey: Map<string, AssistantTag>;
  localeKey: string;
  onAdd: (item: ISkillMarketItem) => void;
};

const MAX_VISIBLE_TAGS = 4;

const resolveTagLabel = (tag: AssistantTag, localeKey: string): string => tag.label_i18n?.[localeKey] || tag.label;

const MarketSourceBadge: React.FC<{ source: ISkillMarketItem['source'] }> = ({ source }) => {
  const label = source === 'clawhub' ? 'ClawHub' : 'Skills.sh';
  const className =
    source === 'clawhub'
      ? '!bg-primary-1 !text-primary-6'
      : '!bg-[rgba(var(--success-6),0.1)] !text-[rgb(var(--success-6))]';

  return (
    <Tag
      size='small'
      bordered={false}
      className={`!flex-shrink-0 !text-10px !leading-14px !px-6px !py-0 !rounded-6px ${className}`}
    >
      {label}
    </Tag>
  );
};

const SkillMarketCard: React.FC<SkillMarketCardProps> = ({ item, tagByKey, localeKey, onAdd }) => {
  const { t } = useTranslation();
  const testId = normalizeTestId(item.id);
  const resolvedTags = [...(item.audience_tags ?? []), ...(item.scenario_tags ?? [])]
    .map((key) => tagByKey.get(key))
    .filter((tag): tag is AssistantTag => Boolean(tag));
  const rawTags = (item.tags ?? []).filter((tag) => !tagByKey.has(tag));
  const visibleResolvedTags = resolvedTags.slice(0, MAX_VISIBLE_TAGS);
  const visibleRawTags = resolvedTags.length === 0 ? rawTags.slice(0, MAX_VISIBLE_TAGS) : [];
  const totalTagCount = resolvedTags.length > 0 ? resolvedTags.length : rawTags.length;
  const overflowCount = Math.max(0, totalTagCount - MAX_VISIBLE_TAGS);
  const description = translateMarketDescription(item.description, item, localeKey);

  return (
    <div
      data-testid={`skill-market-card-${testId}`}
      className={[
        'group relative flex flex-col rounded-16px border border-solid p-14px outline-none',
        'transition-all duration-180',
        'border-[var(--color-border-2)] bg-[var(--color-bg-2)] hover:border-[var(--color-primary-light-4)] hover:shadow-[0_4px_16px_rgba(0,0,0,0.06)]',
      ].join(' ')}
    >
      <Button
        size='mini'
        type='primary'
        data-testid={`btn-add-market-skill-${testId}`}
        className='!absolute !right-12px !top-12px !rounded-[100px] !h-26px !px-10px !text-12px'
        icon={<Plus theme='outline' size={12} strokeWidth={3} />}
        onClick={() => onAdd(item)}
      >
        {t('common.add', { defaultValue: 'Add' })}
      </Button>

      <div className='flex items-start gap-10px pr-68px'>
        <div
          className={`flex-shrink-0 w-36px h-36px rounded-10px flex items-center justify-center font-bold text-13px shadow-sm ${getAvatarColorClass(item.name)}`}
          title={`#${item.rank || '-'}`}
        >
          {item.rank ? `#${item.rank}` : item.name.charAt(0).toUpperCase()}
        </div>
        <div className='min-w-0 flex-1 pt-2px'>
          <div className='flex items-center gap-6px min-w-0 flex-wrap'>
            <span
              className='truncate max-w-full text-14px font-medium leading-20px text-[var(--color-text-1)]'
              title={item.name}
            >
              {item.name}
            </span>
            <MarketSourceBadge source={item.source} />
          </div>
          {item.stats && <div className='mt-2px text-11px text-[var(--color-text-3)] truncate'>{item.stats}</div>}
        </div>
      </div>

      <div
        className='mt-10px text-12px leading-18px text-[var(--color-text-3)] min-h-[36px]'
        title={item.description || undefined}
        style={{
          display: '-webkit-box',
          WebkitLineClamp: 2,
          WebkitBoxOrient: 'vertical',
          overflow: 'hidden',
        }}
      >
        {description || t('settings.skillsMarket.noDescription', { defaultValue: '暂无描述。' })}
      </div>

      {(visibleResolvedTags.length > 0 || visibleRawTags.length > 0) && (
        <div className='mt-12px flex flex-wrap items-center gap-6px'>
          {visibleResolvedTags.map((tag) => (
            <span
              key={tag.key}
              className='inline-flex items-center rounded-[12px] px-8px py-1px text-11px leading-16px bg-[var(--color-fill-2)] text-[var(--color-text-2)] border border-solid border-[var(--color-border-2)]'
            >
              {resolveTagLabel(tag, localeKey)}
            </span>
          ))}
          {visibleRawTags.map((tag) => (
            <span
              key={tag}
              className='inline-flex items-center rounded-[12px] px-8px py-1px text-11px leading-16px bg-[var(--color-fill-2)] text-[var(--color-text-2)] border border-solid border-[var(--color-border-2)]'
            >
              {tag}
            </span>
          ))}
          {overflowCount > 0 && (
            <span className='inline-flex items-center rounded-[12px] px-7px py-1px text-11px leading-16px text-[var(--color-text-3)]'>
              +{overflowCount}
            </span>
          )}
        </div>
      )}

      <div className='mt-12px pt-10px flex min-w-0 items-center justify-between gap-10px border-t border-solid border-[var(--color-border-1)]'>
        <span className='truncate text-11px text-[var(--color-text-3)] font-mono' title={item.install_command}>
          {item.install_command}
        </span>
      </div>
    </div>
  );
};

export default SkillMarketCard;
