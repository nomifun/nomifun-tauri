/**
 * PresetListPanel — Renders presets as a responsive card grid with a
 * two-dimension tag filter bar (Audience / Skill Scenario) and a search toggle.
 * Replaces the old source-Tabs + enabled/disabled-section layout.
 */
import { filterPresetsByTags, type TagFilterState } from './presetUtils';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import type { PresetReference, PresetTag } from '@/common/types/agent/presetTypes';
import type { PresetListItem } from './types';
import PresetCard from './PresetCard';
import PresetTagFilterBar from './PresetTagFilterBar';
import { Button, Input } from '@arco-design/web-react';
import { Plus, Search, CloseSmall } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

/**
 * 卡片网格按「内容容器实际宽度」自动定列(auto-fill),而非视口断点 —— 设置内容
 * 面板被一级 rail + 二级 ContentSider 占去宽度,视口宽 ≠ 面板可用宽。设定卡片
 * 比 AgentCard 更厚(头像+名称+描述+标签+开关),取较宽的 232px 下限。
 * Card grids auto-fit columns to the actual container width (not viewport
 * breakpoints): the settings pane is narrower than the viewport, so viewport
 * breakpoints over-column and clip cards on a narrow pane.
 */
const CARD_GRID_COLS = 'repeat(auto-fill, minmax(min(232px, 100%), 1fr))';

type PresetListPanelProps = {
  presets: PresetListItem[];
  localeKey: string;
  avatarImageMap: Record<string, string>;
  isExtensionPreset: (preset: PresetListItem | null | undefined) => boolean;
  onEdit: (preset: PresetListItem) => void;
  onDuplicate: (preset: PresetListItem) => void;
  onCreate: () => void;
  onToggleEnabled: (preset: PresetListItem, checked: boolean) => void;
  setActivePresetId: (id: PresetReference) => void;
  /** When set, scroll to and highlight the matching preset card */
  highlightId?: string | null;
  /** Called after the highlight animation completes so the parent can clear the param */
  onHighlightConsumed?: () => void;
  // Tag facets
  audienceTags: PresetTag[];
  scenarioTags: PresetTag[];
  tagById: Map<string, PresetTag>;
  onManageTags: () => void;
};

const PresetListPanel: React.FC<PresetListPanelProps> = ({
  presets,
  localeKey,
  avatarImageMap,
  isExtensionPreset,
  onEdit,
  onDuplicate,
  onCreate,
  onToggleEnabled,
  setActivePresetId,
  highlightId,
  onHighlightConsumed,
  audienceTags,
  scenarioTags,
  tagById,
  onManageTags,
}) => {
  const { t } = useTranslation();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const [search_query, setSearchQuery] = useState('');
  const [searchExpanded, setSearchExpanded] = useState(false);
  const [tagFilter, setTagFilter] = useState<TagFilterState>({ audience: [], scenario: [] });
  const [highlightedId, setHighlightedId] = useState<string | null>(null);
  const cardRefs = useRef<Record<string, HTMLDivElement | null>>({});
  const cardRefSetter = useCallback(
    (id: string) => (el: HTMLDivElement | null) => {
      cardRefs.current[id] = el;
    },
    []
  );

  // Scroll to and highlight an preset card when navigated with ?highlight=id.
  // Depends on `presets` so it re-runs after async data loads and refs are
  // populated. A short delay ensures the layout is settled on first mount.
  useEffect(() => {
    if (!highlightId || presets.length === 0) return;
    const el = cardRefs.current[highlightId];
    if (!el) return;

    const timer = setTimeout(() => {
      el.scrollIntoView({ behavior: 'smooth', block: 'center' });
      setHighlightedId(highlightId);
      setTimeout(() => {
        setHighlightedId(null);
        onHighlightConsumed?.();
      }, 2000);
    }, 150);

    return () => clearTimeout(timer);
  }, [highlightId, presets, onHighlightConsumed]);

  // Self-heal the tag filter against the current vocabulary: when a tag is
  // deleted in the management modal, its chip vanishes from the bar but a
  // selected key could linger in `tagFilter.<dim>`, invisibly constraining the
  // facet. Drop any selected key that no longer exists. The `return prev`
  // no-change guard prevents render loops.
  useEffect(() => {
    const audIds = new Set<string>(audienceTags.map((tag) => tag.preset_tag_id));
    const scnIds = new Set<string>(scenarioTags.map((tag) => tag.preset_tag_id));
    setTagFilter((prev) => {
      const audience = prev.audience.filter((id) => audIds.has(id));
      const scenario = prev.scenario.filter((id) => scnIds.has(id));
      if (audience.length === prev.audience.length && scenario.length === prev.scenario.length) return prev;
      return { audience, scenario };
    });
  }, [audienceTags, scenarioTags]);

  const filteredPresets = useMemo(
    () => filterPresetsByTags(presets, search_query, tagFilter, localeKey),
    [presets, search_query, tagFilter, localeKey]
  );

  const isSearchVisible = searchExpanded || search_query.length > 0;

  return (
    <div className='py-2'>
      <div className={`bg-fill-2 rounded-24px ${isMobile ? 'p-16px' : 'p-20px'}`}>
        <div className='flex flex-col gap-16px mb-20px'>
          <div className={`flex gap-12px ${isMobile ? 'flex-col' : 'items-start justify-between'}`}>
            <div className='min-w-0'>
              <h2 className='m-0 text-28px font-700 leading-[1.1] text-t-primary'>
                {t('settings.presets', { defaultValue: 'Presets' })}
              </h2>
              <p className='mt-8px mb-0 max-w-[680px] text-14px text-t-secondary leading-relaxed'>
                {t('settings.presetsListDescription', {
                  defaultValue:
                    'Save Agent instructions, preferences, Skills and knowledge scope as reusable one-click configurations.',
                })}
              </p>
            </div>
            <div className={`flex items-center gap-10px ${isMobile ? 'w-full' : 'flex-shrink-0'}`}>
              <Button
                type={isSearchVisible ? 'secondary' : 'text'}
                size='small'
                data-testid='btn-search-toggle'
                className='!rounded-10px !h-34px !w-34px !p-0 flex items-center justify-center !text-t-secondary hover:!bg-fill-1 hover:!text-t-primary'
                icon={
                  isSearchVisible ? (
                    <CloseSmall size={16} fill='currentColor' />
                  ) : (
                    <Search size={16} fill='currentColor' />
                  )
                }
                onClick={() => {
                  if (isSearchVisible) {
                    setSearchExpanded(false);
                    setSearchQuery('');
                    return;
                  }
                  setSearchExpanded(true);
                }}
              />
              <Button
                type='primary'
                size='small'
                className={`!rounded-[100px] ${isMobile ? '!flex-1 !h-36px' : '!px-16px !h-34px'}`}
                icon={<Plus size={14} fill='currentColor' />}
                onClick={onCreate}
                data-testid='btn-create-preset'
              >
                {t('settings.createPreset', { defaultValue: 'Create Preset' })}
              </Button>
            </div>
          </div>

          {isSearchVisible && (
            <Input
              allowClear
              autoFocus
              value={search_query}
              onChange={setSearchQuery}
              data-testid='input-search-preset'
              className='!bg-[var(--color-bg-2)]'
              placeholder={t('settings.searchPresets', {
                defaultValue: 'Search presets by name or description',
              })}
              prefix={<Search size={14} fill='currentColor' />}
            />
          )}

          <PresetTagFilterBar
            audienceTags={audienceTags}
            scenarioTags={scenarioTags}
            value={tagFilter}
            onChange={setTagFilter}
            localeKey={localeKey}
            onManageTags={onManageTags}
          />
        </div>

        {filteredPresets.length > 0 ? (
          <div className='grid gap-12px' style={{ gridTemplateColumns: CARD_GRID_COLS }}>
            {filteredPresets.map((preset) => (
              <PresetCard
                key={preset.preset_id}
                preset={preset}
                localeKey={localeKey}
                avatarImageMap={avatarImageMap}
                tagById={tagById}
                isExtensionPreset={isExtensionPreset}
                onEdit={(a) => {
                  setActivePresetId(a.preset_id);
                  onEdit(a);
                }}
                onDuplicate={onDuplicate}
                onToggleEnabled={onToggleEnabled}
                highlighted={highlightedId === preset.preset_id}
                cardRef={cardRefSetter(preset.preset_id)}
              />
            ))}
          </div>
        ) : (
          <div className='text-center text-t-secondary py-32px'>
            {presets.length === 0
              ? t('settings.presetsEmpty', {
                  defaultValue: 'No presets yet. Create one to save a reusable launch configuration.',
                })
              : t('settings.presetNoMatch', {
                  defaultValue: 'No presets match the current filters.',
                })}
          </div>
        )}
      </div>
    </div>
  );
};

export default PresetListPanel;
