/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import { Divider, Typography, Button } from '@arco-design/web-react';
import { Download, Github, Refresh, Right } from '@icon-park/react';
import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import classNames from 'classnames';
import { useSettingsViewMode } from '../settingsViewContext';
import { isDesktopShell, openExternalUrl } from '@/renderer/utils/platform';
import { httpGet } from '@/common/adapter/httpBridge';
import { NOMIFUN_PUBLIC_LINKS } from './FeedbackReportModal';

// Real app version from the backend `/health` endpoint (public, no auth). The
// version there is `CARGO_PKG_VERSION`, which follows the single-source
// workspace version — so it stays correct in both the desktop shell and the
// WebUI browser without a Tauri-only `getVersion()` call.
const healthGet = httpGet<{ version?: string }>('/health');

type LinkItem =
  | { title: string; detail: string; url: string; icon: React.ReactNode; onClick?: never }
  | { title: string; detail: string; onClick: () => void; icon: React.ReactNode; url?: never };

const AboutModalContent: React.FC = () => {
  const { t } = useTranslation();
  const viewMode = useSettingsViewMode();
  const isPageMode = viewMode === 'page';
  // The in-app updater only runs in the bundled desktop shell (Tauri); the WebUI
  // browser has no updater, so the check-update entry is shell-gated.
  const isDesktop = isDesktopShell();

  const [appVersion, setAppVersion] = useState('');

  useEffect(() => {
    let alive = true;
    healthGet
      .invoke()
      .then((health) => {
        if (alive && health?.version) setAppVersion(health.version);
      })
      .catch((error) => console.error('Failed to read app version:', error));
    return () => {
      alive = false;
    };
  }, []);

  const openLink = async (url: string) => {
    try {
      await openExternalUrl(url);
    } catch (error) {
      console.log('Failed to open link:', error);
    }
  };

  const checkUpdate = () => {
    // 使用 window 自定义事件在渲染进程内部通信（buildEmitter 只支持主进程->渲染进程）
    // Use window custom event for renderer-side communication (buildEmitter only works main->renderer)
    window.dispatchEvent(new CustomEvent('nomifun-open-update-modal', { detail: { source: 'about' } }));
  };

  const linkItems: LinkItem[] = [
    {
      title: t('settings.helpDocumentation'),
      detail: NOMIFUN_PUBLIC_LINKS.officialWebsite,
      url: NOMIFUN_PUBLIC_LINKS.officialWebsite,
      icon: <Right theme='outline' size='16' />,
    },
    {
      title: t('settings.updateLog'),
      detail: NOMIFUN_PUBLIC_LINKS.releases,
      url: NOMIFUN_PUBLIC_LINKS.releases,
      icon: <Right theme='outline' size='16' />,
    },
    {
      title: t('settings.bugReport'),
      detail: NOMIFUN_PUBLIC_LINKS.issues,
      url: NOMIFUN_PUBLIC_LINKS.issues,
      icon: <Right theme='outline' size='16' />,
    },
    {
      title: t('settings.contactMe'),
      detail: NOMIFUN_PUBLIC_LINKS.contact,
      url: NOMIFUN_PUBLIC_LINKS.contact,
      icon: <Right theme='outline' size='16' />,
    },
    {
      title: t('settings.officialWebsite'),
      detail: NOMIFUN_PUBLIC_LINKS.officialWebsite,
      url: NOMIFUN_PUBLIC_LINKS.officialWebsite,
      icon: <Right theme='outline' size='16' />,
    },
    {
      title: t('settings.contactEmail'),
      detail: `${NOMIFUN_PUBLIC_LINKS.email}${t('settings.contactEmailPending')}`,
      url: NOMIFUN_PUBLIC_LINKS.emailHref,
      icon: <Right theme='outline' size='16' />,
    },
  ];

  return (
    <div className='flex flex-col h-full w-full'>
      {/* Content Area */}
      <div
        className={classNames(
          'flex-1 min-h-0 overflow-y-auto overflow-x-hidden px-24px',
          isPageMode && 'px-0 overflow-visible'
        )}
      >
        <div className='flex flex-col max-w-500px mx-auto'>
          {/* App Info Section */}
          <div className='flex flex-col items-center pb-24px'>
            <Typography.Title heading={3} className='text-24px font-bold text-t-primary mb-8px'>
              NomiFun
            </Typography.Title>
            <Typography.Text className='text-14px text-t-secondary mb-12px text-center'>
              {t('settings.appDescription')}
            </Typography.Text>
            <div className='flex items-center justify-center gap-8px mb-16px'>
              <span className='px-10px py-4px rd-6px text-13px bg-fill-2 text-t-primary font-500'>
                v{appVersion || '—'}
              </span>
              <div
                className='text-t-primary cursor-pointer hover:text-t-secondary transition-colors p-4px'
                onClick={() =>
                  openLink(NOMIFUN_PUBLIC_LINKS.repository).catch((error) => console.error('Failed to open link:', error))
                }
              >
                <Github theme='outline' size='20' />
              </div>
            </div>

            {/* Check Update Section */}
            {isDesktop && (
              <div className='flex flex-wrap items-center justify-center gap-8px w-full max-w-360px bg-fill-2 p-16px rounded-lg'>
                <Button
                  type='primary'
                  onClick={checkUpdate}
                  icon={<Refresh theme='outline' size='14' />}
                  className='min-w-120px flex-1 !px-12px'
                >
                  {t('settings.checkForUpdates')}
                </Button>
                <Button
                  onClick={() =>
                    openLink(NOMIFUN_PUBLIC_LINKS.baiduPan).catch((error) =>
                      console.error('Failed to open Baidu manual download:', error)
                    )
                  }
                  icon={<Download theme='outline' size='14' />}
                  className='min-w-144px flex-1 !px-12px'
                >
                  {t('settings.baiduManualDownload')}
                </Button>
              </div>
            )}
          </div>

          {/* Divider */}
          <Divider className='my-16px' />

          {/* Links Section */}
          <div className='flex flex-col gap-4px pt-8px'>
            {linkItems.map((item, index) => (
              <div
                key={index}
                className='flex items-center justify-between px-16px py-12px rd-8px hover:bg-fill-2 transition-all cursor-pointer group'
                onClick={(e) => {
                  e.preventDefault();
                  e.stopPropagation();
                  if (item.onClick) {
                    item.onClick();
                  } else {
                    openLink(item.url).catch((error) => console.error('Failed to open link:', error));
                  }
                }}
              >
                <div className='min-w-0 pr-12px'>
                  <Typography.Text className='block text-14px text-t-primary'>{item.title}</Typography.Text>
                  <Typography.Text className='block break-all text-12px leading-18px text-t-secondary'>
                    {item.detail}
                  </Typography.Text>
                </div>
                <div className='text-t-secondary group-hover:text-t-primary transition-colors'>{item.icon}</div>
              </div>
            ))}
          </div>
        </div>
      </div>
    </div>
  );
};

export default AboutModalContent;
