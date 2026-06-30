/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Dropdown, Input } from '@arco-design/web-react';
import { ArrowUp, Brain, Down, Send, Shield } from '@icon-park/react';
import type { TModelRef } from '@/common/types/orchestrator/orchestratorTypes';
import NomiSelect from '@/renderer/components/base/NomiSelect';
import { useInputFocusRing } from '@/renderer/hooks/chat/useInputFocusRing';
import { useCompositionInput } from '@/renderer/hooks/chat/useCompositionInput';
import { iconColors } from '@/renderer/styles/colors';
import { type ModelMode, encodePair, useModelRange } from './useModelRange';
import styles from './orchestratorComposer.module.css';

/** The orchestration autonomy levels — mirrors {@link NewRunComposer}'s create
 * options (`interactive` = review the plan first, `supervised` = run directly). */
export type AutonomyLevel = 'interactive' | 'supervised';

/** The model-range selection the composer's pill edits. Carries the mode plus
 * the encoded-pair selections for `single` / `range`; the parent resolves it
 * into a wire {@link TModelRange} via {@link useModelRange.buildModelRange} so
 * the multi-model semantics (auto → every enabled pair, single, range) stay in
 * one place. */
export interface ComposerModelRange {
  mode: ModelMode;
  /** Encoded provider+model pair for `single` mode. */
  single: string;
  /** Encoded provider+model pairs for `range` mode. */
  range: string[];
}

export interface OrchestratorComposerProps {
  /** Controlled intent text. */
  value: string;
  onChange: (value: string) => void;
  /** Submit the trimmed intent (create / adjust). Awaited so the composer can
   * keep the in-flight (submitting) affordance until it resolves. */
  onSubmit: (text: string) => Promise<void>;
  /** In-flight — disables input + spins the send affordance. */
  submitting?: boolean;
  placeholder?: string;
  /** Small primary-tinted label inside the card (e.g. 「新建编排」/「调整编排」). */
  label?: string;
  /** Drop the 800px centered column so the composer fills its container width.
   * For narrow surfaces like the conversation right-rail (F5 编排 tab) where the
   * centered clamp would overflow / waste the rail. Default `false` keeps the
   * existing centered behavior byte-identical (standalone 编排页 / RunIntentBox). */
  fluid?: boolean;

  // ── Toolbar pills (advanced controls) ──────────────────────────────────────
  /** Show the model-range pill (new-run surface). Adjust surfaces hide it. */
  showModelRange?: boolean;
  modelRange?: ComposerModelRange;
  onModelRangeChange?: (next: ComposerModelRange) => void;
  /** Show the autonomy pill (new-run surface). Adjust surfaces hide it. */
  showAutonomy?: boolean;
  autonomy?: AutonomyLevel;
  onAutonomyChange?: (next: AutonomyLevel) => void;
}

/** Case-insensitive substring match against an option's text label — mirrors
 * NewRunComposer / NewRunIntentBox's picker filter (Arco types the option as a
 * bare `ReactNode`). */
const filterByLabel = (input: string, option: React.ReactNode): boolean => {
  const children = (option as React.ReactElement<{ children?: React.ReactNode }>)?.props?.children;
  return String(children ?? '')
    .toLowerCase()
    .includes(input.toLowerCase());
};

/**
 * OrchestratorComposer — the shared, conversation-styled「智能编排」composer.
 * Replaces the old hand-built flat intent cards (NewRunIntentBox /
 * RunIntentBox's input halves) with the GuidInputCard visual language:
 *
 *  - an outer `--bg-2` shell wrapping an inner **rd-24** card (`--bg-base`, 1px
 *    `--color-border-3`), the lilac focus glow swapped in via
 *    {@link useInputFocusRing};
 *  - a transparent, borderless autoSize `Input.TextArea`;
 *  - a bottom toolbar whose right edge carries (optionally) a **model-range
 *    pill** + an **autonomy pill** (both `.sendbox-model-btn` round/small,
 *    icon + label + chevron, opening a popover) and always the **circular send
 *    button** (`.send-button-custom`, white ArrowUp);
 *  - an 800px centered column.
 *
 * It is intentionally self-contained — it borrows only class tokens + the
 * {@link useInputFocusRing} / {@link useModelRange} hooks, and does NOT touch
 * `ConversationContext` / `PreviewContext` (so it drops into the orchestrator
 * detail pane without any conversation plumbing). Enter sends · Shift+Enter
 * inserts a newline · an IME `isComposing` guard prevents an accidental send
 * mid-composition.
 */
const OrchestratorComposer: React.FC<OrchestratorComposerProps> = ({
  value,
  onChange,
  onSubmit,
  submitting = false,
  placeholder,
  label,
  fluid = false,
  showModelRange = false,
  modelRange,
  onModelRangeChange,
  showAutonomy = false,
  autonomy = 'interactive',
  onAutonomyChange,
}) => {
  const { t } = useTranslation();
  const { activeBorderColor, inactiveBorderColor, activeShadow } = useInputFocusRing();
  const { isComposing, compositionHandlers } = useCompositionInput();
  const { providers, getAvailableModels, formatModelLabel, allPairs, hasModels } = useModelRange();

  const [isFocused, setIsFocused] = useState(false);
  const [modelOpen, setModelOpen] = useState(false);
  const [autonomyOpen, setAutonomyOpen] = useState(false);

  const trimmed = value.trim();
  const canSubmit = trimmed.length > 0 && !submitting;

  const handleSubmit = useCallback(() => {
    if (!canSubmit) return;
    void onSubmit(value.trim());
  }, [canSubmit, onSubmit, value]);

  // Enter sends; Shift+Enter inserts a newline; the IME guard (ref + the
  // native `isComposing`) prevents an accidental send mid-composition.
  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (isComposing.current || e.nativeEvent.isComposing) return;
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        handleSubmit();
      }
    },
    [handleSubmit, isComposing]
  );

  // Focus glow — swap the inner card's border + shadow exactly like GuidInputCard.
  const innerBorderColor = isFocused ? activeBorderColor : inactiveBorderColor;
  const innerShadow = isFocused ? activeShadow : 'none';

  // ── Model-range pill ────────────────────────────────────────────────────────
  const modelLabel = useMemo(() => {
    if (!hasModels) return t('orchestrator.composer.model.auto');
    const mode = modelRange?.mode ?? 'auto';
    if (mode === 'auto') return t('orchestrator.composer.model.auto');
    if (mode === 'single') {
      if (!modelRange?.single) return t('orchestrator.composer.model.single');
      const ref = allPairs.find((p) => encodePair(p) === modelRange.single);
      return ref ? ref.model : t('orchestrator.composer.model.single');
    }
    const count = modelRange?.range.length ?? 0;
    return count > 0 ? t('orchestrator.composer.model.rangeCount', { count }) : t('orchestrator.composer.model.range');
  }, [hasModels, modelRange?.mode, modelRange?.single, modelRange?.range, allPairs, t]);

  const setMode = useCallback(
    (mode: ModelMode) => {
      onModelRangeChange?.({
        mode,
        single: modelRange?.single ?? '',
        range: modelRange?.range ?? [],
      });
    },
    [onModelRangeChange, modelRange?.single, modelRange?.range]
  );

  const modelModeItems: { key: ModelMode; label: string }[] = useMemo(
    () => [
      { key: 'auto', label: t('orchestrator.composer.model.auto') },
      { key: 'single', label: t('orchestrator.composer.model.single') },
      { key: 'range', label: t('orchestrator.composer.model.range') },
    ],
    [t]
  );

  const modelPanel = (
    <div className={styles.composerPopover}>
      <div className='flex flex-col gap-10px'>
        <div className='flex items-center gap-8px'>
          <Brain theme='outline' size='14' fill='rgb(var(--primary-6))' className='shrink-0' />
          <span className={styles.composerPopoverTitle}>{t('orchestrator.composer.modelLabel')}</span>
        </div>

        {!hasModels ? (
          <div className='text-12px leading-18px text-warning-6'>{t('orchestrator.composer.noModels')}</div>
        ) : (
          <>
            <div className={styles.composerSegment}>
              {modelModeItems.map((item) => {
                const active = (modelRange?.mode ?? 'auto') === item.key;
                return (
                  <div
                    key={item.key}
                    role='button'
                    tabIndex={0}
                    aria-pressed={active}
                    onClick={() => setMode(item.key)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault();
                        setMode(item.key);
                      }
                    }}
                    className={`${styles.composerSegmentItem} ${active ? styles.composerSegmentItemActive : ''}`}
                  >
                    {item.label}
                  </div>
                );
              })}
            </div>

            {(modelRange?.mode ?? 'auto') === 'auto' ? (
              <div className={styles.composerHint}>
                {t('orchestrator.composer.model.autoHint', { count: allPairs.length })}
              </div>
            ) : (modelRange?.mode ?? 'auto') === 'single' ? (
              <NomiSelect
                value={modelRange?.single || undefined}
                onChange={(v) =>
                  onModelRangeChange?.({ mode: 'single', single: v as string, range: modelRange?.range ?? [] })
                }
                placeholder={t('orchestrator.composer.model.singlePlaceholder')}
                showSearch
                filterOption={filterByLabel}
                className='w-full'
              >
                {providers.map((p) => (
                  <NomiSelect.OptGroup key={p.id} label={p.name || p.platform}>
                    {getAvailableModels(p).map((m) => {
                      const ref: TModelRef = { provider_id: p.id, model: m };
                      return (
                        <NomiSelect.Option key={encodePair(ref)} value={encodePair(ref)}>
                          {formatModelLabel(p, m)}
                        </NomiSelect.Option>
                      );
                    })}
                  </NomiSelect.OptGroup>
                ))}
              </NomiSelect>
            ) : (
              <NomiSelect
                mode='multiple'
                value={modelRange?.range ?? []}
                onChange={(v) =>
                  onModelRangeChange?.({ mode: 'range', single: modelRange?.single ?? '', range: v as string[] })
                }
                placeholder={t('orchestrator.composer.model.rangePlaceholder')}
                showSearch
                filterOption={filterByLabel}
                className='w-full'
              >
                {providers.map((p) => (
                  <NomiSelect.OptGroup key={p.id} label={p.name || p.platform}>
                    {getAvailableModels(p).map((m) => {
                      const ref: TModelRef = { provider_id: p.id, model: m };
                      return (
                        <NomiSelect.Option key={encodePair(ref)} value={encodePair(ref)}>
                          {formatModelLabel(p, m)}
                        </NomiSelect.Option>
                      );
                    })}
                  </NomiSelect.OptGroup>
                ))}
              </NomiSelect>
            )}
          </>
        )}
      </div>
    </div>
  );

  // ── Autonomy pill ────────────────────────────────────────────────────────────
  const autonomyItems: { key: AutonomyLevel; label: string; hint: string }[] = useMemo(
    () => [
      {
        key: 'interactive',
        label: t('orchestrator.composer.autonomy.interactive'),
        hint: t('orchestrator.composer.autonomy.interactiveHint'),
      },
      {
        key: 'supervised',
        label: t('orchestrator.composer.autonomy.supervised'),
        hint: t('orchestrator.composer.autonomy.supervisedHint'),
      },
    ],
    [t]
  );

  const autonomyPanel = (
    <div className={styles.composerPopover}>
      <div className='flex flex-col gap-10px'>
        <div className='flex items-center gap-8px'>
          <Shield theme='outline' size='14' fill='rgb(var(--primary-6))' className='shrink-0' />
          <span className={styles.composerPopoverTitle}>{t('orchestrator.composer.autonomyLabel')}</span>
        </div>
        <div className='flex flex-col gap-6px'>
          {autonomyItems.map((item) => {
            const active = autonomy === item.key;
            return (
              <div
                key={item.key}
                role='button'
                tabIndex={0}
                aria-pressed={active}
                onClick={() => {
                  onAutonomyChange?.(item.key);
                  setAutonomyOpen(false);
                }}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    onAutonomyChange?.(item.key);
                    setAutonomyOpen(false);
                  }
                }}
                className='flex cursor-pointer flex-col gap-2px rd-8px px-10px py-8px transition-colors'
                style={{
                  background: active ? 'color-mix(in srgb, rgb(var(--primary-6)) 10%, transparent)' : 'transparent',
                  border: `1px solid ${active ? 'color-mix(in srgb, rgb(var(--primary-6)) 26%, transparent)' : 'var(--color-border-2)'}`,
                }}
              >
                <span
                  className='text-12px font-600 leading-none'
                  style={{ color: active ? 'rgb(var(--primary-6))' : 'var(--color-text-1)' }}
                >
                  {item.label}
                </span>
                <span className='text-11px leading-15px text-t-secondary'>{item.hint}</span>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );

  const autonomyLabel =
    autonomy === 'supervised'
      ? t('orchestrator.composer.autonomy.supervised')
      : t('orchestrator.composer.autonomy.interactive');

  return (
    <div className={`${styles.composerLayout} ${fluid ? styles.composerLayoutFluid : ''}`}>
      {/* Outer `--bg-2` shell wrapping the inner rd-24 card (mirrors GuidInputCard). */}
      <div className={styles.composerWrap} style={{ padding: 6 }}>
        <div
          className={`${styles.composerInner} flex flex-col gap-8px p-12px`}
          style={{ borderColor: innerBorderColor, boxShadow: innerShadow }}
        >
          {label && (
            <span className={styles.composerLabel}>
              <Send theme='outline' size='12' strokeWidth={3} fill='rgb(var(--primary-6))' />
              <span>{label}</span>
            </span>
          )}

          <Input.TextArea
            value={value}
            onChange={onChange}
            disabled={submitting}
            autoSize={{ minRows: 2, maxRows: 12 }}
            placeholder={placeholder}
            spellCheck={false}
            className={styles.composerTextarea}
            onFocus={() => setIsFocused(true)}
            onBlur={() => setIsFocused(false)}
            {...compositionHandlers}
            onKeyDown={handleKeyDown}
            data-testid='orchestrator-composer-input'
          />

          {/* Bottom toolbar — pills on the right, circular send at the far right. */}
          <div className={styles.composerToolbar}>
            {showModelRange && (
              <Dropdown
                trigger='click'
                popupVisible={modelOpen}
                onVisibleChange={setModelOpen}
                droplist={modelPanel}
                position='tr'
              >
                <Button className='sendbox-model-btn' shape='round' size='small' data-testid='orchestrator-model-pill'>
                  <span className='flex items-center gap-6px min-w-0'>
                    <Brain theme='outline' size='14' fill={iconColors.secondary} className='shrink-0' />
                    <span className='truncate'>{modelLabel}</span>
                    <Down theme='outline' size='12' fill={iconColors.secondary} className='shrink-0' />
                  </span>
                </Button>
              </Dropdown>
            )}

            {showAutonomy && (
              <Dropdown
                trigger='click'
                popupVisible={autonomyOpen}
                onVisibleChange={setAutonomyOpen}
                droplist={autonomyPanel}
                position='tr'
              >
                <Button className='sendbox-model-btn' shape='round' size='small' data-testid='orchestrator-autonomy-pill'>
                  <span className='flex items-center gap-6px min-w-0'>
                    <Shield theme='outline' size='14' fill={iconColors.secondary} className='shrink-0' />
                    <span className='truncate'>{autonomyLabel}</span>
                    <Down theme='outline' size='12' fill={iconColors.secondary} className='shrink-0' />
                  </span>
                </Button>
              </Dropdown>
            )}

            {/* Circular send button — Arco primary circle (mirrors GuidActionRow's
                send affordance). White ArrowUp; disabled goes through the
                `.send-button-custom` class default (no inline override). */}
            <Button
              shape='circle'
              type='primary'
              loading={submitting}
              disabled={!canSubmit}
              className='send-button-custom'
              icon={<ArrowUp theme='filled' size='14' fill='white' strokeWidth={5} />}
              onClick={handleSubmit}
              data-testid='orchestrator-send-btn'
              aria-label={placeholder}
            />
          </div>
        </div>
      </div>
    </div>
  );
};

export default OrchestratorComposer;
