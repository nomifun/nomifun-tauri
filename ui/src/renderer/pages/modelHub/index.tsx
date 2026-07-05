/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';
import classNames from 'classnames';
import { LinkCloud, Robot, SettingTwo, Platte } from '@icon-park/react';
import ContentSider from '@/renderer/components/layout/ContentSider';
import SegmentedTabs, { type SegmentedTabItem } from '@/renderer/components/base/SegmentedTabs';
import { SettingsViewModeProvider } from '@/renderer/components/settings/SettingsModal/settingsViewContext';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { useResizableSplit } from '@/renderer/hooks/ui/useResizableSplit';
import { useContainerWidth } from '@/renderer/hooks/ui/useContainerWidth';
import AgentModalContent from '@/renderer/components/settings/SettingsModal/contents/AgentModalContent';
import ModelModalContent from '@/renderer/components/settings/SettingsModal/contents/ModelModalContent';
import GlobalModelConfig from './GlobalModelConfig';
import CreationModelsContent from './CreationModelsContent';

type Section = 'agents' | 'models' | 'creation' | 'global';

const isSection = (value: string | null): value is Section =>
  value === 'agents' || value === 'models' || value === 'creation' || value === 'global';

const MODELHUB_SIDER_STORAGE_KEY = 'nomifun:modelhub-sider-width';

interface SectionDef {
  key: Section;
  label: string;
  icon: React.ReactNode;
}

/**
 * ModelHubPage (/models) — "Model Management". The primary level is a
 * content-area secondary sidebar (mirroring the conversation `ContentSider`):
 * a left section list (Agents / Models / Global Model Config) drives the right
 * content pane. The agents section keeps its own local/remote second-level tabs.
 *
 * The sidebar width is drag-resizable and persisted. On mobile the left sidebar
 * collapses to a horizontal segmented bar above the content.
 *
 * The level syncs to `?section=` (not `?tab=`) so it never collides with the
 * local/remote `?tab=` used inside AgentModalContent.
 */
const ModelHubPage: React.FC = () => {
  const { t } = useTranslation();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const [searchParams, setSearchParams] = useSearchParams();

  const [section, setSection] = useState<Section>(() => {
    const param = searchParams.get('section');
    return isSection(param) ? param : 'agents';
  });

  useEffect(() => {
    const param = searchParams.get('section');
    if (isSection(param) && param !== section) {
      setSection(param);
    }
  }, [searchParams, section]);

  const handleSectionChange = useCallback(
    (key: string) => {
      if (!isSection(key)) return;
      setSection(key);
      const next = new URLSearchParams(searchParams);
      next.set('section', key);
      setSearchParams(next, { replace: true });
    },
    [searchParams, setSearchParams]
  );

  const resize = useResizableSplit({
    unit: 'px',
    defaultWidth: 248,
    minWidth: 200,
    maxWidth: 360,
    storageKey: MODELHUB_SIDER_STORAGE_KEY,
  });

  // 内容面板的可用宽度 = 视口 − 一次 rail − 二级 ContentSider − 拖拽宽度，远小于视口。
  // 按面板实宽（而非视口断点）给横向 padding，窄面板不再被 md:px-40px 白吃 80px。
  const { ref: paneRef, width: paneWidth } = useContainerWidth<HTMLDivElement>();
  const panePadX = paneWidth === 0 ? 'px-24px' : paneWidth >= 600 ? 'px-40px' : paneWidth >= 420 ? 'px-24px' : 'px-16px';

  const sections: SectionDef[] = useMemo(
    () => [
      { key: 'agents', label: t('settings.modelHub.sectionAgents'), icon: <Robot theme='outline' size='16' strokeWidth={3} /> },
      { key: 'models', label: t('settings.modelHub.sectionModels'), icon: <LinkCloud theme='outline' size='16' strokeWidth={3} /> },
      { key: 'creation', label: t('settings.modelHub.sectionCreation'), icon: <Platte theme='outline' size='16' strokeWidth={3} /> },
      { key: 'global', label: t('settings.modelHub.sectionGlobal'), icon: <SettingTwo theme='outline' size='16' strokeWidth={3} /> },
    ],
    [t]
  );

  const content = (
    <>
      {section === 'agents' && <AgentModalContent />}
      {section === 'models' && <ModelModalContent />}
      {section === 'creation' && <CreationModelsContent />}
      {section === 'global' && <GlobalModelConfig />}
    </>
  );

  // Mobile: horizontal segmented nav above the content (no left sidebar).
  if (isMobile) {
    const segmentedItems: SegmentedTabItem[] = sections.map((s) => ({ key: s.key, label: s.label, icon: s.icon }));
    return (
      <SettingsViewModeProvider value='page'>
        <div className='w-full min-h-full box-border overflow-y-auto px-16px py-16px'>
          <div className='text-20px font-600 text-t-primary leading-tight'>{t('settings.modelHub.title')}</div>
          <div className='mt-4px mb-14px text-12px leading-16px text-t-tertiary'>{t('settings.modelHub.subtitle')}</div>
          <div className='mb-16px'>
            <SegmentedTabs items={segmentedItems} activeKey={section} onChange={handleSectionChange} size='sm' />
          </div>
          {content}
        </div>
      </SettingsViewModeProvider>
    );
  }

  const siderHeader = (
    <div className='px-16px pt-16px pb-10px'>
      <div className='text-15px font-600 text-t-primary leading-none'>{t('settings.modelHub.title')}</div>
      <div className='mt-4px text-12px leading-16px text-t-tertiary'>{t('settings.modelHub.subtitle')}</div>
    </div>
  );

  return (
    <div className='relative flex size-full min-h-0'>
      <ContentSider
        width={resize.splitRatio}
        header={siderHeader}
        ariaLabel={t('settings.modelHub.title')}
        resizeHandle={resize.createDragHandle({ className: 'right-0' })}
      >
        <div className='flex flex-col gap-2px px-8px pb-8px' role='tablist' aria-orientation='vertical'>
          {sections.map((s) => {
            const selected = section === s.key;
            return (
              <div
                key={s.key}
                role='tab'
                aria-selected={selected}
                tabIndex={0}
                onClick={() => handleSectionChange(s.key)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    handleSectionChange(s.key);
                  }
                }}
                className={classNames(
                  'h-34px rd-8px flex items-center gap-8px px-10px cursor-pointer shrink-0 transition-colors outline-none text-t-primary',
                  selected ? '!bg-primary-1 !text-primary-6' : 'hover:bg-fill-2 active:bg-fill-3'
                )}
              >
                <span
                  className={classNames(
                    'size-22px flex items-center justify-center shrink-0 line-height-0',
                    selected ? 'text-primary-6' : 'text-t-secondary'
                  )}
                >
                  {s.icon}
                </span>
                <span className='text-14px font-[500] leading-24px truncate'>{s.label}</span>
              </div>
            );
          })}
        </div>
      </ContentSider>
      <div className='flex-1 min-w-0 min-h-0 overflow-y-auto' role='tabpanel' aria-label={t('settings.modelHub.title')} ref={paneRef}>
        <SettingsViewModeProvider value='page'>
          <div className={classNames('mx-auto w-full max-w-1100px box-border py-32px', panePadX)}>{content}</div>
        </SettingsViewModeProvider>
      </div>
    </div>
  );
};

export default ModelHubPage;
