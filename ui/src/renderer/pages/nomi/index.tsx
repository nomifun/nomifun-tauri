/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate, useSearchParams } from 'react-router-dom';
import { Button, Empty, Message, Radio, Spin, Tabs } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import OverviewTab from './tabs/OverviewTab';
import MemoriesTab from './tabs/MemoriesTab';
import ReviewTab from './tabs/ReviewTab';
import CollectTab from './tabs/CollectTab';
import LearnTab from './tabs/LearnTab';
import SuggestionsTab from './tabs/SuggestionsTab';
import MigrateTab from './tabs/MigrateTab';
import KnowledgeTab from './tabs/KnowledgeTab';
import RemoteTab from './tabs/RemoteTab';
import SettingsTab from './tabs/SettingsTab';
import SecretsTab from './tabs/SecretsTab';
import SkillsTab from './tabs/SkillsTab';
import CompanionSessionRail from './CompanionSessionRail';
import FigureLibraryPage from './FigureLibraryPage';
import { useCompanion, useCompanions, useCompanionShared } from './useNomi';
import type { CompanionId } from '@/common/types/ids';

/** Companion-domain tabs follow the selected companion; shared-domain tabs are cross-companion.
 *  Sub-tab render order under the workbench puts 总览 (overview) first — see the right-pane strip.
 *  `memories` lives in the companion domain (shared + that companion's private, scope-aware) so it
 *  is one click from the selected companion rather than buried under a "shared" domain switch.
 *  聊天已迁出管理中心 → 统一从「会话」侧边栏的桌面伙伴分组进入标准 /conversation/:id；
 *  本页只保留管理（形象/远程/记忆/技能/知识/设置）。 */
const COMPANION_TABS = ['overview', 'remote', 'memories', 'review', 'knowledge', 'skills', 'secrets', 'settings'] as const;
const SHARED_TABS = ['collect', 'learn', 'suggestions', 'migrate'] as const;
const ALL_TABS: readonly string[] = [...COMPANION_TABS, ...SHARED_TABS];
/** Standalone figure-library domain (not companion-scoped, no tab set of its own). */
const FIGURES_TAB = 'figures';
type TabKey = (typeof COMPANION_TABS)[number] | (typeof SHARED_TABS)[number];
type Domain = 'companion' | 'shared' | 'figures';

/** nomi 配置中心：伙伴条 + 双域（伙伴/共享）Tab。深链 /nomi?companion=<id>&tab=<key>。 */
const NomiConfigPage: React.FC = () => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const companionsApi = useCompanions();
  const shared = useCompanionShared();
  const { companions } = companionsApi;

  // ?tab= deep link (legacy values keep working: overview/settings/memories →
  // companion domain, collect/learn/suggestions → shared domain, figures →
  // the standalone figure library). Back-compat: the old `modelKnowledge` tab
  // split into `knowledge`, so map it. `chat` is no longer a tab — 聊天已迁进会话；
  // 旧的 ?tab=chat 深链由下方 effect 重定向到伙伴会话 /conversation/:id。
  const rawTabParam = searchParams.get('tab');
  const tabParam = rawTabParam === 'modelKnowledge' ? 'knowledge' : rawTabParam;
  const isFigures = tabParam === FIGURES_TAB;
  // 默认落地为 总览(overview)：进入伙伴域且无 ?tab= 时先看伙伴总览，而非直接进会话。
  const activeTab: TabKey = !isFigures && tabParam && ALL_TABS.includes(tabParam) ? (tabParam as TabKey) : 'overview';
  const domain: Domain = isFigures ? 'figures' : (COMPANION_TABS as readonly string[]).includes(activeTab) ? 'companion' : 'shared';

  // ?companion= selection; fall back to the first companion in the roster.
  const companionParam = searchParams.get('companion');
  const selectedCompanionId = useMemo(() => {
    if (companionParam) {
      const matched = companions.find((p) => p.id === companionParam);
      if (matched) return matched.id;
    }
    return companions[0]?.id ?? null;
  }, [companionParam, companions]);

  const companion = useCompanion(selectedCompanionId);

  // 打开伙伴聊天：解析（幂等 ensure）其唯一会话并跳转标准 /conversation/:id。未配置模型时
  // ensure 返回 400 → 留在管理中心引导配置（不跳走）。供「打开聊天」按钮与旧 ?tab=chat 深链复用。
  const openChat = useCallback(
    async (companionId: CompanionId | null) => {
      if (!companionId) return;
      try {
        const thread = await ipcBridge.companion.ensureCompanionSession.invoke({ companion_id: companionId });
        void navigate(`/conversation/${thread.conversation_id}`);
      } catch {
        Message.info(t('nomi.chat.modelMissing'));
      }
    },
    [navigate, t]
  );

  // 旧的 ?tab=chat 深链（如历史书签、旧桌宠菜单）：迁移后聊天在会话里，重定向到伙伴会话。
  useEffect(() => {
    if (rawTabParam === 'chat' && selectedCompanionId) void openChat(selectedCompanionId);
  }, [rawTabParam, selectedCompanionId, openChat]);

  // The companion session rail reads the roster (useCompanions), which only
  // refreshes on the WS round-trip — so a figure/character change made through
  // the picker (which updates useCompanion's profile optimistically) wouldn't
  // show on the selected companion's rail avatar until the broadcast echoed back
  // ("最上面/侧栏的形象一直不变, 切 tab 才刷新"). Overlay the live optimistic profile
  // onto the selected row so its avatar/name flip instantly.
  const companionsForBar = useMemo(() => {
    const live = companion.profile;
    // Only overlay when the live optimistic profile actually belongs to the
    // selected row. On a companion switch, `companion.profile` briefly still
    // holds the PREVIOUS companion's profile (useCompanion's reset effect runs
    // AFTER this render); spreading it would rewrite the selected row's id — and
    // thus its React `key` — to the previous companion's id, producing two rows
    // with the same key and the "切换伙伴时侧栏疯狂复制" duplication that only a
    // remount (switching sidebar tab) cleared. The `id: c.id` pin is belt-and-
    // suspenders: with the guard, live.id already equals c.id.
    if (!live || live.id !== selectedCompanionId) return companions;
    return companions.map((c) => (c.id === selectedCompanionId ? { ...c, ...live, id: c.id } : c));
  }, [companions, selectedCompanionId, companion.profile]);

  const setTab = useCallback(
    (key: string) => {
      setSearchParams(
        (prev) => {
          prev.set('tab', key);
          return prev;
        },
        { replace: true }
      );
    },
    [setSearchParams]
  );

  const selectCompanion = useCallback(
    (id: CompanionId) => {
      setSearchParams(
        (prev) => {
          prev.set('companion', id);
          return prev;
        },
        { replace: true }
      );
    },
    [setSearchParams]
  );

  const setDomain = useCallback(
    (d: Domain) => setTab(d === 'companion' ? 'overview' : d === 'shared' ? 'collect' : FIGURES_TAB),
    [setTab]
  );

  const handleCreated = useCallback(
    async (profile: { id: CompanionId }) => {
      await companionsApi.refresh();
      void shared.refresh();
      // 新建后落到 总览(overview)：新伙伴尚未配置模型，先在管理中心引导配置，配置后再从
      // 「打开聊天」或会话侧边栏的桌面伙伴分组进入对话。One atomic update: two sequential
      // functional setSearchParams calls would both read the pre-navigation params and the
      // second would drop the first's `companion=` change.
      setSearchParams(
        (prev) => {
          prev.set('companion', profile.id);
          prev.set('tab', 'overview');
          return prev;
        },
        { replace: true }
      );
    },
    [companionsApi, shared, setSearchParams]
  );

  const handleDeleted = useCallback(
    (deletedId: CompanionId) => {
      const rest = companions.filter((p) => p.id !== deletedId);
      setSearchParams(
        (prev) => {
          if (rest[0]) prev.set('companion', rest[0].id);
          else prev.delete('companion');
          return prev;
        },
        { replace: true }
      );
      void companionsApi.refresh();
      void shared.refresh();
    },
    [companions, companionsApi, shared, setSearchParams]
  );

  const booting = companionsApi.loading || shared.loading;

  return (
    <div className='w-full min-h-full box-border overflow-y-auto px-16px py-20px'>
      <div className='mx-auto flex w-full max-w-[95%] box-border flex-col'>
        <h1 className='m-0 mb-4px text-20px font-600 text-t-primary'>{t('nomi.title')}</h1>
        <p className='m-0 mb-12px text-13px text-t-secondary'>{t('nomi.subtitle')}</p>

        {booting ? (
          <div className='flex justify-center py-40px'>
            <Spin />
          </div>
        ) : (
          <>
            <div className='flex items-center gap-12px mb-4px'>
              <Radio.Group type='button' size='small' value={domain} onChange={(d: Domain) => setDomain(d)}>
                <Radio value='companion'>{t('nomi.domains.companion')}</Radio>
                <Radio value='shared'>{t('nomi.domains.shared')}</Radio>
                <Radio value='figures'>{t('nomi.customFigure.libraryTitle')}</Radio>
              </Radio.Group>
            </div>

            {domain === 'figures' ? (
              <FigureLibraryPage />
            ) : domain === 'companion' ? (
              // 统一会话工作台：左「会话切换栏」(每个伙伴=一条会话) + 右「对话优先」子视图。
              <div
                className='flex gap-12px mt-8px'
                style={{ height: 'calc(100vh - 196px)', minHeight: 460 }}
              >
                <CompanionSessionRail
                  companions={companionsForBar}
                  selectedId={selectedCompanionId}
                  onSelect={selectCompanion}
                  onCreated={handleCreated}
                  onDeleted={handleDeleted}
                  className='w-200px shrink-0'
                />
                {selectedCompanionId ? (
                  <div className='flex-1 min-w-0 flex flex-col'>
                    <div className='shrink-0 mb-8px flex items-center justify-between gap-8px'>
                      <Radio.Group type='button' size='small' value={activeTab} onChange={setTab}>
                        <Radio value='overview'>{t('nomi.tabs.overview')}</Radio>
                        <Radio value='remote'>{t('nomi.tabs.remote')}</Radio>
                        <Radio value='memories'>{t('nomi.tabs.memories')}</Radio>
                        <Radio value='review'>{t('nomi.tabs.review')}</Radio>
                        <Radio value='knowledge'>{t('nomi.tabs.knowledge')}</Radio>
                        <Radio value='skills'>{t('nomi.tabs.skills', { defaultValue: '技能' })}</Radio>
                        <Radio value='secrets'>{t('nomi.tabs.secrets')}</Radio>
                        <Radio value='settings'>{t('nomi.tabs.settings')}</Radio>
                      </Radio.Group>
                      <Button
                        type='primary'
                        size='small'
                        className='shrink-0'
                        onClick={() => void openChat(selectedCompanionId)}
                      >
                        {t('nomi.openChat')}
                      </Button>
                    </div>
                    <div className='flex-1 min-h-0 overflow-y-auto pr-2px'>
                      {activeTab === 'overview' && (
                        <OverviewTab key={selectedCompanionId} companion={companion} onGoTab={setTab} />
                      )}
                      {activeTab === 'memories' && (
                        <MemoriesTab key={selectedCompanionId} companionId={selectedCompanionId} companions={companions} />
                      )}
                      {activeTab === 'review' && <ReviewTab key={selectedCompanionId} companionId={selectedCompanionId} />}
                      {activeTab === 'knowledge' && <KnowledgeTab key={selectedCompanionId} companion={companion} />}
                      {activeTab === 'skills' && <SkillsTab key={selectedCompanionId} companion={companion} />}
                      {activeTab === 'remote' && <RemoteTab key={selectedCompanionId} companion={companion} />}
                      {activeTab === 'secrets' && <SecretsTab key={selectedCompanionId} companion={companion} />}
                      {activeTab === 'settings' && (
                        <SettingsTab key={selectedCompanionId} companion={companion} onDeleted={handleDeleted} />
                      )}
                    </div>
                  </div>
                ) : (
                  <div className='flex-1 flex items-center justify-center bg-fill-1 rd-12px box-border'>
                    <Empty description={t('nomi.companions.empty')} />
                  </div>
                )}
              </div>
            ) : (
              <Tabs activeTab={activeTab} onChange={setTab} lazyload>
                <Tabs.TabPane key='collect' title={t('nomi.tabs.collect')}>
                  <CollectTab shared={shared} />
                </Tabs.TabPane>
                <Tabs.TabPane key='learn' title={t('nomi.tabs.learn')}>
                  <LearnTab shared={shared} />
                </Tabs.TabPane>
                <Tabs.TabPane key='suggestions' title={t('nomi.tabs.suggestions')}>
                  <SuggestionsTab />
                </Tabs.TabPane>
                <Tabs.TabPane key='migrate' title={t('nomi.tabs.migrate')}>
                  <MigrateTab companions={companions} />
                </Tabs.TabPane>
              </Tabs>
            )}
          </>
        )}
      </div>
    </div>
  );
};

export default NomiConfigPage;
