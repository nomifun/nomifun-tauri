/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * KnowledgeControl — Per-session knowledge-base mounting popover.
 *
 * Trigger button + popover panel are deliberately aligned with the sibling
 * conversation-header controls (AutoWork / IDMM / MultiAgent): a compact
 * `Button size='mini' shape='round'` with an icon + label + tri-state status
 * dot, and a popover whose layout mirrors IdmmControl's design language
 * (icon-chip header + status pill + rounded `bg-fill-1` card sections), instead
 * of the earlier bespoke square icon-button + full-bleed-divider panel.
 *
 * Preserved behaviors from the original implementation:
 * - Three-target resolution: conversation/terminal → workpath, companion → per-profile
 * - Draft mode (Guid page pre-creation binding)
 * - Binding read/write via `POST /api/knowledge/binding/{kind}/{target_id}`
 * - `knowledge.binding-changed` / base-created/updated/deleted WS refresh
 * - `disabledReason` tooltip, `applyNote`, `footer` passthrough
 * - First-time discoverability hint tooltip
 */

import React, { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';
import { Button, Input, Message, Popover, Switch, Tooltip } from '@arco-design/web-react';
import { BookOne } from '@icon-park/react';
import { useNavigate } from 'react-router-dom';
import { ipcBridge } from '@/common';
import type {
  IKnowledgeBase,
  IKnowledgeBinding,
  IKnowledgeTag,
  KnowledgeBindingKind,
  KnowledgeWritebackEagerness,
  KnowledgeWritebackMode,
} from '@/common/adapter/ipcBridge';
import { useConversationHistoryContext } from '@/renderer/hooks/context/ConversationHistoryContext';
import { useTerminalSessions } from '@/renderer/pages/terminal/useTerminalSessions';
import {
  workpathKeyForConversation,
  workpathKeyForTerminal,
} from '@/renderer/pages/conversation/SessionList/utils/sessionWorkpath';
import { useKnowledgeTags } from '@/renderer/pages/knowledge/useKnowledgeTags';
import { CAPABILITY_COLORS } from '@/renderer/components/capability/CapabilityIcon';
import {
  filterKnowledgeBasesByQuery,
  shouldShowKnowledgeBaseSearch,
} from './KnowledgeControl.utils';
import { capabilityHeaderButtonClass, capabilityHeaderButtonStyle } from './CapabilityHeaderButton';

export type KnowledgeTarget =
  | { kind: 'conversation'; id: number }
  | { kind: 'terminal'; id: number }
  | { kind: 'companion'; id: string }
  | { kind: 'workpath'; id: string };

/** Draft (pre-creation) mode: the binding lives in the parent's state and is
 * persisted by the parent once a target exists. */
export type KnowledgeDraft = {
  value: IKnowledgeBinding;
  onChange: (next: IKnowledgeBinding) => void;
};

type KnowledgeControlProps = {
  target?: KnowledgeTarget;
  draft?: KnowledgeDraft;
  disabledReason?: string;
  applyNote?: string;
  footer?: React.ReactNode;
};

export const defaultKnowledgeBinding = (): IKnowledgeBinding => ({
  enabled: false,
  writeback: false,
  writeback_mode: 'staged',
  writeback_eagerness: 'conservative',
  channel_write_enabled: false,
  kb_ids: [],
});

// ─── Shared visual tokens (mirror IdmmControl's panel design language) ───────

/** Rounded card section — identical token to IdmmControl's `sectionClass`. */
const sectionClass =
  'flex flex-col gap-8px rounded-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-1)] px-12px py-10px';
const fieldStackClass = 'min-w-0 flex flex-col gap-4px';
const fieldLabelClass = 'text-[var(--color-text-1)] text-11px font-600 leading-15px';
const subtleInsetClass =
  'rounded-8px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-1)] px-10px py-8px';
/** Tinted surface — mirrors IdmmControl's `watchTone`; `--primary-6` is an RGB triplet. */
const tintBg = (color: string, amount = 12): string =>
  `color-mix(in srgb, rgb(${color}) ${amount}%, var(--color-bg-1))`;
const lineClamp2Style: React.CSSProperties = {
  display: '-webkit-box',
  WebkitBoxOrient: 'vertical',
  WebkitLineClamp: 2,
  overflow: 'hidden',
};

// ─── Kind badge config (mirrors KnowledgeCard getKindConfig) ─────────────────

type KindBadgeStyle = { bgClass: string; textClass: string; borderClass: string };

function getKindBadge(kind: IKnowledgeBase['kind']): KindBadgeStyle {
  switch (kind) {
    case 'local':
      return {
        bgClass: 'bg-[rgba(var(--primary-6),0.1)]',
        textClass: 'text-[rgb(var(--primary-5))]',
        borderClass: 'border-[rgba(var(--primary-6),0.3)]',
      };
    case 'web':
      return {
        bgClass: 'bg-[rgba(var(--success-6),0.1)]',
        textClass: 'text-[rgb(var(--success-5))]',
        borderClass: 'border-[rgba(var(--success-6),0.3)]',
      };
    case 'feishu':
      return {
        bgClass: 'bg-[rgba(var(--warning-6),0.12)]',
        textClass: 'text-[rgb(var(--warning-5))]',
        borderClass: 'border-[rgba(var(--warning-6),0.3)]',
      };
    case 'blank':
    default:
      return {
        bgClass: 'bg-fill-2',
        textClass: 'text-[var(--color-text-2)]',
        borderClass: 'border-[var(--color-border-2)]',
      };
  }
}

function kindLabel(kind: IKnowledgeBase['kind'], t: TFunction): string {
  switch (kind) {
    case 'local':
      return t('knowledge.card.kindLocal', { defaultValue: '本地文件夹' });
    case 'web':
      return t('knowledge.card.kindWeb', { defaultValue: '网页' });
    case 'feishu':
      return t('knowledge.card.kindFeishu', { defaultValue: '飞书' });
    case 'blank':
    default:
      return t('knowledge.card.kindBlank', { defaultValue: '空白' });
  }
}

// ─── Main Component ──────────────────────────────────────────────────────────

const KnowledgeControl: React.FC<KnowledgeControlProps> = ({ target, draft, disabledReason, applyNote, footer }) => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { conversations } = useConversationHistoryContext();
  const { sessions: terminalSessions } = useTerminalSessions();
  const { tags: allTags } = useKnowledgeTags();

  // Build tag key → IKnowledgeTag map
  const tagMap = useMemo(() => {
    const m: Record<string, IKnowledgeTag> = {};
    for (const tag of allTags) m[tag.key] = tag;
    return m;
  }, [allTags]);

  const tagLabelsByKey = useMemo(() => {
    const m: Record<string, string> = {};
    for (const tag of allTags) m[tag.key] = tag.label;
    return m;
  }, [allTags]);

  const kindLabelsByKind = useMemo(
    () => ({
      blank: kindLabel('blank', t),
      feishu: kindLabel('feishu', t),
      local: kindLabel('local', t),
      web: kindLabel('web', t),
    }),
    [t]
  );

  // ─── Target resolution (unchanged logic) ─────────────────────────────────
  const resolved = useMemo((): { kind: KnowledgeBindingKind; id: string } | null => {
    if (!target) return null;
    if (target.kind === 'companion') return { kind: 'companion', id: target.id };
    if (target.kind === 'conversation') {
      const conv = conversations.find((c) => c.id === target.id);
      if (!conv) return null;
      return { kind: 'workpath', id: workpathKeyForConversation(conv.extra as Record<string, unknown>) };
    }
    if (target.kind === 'terminal') {
      const session = terminalSessions.find((s) => s.id === target.id);
      if (!session) return null;
      return { kind: 'workpath', id: workpathKeyForTerminal(session) };
    }
    return { kind: 'workpath', id: target.id };
  }, [target?.kind, target?.id, conversations, terminalSessions]);

  const kind = resolved?.kind;
  const id = resolved?.id;
  const targetUnresolved = !draft && !!target && target.kind !== 'companion' && !resolved;

  // The resolved workpath (for scope display)
  const workpathDisplay = useMemo(() => {
    if (!resolved || resolved.kind === 'companion') return null;
    return resolved.id;
  }, [resolved]);

  // ─── State ────────────────────────────────────────────────────────────────
  const [bases, setBases] = useState<IKnowledgeBase[]>([]);
  const [basesLoaded, setBasesLoaded] = useState(false);
  const [persistedBinding, setPersistedBinding] = useState<IKnowledgeBinding>(defaultKnowledgeBinding);
  const binding = draft ? draft.value : persistedBinding;
  const [searchQuery, setSearchQuery] = useState('');

  // ─── Discoverability hint ─────────────────────────────────────────────────
  const [hintVisible, setHintVisible] = useState(false);
  const dismissHint = () => {
    setHintVisible(false);
    try {
      localStorage.setItem('knowledge.control.hintSeen', '1');
    } catch {
      /* private mode */
    }
  };
  useEffect(() => {
    let seen = false;
    try {
      seen = localStorage.getItem('knowledge.control.hintSeen') === '1';
    } catch {
      /* ignore */
    }
    if (!basesLoaded || seen || disabledReason) return;
    if (bases.length === 0) {
      setHintVisible(true);
      const timer = setTimeout(dismissHint, 8000);
      return () => clearTimeout(timer);
    }
    setHintVisible(false);
    return undefined;
  }, [basesLoaded, bases.length, disabledReason]);

  // ─── Load bases + binding ─────────────────────────────────────────────────
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [list, b] = await Promise.all([
          ipcBridge.knowledge.listBases.invoke(),
          draft || !kind || !id
            ? Promise.resolve(null)
            : ipcBridge.knowledge.getBinding.invoke({ kind, target_id: id }),
        ]);
        if (cancelled) return;
        setBases(list);
        if (b) setPersistedBinding(b);
      } catch {
        /* ignore — keep defaults */
      } finally {
        if (!cancelled) setBasesLoaded(true);
      }
    })();
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [kind, id, !!draft]);

  // Keep base list fresh
  useEffect(() => {
    const reload = () => {
      void ipcBridge.knowledge.listBases
        .invoke()
        .then(setBases)
        .catch(() => {});
    };
    const unsubs = [
      ipcBridge.knowledge.onBaseCreated.on(reload),
      ipcBridge.knowledge.onBaseUpdated.on(reload),
      ipcBridge.knowledge.onBaseDeleted.on(reload),
    ];
    return () => unsubs.forEach((u) => u());
  }, []);

  // ─── Persist ──────────────────────────────────────────────────────────────
  const persist = async (next: IKnowledgeBinding) => {
    if (draft) {
      draft.onChange(next);
      return;
    }
    if (!kind || !id) return;
    setPersistedBinding(next);
    try {
      await ipcBridge.knowledge.setBinding.invoke({ kind, target_id: id, ...next });
      if (next.enabled !== binding.enabled) {
        Message.success(next.enabled ? t('knowledge.control.enabledOk') : t('knowledge.control.disabledOk'));
      }
    } catch (e) {
      Message.error(String(e));
    }
  };

  // ─── Handlers ─────────────────────────────────────────────────────────────
  const handleToggleBase = (baseId: string) => {
    const isSelected = binding.kb_ids.includes(baseId);
    const nextIds = isSelected ? binding.kb_ids.filter((x) => x !== baseId) : [...binding.kb_ids, baseId];
    // Auto-enable when first base selected; auto-disable when last removed
    const nextEnabled = nextIds.length > 0 ? true : false;
    void persist({ ...binding, kb_ids: nextIds, enabled: nextEnabled });
  };

  const handleWritebackToggle = (v: boolean) => {
    void persist({ ...binding, writeback: v });
  };

  const handleWritebackMode = (mode: KnowledgeWritebackMode) => {
    void persist({ ...binding, writeback_mode: mode });
  };

  const handleWritebackEagerness = (eagerness: KnowledgeWritebackEagerness) => {
    void persist({ ...binding, writeback_eagerness: eagerness });
  };

  const mountedCount = binding.enabled ? binding.kb_ids.length : 0;

  // Filter bases by search query
  const filteredBases = useMemo(() => {
    return filterKnowledgeBasesByQuery(bases, searchQuery, tagLabelsByKey, kindLabelsByKind);
  }, [bases, searchQuery, kindLabelsByKind, tagLabelsByKey]);

  // ─── Derived status (shared by trigger button + panel header pill) ─────────
  // Knowledge has no live run-state — the dot is a binary enabled/off marker
  // (primary when mounted, gray otherwise).
  const dotColor = binding.enabled ? CAPABILITY_COLORS.primary : CAPABILITY_COLORS.off;
  const statusText = draft
    ? binding.enabled
      ? t('guid.advanced.draftOn')
      : t('guid.advanced.draftOff')
    : binding.enabled
      ? t('knowledge.control.mounted', { count: mountedCount })
      : t('knowledge.control.off');

  // Compact segmented control (writeback mode / eagerness) — tinted track with a
  // primary active pill, sitting on the section's bg-fill-1 surface.
  const renderSegment = (
    current: string,
    options: Array<{ value: string; label: string }>,
    onPick: (v: string) => void
  ) => (
    <div className='inline-flex w-fit gap-2px rounded-8px bg-fill-2 p-2px'>
      {options.map((o) => {
        const activeSeg = current === o.value;
        return (
          <div
            key={o.value}
            className={[
              'cursor-pointer rounded-6px px-10px py-4px text-11px leading-none',
              activeSeg
                ? 'border border-solid border-[rgba(var(--primary-6),0.28)] bg-[rgba(var(--primary-6),0.12)] text-[rgb(var(--primary-6))] font-600'
                : 'border border-solid border-transparent text-[var(--color-text-1)] hover:bg-[var(--color-fill-3)]',
              targetUnresolved && 'cursor-not-allowed opacity-60',
            ]
              .filter(Boolean)
              .join(' ')}
            onClick={() => !targetUnresolved && onPick(o.value)}
          >
            {o.label}
          </div>
        );
      })}
    </div>
  );

  const writebackModeHint =
    binding.writeback_mode === 'staged'
      ? t('knowledge.control.modeStagedHint')
      : t('knowledge.control.modeDirectHint');
  const writebackEagernessHint =
    binding.writeback_eagerness === 'conservative'
      ? t('knowledge.control.eagernessConservativeHint')
      : t('knowledge.control.eagernessAggressiveHint');

  // One mounted base row.
  const renderBaseRow = (base: IKnowledgeBase) => {
    const isSelected = binding.kb_ids.includes(base.id);
    const rootMissing = !base.root_exists;
    const cannotSelect = rootMissing && !isSelected;
    const badge = getKindBadge(base.kind);
    const baseTags = base.tags.map((tk) => tagMap[tk]).filter((x): x is IKnowledgeTag => !!x);
    const firstTag = baseTags[0];
    return (
      <div
        key={base.id}
        className={[
          'flex items-center gap-9px rounded-8px border border-solid bg-[var(--color-bg-1)] px-8px py-7px cursor-pointer transition-colors',
          isSelected ? 'border-[rgba(var(--primary-6),0.38)]' : 'border-[var(--color-border-2)] hover:bg-fill-2',
          targetUnresolved && 'opacity-50 cursor-not-allowed',
          cannotSelect && 'cursor-not-allowed opacity-65 hover:bg-[var(--color-bg-1)]',
        ]
          .filter(Boolean)
          .join(' ')}
        style={isSelected ? { background: tintBg('var(--primary-6)', 7) } : undefined}
        onClick={() => !targetUnresolved && (!rootMissing || isSelected) && handleToggleBase(base.id)}
      >
        {/* Checkbox */}
        <span
          className={[
            'grid h-17px w-17px flex-none place-items-center rounded-5px border-1.5px border-solid text-10px leading-none',
            isSelected
              ? 'border-[rgba(var(--primary-6),0.48)] bg-[rgba(var(--primary-6),0.12)] text-[rgb(var(--primary-6))]'
              : 'border-[var(--color-text-3)] bg-[var(--color-bg-1)] text-transparent',
          ].join(' ')}
        >
          ✓
        </span>

        {/* Content */}
        <span className='min-w-0 flex-1'>
          <span className='flex items-center gap-6px'>
            <span className='truncate text-13px font-600 text-[var(--color-text-1)]'>{base.name}</span>
            <span
              className={[
                'inline-flex shrink-0 items-center rounded-5px border border-solid px-5px py-1px text-9px font-600',
                badge.bgClass,
                badge.textClass,
                badge.borderClass,
              ].join(' ')}
            >
              {kindLabel(base.kind, t)}
            </span>
            <span className='knowledge-control-base-meta shrink-0 text-11px font-500 text-[var(--color-text-2)]'>
              {rootMissing
                ? t('knowledge.mount.rootMissing', { defaultValue: '目录不可用' })
                : base.kind === 'web'
                  ? t('knowledge.mount.realtime', { defaultValue: '实时' })
                  : t('knowledge.mount.fileCount', { defaultValue: '{{count}} 篇', count: base.file_count })}
            </span>
          </span>
          {rootMissing && (
            <span className='knowledge-control-root-missing mt-2px block text-11px leading-15px text-[rgb(var(--danger-6))]'>
              {t('knowledge.mount.rootMissingHint', {
                defaultValue: '源目录不存在或暂时不可访问，恢复目录后再挂载。',
              })}
            </span>
          )}
          {firstTag && (
            <span className='mt-2px flex items-center gap-3px text-11px text-[var(--color-text-2)]'>
              <span
                className='inline-block h-6px w-6px rounded-full'
                style={{ background: firstTag.color || 'var(--color-text-3)' }}
              />
              {firstTag.label}
            </span>
          )}
        </span>
      </div>
    );
  };

  // ─── Panel content ────────────────────────────────────────────────────────
  const showSearch = shouldShowKnowledgeBaseSearch(bases.length);
  const panel = (
    <div className='box-border flex w-340px max-h-500px flex-col gap-10px overflow-hidden p-12px'>
      {/* Header — identical structure to IdmmControl: icon chip + title on the
          left, status pill on the right, hint below. The earlier "container
          misalignment" was Arco's own popover-shell padding (now zeroed via
          .knowledge-control-popover in arco-override.css, same fix IdmmControl
          already uses), NOT this row's layout. */}
      <div className='flex flex-col gap-6px'>
        <div className='flex items-center justify-between gap-10px'>
          <span className='inline-flex min-w-0 items-center gap-8px'>
            <span
              className='inline-flex h-24px w-24px shrink-0 items-center justify-center rounded-6px'
              style={{ background: tintBg('var(--primary-6)', 10), color: CAPABILITY_COLORS.primary }}
            >
              <BookOne theme='outline' size='15' fill='currentColor' />
            </span>
            <span className='min-w-0 truncate text-t-primary text-13px font-600'>{t('knowledge.control.label')}</span>
          </span>
          <span className='inline-flex shrink-0 items-center gap-5px rounded-full border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-1)] px-7px py-3px text-11px font-500 text-[var(--color-text-1)]'>
            <span className='inline-block h-6px w-6px rounded-full' style={{ backgroundColor: dotColor }} />
            {statusText}
          </span>
        </div>
        <div className='text-[var(--color-text-2)] text-11px leading-16px' style={lineClamp2Style}>
          {t('knowledge.control.hint')}
        </div>
        <div
          className='self-start text-11px font-600 text-[rgb(var(--primary-6))] cursor-pointer hover:underline'
          onClick={() => navigate('/knowledge')}
        >
          {t('knowledge.mount.manage', { defaultValue: '管理知识库 ›' })}
        </div>
      </div>

      {basesLoaded && bases.length === 0 ? (
        // ─── Empty state ───────────────────────────────────────────────────
        <div className='flex flex-col items-center gap-12px px-12px py-22px text-center'>
          <span
            className='inline-flex h-44px w-44px items-center justify-center rounded-12px'
            style={{ background: tintBg('var(--primary-6)', 12), color: CAPABILITY_COLORS.primary }}
          >
            <BookOne theme='outline' size='22' fill='currentColor' />
          </span>
          <p className='m-0 whitespace-pre-line text-12px text-[var(--color-text-2)] leading-17px'>
            {t('knowledge.mount.emptyHint', {
              defaultValue: '你还没有任何知识库。\n知识库能给这个会话补上专属的领域知识。',
            })}
          </p>
          <Button type='primary' size='small' shape='round' onClick={() => navigate('/knowledge')}>
            {t('knowledge.mount.createFirst', { defaultValue: '＋ 新建第一个知识库' })}
          </Button>
        </div>
      ) : (
        <>
          {/* Scope inset + (conditional) search — fixed above the scroll body */}
          {workpathDisplay && (
            <div className={`${subtleInsetClass} text-[var(--color-text-2)] text-11px leading-16px`}>
              {t('knowledge.mount.scope', {
                defaultValue: '作用范围：工作区 {{path}}。同一工作区下的所有会话共享这套挂载设置。',
                path: workpathDisplay,
              })}
            </div>
          )}
          {showSearch && (
            <Input
              size='small'
              allowClear
              className='knowledge-control-search'
              value={searchQuery}
              onChange={(v: string) => setSearchQuery(v)}
              aria-label={t('knowledge.mount.searchPlaceholder', { defaultValue: '搜索 / 筛选知识库…' })}
              placeholder={t('knowledge.mount.searchPlaceholder', { defaultValue: '搜索 / 筛选知识库…' })}
            />
          )}

          {/* Scrollable body */}
          <div className='flex min-h-0 flex-1 flex-col gap-8px overflow-y-auto'>
            {/* Mounted-bases section */}
            <div className={sectionClass}>
              <span className={fieldLabelClass}>{t('knowledge.control.basesLabel', { defaultValue: '挂载的知识库' })}</span>
              <div className='flex flex-col gap-3px'>
                {filteredBases.length === 0 ? (
                  <span className='py-4px text-[var(--color-text-2)] text-11px'>
                    {t('knowledge.filterEmpty', { defaultValue: '没有匹配的知识库' })}
                  </span>
                ) : (
                  filteredBases.map(renderBaseRow)
                )}
              </div>
            </div>

            {/* Writeback section */}
            <div className={sectionClass}>
              <div className='flex items-center justify-between gap-10px'>
                <span className='min-w-0 flex flex-col gap-2px'>
                  <span className='text-[var(--color-text-1)] text-13px font-600'>
                    {t('knowledge.control.writeback', { defaultValue: '回血知识库' })}
                  </span>
                  <span className='text-[var(--color-text-2)] text-11px leading-15px'>
                    {t('knowledge.mount.writebackDesc', { defaultValue: '让本会话把新学到的知识写回知识库' })}
                  </span>
                </span>
                <Switch
                  size='small'
                  checked={binding.writeback}
                  disabled={targetUnresolved}
                  onChange={handleWritebackToggle}
                />
              </div>

              {binding.writeback && (
                <div className='flex flex-col gap-9px'>
                  {/* Mode */}
                  <div className={fieldStackClass}>
                    <span className={fieldLabelClass}>
                      {t('knowledge.control.writebackMode', { defaultValue: '回血模式' })}
                    </span>
                    {renderSegment(
                      binding.writeback_mode,
                      [
                        { value: 'staged', label: t('knowledge.control.modeStaged', { defaultValue: '暂存回血' }) },
                        { value: 'direct', label: t('knowledge.control.modeDirect', { defaultValue: '直接回血' }) },
                      ],
                      (v) => handleWritebackMode(v as KnowledgeWritebackMode)
                    )}
                    <span className='text-[var(--color-text-2)] text-11px leading-15px'>{writebackModeHint}</span>
                  </div>

                  {/* Eagerness */}
                  <div className={fieldStackClass}>
                    <span className={fieldLabelClass}>
                      {t('knowledge.control.writebackEagerness', { defaultValue: '回写意识' })}
                    </span>
                    {renderSegment(
                      binding.writeback_eagerness,
                      [
                        {
                          value: 'conservative',
                          label: t('knowledge.control.eagernessConservative', { defaultValue: '保守型' }),
                        },
                        {
                          value: 'aggressive',
                          label: t('knowledge.control.eagernessAggressive', { defaultValue: '激进型' }),
                        },
                      ],
                      (v) => handleWritebackEagerness(v as KnowledgeWritebackEagerness)
                    )}
                    <span className='text-[var(--color-text-2)] text-11px leading-15px'>{writebackEagernessHint}</span>
                  </div>
                </div>
              )}
            </div>

            {applyNote && <div className='text-[var(--color-text-2)] text-11px leading-15px'>{applyNote}</div>}
          </div>
        </>
      )}

      {footer ? <div className='shrink-0 border-t border-[var(--color-border-1)] pt-8px'>{footer}</div> : null}
    </div>
  );

  // ─── Trigger button (aligned with AutoWork / IDMM / MultiAgent) ────────────
  const button = (
    <Button
      size='mini'
      shape='round'
      type='secondary'
      disabled={!!disabledReason}
      className={capabilityHeaderButtonClass(binding.enabled, 'shrink-0')}
      style={capabilityHeaderButtonStyle(dotColor)}
    >
      <span className='inline-flex items-center gap-6px leading-none'>
        {/* Icon tinted by enabled-state (primary when mounted, gray off) — the
            status used to live on a separate dot beside a primary-blue button. */}
        <BookOne theme='outline' size='14' fill={dotColor} className='block' style={{ lineHeight: 0 }} />
        <span className='text-12px'>{t('knowledge.control.label')}</span>
      </span>
    </Button>
  );

  if (disabledReason) {
    return (
      <Tooltip content={disabledReason}>
        <span className='inline-flex'>{button}</span>
      </Tooltip>
    );
  }

  return (
    <Popover
      className='knowledge-control-popover'
      trigger='click'
      position='br'
      content={panel}
      onVisibleChange={(v) => {
        if (v) {
          dismissHint();
          setSearchQuery('');
        }
      }}
    >
      <Tooltip content={t('knowledge.control.discoverHint')} popupVisible={hintVisible} position='bottom'>
        {button}
      </Tooltip>
    </Popover>
  );
};

export default KnowledgeControl;
