/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * KnowledgeCard — A grid item for the knowledge base list.
 * Mirrors AssistantCard visual language (rounded-16px bordered surface, soft hover)
 * with knowledge-specific additions: kind icon + badge, status tags, user tag chips,
 * meta row, pending-inbox badge, and hover-revealed actions.
 *
 * Theme variables only; `<div onClick>` for clickables (no <button>).
 */
import React from 'react';
import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';
import { Delete, Earth, EditTwo, FolderOpen, LinkOne, SettingOne } from '@icon-park/react';
import type { IKnowledgeBase, IKnowledgeTag } from '@/common/adapter/ipcBridge';
import { formatSize } from './useKnowledge';

// ─── Props ────────────────────────────────────────────────────────────────────

export interface KnowledgeCardProps {
  base: IKnowledgeBase;
  /** Map of tag key → IKnowledgeTag, for resolving base.tags to label + color. */
  tagMap?: Record<string, IKnowledgeTag>;
  onOpen?: (base: IKnowledgeBase) => void;
  onEdit?: (base: IKnowledgeBase) => void;
  onDelete?: (base: IKnowledgeBase, e: React.MouseEvent) => void;
}

// ─── Kind → theme color mapping ───────────────────────────────────────────────

type KindConfig = {
  label: string;
  /** UnoCSS bg class (translucent) */
  bgClass: string;
  /** UnoCSS text class */
  textClass: string;
  /** UnoCSS border class */
  borderClass: string;
  /** Icon bg/border CSS vars for the round icon container */
  iconBg: string;
  iconBorder: string;
  iconColor: string;
};

/**
 * Per-kind badge + icon styling. Uses theme semantic colors:
 * - blank = neutral/gray (fill-2 / text-2)
 * - local = primary (blue)
 * - web = success (green)
 * - feishu = warning (orange)
 */
function getKindConfig(kind: IKnowledgeBase['kind'], t: TFunction): KindConfig {
  switch (kind) {
    case 'local':
      return {
        label: t('knowledge.card.kindLocal', { defaultValue: '本地文件夹' }),
        bgClass: 'bg-[rgba(var(--primary-6),0.1)]',
        textClass: 'text-[rgb(var(--primary-5))]',
        borderClass: 'border-[rgba(var(--primary-6),0.3)]',
        iconBg: 'rgba(var(--primary-6),0.1)',
        iconBorder: 'rgba(var(--primary-6),0.3)',
        iconColor: 'rgb(var(--primary-5))',
      };
    case 'web':
      return {
        label: t('knowledge.card.kindWeb', { defaultValue: '网页' }),
        bgClass: 'bg-[rgba(var(--success-6),0.1)]',
        textClass: 'text-[rgb(var(--success-5))]',
        borderClass: 'border-[rgba(var(--success-6),0.3)]',
        iconBg: 'rgba(var(--success-6),0.1)',
        iconBorder: 'rgba(var(--success-6),0.3)',
        iconColor: 'rgb(var(--success-5))',
      };
    case 'feishu':
      return {
        label: t('knowledge.card.kindFeishu', { defaultValue: '飞书' }),
        bgClass: 'bg-[rgba(var(--warning-6),0.12)]',
        textClass: 'text-[rgb(var(--warning-5))]',
        borderClass: 'border-[rgba(var(--warning-6),0.3)]',
        iconBg: 'rgba(var(--warning-6),0.12)',
        iconBorder: 'rgba(var(--warning-6),0.3)',
        iconColor: 'rgb(var(--warning-5))',
      };
    case 'blank':
    default:
      return {
        label: t('knowledge.card.kindBlank', { defaultValue: '空白' }),
        bgClass: 'bg-fill-2',
        textClass: 'text-[var(--color-text-2)]',
        borderClass: 'border-[var(--color-border-2)]',
        iconBg: 'var(--color-fill-2)',
        iconBorder: 'var(--color-border-2)',
        iconColor: 'var(--color-text-2)',
      };
  }
}

// ─── Sub-components ───────────────────────────────────────────────────────────

/** Kind icon in a rounded square container. */
function KindIcon({ kind, config }: { kind: IKnowledgeBase['kind']; config: KindConfig }) {
  const iconProps = { theme: 'outline' as const, size: 20, strokeWidth: 3 };
  return (
    <div
      className='w-42px h-42px rounded-12px flex-none grid place-items-center border border-solid'
      style={{
        background: config.iconBg,
        borderColor: config.iconBorder,
        color: config.iconColor,
      }}
    >
      {kind === 'local' && <FolderOpen {...iconProps} />}
      {kind === 'web' && <Earth {...iconProps} />}
      {kind === 'feishu' && <SettingOne {...iconProps} />}
      {kind === 'blank' && <EditTwo {...iconProps} />}
    </div>
  );
}

/** Source-mode status badges (live/snapshot/sync interval). */
function StatusBadges({
  base,
  t,
}: {
  base: IKnowledgeBase;
  t: TFunction;
}) {
  const badges: React.ReactNode[] = [];

  if (!base.root_exists) {
    badges.push(
      <span
        key='root-missing'
        className='knowledge-card-root-missing inline-flex items-center rounded-6px px-8px py-2px text-10px font-600 border border-solid border-[rgba(var(--danger-6),0.35)] text-[rgb(var(--danger-6))] bg-[rgba(var(--danger-6),0.08)]'
      >
        {t('knowledge.card.rootMissing', { defaultValue: '目录不可用' })}
      </span>
    );
  }

  if (base.source) {
    if (base.source.mode === 'live') {
      badges.push(
        <span
          key='live'
          className='inline-flex items-center rounded-6px px-8px py-2px text-10px font-600 border border-solid border-[rgba(var(--success-6),0.4)] text-[rgb(var(--success-5))] bg-transparent'
        >
          {t('knowledge.card.modeLive', { defaultValue: '实时' })}
        </span>
      );
    } else if (base.source.mode === 'snapshot') {
      badges.push(
        <span
          key='snapshot'
          className='inline-flex items-center rounded-6px px-8px py-2px text-10px font-600 border border-solid border-[var(--color-border-2)] text-[var(--color-text-2)] bg-fill-2'
        >
          {t('knowledge.card.modeSnapshot', { defaultValue: '快照' })}
        </span>
      );
    }

    // Feishu sync interval badge
    if (base.source.sync?.intervalMinutes) {
      const interval = base.source.sync.intervalMinutes;
      let label: string;
      if (interval <= 60) {
        label = t('knowledge.card.syncHourly', { defaultValue: '每小时同步' });
      } else if (interval <= 1440) {
        label = t('knowledge.card.syncDaily', { defaultValue: '每天同步' });
      } else {
        label = t('knowledge.card.syncWeekly', { defaultValue: '每周同步' });
      }
      badges.push(
        <span
          key='sync'
          className='inline-flex items-center rounded-6px px-8px py-2px text-10px font-600 border border-solid border-[var(--color-border-2)] text-[var(--color-text-2)] bg-fill-2'
        >
          {label}
        </span>
      );
    }
  }

  return badges.length > 0 ? <>{badges}</> : null;
}

/** User tag chips row with colored dots. */
function TagChips({
  tags,
  tagMap,
}: {
  tags: string[];
  tagMap?: Record<string, IKnowledgeTag>;
}) {
  if (!tags.length || !tagMap) return null;

  const resolved = tags
    .map((key) => tagMap[key])
    .filter((t): t is IKnowledgeTag => Boolean(t));

  if (!resolved.length) return null;

  return (
    <div className='flex flex-wrap items-center gap-6px'>
      {resolved.map((tag) => (
        <div
          key={tag.key}
          className='inline-flex items-center gap-5px text-11px text-[var(--color-text-2)] bg-[var(--color-fill-2)] border border-solid border-[var(--color-border-2)] rounded-6px px-8px py-2px'
        >
          {tag.color && (
            <i
              className='w-6px h-6px rounded-full flex-none'
              style={{ background: tag.color }}
            />
          )}
          {tag.label}
        </div>
      ))}
    </div>
  );
}

/** Relative time format (simple). */
function formatRelativeTime(epochMs: number, t: TFunction): string {
  const now = Date.now();
  const diff = now - epochMs;
  const seconds = Math.floor(diff / 1000);
  const minutes = Math.floor(seconds / 60);
  const hours = Math.floor(minutes / 60);
  const days = Math.floor(hours / 24);

  if (seconds < 60) return t('knowledge.card.timeJustNow', { defaultValue: '刚刚' });
  if (minutes < 60) return t('knowledge.card.timeMinutesAgo', { count: minutes, defaultValue: '{{count}} 分钟前' });
  if (hours < 24) return t('knowledge.card.timeHoursAgo', { count: hours, defaultValue: '{{count}} 小时前' });
  if (days === 1) return t('knowledge.card.timeYesterday', { defaultValue: '昨天' });
  if (days < 7) return t('knowledge.card.timeDaysAgo', { count: days, defaultValue: '{{count}} 天前' });
  return t('knowledge.card.timeWeeksAgo', { defaultValue: '上周' });
}

// ─── Main Component ───────────────────────────────────────────────────────────

export const KnowledgeCard: React.FC<KnowledgeCardProps> = ({
  base,
  tagMap,
  onOpen,
  onEdit,
  onDelete,
}) => {
  const { t } = useTranslation();
  const kindConfig = getKindConfig(base.kind, t);
  const metaItems = [
    base.file_count > 0 ? t('knowledge.card.fileCount', { count: base.file_count, defaultValue: '{{count}} 篇' }) : null,
    base.total_size > 0 ? formatSize(base.total_size) : null,
    formatRelativeTime(base.updated_at, t),
  ].filter((item): item is string => Boolean(item));

  return (
    <div
      className={[
        'group relative flex flex-col gap-11px rounded-16px border border-solid',
        'border-[var(--color-border-2)] bg-[var(--color-bg-2)] p-18px box-border cursor-pointer',
        'min-h-188px',
        'transition-all duration-160',
        'hover:border-[var(--color-border-3)] hover:shadow-[0_14px_38px_rgba(0,0,0,0.15)] hover:-translate-y-2px',
      ].join(' ')}
      onClick={() => onOpen?.(base)}
    >
      {/* Pending inbox badge (top-right) */}
      {base.pending_inbox > 0 && (
        <span
          className={[
            'absolute top-14px right-14px inline-flex items-center gap-5px',
            'rounded-full px-9px py-3px',
            'text-11px font-600',
            'bg-[rgba(var(--warning-6),0.14)] text-[rgb(var(--warning-5))] border border-solid border-[rgba(var(--warning-6),0.4)]',
          ].join(' ')}
        >
          <i className='w-6px h-6px rounded-full bg-[rgb(var(--warning-6))] shadow-[0_0_8px_rgb(var(--warning-6))]' />
          {t('knowledge.card.pending', { count: base.pending_inbox, defaultValue: '{{count}} 待审' })}
        </span>
      )}

      {/* Header: icon + name + badges */}
      <div className='flex items-center gap-12px'>
        <KindIcon kind={base.kind} config={kindConfig} />
        <div className='min-w-0 flex-1'>
          <div className='text-15px font-700 leading-[1.3] text-[var(--color-text-1)] truncate'>
            {base.name}
          </div>
          <div className='flex flex-wrap gap-6px mt-4px'>
            {/* Kind badge */}
            <span
              className={[
                'inline-flex items-center rounded-6px px-8px py-2px text-10px font-600 border border-solid',
                kindConfig.bgClass,
                kindConfig.textClass,
                kindConfig.borderClass,
              ].join(' ')}
            >
              {kindConfig.label}
            </span>
            {/* Status badges */}
            <StatusBadges base={base} t={t} />
          </div>
        </div>
      </div>

      {/* Description (2-line clamp) */}
      <div
        className='text-13px leading-[1.55] text-[var(--color-text-2)] flex-1'
        style={{
          display: '-webkit-box',
          WebkitLineClamp: 2,
          WebkitBoxOrient: 'vertical',
          overflow: 'hidden',
        }}
      >
        {base.description || t('knowledge.card.noDescription', { defaultValue: '暂无描述' })}
      </div>

      {/* User tags row */}
      <TagChips tags={base.tags} tagMap={tagMap} />

      <div className='knowledge-card-footer mt-auto flex min-h-32px items-center gap-10px pt-2px'>
        <div className='knowledge-card-meta flex min-w-0 flex-wrap items-center gap-7px text-12px leading-16px text-[var(--color-text-3)]'>
          {metaItems.map((item, index) => (
            <React.Fragment key={`${item}-${index}`}>
              {index > 0 && <i className='h-3px w-3px rounded-full bg-[var(--color-fill-4)]' aria-hidden='true' />}
              <span className='whitespace-nowrap'>{item}</span>
            </React.Fragment>
          ))}
        </div>

        <div
          className='knowledge-card-actions pointer-events-none ml-auto flex shrink-0 gap-6px opacity-0 transition-opacity duration-150 group-hover:pointer-events-auto group-hover:opacity-100 group-focus-within:pointer-events-auto group-focus-within:opacity-100'
          onClick={(e) => e.stopPropagation()}
        >
          <div
            onClick={() => onOpen?.(base)}
            className={[
              'grid h-30px w-30px place-items-center rounded-8px',
              'border border-solid border-transparent',
              'bg-transparent text-[var(--color-text-3)] cursor-pointer',
              'hover:border-[var(--color-border-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]',
              'transition-colors',
            ].join(' ')}
            title={t('knowledge.card.actionOpen', { defaultValue: '打开' })}
          >
            <LinkOne theme='outline' size={13} strokeWidth={3} />
          </div>
          <div
            onClick={() => onEdit?.(base)}
            className={[
              'grid h-30px w-30px place-items-center rounded-8px',
              'border border-solid border-transparent',
              'bg-transparent text-[var(--color-text-3)] cursor-pointer',
              'hover:border-[var(--color-border-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]',
              'transition-colors',
            ].join(' ')}
            title={t('knowledge.card.actionEdit', { defaultValue: '编辑' })}
          >
            <EditTwo theme='outline' size={13} strokeWidth={3} />
          </div>
          <div
            onClick={(e) => onDelete?.(base, e)}
            className={[
              'grid h-30px w-30px place-items-center rounded-8px',
              'border border-solid border-transparent',
              'bg-transparent text-[var(--color-text-3)] cursor-pointer',
              'hover:border-[rgba(var(--danger-6),0.28)] hover:bg-[rgba(var(--danger-6),0.08)] hover:text-[rgb(var(--danger-6))]',
              'transition-colors',
            ].join(' ')}
            title={t('knowledge.actions.delete', { defaultValue: '删除' })}
          >
            <Delete theme='outline' size={13} strokeWidth={3} />
          </div>
        </div>
      </div>
    </div>
  );
};

export default KnowledgeCard;
