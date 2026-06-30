/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import { configService } from '@/common/config/configService';
import NomiScrollArea from '@/renderer/components/base/NomiScrollArea';
import { Alert, Switch } from '@arco-design/web-react';
import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSettingsViewMode } from '../settingsViewContext';
import PreferenceRow from './SystemModalContent/PreferenceRow';

const BrowserUseSettingsContent: React.FC = () => {
  const { t } = useTranslation();
  const viewMode = useSettingsViewMode();
  const isPageMode = viewMode === 'page';
  const [browserUse, setBrowserUse] = useState(false);
  const [persistentLogin, setPersistentLogin] = useState(true);
  const [fullPower, setFullPower] = useState(false);
  const [siteMemory, setSiteMemory] = useState(false);
  const [takeover, setTakeover] = useState(false);
  const [visualFallback, setVisualFallback] = useState(false);

  useEffect(() => {
    const storedPersistentLogin = configService.get('agent.browserUse.persistentLogin') ?? true;
    const storedFullPower = configService.get('agent.browserUse.fullPower') ?? false;

    setBrowserUse(configService.get('agent.browserUse') ?? false);
    setPersistentLogin(storedPersistentLogin);
    setFullPower(storedPersistentLogin ? false : storedFullPower);
    setSiteMemory(configService.get('agent.browserUse.siteMemory') ?? false);
    setTakeover(configService.get('agent.browserUse.takeover') ?? false);
    setVisualFallback(configService.get('agent.browserUse.visualFallback') ?? false);

    if (storedPersistentLogin && storedFullPower) {
      configService.set('agent.browserUse.fullPower', false).catch(() => {});
    }
  }, []);

  const persistBoolean = useCallback(
    (key: Parameters<typeof configService.set>[0], checked: boolean, revert: () => void) => {
      configService.set(key, checked).catch(() => {
        revert();
        configService.setLocal(key, !checked);
      });
    },
    []
  );

  const handleBrowserUseChange = useCallback(
    (checked: boolean) => {
      setBrowserUse(checked);
      persistBoolean('agent.browserUse', checked, () => setBrowserUse(!checked));
    },
    [persistBoolean]
  );

  const handlePersistentLoginChange = useCallback(
    (checked: boolean) => {
      setPersistentLogin(checked);
      persistBoolean('agent.browserUse.persistentLogin', checked, () => setPersistentLogin(!checked));

      if (checked && fullPower) {
        setFullPower(false);
        configService.set('agent.browserUse.fullPower', false).catch(() => {
          setFullPower(true);
          configService.setLocal('agent.browserUse.fullPower', true);
        });
      }
    },
    [fullPower, persistBoolean]
  );

  const handleFullPowerChange = useCallback(
    (checked: boolean) => {
      setFullPower(checked);
      persistBoolean('agent.browserUse.fullPower', checked, () => setFullPower(!checked));
    },
    [persistBoolean]
  );

  const handleSiteMemoryChange = useCallback(
    (checked: boolean) => {
      setSiteMemory(checked);
      persistBoolean('agent.browserUse.siteMemory', checked, () => setSiteMemory(!checked));
    },
    [persistBoolean]
  );

  const handleTakeoverChange = useCallback(
    (checked: boolean) => {
      setTakeover(checked);
      persistBoolean('agent.browserUse.takeover', checked, () => setTakeover(!checked));
    },
    [persistBoolean]
  );

  const handleVisualFallbackChange = useCallback(
    (checked: boolean) => {
      setVisualFallback(checked);
      persistBoolean('agent.browserUse.visualFallback', checked, () => setVisualFallback(!checked));
    },
    [persistBoolean]
  );

  const fullPowerDisabled = !browserUse || persistentLogin;

  return (
    <div className='flex flex-col h-full w-full'>
      <NomiScrollArea className='flex-1 min-h-0 pb-16px' disableOverflow={isPageMode}>
        <div className='space-y-16px'>
          <div className='px-[12px] md:px-[32px] py-16px bg-2 rd-16px space-y-12px'>
            <div className='text-13px font-600 text-t-secondary'>{t('settings.browserUseSection')}</div>
            <div className='w-full flex flex-col divide-y divide-border-2'>
              <PreferenceRow label={t('settings.browserUse')} description={t('settings.browserUseDesc')}>
                <Switch checked={browserUse} onChange={handleBrowserUseChange} />
              </PreferenceRow>
              <PreferenceRow
                label={t('settings.browserPersistentLogin')}
                description={t('settings.browserPersistentLoginDesc')}
              >
                <Switch checked={persistentLogin} disabled={!browserUse} onChange={handlePersistentLoginChange} />
              </PreferenceRow>
              <PreferenceRow
                label={t('settings.browserFullPower')}
                description={
                  persistentLogin
                    ? t('settings.browserFullPowerDisabledByPersistentLogin')
                    : t('settings.browserFullPowerDesc')
                }
              >
                <Switch checked={fullPower} disabled={fullPowerDisabled} onChange={handleFullPowerChange} />
              </PreferenceRow>
              <PreferenceRow label={t('settings.browserSiteMemory')} description={t('settings.browserSiteMemoryDesc')}>
                <Switch checked={siteMemory} disabled={!browserUse} onChange={handleSiteMemoryChange} />
              </PreferenceRow>
              <PreferenceRow label={t('settings.browserTakeover')} description={t('settings.browserTakeoverDesc')}>
                <Switch checked={takeover} disabled={!browserUse} onChange={handleTakeoverChange} />
              </PreferenceRow>
              <PreferenceRow
                label={t('settings.browserVisualFallback')}
                description={t('settings.browserVisualFallbackDesc')}
              >
                <Switch checked={visualFallback} disabled={!browserUse} onChange={handleVisualFallbackChange} />
              </PreferenceRow>
            </div>
          </div>

          <Alert type='warning' showIcon content={t('settings.browserUseRiskHint')} />
        </div>
      </NomiScrollArea>
    </div>
  );
};

export default BrowserUseSettingsContent;
