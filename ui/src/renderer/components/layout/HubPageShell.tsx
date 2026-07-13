/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import classNames from 'classnames';
import React from 'react';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { SettingsViewModeProvider } from '@/renderer/components/settings/SettingsModal/settingsViewContext';

interface HubPageShellProps {
  title: string;
  subtitle?: string;
  /** Optional scope hook for page-specific visual contracts. */
  className?: string;
  /** Tailwind max-width class for the centered content column. */
  maxWidthClass?: string;
  /** Rendered between the header and the body (e.g. a segmented tab bar). */
  toolbar?: React.ReactNode;
  children: React.ReactNode;
}

/**
 * HubPageShell — shared chrome for the homepage "hub" destinations (Model
 * Management, Presets, Skills, MCP). Mirrors the scroll container + centered content
 * column of `SettingsPageWrapper`, and provides the `page` view-mode context so
 * the embedded settings content components (which were originally authored for
 * the settings modal) lay out correctly — but without the settings-specific
 * mobile top navigation.
 */
const HubPageShell: React.FC<HubPageShellProps> = ({
  title,
  subtitle,
  className,
  maxWidthClass = 'md:max-w-1100px',
  toolbar,
  children,
}) => {
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;

  return (
    <SettingsViewModeProvider value='page'>
      <div
        className={classNames(
          'w-full min-h-full box-border overflow-y-auto',
          className,
          isMobile ? 'px-16px py-16px' : 'px-12px md:px-40px py-32px'
        )}
      >
        <div className={classNames('mx-auto w-full', maxWidthClass)}>
          <div className='mb-18px'>
            <div className='text-22px font-600 text-t-primary leading-tight'>{title}</div>
            {subtitle && <div className='mt-6px text-13px leading-18px text-t-tertiary'>{subtitle}</div>}
          </div>
          {toolbar && <div className='mb-20px'>{toolbar}</div>}
          {children}
        </div>
      </div>
    </SettingsViewModeProvider>
  );
};

export default HubPageShell;
