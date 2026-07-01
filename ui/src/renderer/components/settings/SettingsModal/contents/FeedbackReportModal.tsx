/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import ModalWrapper from '@renderer/components/base/ModalWrapper';
import { openExternalUrl } from '@renderer/utils/platform';
import CopyIconButton from '@/renderer/components/base/CopyIconButton';
import { Link } from '@icon-park/react';
import { Tooltip } from '@arco-design/web-react';
import React, { useCallback } from 'react';
import { useTranslation } from 'react-i18next';

export const NOMIFUN_PUBLIC_LINKS = {
  repository: 'https://github.com/nomifun/nomifun-tauri',
  officialWebsite: 'https://www.nomifun.com',
  contact: 'https://www.nomifun.com/contact',
  issues: 'https://github.com/nomifun/nomifun-tauri/issues',
  releases: 'https://github.com/nomifun/nomifun-tauri/releases',
  baiduPan: 'https://pan.baidu.com/s/5GPonoJNrwJ7GciBSDgXLaA',
  email: '535526063@qq.com',
  emailHref: 'mailto:535526063@qq.com',
} as const;

const COPYRIGHT = '© 2025-2026 NomiFun · www.nomifun.com';

const CONTACT_ITEMS = [
  {
    labelKey: 'settings.contactAddress',
    fallbackLabel: '联系地址',
    value: NOMIFUN_PUBLIC_LINKS.contact,
    url: NOMIFUN_PUBLIC_LINKS.contact,
    copyValue: NOMIFUN_PUBLIC_LINKS.contact,
  },
  {
    labelKey: 'settings.githubIssues',
    fallbackLabel: 'GitHub Issues',
    value: NOMIFUN_PUBLIC_LINKS.issues,
    url: NOMIFUN_PUBLIC_LINKS.issues,
    copyValue: NOMIFUN_PUBLIC_LINKS.issues,
  },
  {
    labelKey: 'settings.officialWebsite',
    fallbackLabel: '官网',
    value: NOMIFUN_PUBLIC_LINKS.officialWebsite,
    url: NOMIFUN_PUBLIC_LINKS.officialWebsite,
    copyValue: NOMIFUN_PUBLIC_LINKS.officialWebsite,
  },
  {
    labelKey: 'settings.contactEmail',
    fallbackLabel: '邮箱',
    value: NOMIFUN_PUBLIC_LINKS.email,
    url: NOMIFUN_PUBLIC_LINKS.emailHref,
    copyValue: NOMIFUN_PUBLIC_LINKS.email,
    trailingKey: 'settings.contactEmailPending',
    trailingFallback: '、（待补充中）……',
  },
] as const;

// 以下导出类型与 props 形状保持不变，以兼容现有调用方（FeedbackButton / 一键反馈入口等）。
export type PrefilledScreenshot = {
  filename: string;
  data: Uint8Array;
  type: string;
};

export type FeedbackEventTags = Record<string, string>;
export type FeedbackEventExtra = Record<string, unknown>;

type FeedbackReportModalProps = {
  visible: boolean;
  onCancel: () => void;
  defaultModule?: string;
  prefilledScreenshots?: PrefilledScreenshot[];
  feedbackTags?: FeedbackEventTags;
  feedbackExtra?: FeedbackEventExtra;
};

/**
 * “联系我们”面板：不再在客户端收集/上报反馈，仅展示官方联系渠道。
 */
const FeedbackReportModal: React.FC<FeedbackReportModalProps> = ({ visible, onCancel }) => {
  const { t } = useTranslation();

  const openContactPage = useCallback(() => {
    void openExternalUrl(NOMIFUN_PUBLIC_LINKS.contact).catch((e) => console.error('open contact page failed', e));
  }, []);

  const openContactTarget = useCallback((url: string) => {
    void openExternalUrl(url).catch((e) => console.error('open contact target failed', e));
  }, []);

  return (
    <ModalWrapper
      title={t('settings.contactTitle')}
      visible={visible}
      onCancel={onCancel}
      onOk={openContactPage}
      okText={t('settings.contactOpenContactPage')}
      cancelText={t('settings.bugReportCancel')}
      alignCenter
      className='w-[min(460px,calc(100vw-32px))] max-w-460px rd-16px'
      autoFocus={false}
      wrapStyle={{ zIndex: 1050 }}
      maskStyle={{ zIndex: 1050 }}
    >
      <div className='px-24px pb-8px pt-2px'>
        <p className='m-0 text-13px leading-20px text-t-secondary'>
          {t('settings.contactDescription')}
        </p>
        <div className='mt-16px overflow-hidden rd-10px border border-border-2 bg-bg-1'>
          {CONTACT_ITEMS.map((item) => (
            <div
              key={item.labelKey}
              className='group flex min-h-48px items-center gap-12px border-b border-border-2 px-14px py-10px last:border-b-0 hover:bg-fill-1'
            >
              <div className='w-88px shrink-0 text-13px font-500 text-t-primary'>
                {t(item.labelKey, item.fallbackLabel)}
              </div>
              <button
                type='button'
                className='min-w-0 flex-1 border-0 bg-transparent p-0 text-left text-13px leading-19px text-t-secondary transition-colors hover:text-primary-6'
                onClick={() => openContactTarget(item.url)}
              >
                <span className='break-all'>
                  {item.value}
                  {'trailingKey' in item ? t(item.trailingKey, item.trailingFallback) : ''}
                </span>
              </button>
              <CopyIconButton
                text={item.copyValue}
                className='h-26px w-26px shrink-0 opacity-70 hover:bg-fill-2 group-hover:opacity-100'
              />
              <Tooltip content={t('settings.openLink', '打开链接')} position='top' mini>
                <button
                  type='button'
                  aria-label={t('settings.openLink', '打开链接')}
                  className='inline-flex h-26px w-26px shrink-0 items-center justify-center rd-4px border-0 bg-transparent p-0 text-t-tertiary opacity-70 transition-colors hover:bg-fill-2 hover:text-t-primary group-hover:opacity-100'
                  onClick={() => openContactTarget(item.url)}
                >
                  <Link theme='outline' size='14' fill='currentColor' />
                </button>
              </Tooltip>
            </div>
          ))}
        </div>
        <div className='mt-12px text-center text-12px text-t-tertiary'>{COPYRIGHT}</div>
      </div>
    </ModalWrapper>
  );
};

export default FeedbackReportModal;
