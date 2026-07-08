/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import { Tooltip } from '@arco-design/web-react';
import { ImageFiles } from '@icon-park/react';
import classNames from 'classnames';
import type { SiderTooltipProps } from '@renderer/utils/ui/siderTooltip';

interface SiderAssetLibraryEntryProps {
  isMobile: boolean;
  isActive: boolean;
  collapsed: boolean;
  siderTooltipProps: SiderTooltipProps;
  onClick: () => void;
}

/**
 * SiderAssetLibraryEntry — the 数据空间 (Data Space) group's Asset Library
 * destination (`/assets`). Mirrors the sibling entries' collapsed/expanded
 * layout and active styling; no badge machinery.
 */
const SiderAssetLibraryEntry: React.FC<SiderAssetLibraryEntryProps> = ({
  isMobile,
  isActive,
  collapsed,
  siderTooltipProps,
  onClick,
}) => {
  const { t } = useTranslation();

  if (collapsed) {
    return (
      <Tooltip {...siderTooltipProps} content={t('assetLibrary.title')} position='right'>
        <div
          className={classNames(
            'w-full h-34px flex items-center justify-center cursor-pointer transition-colors rd-8px text-t-primary',
            isActive ? '!bg-primary-1 !text-primary-6' : 'hover:bg-fill-2 active:bg-fill-3'
          )}
          onClick={onClick}
        >
          <span className='relative block leading-none shrink-0' style={{ lineHeight: 0 }}>
            <ImageFiles theme='outline' size='20' fill='currentColor' className='block leading-none' />
          </span>
        </div>
      </Tooltip>
    );
  }

  return (
    <Tooltip {...siderTooltipProps} content={t('assetLibrary.title')} position='right'>
      <div
        className={classNames(
          'box-border group h-34px w-full flex items-center justify-start gap-8px pl-10px pr-8px rd-0.5rem cursor-pointer shrink-0 transition-all text-t-primary',
          isMobile && 'sider-action-btn-mobile',
          isActive ? '!bg-primary-1 !text-primary-6' : 'hover:bg-fill-2 active:bg-fill-3'
        )}
        onClick={onClick}
      >
        <span className='relative size-22px flex items-center justify-center shrink-0'>
          <ImageFiles
            theme='outline'
            size='16'
            fill='currentColor'
            className='block leading-none'
            style={{ lineHeight: 0 }}
          />
        </span>
        <span className='collapsed-hidden text-14px font-[500] leading-24px'>{t('assetLibrary.title')}</span>
      </div>
    </Tooltip>
  );
};

export default SiderAssetLibraryEntry;
