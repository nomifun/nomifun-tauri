/**
 * PresetPickerDrawer — Right-side drawer with two modes:
 * - Preset mode: single-select from filtered preset list.
 * - Skills mode: multi-select from filtered skill list with auto-inject defaults.
 *
 * Reuses PresetTagFilterBar, filterPresetsByTags, filterSkillsByTags,
 * usePresetTags, PresetAvatar (via DrawerPresetCard).
 */
import type { Preset, PresetReference } from '@/common/types/agent/presetTypes';
import type { SkillInfo } from '@/renderer/pages/settings/PresetSettings/types';
import type { TagFilterState } from '@/renderer/pages/settings/PresetSettings/presetUtils';
import type { SkillTagFilterState } from '@/renderer/pages/settings/skill/skillFilter';

import { ipcBridge } from '@/common';
import { Drawer, Input } from '@arco-design/web-react';
import { Close, Search } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { usePresetTags } from '@/renderer/hooks/preset';
import PresetTagFilterBar from '@/renderer/pages/settings/PresetSettings/PresetTagFilterBar';
import { filterPresetsByTags } from '@/renderer/pages/settings/PresetSettings/presetUtils';
import { filterSkillsByTags } from '@/renderer/pages/settings/skill/skillFilter';
import DrawerPresetCard from './DrawerPresetCard';
import DrawerSkillCard from './DrawerSkillCard';
import styles from '../index.module.css';

// ─── Props ────────────────────────────────────────────────────────────────────

export interface PresetPickerDrawerProps {
  visible: boolean;
  mode: 'preset' | 'skills';
  onModeChange: (m: 'preset' | 'skills') => void;
  onClose: () => void;
  presets: Preset[];
  localeKey: string;
  // Preset single-select
  onSelectPreset: (presetId: PresetReference) => void;
  onFree: () => void;
  // Skills multi-select (controlled by parent GuidPage)
  allSkills: Array<{ name: string; description: string; isAuto: boolean }>;
  enabledSkills: string[];
  disabledBuiltinSkills: string[];
  onToggleSkill: (name: string, isAuto: boolean) => void;
}

// ─── Drawer width (responsive, mirrors PresetEditDrawer) ───────────────────

function computeDrawerWidth(): number {
  const viewportWidth = window.innerWidth || 1024;
  const targetWidth = Math.max(360, Math.floor(viewportWidth * 0.52));
  return Math.min(1024, targetWidth, Math.max(280, viewportWidth - 24));
}

const AVATAR_IMAGE_MAP: Record<string, string> = {};

// ─── Component ────────────────────────────────────────────────────────────────

const PresetPickerDrawer: React.FC<PresetPickerDrawerProps> = ({
  visible,
  mode,
  onModeChange,
  onClose,
  presets,
  localeKey,
  onSelectPreset,
  onFree,
  allSkills,
  enabledSkills,
  disabledBuiltinSkills,
  onToggleSkill,
}) => {
  const { t } = useTranslation();
  const { audienceTags, scenarioTags, tagById } = usePresetTags();

  // ── Responsive width ──
  const [drawerWidth, setDrawerWidth] = useState(computeDrawerWidth);
  useEffect(() => {
    const handler = () => setDrawerWidth(computeDrawerWidth());
    window.addEventListener('resize', handler);
    return () => window.removeEventListener('resize', handler);
  }, []);

  // ── Local search + tag filter state ──
  const [query, setQuery] = useState('');
  const [tagFilter, setTagFilter] = useState<TagFilterState>({ audience: [], scenario: [] });

  // Reset local state when drawer opens/mode changes
  useEffect(() => {
    if (visible) {
      setQuery('');
      setTagFilter({ audience: [], scenario: [] });
    }
  }, [visible, mode]);

  // ── Skills data (loaded internally for tag-filterable SkillInfo[]) ──
  const [skillInfos, setSkillInfos] = useState<SkillInfo[]>([]);
  const [builtinAutoNames, setBuiltinAutoNames] = useState<Set<string>>(new Set());

  useEffect(() => {
    if (!visible || mode !== 'skills') return;
    let cancelled = false;
    (async () => {
      try {
        const [skills, autoSkills] = await Promise.all([
          ipcBridge.fs.listAvailableSkills.invoke(),
          ipcBridge.fs.listBuiltinAutoSkills.invoke(),
        ]);
        if (cancelled) return;
        setSkillInfos(skills as SkillInfo[]);
        setBuiltinAutoNames(new Set((autoSkills as Array<{ name: string }>).map((s) => s.name)));
      } catch (e) {
        console.warn('[PresetPickerDrawer] failed to load skills', e);
      }
    })();
    return () => { cancelled = true; };
  }, [visible, mode]);

  // ── Filtered results ──
  const filteredPresets = useMemo(
    () =>
      // PresetListItem is a type alias for Preset — structurally identical
      filterPresetsByTags(presets, query, tagFilter, localeKey),
    [presets, query, tagFilter, localeKey]
  );

  const filteredSkills = useMemo(
    () => filterSkillsByTags(skillInfos, query, tagFilter as SkillTagFilterState),
    [skillInfos, query, tagFilter]
  );

  // ── Skill checked state helpers ──
  const isSkillChecked = useCallback(
    (name: string): boolean => {
      const isAuto = builtinAutoNames.has(name);
      return isAuto ? !disabledBuiltinSkills.includes(name) : enabledSkills.includes(name);
    },
    [builtinAutoNames, disabledBuiltinSkills, enabledSkills]
  );

  // ── Selected skill count (for footer) ──
  const selectedSkillCount = useMemo(() => {
    return allSkills.filter((s) => {
      return s.isAuto ? !disabledBuiltinSkills.includes(s.name) : enabledSkills.includes(s.name);
    }).length;
  }, [allSkills, disabledBuiltinSkills, enabledSkills]);

  // ── Handle preset select (single-select then close) ──
  const handleSelectPreset = useCallback(
    (id: PresetReference) => {
      onSelectPreset(id);
      onClose();
    },
    [onSelectPreset, onClose]
  );

  // ── Render ──
  return (
    <Drawer
      closable={false}
      visible={visible}
      placement="right"
      width={drawerWidth}
      zIndex={1200}
      getPopupContainer={() => document.body}
      autoFocus={false}
      onCancel={onClose}
      footer={null}
      headerStyle={{ display: 'none' }}
      bodyStyle={{ padding: 0, height: '100%' }}
    >
      <div className={styles.drawerSurface}>
        {/* Header: Segmented toggle + close */}
        <div className={styles.drawerTopbar}>
          <div
            className={styles.drawerSegmented}
            role='tablist'
            aria-label={`${t('guid.drawer.presetTab', { defaultValue: '设定' })} / ${t('guid.drawer.skillsTab', { defaultValue: 'Skills' })}`}
          >
            <button
              type='button'
              role='tab'
              aria-selected={mode === 'preset'}
              className={[
                styles.drawerSegment,
                mode === 'preset' ? styles.drawerSegmentActive : '',
              ].filter(Boolean).join(' ')}
              onClick={() => onModeChange('preset')}
            >
              {t('guid.drawer.presetTab', { defaultValue: '设定' })}
            </button>
            <button
              type='button'
              role='tab'
              aria-selected={mode === 'skills'}
              className={[
                styles.drawerSegment,
                mode === 'skills' ? styles.drawerSegmentActive : '',
              ].filter(Boolean).join(' ')}
              onClick={() => onModeChange('skills')}
            >
              {t('guid.drawer.skillsTab', { defaultValue: 'Skills' })}
            </button>
          </div>

          {/* Close button */}
          <button
            type='button'
            className={styles.drawerCloseButton}
            onClick={onClose}
            aria-label={t('common.close', { defaultValue: 'Close' })}
          >
            <Close theme='outline' size={16} strokeWidth={3} />
          </button>
        </div>

        {/* Search */}
        <div className={styles.drawerSearchPanel}>
          <Input
            prefix={<Search theme='outline' size={15} />}
            placeholder={
              mode === 'preset'
                ? t('guid.drawer.searchPreset', { defaultValue: '搜索设定名称或描述...' })
                : t('guid.drawer.searchSkill', { defaultValue: '搜索 Skill 名称或描述...' })
            }
            value={query}
            onChange={setQuery}
            allowClear
            className={styles.drawerSearchInput}
          />
        </div>

        {/* Tag filter */}
        <div className={styles.drawerFilterPanel}>
          <PresetTagFilterBar
            audienceTags={audienceTags}
            scenarioTags={scenarioTags}
            value={tagFilter}
            onChange={setTagFilter}
            localeKey={localeKey}
            onManageTags={() => {/* Not in scope for this drawer */}}
            variant='drawer'
            hideManageTags
          />
        </div>

        {/* Result count */}
        <div className={styles.drawerResultMeta}>
          <span>
            <strong>{mode === 'preset' ? filteredPresets.length : filteredSkills.length}</strong>
            {' '}
            {mode === 'preset'
              ? t('guid.drawer.presetCount', { defaultValue: '个设定' })
              : t('guid.drawer.skillCount', { defaultValue: '个 Skill' })}
          </span>
        </div>

        {/* Card list */}
        <div className={styles.drawerList}>
          {mode === 'preset' ? (
            filteredPresets.length > 0 ? filteredPresets.map((a) => (
                <DrawerPresetCard
                  key={a.preset_id}
                  preset={a}
                  selected={false}
                  localeKey={localeKey}
                  avatarImageMap={AVATAR_IMAGE_MAP}
                  tagById={tagById}
                  onSelect={handleSelectPreset}
                />
              )) : (
                <div className={styles.drawerEmptyState}>
                  {presets.length === 0
                    ? t('guid.drawer.presetEmpty', {
                        defaultValue: 'No presets yet. Create one in Presets, or stay free-form.',
                      })
                    : t('guid.drawer.presetNoMatch', {
                        defaultValue: 'No presets match the current search and filters.',
                      })}
                </div>
              )
          ) : (
            filteredSkills.length > 0 ? filteredSkills.map((skill) => (
                <DrawerSkillCard
                  key={skill.name}
                  skill={skill}
                  checked={isSkillChecked(skill.name)}
                  isAuto={builtinAutoNames.has(skill.name)}
                  onToggle={onToggleSkill}
                />
              )) : (
                <div className={styles.drawerEmptyState}>
                  {t('guid.drawer.skillNoMatch', {
                    defaultValue: 'No Skills match the current search and filters.',
                  })}
                </div>
              )
          )}
        </div>

        {/* Footer */}
        {mode === 'preset' ? (
          <div className={styles.drawerFooter}>
            <span className={styles.drawerFooterHint}>
              {t('guid.drawer.presetHint', { defaultValue: '选择一个设定，本次会话将固化它的配置快照。' })}
            </span>
            <button
              type='button'
              className={styles.drawerGhostButton}
              onClick={() => { onFree(); onClose(); }}
            >
              {t('guid.drawer.keepFree', { defaultValue: '保持自由发挥' })}
            </button>
          </div>
        ) : (
          <div className={styles.drawerFooter}>
            <span className={styles.drawerFooterHint}>
              {t('guid.drawer.selectedCount', { defaultValue: '已选 {{count}} 个 Skill', count: selectedSkillCount })}
            </span>
            <button
              type='button'
              className={styles.drawerPrimaryButton}
              onClick={onClose}
            >
              {t('guid.drawer.applySkills', { defaultValue: '应用到本次会话' })}
            </button>
          </div>
        )}
      </div>
    </Drawer>
  );
};

export default PresetPickerDrawer;
