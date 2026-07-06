/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { Button, Popover, Switch } from '@arco-design/web-react';
import { CheckOne, Down } from '@icon-park/react';
import React from 'react';
import { useTranslation } from 'react-i18next';
import { iconColors } from '@/renderer/styles/colors';

export type GuidClusterApprovalMode = 'auto' | 'manual';

const GuidClusterApprovalSelector: React.FC<{
  value: GuidClusterApprovalMode;
  onChange: (next: GuidClusterApprovalMode) => void;
}> = ({ value, onChange }) => {
  const { t } = useTranslation();
  const manual = value === 'manual';

  const content = (
    <div className='flex w-220px items-start justify-between gap-12px py-2px'>
      <div className='flex min-w-0 flex-col gap-2px'>
        <span className='text-13px font-600 text-t-primary'>
          {t('guid.orchestration.approval.title', { defaultValue: '审批模式' })}
        </span>
        <span className='text-11px leading-16px text-t-tertiary'>
          {t('guid.orchestration.approval.desc', { defaultValue: '关键决策先暂停，等你确认。' })}
        </span>
      </div>
      <Switch size='small' checked={manual} onChange={(checked) => onChange(checked ? 'manual' : 'auto')} />
    </div>
  );

  return (
    <Popover content={content} trigger='click' position='top' unmountOnExit>
      <Button
        className='sendbox-model-btn guid-config-btn'
        shape='round'
        size='small'
        data-testid='guid-cluster-approval-selector'
      >
        <span className='flex items-center gap-6px min-w-0'>
          <CheckOne theme='outline' size='14' fill={iconColors.secondary} className='shrink-0' />
          <span className='truncate'>
            {manual
              ? t('guid.orchestration.approval.manual', { defaultValue: '审批 · 确认' })
              : t('guid.orchestration.approval.auto', { defaultValue: '审批 · 自动' })}
          </span>
          <Down theme='outline' size='12' fill={iconColors.secondary} className='shrink-0' />
        </span>
      </Button>
    </Popover>
  );
};

export default GuidClusterApprovalSelector;
