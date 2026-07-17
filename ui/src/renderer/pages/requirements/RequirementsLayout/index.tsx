/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { Outlet, useLocation, useNavigate } from 'react-router-dom';
import classNames from 'classnames';
import ContentSider from '@/renderer/components/layout/ContentSider';
import SegmentedTabs, { type SegmentedTabItem } from '@/renderer/components/base/SegmentedTabs';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { useResizableSplit } from '@/renderer/hooks/ui/useResizableSplit';
import { useContainerWidth } from '@/renderer/hooks/ui/useContainerWidth';
import { type RequirementsSection, useRequirementsSections } from './sections';

const REQUIREMENTS_SIDER_STORAGE_KEY = 'nomifun:requirements-sider-width';

/**
 * Derive the active section from a pathname. `/requirements` (the workspace
 * index) matches ONLY when the path is exactly `/requirements` or
 * `/requirements/`; the deeper sections match by prefix. This keeps the index
 * from greedily winning over `/requirements/extensions` etc.
 */
const activeSectionForPath = (pathname: string): RequirementsSection => {
  if (pathname === '/requirements/extensions' || pathname.startsWith('/requirements/extensions/')) {
    return 'extensions';
  }
  if (pathname === '/requirements/sources' || pathname.startsWith('/requirements/sources/')) {
    return 'sources';
  }
  return 'workspace';
};

/**
 * RequirementsLayout (/requirements) — the requirements-platform shell. The
 * primary level is a content-area secondary sidebar (mirroring `ModelHubPage`
 * and the conversation `ContentSider`): a left section rail (需求 / 扩展能力 /
 * 数据源) drives the right content pane.
 *
 * Unlike ModelHubPage — whose sections are inlined `?section=` state — our
 * sections are REAL nested routes. The rail therefore navigates (`useNavigate`),
 * derives the active section from the URL (`useLocation`), and the right pane
 * renders the matched child route via `<Outlet/>`. This way the ContentSider
 * persists across section navigations.
 *
 * The sidebar width is drag-resizable and persisted. On mobile the left sidebar
 * collapses to a horizontal segmented bar above the content.
 */
const RequirementsLayout: React.FC = () => {
  const { t } = useTranslation();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const navigate = useNavigate();
  const { pathname } = useLocation();

  const sections = useRequirementsSections();
  const section = useMemo(() => activeSectionForPath(pathname), [pathname]);

  const handleSectionChange = useCallback(
    (key: string) => {
      const target = sections.find((s) => s.key === key);
      if (!target) return;
      void Promise.resolve(navigate(target.path)).catch((error) => {
        console.error('Navigation failed:', error);
      });
    },
    [navigate, sections]
  );

  const resize = useResizableSplit({
    unit: 'px',
    defaultWidth: 248,
    minWidth: 200,
    maxWidth: 360,
    storageKey: REQUIREMENTS_SIDER_STORAGE_KEY,
  });

  // 内容面板的可用宽度 = 视口 − 一次 rail − 二级 ContentSider − 拖拽宽度，远小于视口。
  // 按面板实宽（而非视口断点）给横向 padding，窄面板不再被 md:px-40px 白吃 80px。
  const { ref: paneRef, width: paneWidth } = useContainerWidth<HTMLDivElement>();
  const panePadX = paneWidth === 0 ? 'px-24px' : paneWidth >= 600 ? 'px-40px' : paneWidth >= 420 ? 'px-24px' : 'px-16px';

  // Mobile: horizontal segmented nav above the content (no left sidebar).
  if (isMobile) {
    const segmentedItems: SegmentedTabItem[] = sections.map((s) => ({ key: s.key, label: s.label, icon: s.icon }));
    return (
      <div className='w-full min-h-full box-border overflow-y-auto px-16px py-16px'>
        <div className='text-20px font-600 text-t-primary leading-tight'>{t('requirements.title')}</div>
        <div className='mt-4px mb-14px text-12px leading-16px text-t-tertiary'>{t('requirements.subtitle')}</div>
        <div className='mb-16px'>
          <SegmentedTabs items={segmentedItems} activeKey={section} onChange={handleSectionChange} size='sm' />
        </div>
        <Outlet />
      </div>
    );
  }

  const siderHeader = (
    <div className='px-16px pt-16px pb-10px'>
      <div className='text-15px font-600 text-t-primary leading-none'>{t('requirements.title')}</div>
      <div className='mt-4px text-12px leading-16px text-t-tertiary'>{t('requirements.subtitle')}</div>
    </div>
  );

  return (
    <div className='relative flex size-full min-h-0'>
      <ContentSider
        width={resize.splitRatio}
        header={siderHeader}
        ariaLabel={t('requirements.title')}
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
      <div className='flex-1 min-w-0 min-h-0 overflow-y-auto' role='tabpanel' aria-label={t('requirements.title')} ref={paneRef}>
        <div className={classNames('mx-auto w-full max-w-1100px box-border h-full pt-32px pb-4px', panePadX)}>
          <Outlet />
        </div>
      </div>
    </div>
  );
};

export default RequirementsLayout;
