/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { Trigger } from '@arco-design/web-react';
import { Lightning, Robot, Star, Workbench } from '@icon-park/react';
import React, { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import styles from '../index.module.css';

export type GuidActiveSkill = {
  name: string;
  description?: string;
  isAuto?: boolean;
};

export interface ComposerEntryStripProps {
  isPresetAgent: boolean;
  assistantLabel?: string;
  assistantAvatar?: { kind: 'image' | 'emoji' | 'icon'; value?: string };
  onSummon: () => void;
  onAdjustSkills: () => void;
  onFree: () => void;
  onOrchestrate: () => void;
  isOrchestrationMode?: boolean;
  activeSkillCount?: number;
  activeSkills?: GuidActiveSkill[];
}

/**
 * ComposerEntryStrip — top-edge entry bar inside the chat composer.
 *
 * Two states:
 * - Default (isPresetAgent=false): [自由发挥] | [召唤助手] [使用 Skills + inline count]
 * - Summoned (isPresetAgent=true): [persona token: avatar + label + close] [使用 Skills + inline count] ... [自由发挥]
 */
const ComposerEntryStrip: React.FC<ComposerEntryStripProps> = ({
  isPresetAgent,
  assistantLabel,
  assistantAvatar,
  onSummon,
  onAdjustSkills,
  onFree,
  onOrchestrate,
  isOrchestrationMode = false,
  activeSkillCount,
  activeSkills = [],
}) => {
  const { t } = useTranslation();
  const [skillsOpen, setSkillsOpen] = useState(false);
  const skillCount = activeSkills.length > 0 ? activeSkills.length : (activeSkillCount ?? 0);
  const skillsLabel = skillCount > 0
    ? t('guid.entry.skillsActive', { defaultValue: '使用 Skills · 已启用' })
    : t('guid.entry.skills', { defaultValue: '使用 Skills' });
  const visibleSkills = useMemo(() => activeSkills.slice(0, 4), [activeSkills]);
  const overflowSkillCount = Math.max(0, activeSkills.length - visibleSkills.length);

  // --- Avatar renderer (mirrors GuidPage selectedAssistantAvatar pattern) ---
  const renderAvatar = () => {
    if (!assistantAvatar) return <Robot theme='outline' size={16} fill='currentColor' />;
    switch (assistantAvatar.kind) {
      case 'image':
        return (
          <img
            src={assistantAvatar.value}
            alt=''
            className='w-20px h-20px rounded-6px object-contain'
          />
        );
      case 'emoji':
        return <span className='text-14px leading-none'>{assistantAvatar.value}</span>;
      case 'icon':
      default:
        return <Robot theme='outline' size={16} fill='currentColor' />;
    }
  };

  const skillsPopover =
    activeSkills.length > 0 ? (
      <div className={styles.entrySkillPopover} data-testid='guid-current-skills-popover'>
        <div className={styles.entrySkillPopoverTitleRow}>
          <div className={styles.entrySkillPopoverTitle}>
            {t('guid.skillsPopover.title', { defaultValue: '本次会话使用的 Skills' })}
          </div>
          <span className={styles.entrySkillPopoverCount}>
            {t('guid.skillsPopover.enabledCount', {
              count: skillCount,
              defaultValue: '已启用 {{count}} 个',
            })}
          </span>
        </div>
        <div className={styles.entrySkillPopoverDesc}>
          {t('guid.skillsPopover.description', {
            defaultValue: '这些 Skills 会随本次会话注入，可在发送前由「使用 Skills」调整。',
          })}
        </div>

        <div className={styles.entrySkillCompactList}>
          {visibleSkills.map((skill) => (
            <div className={styles.entrySkillCompactRow} key={skill.name}>
              <span className={styles.entrySkillIcon}>
                <Lightning theme='outline' size={13} strokeWidth={3} />
              </span>
              <div className={styles.entrySkillCompactBody}>
                <div className={styles.entrySkillCompactNameRow}>
                  <span className={styles.entrySkillCompactName} title={skill.name}>
                    {skill.name}
                  </span>
                  {skill.isAuto && (
                    <span className={styles.entrySkillSource}>
                      {t('guid.drawer.autoInject', { defaultValue: '自动注入' })}
                    </span>
                  )}
                </div>
                {skill.description && (
                  <div className={styles.entrySkillCompactDesc} title={skill.description}>
                    {skill.description}
                  </div>
                )}
              </div>
            </div>
          ))}
          {overflowSkillCount > 0 && (
            <div className={styles.entrySkillOverflow}>
              {t('guid.skillsPopover.overflowCount', {
                count: overflowSkillCount,
                defaultValue: '还有 {{count}} 个 Skills',
              })}
            </div>
          )}
        </div>

        <div className={styles.entrySkillCompactHint}>
          {t('guid.skillsPopover.adjustHint', { defaultValue: '点击「使用 Skills」调整本次会话。' })}
        </div>
      </div>
    ) : null;

  // --- Skills entry (shared in both states) ---
  const skillsButton = (
    <button
      type='button'
      className={`${styles.entryButton} ${styles.entryButtonInteractive}`}
      onClick={onAdjustSkills}
      aria-label={
        skillCount > 0
          ? t('guid.entry.skillsAdjustAria', {
              count: skillCount,
              defaultValue: '调整本次会话已启用的 {{count}} 个 Skills',
            })
          : t('guid.entry.skills', { defaultValue: '使用 Skills' })
      }
    >
      <Lightning theme='outline' size={15} strokeWidth={3} />
      <span className={styles.entryButtonText}>{skillsLabel}</span>
    </button>
  );

  const skillsEntry =
    skillsPopover ? (
      <span className={styles.entrySkillControl}>
        {skillsButton}
        <Trigger
          popup={() => skillsPopover}
          trigger='click'
          position='top'
          popupVisible={skillsOpen}
          onVisibleChange={setSkillsOpen}
          clickToClose
        >
          <button
            type='button'
            className={`${styles.entryCountBadge} ${styles.entrySkillCountTrigger}`}
            aria-label={t('guid.entry.skillsActiveAria', {
              count: skillCount,
              defaultValue: '查看本次会话已启用的 {{count}} 个 Skills',
            })}
          >
            {skillCount}
          </button>
        </Trigger>
      </span>
    ) : (
      <span className={styles.entrySkillControl}>
        {skillsButton}
        {skillCount > 0 && (
          <span className={styles.entryCountBadge} aria-label={`${skillCount} skills`}>
            {skillCount}
          </span>
        )}
      </span>
    );

  // --- Summoned state ---
  if (isPresetAgent) {
    return (
      <div className={styles.entryStrip}>
        {/* Persona token */}
        <span className={`${styles.entryButton} ${styles.entryButtonActive} ${styles.entryPersonaButton}`}>
          <span className={styles.entryAvatar}>
            {renderAvatar()}
          </span>
          <span className={styles.entryButtonText}>{assistantLabel || t('guid.entry.summon', { defaultValue: '召唤助手' })}</span>
          <button
            type='button'
            className={styles.entryDismiss}
            onClick={onFree}
            aria-label={t('guid.entry.backToFree', { defaultValue: '自由发挥' })}
          >
            ✕
          </button>
        </span>

        {/* Skills */}
        {skillsEntry}

        {/* Right: back to free */}
        <button
          type='button'
          className={styles.entryBackButton}
          onClick={onFree}
        >
          <span>↩</span>
          <span>{t('guid.entry.backToFree', { defaultValue: '自由发挥' })}</span>
        </button>
      </div>
    );
  }

  // --- Default state ---
  return (
    <div className={styles.entryStrip}>
      {/* Free mode (highlighted) */}
      <span className={`${styles.entryButton} ${styles.entryButtonActive}`}>
        <Star theme='filled' size={13} fill='currentColor' />
        <span className={styles.entryButtonText}>{t('guid.entry.free', { defaultValue: '自由发挥' })}</span>
      </span>

      {/* Divider */}
      <span className={styles.entryDivider} />

      {/* Summon assistant */}
      <button
        type='button'
        className={`${styles.entryButton} ${styles.entryButtonInteractive}`}
        onClick={onSummon}
      >
        <Robot theme='outline' size={15} fill='currentColor' />
        <span className={styles.entryButtonText}>{t('guid.entry.summon', { defaultValue: '召唤助手' })}</span>
      </button>

      {/* Orchestration mode (always visible — homepage on-ramp) */}
      <button
        type='button'
        className={`${styles.entryButton} ${styles.entryButtonInteractive} ${isOrchestrationMode ? styles.entryButtonActive : ''}`}
        onClick={onOrchestrate}
      >
        <Workbench theme='outline' size={15} fill='currentColor' />
        <span className={styles.entryButtonText}>{t('guid.entry.orchestrate', { defaultValue: '智能编排' })}</span>
      </button>

      {/* Skills */}
      {skillsEntry}
    </div>
  );
};

export default ComposerEntryStrip;
