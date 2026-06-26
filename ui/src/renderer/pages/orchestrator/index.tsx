/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';
import classNames from 'classnames';
import { History, PeopleTopCard, Workbench } from '@icon-park/react';
import ContentSider from '@/renderer/components/layout/ContentSider';
import SegmentedTabs, { type SegmentedTabItem } from '@/renderer/components/base/SegmentedTabs';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { useResizableSplit } from '@/renderer/hooks/ui/useResizableSplit';
import { useContainerWidth } from '@/renderer/hooks/ui/useContainerWidth';
import WorkspaceList from './WorkspaceList';
import FleetManager from './FleetManager';
import RunHistory from './RunHistory';

type Section = 'workspace' | 'fleet' | 'run-history';

const isSection = (value: string | null): value is Section =>
  value === 'workspace' || value === 'fleet' || value === 'run-history';

const ORCHESTRATOR_SIDER_STORAGE_KEY = 'nomifun:orchestrator-sider-width';

interface SectionDef {
  key: Section;
  label: string;
  icon: React.ReactNode;
}

/**
 * OrchestratorPage (/orchestrator) — 「智能编排」(orchestration). Mirrors the
 * ModelHub shell: a content-area secondary sidebar (`ContentSider`) drives a
 * right content pane. Three sections — Workspaces / Fleets / Run History — sync
 * to `?section=` (default `fleet`). On mobile the left sidebar collapses to a
 * horizontal segmented bar above the content.
 *
 * The fleet section renders a temporary placeholder; Task 12 replaces it with
 * the real `<FleetManager/>` (drop-in: swap the `section === 'fleet'` branch).
 */
const OrchestratorPage: React.FC = () => {
  const { t } = useTranslation();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const [searchParams, setSearchParams] = useSearchParams();

  const [section, setSection] = useState<Section>(() => {
    const param = searchParams.get('section');
    return isSection(param) ? param : 'fleet';
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
    storageKey: ORCHESTRATOR_SIDER_STORAGE_KEY,
  });

  // Pad by the pane's real width (not the viewport breakpoint) so the narrow
  // content pane isn't robbed of horizontal space by a viewport-based class.
  const { ref: paneRef, width: paneWidth } = useContainerWidth<HTMLDivElement>();
  const panePadX = paneWidth === 0 ? 'px-24px' : paneWidth >= 600 ? 'px-40px' : paneWidth >= 420 ? 'px-24px' : 'px-16px';

  const sections: SectionDef[] = useMemo(
    () => [
      { key: 'workspace', label: t('orchestrator.section.workspace'), icon: <Workbench theme='outline' size='16' strokeWidth={3} /> },
      { key: 'fleet', label: t('orchestrator.section.fleet'), icon: <PeopleTopCard theme='outline' size='16' strokeWidth={3} /> },
      { key: 'run-history', label: t('orchestrator.section.runHistory'), icon: <History theme='outline' size='16' strokeWidth={3} /> },
    ],
    [t]
  );

  const content = (
    <>
      {section === 'workspace' && <WorkspaceList />}
      {section === 'fleet' && <FleetManager />}
      {section === 'run-history' && <RunHistory />}
    </>
  );

  // Mobile: horizontal segmented nav above the content (no left sidebar).
  if (isMobile) {
    const segmentedItems: SegmentedTabItem[] = sections.map((s) => ({ key: s.key, label: s.label, icon: s.icon }));
    return (
      <div className='w-full min-h-full box-border overflow-y-auto px-16px py-16px'>
        <div className='text-20px font-600 text-t-primary leading-tight'>{t('orchestrator.title')}</div>
        <div className='mt-4px mb-14px text-12px leading-16px text-t-tertiary'>{t('orchestrator.subtitle')}</div>
        <div className='mb-16px'>
          <SegmentedTabs items={segmentedItems} activeKey={section} onChange={handleSectionChange} size='sm' />
        </div>
        {content}
      </div>
    );
  }

  const siderHeader = (
    <div className='px-16px pt-16px pb-10px'>
      <div className='text-15px font-600 text-t-primary leading-none'>{t('orchestrator.title')}</div>
      <div className='mt-4px text-12px leading-16px text-t-tertiary'>{t('orchestrator.subtitle')}</div>
    </div>
  );

  return (
    <div className='relative flex size-full min-h-0'>
      <ContentSider
        width={resize.splitRatio}
        header={siderHeader}
        ariaLabel={t('orchestrator.title')}
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
      <div className='flex-1 min-w-0 min-h-0 overflow-y-auto' role='tabpanel' aria-label={t('orchestrator.title')} ref={paneRef}>
        <div className={classNames('mx-auto w-full max-w-1100px box-border py-32px', panePadX)}>{content}</div>
      </div>
    </div>
  );
};

export default OrchestratorPage;
