import { ipcBridge } from '@/common';
import type { ISkillMarketItem, SkillMarketSource } from '@/common/adapter/ipcBridge';
import { resolveLocaleKey } from '@/common/utils';
import { useNomiQuickStart } from '@/renderer/hooks/agent/useNomiQuickStart';
import { useAssistantTags } from '@/renderer/hooks/assistant';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { openExternalUrl } from '@/renderer/utils/platform';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import AssistantTagFilterBar from './AssistantSettings/AssistantTagFilterBar';
import type { SkillTagFilterState } from './skill/skillFilter';
import SkillMarketCard from './skill/SkillMarketCard';
import {
  buildSkillMarketConversationName,
  buildSkillMarketInstallPrompt,
  cleanMarketText,
  filterSkillMarketItems,
  normalizeSkillMarketItems,
  SKILL_MARKET_SOURCES,
} from './skill/skillMarket';
import { Button, Input } from '@arco-design/web-react';
import { CloseSmall, LinkOne, Refresh, Search } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

const CARD_GRID_COLS = 'repeat(auto-fill, minmax(min(232px, 100%), 1fr))';
const CACHE_KEY = 'nomifun.skillMarket.rankings.v1';
const AUTO_SYNC_KEY = 'nomifun.skillMarket.autoSynced.v1';

type SkillMarketCache = {
  fetched_at?: number;
  items?: unknown;
  errors?: unknown;
};

const sourceLabel = (source: SkillMarketSource): string => (source === 'clawhub' ? 'clawhub' : 'shillhub');
const sourceMarketUrl = (source: SkillMarketSource): string =>
  source === 'clawhub' ? 'https://clawhub.ai/' : 'https://www.skills.sh/';

const SkillMarketSettings: React.FC = () => {
  const { t, i18n } = useTranslation();
  const localeKey = resolveLocaleKey(i18n.language);
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const [message, messageContext] = useArcoMessage({ maxCount: 10 });
  const tags = useAssistantTags();
  const { start } = useNomiQuickStart();
  const autoSyncStartedRef = useRef(false);

  const [activeSource, setActiveSource] = useState<SkillMarketSource>('clawhub');
  const [items, setItems] = useState<ISkillMarketItem[]>([]);
  const [fetchedAt, setFetchedAt] = useState<number | null>(null);
  const [errors, setErrors] = useState<string[]>([]);
  const [loading, setLoading] = useState(false);
  const [searchQuery, setSearchQuery] = useState('');
  const [searchExpanded, setSearchExpanded] = useState(false);
  const [tagFilter, setTagFilter] = useState<SkillTagFilterState>({ audience: [], scenario: [] });

  useEffect(() => {
    try {
      const raw = localStorage.getItem(CACHE_KEY);
      if (!raw) return;
      const cache = JSON.parse(raw) as SkillMarketCache;
      const normalized = normalizeSkillMarketItems(cache.items);
      setItems(normalized);
      setFetchedAt(typeof cache.fetched_at === 'number' ? cache.fetched_at : null);
      setErrors(Array.isArray(cache.errors) ? cache.errors.filter((item): item is string => typeof item === 'string') : []);
    } catch {
      localStorage.removeItem(CACHE_KEY);
    }
  }, []);

  useEffect(() => {
    const audKeys = new Set(tags.audienceTags.map((tag) => tag.key));
    const scnKeys = new Set(tags.scenarioTags.map((tag) => tag.key));
    setTagFilter((prev) => {
      const audience = prev.audience.filter((key) => audKeys.has(key));
      const scenario = prev.scenario.filter((key) => scnKeys.has(key));
      if (audience.length === prev.audience.length && scenario.length === prev.scenario.length) return prev;
      return { audience, scenario };
    });
  }, [tags.audienceTags, tags.scenarioTags]);

  const syncMarket = useCallback(async (options?: { showToast?: boolean }) => {
    const showToast = options?.showToast ?? true;
    setLoading(true);
    try {
      const result = await ipcBridge.fs.syncSkillMarketRankings.invoke({ sources: SKILL_MARKET_SOURCES });
      const normalized = normalizeSkillMarketItems(result.items);
      setItems(normalized);
      setFetchedAt(result.fetched_at);
      setErrors(result.errors ?? []);
      localStorage.setItem(
        CACHE_KEY,
        JSON.stringify({
          fetched_at: result.fetched_at,
          items: normalized,
          errors: result.errors ?? [],
        })
      );
      if (showToast) {
        if (normalized.length > 0) {
          message.success(t('settings.skillsMarket.syncSuccess', { defaultValue: '技能市场已更新' }));
        } else {
          message.warning(t('settings.skillsMarket.syncEmpty', { defaultValue: '未采集到榜单数据。' }));
        }
      }
    } catch (error) {
      console.error('Failed to sync skill market:', error);
      const errorText = t('settings.skillsMarket.syncError', { defaultValue: '更新技能市场失败' });
      setErrors([errorText]);
      if (showToast) message.error(errorText);
    } finally {
      setLoading(false);
    }
  }, [message, t]);

  useEffect(() => {
    if (autoSyncStartedRef.current) return;
    autoSyncStartedRef.current = true;
    try {
      if (sessionStorage.getItem(AUTO_SYNC_KEY) === '1') return;
      sessionStorage.setItem(AUTO_SYNC_KEY, '1');
    } catch {
      // Ignore storage failures; the refresh itself is still the useful work.
    }
    void syncMarket({ showToast: false });
  }, [syncMarket]);

  const filteredItems = useMemo(
    () => filterSkillMarketItems(items, activeSource, searchQuery, tagFilter),
    [items, activeSource, searchQuery, tagFilter]
  );

  const sourceCounts = useMemo(() => {
    const counts: Record<SkillMarketSource, number> = { clawhub: 0, shillhub: 0 };
    for (const item of items) counts[item.source] += 1;
    return counts;
  }, [items]);

  const handleAdd = useCallback(
    async (item: ISkillMarketItem) => {
      await start({
        name: buildSkillMarketConversationName(item),
        prompt: buildSkillMarketInstallPrompt(item),
        send: false,
      });
    },
    [start]
  );

  const handleOpenMarket = useCallback(async () => {
    try {
      await openExternalUrl(sourceMarketUrl(activeSource));
    } catch (error) {
      console.error('Failed to open skill market:', error);
      message.error(t('settings.skillsMarket.openMarketFailed', { defaultValue: '无法打开技能市场' }));
    }
  }, [activeSource, message, t]);

  const isSearchVisible = searchExpanded || searchQuery.length > 0;
  const activeSearch = searchQuery.trim().length > 0;
  const emptyText = loading
    ? t('common.loading', { defaultValue: '请稍候...' })
    : items.length === 0
      ? t('settings.skillsMarket.empty', { defaultValue: '正在准备榜单，点击刷新可重新采集。' })
      : activeSearch
        ? t('settings.skillsMarket.noSearchMatch', {
            query: searchQuery.trim(),
            source: sourceLabel(activeSource),
            defaultValue: `当前 ${sourceLabel(activeSource)} 未找到“${searchQuery.trim()}”相关技能。`,
          })
        : t('settings.skillsMarket.noMatch', { defaultValue: '没有符合当前筛选条件的技能。' });

  return (
    <div className='flex flex-col h-full w-full'>
      {messageContext}
      <div className='space-y-16px pb-24px'>
        <div className={`bg-fill-2 rounded-24px ${isMobile ? 'p-16px' : 'p-20px'}`}>
          <div className='flex flex-col gap-16px mb-20px'>
            <div className={`flex gap-12px ${isMobile ? 'flex-col' : 'items-start justify-between'}`}>
              <div className='min-w-0'>
                <h2 className='m-0 text-28px font-700 leading-[1.1] text-t-primary'>
                  {t('settings.skillsMarket.title', { defaultValue: '技能市场' })}
                </h2>
                <p className='mt-8px mb-0 max-w-[680px] text-14px text-t-secondary leading-relaxed'>
                  {t('settings.skillsMarket.description', {
                    defaultValue: '同步 ClawHub 与 shillhub 最新榜单，选择技能后交给 Nomi 生成安装确认草稿。',
                  })}
                </p>
              </div>
              <div className={`flex items-center gap-10px ${isMobile ? 'w-full flex-wrap' : 'flex-shrink-0'}`}>
                <div className='inline-flex items-center gap-4px rounded-12px bg-[var(--color-bg-2)] p-3px border border-solid border-[var(--color-border-2)]'>
                  {SKILL_MARKET_SOURCES.map((source) => (
                    <Button
                      key={source}
                      size='small'
                      type={activeSource === source ? 'primary' : 'text'}
                      data-testid={`btn-skill-market-source-${source}`}
                      className='!rounded-9px !h-28px !px-12px !text-12px'
                      onClick={() => setActiveSource(source)}
                    >
                      {sourceLabel(source)}
                      {sourceCounts[source] > 0 ? ` ${sourceCounts[source]}` : ''}
                    </Button>
                  ))}
                </div>
                <Button
                  type='text'
                  size='small'
                  data-testid='btn-sync-skill-market'
                  className='!rounded-10px !h-34px !w-34px !p-0 flex items-center justify-center !text-t-secondary hover:!bg-fill-1 hover:!text-t-primary'
                  icon={<Refresh size={16} fill='currentColor' className={loading ? 'animate-spin' : ''} />}
                  onClick={() => void syncMarket()}
                  title={t('common.refresh', { defaultValue: '刷新' })}
                />
                <Button
                  type={isSearchVisible ? 'secondary' : 'text'}
                  size='small'
                  data-testid='btn-search-skill-market'
                  className='!rounded-10px !h-34px !w-34px !p-0 flex items-center justify-center !text-t-secondary hover:!bg-fill-1 hover:!text-t-primary'
                  icon={
                    isSearchVisible ? <CloseSmall size={16} fill='currentColor' /> : <Search size={16} fill='currentColor' />
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
              </div>
            </div>

            {isSearchVisible && (
              <Input
                allowClear
                autoFocus
                value={searchQuery}
                data-testid='input-search-skill-market'
                className='!bg-[var(--color-bg-2)]'
                placeholder={t('settings.skillsMarket.searchPlaceholder', { defaultValue: '搜索当前市场技能...' })}
                prefix={<Search size={14} fill='currentColor' />}
                onChange={(value) => setSearchQuery(cleanMarketText(value, 80))}
              />
            )}

            <AssistantTagFilterBar
              audienceTags={tags.audienceTags}
              scenarioTags={tags.scenarioTags}
              value={tagFilter}
              onChange={setTagFilter}
              localeKey={localeKey}
              onManageTags={() => undefined}
              hideManageTags
            />
          </div>

          {errors.length > 0 && (
            <div className='mb-14px rounded-12px border border-solid border-[rgba(var(--orange-6),0.24)] bg-[rgba(var(--orange-6),0.08)] px-14px py-10px text-12px leading-18px text-[rgb(var(--orange-7))]'>
              {errors.join(' / ')}
            </div>
          )}

          {filteredItems.length > 0 ? (
            <div className='grid gap-12px' style={{ gridTemplateColumns: CARD_GRID_COLS }}>
              {filteredItems.map((item) => (
                <SkillMarketCard
                  key={item.id}
                  item={item}
                  tagByKey={tags.tagByKey}
                  localeKey={localeKey}
                  onAdd={(skill) => void handleAdd(skill)}
                />
              ))}
            </div>
          ) : (
            <div className='text-center text-t-secondary py-40px'>
              {emptyText}
            </div>
          )}

          {(fetchedAt || items.length > 0) && (
            <div className='mt-16px flex items-center justify-between gap-12px text-12px text-t-tertiary'>
              <span>
                {fetchedAt
                  ? t('settings.skillsMarket.lastUpdated', {
                      time: new Date(fetchedAt).toLocaleString(),
                      defaultValue: '上次更新：{{time}}',
                    })
                  : ''}
              </span>
              <Button
                type='text'
                size='mini'
                data-testid='btn-open-skill-market-browser'
                className='!rounded-10px !px-10px !h-28px !text-12px !text-t-secondary hover:!bg-fill-1 hover:!text-t-primary'
                icon={<LinkOne size={14} fill='currentColor' />}
                onClick={() => void handleOpenMarket()}
              >
                {t('settings.skillsMarket.openInBrowser', { defaultValue: '浏览器打开市场' })}
              </Button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
};

export default SkillMarketSettings;
