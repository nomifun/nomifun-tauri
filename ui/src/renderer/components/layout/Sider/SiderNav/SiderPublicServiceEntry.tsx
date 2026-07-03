/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import { Tooltip } from '@arco-design/web-react';
import { Headset } from '@icon-park/react';
import classNames from 'classnames';
import type { SiderTooltipProps } from '@renderer/utils/ui/siderTooltip';

interface SiderPublicServiceEntryProps {
  isMobile: boolean;
  isActive: boolean;
  collapsed: boolean;
  siderTooltipProps: SiderTooltipProps;
  onClick: () => void;
}

/** 对外伙伴 (Public Companion) — top-level rail entry under the 对外服务 group. Mirrors SiderNomiEntry. */
const SiderPublicServiceEntry: React.FC<SiderPublicServiceEntryProps> = ({
  isMobile,
  isActive,
  collapsed,
  siderTooltipProps,
  onClick,
}) => {
  const { t } = useTranslation();
  const title = t('publicCompanion.siderTitle', { defaultValue: '对外伙伴' });

  if (collapsed) {
    return (
      <Tooltip {...siderTooltipProps} content={title} position='right'>
        <div
          className={classNames(
            'w-full h-34px flex items-center justify-center cursor-pointer transition-colors rd-8px text-t-primary',
            isActive ? '!bg-primary-1 !text-primary-6' : 'hover:bg-fill-2 active:bg-fill-3'
          )}
          onClick={onClick}
        >
          <Headset
            theme='outline'
            size='20'
            fill='currentColor'
            className='block leading-none shrink-0'
            style={{ lineHeight: 0 }}
          />
        </div>
      </Tooltip>
    );
  }

  return (
    <Tooltip {...siderTooltipProps} content={title} position='right'>
      <div
        className={classNames(
          'box-border group h-34px w-full flex items-center justify-start gap-8px pl-10px pr-8px rd-0.5rem cursor-pointer shrink-0 transition-all text-t-primary',
          isMobile && 'sider-action-btn-mobile',
          isActive ? '!bg-primary-1 !text-primary-6' : 'hover:bg-fill-2 active:bg-fill-3'
        )}
        onClick={onClick}
      >
        <span className='size-22px flex items-center justify-center shrink-0'>
          <Headset theme='outline' size='16' fill='currentColor' className='block leading-none' style={{ lineHeight: 0 }} />
        </span>
        <span className='collapsed-hidden text-14px font-[500] leading-24px'>{title}</span>
      </div>
    </Tooltip>
  );
};

export default SiderPublicServiceEntry;
