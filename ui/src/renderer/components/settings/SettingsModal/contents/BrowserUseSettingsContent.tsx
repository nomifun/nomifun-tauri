/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { configService } from '@/common/config/configService';
import { ipcBridge } from '@/common';
import NomiScrollArea from '@/renderer/components/base/NomiScrollArea';
import { Alert, Button, Message, Modal, Radio, Switch } from '@arco-design/web-react';
import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSettingsViewMode } from '../settingsViewContext';
import PreferenceRow from './SystemModalContent/PreferenceRow';

const RadioGroup = Radio.Group;

type BrowserSource = 'managed' | 'system';

const BrowserUseSettingsContent: React.FC = () => {
  const { t } = useTranslation();
  const viewMode = useSettingsViewMode();
  const isPageMode = viewMode === 'page';
  const [browserUse, setBrowserUse] = useState(false);
  const [silent, setSilent] = useState(true);
  const [source, setSource] = useState<BrowserSource>('managed');
  const [persistentLogin, setPersistentLogin] = useState(true);
  const [fullPower, setFullPower] = useState(false);
  const [siteMemory, setSiteMemory] = useState(false);
  const [takeover, setTakeover] = useState(true);
  const [unrestrictedApproval, setUnrestrictedApproval] = useState(false);
  const [visualFallback, setVisualFallback] = useState(false);
  // Phase 2b「登录我的浏览器」:是否有可见登录窗口开着 + 操作进行中。
  const [loginOpen, setLoginOpen] = useState(false);
  const [loginBusy, setLoginBusy] = useState(false);

  useEffect(() => {
    const storedPersistentLogin = configService.get('agent.browserUse.persistentLogin') ?? true;
    const storedFullPower = configService.get('agent.browserUse.fullPower') ?? false;

    setBrowserUse(configService.get('agent.browserUse') ?? true);
    // 「浏览器模式」两开关：默认静默(ON)、来源默认内置(managed)。
    setSilent(configService.get('agent.browserUse.silent') ?? true);
    setSource((configService.get('agent.browserUse.source') as BrowserSource) ?? 'managed');
    setPersistentLogin(storedPersistentLogin);
    setFullPower(storedPersistentLogin ? false : storedFullPower);
    setSiteMemory(configService.get('agent.browserUse.siteMemory') ?? false);
    setTakeover(configService.get('agent.browserUse.takeover') ?? true);
    setUnrestrictedApproval(configService.get('agent.browserUse.unrestrictedApproval') ?? false);
    setVisualFallback(configService.get('agent.browserUse.visualFallback') ?? false);

    if (storedPersistentLogin && storedFullPower) {
      configService.set('agent.browserUse.fullPower', false).catch(() => {});
    }
  }, []);

  // Phase 2b: reflect whether a login browser is already open (global singleton).
  useEffect(() => {
    let cancelled = false;
    ipcBridge.browserLogin.status
      .invoke()
      .then((res) => {
        if (!cancelled && res) setLoginOpen(!!res.active);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  // 「登录我的浏览器」:未开 → 拉起可见登录窗口(用当前来源);已开 → 关闭并备份登录态。
  const handleLoginToggle = useCallback(async () => {
    if (loginBusy) return;
    setLoginBusy(true);
    try {
      if (loginOpen) {
        const res = await ipcBridge.browserLogin.close.invoke();
        setLoginOpen(res ? !!res.active : false);
      } else {
        const res = await ipcBridge.browserLogin.open.invoke({ source });
        setLoginOpen(res ? !!res.active : false);
        if (res && !res.active && (res.message || '').startsWith('launch_failed')) {
          Message.error(t('settings.browserLoginFailed'));
        } else if (res && res.active) {
          Message.info(t('settings.browserLoginOpenedHint'));
        }
      }
    } catch {
      Message.error(t('settings.browserLoginFailed'));
    } finally {
      setLoginBusy(false);
    }
  }, [loginBusy, loginOpen, source, t]);

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

  // 后台静默运行（可见性维度）：ON=headless（无窗口）；OFF=弹出可见窗口。
  const handleSilentChange = useCallback(
    (checked: boolean) => {
      setSilent(checked);
      persistBoolean('agent.browserUse.silent', checked, () => setSilent(!checked));
    },
    [persistBoolean]
  );

  // 浏览器来源（与静默正交）：'managed'=内置/下载 CfT；'system'=系统 Chrome/Edge 本体。
  const handleSourceChange = useCallback(
    (value: string) => {
      const next: BrowserSource = value === 'system' ? 'system' : 'managed';
      setSource((prev) => {
        configService.set('agent.browserUse.source', next).catch(() => {
          setSource(prev);
          configService.setLocal('agent.browserUse.source', prev);
        });
        return next;
      });
    },
    []
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

  const handleUnrestrictedApprovalChange = useCallback(
    (checked: boolean) => {
      if (!checked) {
        setUnrestrictedApproval(false);
        persistBoolean('agent.browserUse.unrestrictedApproval', false, () => setUnrestrictedApproval(true));
        return;
      }

      Modal.confirm({
        title: t('settings.browserUnrestrictedApprovalConfirmTitle'),
        content: t('settings.browserUnrestrictedApprovalConfirmContent'),
        okText: t('settings.browserUnrestrictedApprovalConfirmOk'),
        onOk: () => {
          setUnrestrictedApproval(true);
          persistBoolean('agent.browserUse.unrestrictedApproval', true, () => setUnrestrictedApproval(false));
        },
      });
    },
    [persistBoolean, t]
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
              <PreferenceRow label={t('settings.browserSource')} description={t('settings.browserSourceDesc')}>
                <RadioGroup type='button' value={source} disabled={!browserUse} onChange={handleSourceChange}>
                  <Radio value='managed'>{t('settings.browserSourceManaged')}</Radio>
                  <Radio value='system'>{t('settings.browserSourceSystem')}</Radio>
                </RadioGroup>
              </PreferenceRow>
              <PreferenceRow label={t('settings.browserSilent')} description={t('settings.browserSilentDesc')}>
                <Switch checked={silent} disabled={!browserUse} onChange={handleSilentChange} />
              </PreferenceRow>
              <PreferenceRow label={t('settings.browserLogin')} description={t('settings.browserLoginDesc')}>
                <Button size='small' loading={loginBusy} disabled={!browserUse} onClick={handleLoginToggle}>
                  {loginOpen ? t('settings.browserLoginClose') : t('settings.browserLoginOpen')}
                </Button>
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
                label={t('settings.browserUnrestrictedApproval')}
                description={t('settings.browserUnrestrictedApprovalDesc')}
              >
                <Switch
                  checked={unrestrictedApproval}
                  disabled={!browserUse}
                  onChange={handleUnrestrictedApprovalChange}
                />
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
