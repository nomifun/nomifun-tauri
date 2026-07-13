/**
 * SkillsSettingsPage — top-level Skills capability with separate library and
 * market surfaces. Presets remain an independent capability and only reference
 * skills; the market therefore belongs here rather than under Presets.
 */
import { Tabs } from '@arco-design/web-react';
import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';
import SettingsPageWrapper from './components/SettingsPageWrapper';
import SkillMarketSettings from './SkillMarketSettings';
import SkillsHubSettings from './SkillsHubSettings';

type SkillsTab = 'library' | 'market';

const isSkillsTab = (value: string | null): value is SkillsTab => value === 'library' || value === 'market';

const SkillsSettingsPage: React.FC = () => {
  const { t } = useTranslation();
  const [searchParams, setSearchParams] = useSearchParams();
  const [activeTab, setActiveTab] = useState<SkillsTab>(() => {
    const tab = searchParams.get('tab');
    return isSkillsTab(tab) ? tab : 'library';
  });

  useEffect(() => {
    const tab = searchParams.get('tab');
    const nextTab = isSkillsTab(tab) ? tab : 'library';
    if (nextTab !== activeTab) setActiveTab(nextTab);
  }, [activeTab, searchParams]);

  const handleTabChange = (key: string) => {
    if (!isSkillsTab(key)) return;
    setActiveTab(key);
    const next = new URLSearchParams(searchParams);
    if (key === 'library') next.delete('tab');
    else next.set('tab', key);
    setSearchParams(next, { replace: true });
  };

  return (
    <SettingsPageWrapper contentClassName='max-w-1200px'>
      <Tabs
        activeTab={activeTab}
        onChange={handleTabChange}
        type='line'
        lazyload
        className='flex flex-col flex-1 min-h-0 [&>.arco-tabs-content]:pt-0'
      >
        <Tabs.TabPane
          key='library'
          title={t('settings.skillsPage.libraryTab', { defaultValue: 'Installed Skills' })}
        >
          <SkillsHubSettings withWrapper={false} />
        </Tabs.TabPane>
        <Tabs.TabPane key='market' title={t('settings.skillsPage.marketTab', { defaultValue: 'Skill Market' })}>
          <SkillMarketSettings />
        </Tabs.TabPane>
      </Tabs>
    </SettingsPageWrapper>
  );
};

export default SkillsSettingsPage;
