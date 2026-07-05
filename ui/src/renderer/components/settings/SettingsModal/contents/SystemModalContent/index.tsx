/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { IGpuStatus, IStartOnBootStatus } from '@/common/adapter/ipcBridge';
import { configService } from '@/common/config/configService';
import NomiScrollArea from '@/renderer/components/base/NomiScrollArea';
import NomiSelect from '@/renderer/components/base/NomiSelect';
import FeedbackButton from '@/renderer/components/base/FeedbackButton';
import LanguageSwitcher from '@/renderer/components/settings/LanguageSwitcher';
import { iconColors } from '@/renderer/styles/colors';
import { isElectronDesktop } from '@/renderer/utils/platform';
import { useKeepAwake } from '@renderer/hooks/ui/useKeepAwake';
import { Alert, Button, Collapse, Form, Message, Modal, Switch, Tooltip } from '@arco-design/web-react';
import { FolderSearch } from '@icon-park/react';
import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import useSWR from 'swr';
import { useSettingsViewMode } from '../../settingsViewContext';
import DevSettings from './DevSettings';
import DirInputItem from './DirInputItem';
import FactoryResetModal from './FactoryResetModal';
import PreferenceRow from './PreferenceRow';

/**
 * System settings content component
 *
 * Provides system-level configuration options including language, directory config,
 * and developer tools (dev mode only).
 */
const SystemModalContent: React.FC = () => {
  const { t } = useTranslation();
  const isDesktop = isElectronDesktop();
  const [form] = Form.useForm();
  // arco types Modal.useModal() methods (confirm/info/...) as optional even
  // though the hook always supplies them; assert the non-optional shape so
  // `modal.confirm(...)` doesn't trip TS2722.
  const [modalRaw, modalContextHolder] = Modal.useModal();
  const modal = modalRaw as Required<typeof modalRaw>;
  const [error, setError] = useState<string | null>(null);
  const viewMode = useSettingsViewMode();
  const isPageMode = viewMode === 'page';
  const initializingRef = useRef(true);

  const [startOnBoot, setStartOnBoot] = useState<IStartOnBootStatus>({
    supported: false,
    enabled: false,
    isPackaged: false,
    platform: 'web',
  });
  const [gpuStatus, setGpuStatus] = useState<IGpuStatus | null>(null);
  const [notificationEnabled, setNotificationEnabled] = useState(true);
  const [cronNotificationEnabled, setCronNotificationEnabled] = useState(false);
  const [saveUploadToWorkspace, setSaveUploadToWorkspace] = useState(false);
  const [autoPreviewOfficeFiles, setAutoPreviewOfficeFiles] = useState(true);
  const [sendKey, setSendKey] = useState<'enter' | 'mod-enter'>('enter');
  const [factoryResetVisible, setFactoryResetVisible] = useState(false);

  useEffect(() => {
    if (!isDesktop) {
      return;
    }

    ipcBridge.application.getStartOnBootStatus
      .invoke()
      .then((result) => {
        if (result.success && result.data) {
          setStartOnBoot(result.data);
        }
      })
      .catch(() => {});

    ipcBridge.application.getGpuStatus
      .invoke()
      .then((result) => {
        if (result.success && result.data) {
          setGpuStatus(result.data);
        }
      })
      .catch(() => {});
  }, [isDesktop]);

  useEffect(() => {
    setNotificationEnabled(configService.get('system.notificationEnabled') ?? true);
    setCronNotificationEnabled(configService.get('system.cronNotificationEnabled') ?? false);
    setSaveUploadToWorkspace(configService.get('upload.saveToWorkspace') ?? false);
    setAutoPreviewOfficeFiles(configService.get('system.autoPreviewOfficeFiles') ?? true);
    setSendKey(configService.get('chat.sendKey') ?? 'enter');
  }, [isDesktop]);

  const handleHardwareAccelerationChange = useCallback(
    (checked: boolean) => {
      const previous = gpuStatus;
      const optimistic: IGpuStatus = {
        userOverride: checked ? 'force-on' : 'force-off',
        autoDisabled: false,
        crashCount: 0,
        lastCrashAt: gpuStatus?.lastCrashAt ?? null,
      };
      setGpuStatus(optimistic);

      const apply = () => {
        ipcBridge.application.setGpuOverride
          .invoke({ override: checked ? 'force-on' : 'force-off' })
          .then((result) => {
            if (result.success && result.data) {
              setGpuStatus(result.data);
              ipcBridge.application.restart.invoke().catch(() => {});
            } else {
              setGpuStatus(previous);
              Message.error(t('settings.hardwareAccelerationUpdateFailed'));
            }
          })
          .catch(() => {
            setGpuStatus(previous);
            Message.error(t('settings.hardwareAccelerationUpdateFailed'));
          });
      };

      modal.confirm({
        title: t('settings.updateConfirm'),
        content: t('settings.hardwareAccelerationRestartConfirm'),
        onOk: apply,
        onCancel: () => setGpuStatus(previous),
      });
    },
    [gpuStatus, modal, t]
  );

  const handleStartOnBootChange = useCallback(
    (checked: boolean) => {
      const previousStatus = startOnBoot;
      setStartOnBoot((prev) => ({ ...prev, enabled: checked }));

      ipcBridge.application.setStartOnBoot
        .invoke({ enabled: checked })
        .then((result) => {
          if (result.success && result.data) {
            setStartOnBoot(result.data);
            return;
          }

          setStartOnBoot(previousStatus);
          Message.error(result.msg || t('settings.startOnBootUpdateFailed'));
        })
        .catch(() => {
          setStartOnBoot(previousStatus);
          Message.error(t('settings.startOnBootUpdateFailed'));
        });
    },
    [startOnBoot, t]
  );

  const handleNotificationEnabledChange = useCallback((checked: boolean) => {
    setNotificationEnabled(checked);
    configService.set('system.notificationEnabled', checked).catch(() => {
      setNotificationEnabled(!checked);
      configService.setLocal('system.notificationEnabled', !checked);
    });
  }, []);

  const handleCronNotificationEnabledChange = useCallback((checked: boolean) => {
    setCronNotificationEnabled(checked);
    configService.set('system.cronNotificationEnabled', checked).catch(() => {
      setCronNotificationEnabled(!checked);
      configService.setLocal('system.cronNotificationEnabled', !checked);
    });
  }, []);

  const handleSaveUploadToWorkspaceChange = useCallback((checked: boolean) => {
    setSaveUploadToWorkspace(checked);
    configService.set('upload.saveToWorkspace', checked).catch(() => {
      setSaveUploadToWorkspace(!checked);
      configService.setLocal('upload.saveToWorkspace', !checked);
    });
  }, []);

  const handleAutoPreviewOfficeFilesChange = useCallback((checked: boolean) => {
    setAutoPreviewOfficeFiles(checked);
    configService.set('system.autoPreviewOfficeFiles', checked).catch(() => {
      setAutoPreviewOfficeFiles(!checked);
      configService.setLocal('system.autoPreviewOfficeFiles', !checked);
    });
  }, []);

  const handleSendKeyChange = useCallback((value: 'enter' | 'mod-enter') => {
    setSendKey(value);
    configService.set('chat.sendKey', value).catch(() => {
      const fallback = value === 'enter' ? 'mod-enter' : 'enter';
      setSendKey(fallback);
      configService.setLocal('chat.sendKey', fallback);
    });
  }, []);

  const { keepAwake, setKeepAwake: applyKeepAwake } = useKeepAwake();

  const handleKeepAwakeChange = useCallback(async (checked: boolean) => {
    try { await applyKeepAwake(checked); } catch (err) { Message.error(String(err)); }
  }, [applyKeepAwake]);

  // Get system directory info
  const { data: systemInfo } = useSWR('system.dir.info', () => ipcBridge.application.systemInfo.invoke());

  const handleOpenLogDir = useCallback(() => {
    if (!systemInfo?.logDir) return;
    void ipcBridge.shell.openFolderWith
      .invoke({ folder_path: systemInfo.logDir, tool: 'explorer' })
      .catch((caughtError) => {
        console.error('[SystemModalContent] Failed to open log directory:', caughtError);
      });
  }, [systemInfo?.logDir]);

  // Initialize form data
  useEffect(() => {
    if (systemInfo) {
      initializingRef.current = true;
      form.setFieldsValue({ workDir: systemInfo.workDir });
      requestAnimationFrame(() => {
        initializingRef.current = false;
      });
    }
  }, [systemInfo, form]);

  const preferenceItems = [
    { key: 'language', label: t('settings.language'), component: <LanguageSwitcher /> },
    {
      key: 'sendKey',
      label: t('settings.sendKey'),
      description: t('settings.sendKeyDesc'),
      component: (
        <NomiSelect
          className='w-200px'
          value={sendKey}
          onChange={(v) => handleSendKeyChange(v as 'enter' | 'mod-enter')}
        >
          <NomiSelect.Option value='enter'>{t('settings.sendKeyEnter')}</NomiSelect.Option>
          <NomiSelect.Option value='mod-enter'>{t('settings.sendKeyModEnter')}</NomiSelect.Option>
        </NomiSelect>
      ),
    },
    {
      key: 'startOnBoot',
      label: t('settings.startOnBoot'),
      description: startOnBoot.supported ? t('settings.startOnBootDesc') : t('settings.startOnBootUnsupported'),
      component: (
        <Switch checked={startOnBoot.enabled} onChange={handleStartOnBootChange} disabled={!startOnBoot.supported} />
      ),
    },
    {
      key: 'keepAwake',
      label: t('settings.keepAwake'),
      description: t('settings.keepAwakeDesc'),
      component: <Switch checked={keepAwake} onChange={handleKeepAwakeChange} />,
    },
    ...(isDesktop && gpuStatus
      ? [
          {
            key: 'hardwareAcceleration',
            label: t('settings.hardwareAcceleration'),
            description: gpuStatus.autoDisabled
              ? t('settings.hardwareAccelerationAutoDisabled')
              : t('settings.hardwareAccelerationDesc'),
            component: (
              <Switch
                checked={gpuStatus.userOverride !== 'force-off' && !gpuStatus.autoDisabled}
                onChange={handleHardwareAccelerationChange}
              />
            ),
          },
        ]
      : []),
    {
      key: 'saveUploadToWorkspace',
      label: t('settings.saveUploadToWorkspace'),
      component: <Switch checked={saveUploadToWorkspace} onChange={handleSaveUploadToWorkspaceChange} />,
    },
    {
      key: 'autoPreviewOfficeFiles',
      label: t('settings.autoPreviewOfficeFiles'),
      description: t('settings.autoPreviewOfficeFilesDesc'),
      component: <Switch checked={autoPreviewOfficeFiles} onChange={handleAutoPreviewOfficeFilesChange} />,
    },
  ];

  const saveDirConfigValidate = (_values: { workDir: string }): Promise<unknown> => {
    return new Promise((resolve, reject) => {
      modal.confirm({
        title: t('settings.updateConfirm'),
        content: t('settings.restartConfirm'),
        onOk: resolve,
        onCancel: reject,
      });
    });
  };

  const savingRef = useRef(false);

  const handleValuesChange = useCallback(
    async (_changedValue: unknown, allValues: Record<string, string>) => {
      if (initializingRef.current || savingRef.current || !systemInfo) return;
      const { workDir } = allValues;
      const needsRestart = workDir !== systemInfo.workDir;
      if (!needsRestart) return;

      savingRef.current = true;
      setError(null);
      try {
        // Confirm, then persist. A failure (or cancel) here means nothing was
        // written, so reverting the field to the current value is correct.
        try {
          await saveDirConfigValidate({ workDir });
          // Pass systemInfo.cacheDir as-is: cacheDir is no longer user-editable
          // (removed from UI), but the backend IPC interface still expects it.
          // Passing the current value ensures existing custom paths are preserved.
          await ipcBridge.application.updateSystemInfo.invoke({ cacheDir: systemInfo.cacheDir, workDir });
        } catch (persistError: unknown) {
          form.setFieldValue('workDir', systemInfo.workDir);
          if (persistError) {
            setError(persistError instanceof Error ? persistError.message : String(persistError));
          }
          return;
        }
        // Persisted: the new dir is now authoritative and applies on the next
        // boot. Relaunch applies it immediately (and never returns). If relaunch
        // instead throws, do NOT revert the field — the on-disk config already
        // holds the new dir, so reverting would make the UI contradict reality;
        // surface the error and let the user restart manually.
        try {
          await ipcBridge.application.restart.invoke();
        } catch (restartError: unknown) {
          if (restartError) {
            setError(restartError instanceof Error ? restartError.message : String(restartError));
          }
        }
      } finally {
        savingRef.current = false;
      }
    },
    [systemInfo, form, saveDirConfigValidate]
  );

  return (
    <div className='flex flex-col h-full w-full'>
      {modalContextHolder}

      <NomiScrollArea className='flex-1 min-h-0 pb-16px' disableOverflow={isPageMode}>
        <div className='space-y-16px'>
          <div className='px-[12px] md:px-[32px] py-16px bg-2 rd-16px space-y-12px'>
            <div className='w-full flex flex-col divide-y divide-border-2'>
              {preferenceItems.map((item) => (
                <PreferenceRow key={item.key} label={item.label} description={item.description}>
                  {item.component}
                </PreferenceRow>
              ))}
            </div>
            {/* Notification settings with collapsible sub-options */}
            <Collapse
              bordered={false}
              activeKey={notificationEnabled ? ['notification'] : []}
              onChange={(_, keys) => {
                const shouldExpand = (keys as string[]).includes('notification');
                if (shouldExpand && !notificationEnabled) {
                  handleNotificationEnabledChange(true);
                } else if (!shouldExpand && notificationEnabled) {
                  handleNotificationEnabledChange(false);
                }
              }}
              className='[&_.arco-collapse-item]:!border-none [&_.arco-collapse-item-header]:!px-0 [&_.arco-collapse-item-header-title]:!flex-1 [&_.arco-collapse-item-content-box]:!px-0 [&_.arco-collapse-item-content-box]:!pb-0'
            >
              <Collapse.Item
                name='notification'
                showExpandIcon={false}
                header={
                  <div className='flex flex-1 items-center justify-between w-full'>
                    <span className='text-14px text-2 ml-12px'>{t('settings.notification')}</span>
                    <Switch
                      checked={notificationEnabled}
                      onClick={(e) => e.stopPropagation()}
                      onChange={handleNotificationEnabledChange}
                    />
                  </div>
                }
              >
                <div className='pl-12px'>
                  <PreferenceRow label={t('settings.cronNotificationEnabled')}>
                    <Switch
                      checked={cronNotificationEnabled}
                      disabled={!notificationEnabled}
                      onChange={handleCronNotificationEnabledChange}
                    />
                  </PreferenceRow>
                </div>
              </Collapse.Item>
            </Collapse>
            <Form form={form} layout='vertical' className='!mt-32px space-y-16px' onValuesChange={handleValuesChange}>
              <DirInputItem label={t('settings.workDir')} field='workDir' />
              {/* Log directory (read-only, click to open in file manager) */}
              <div>
                <Form.Item label={t('settings.logDir')}>
                  <div className='nomi-dir-input h-[32px] flex items-center rounded-8px border border-solid border-transparent pl-14px bg-[var(--fill-0)] '>
                    <Tooltip content={systemInfo?.logDir || ''} position='top'>
                      <div className='flex-1 min-w-0 text-13px text-t-primary truncate'>{systemInfo?.logDir || ''}</div>
                    </Tooltip>
                    <Button
                      type='text'
                      style={{ borderLeft: '1px solid var(--color-border-2)', borderRadius: '0 8px 8px 0' }}
                      icon={<FolderSearch theme='outline' size='18' fill={iconColors.primary} />}
                      onClick={(e) => {
                        e.stopPropagation();
                        handleOpenLogDir();
                      }}
                    />
                  </div>
                </Form.Item>
              </div>
              {error && (
                <Alert
                  className='mt-16px'
                  type='error'
                  content={
                    <span>
                      {typeof error === 'string' ? error : JSON.stringify(error)}
                      <FeedbackButton module='system-settings' className='ml-6px' />
                    </span>
                  }
                />
              )}
            </Form>
          </div>

          {/* Developer settings: DevTools + CDP (only visible in dev mode) */}
          <DevSettings />

          {/* Danger zone: factory reset (clears the database + derived data) */}
          <div className='px-[12px] md:px-[32px] py-16px bg-2 rd-16px space-y-12px'>
            <div className='text-13px font-600 text-[rgb(var(--danger-6))]'>{t('settings.factoryReset.dangerZone')}</div>
            <div className='flex items-center justify-between gap-12px flex-wrap'>
              <div className='flex-1 min-w-200px'>
                <div className='text-14px text-t-primary'>{t('settings.factoryReset.title')}</div>
                <div className='text-12px text-t-secondary mt-2px leading-20px'>{t('settings.factoryReset.rowDesc')}</div>
              </div>
              <Button status='danger' onClick={() => setFactoryResetVisible(true)}>
                {t('settings.factoryReset.button')}
              </Button>
            </div>
          </div>
        </div>
      </NomiScrollArea>

      <FactoryResetModal visible={factoryResetVisible} onClose={() => setFactoryResetVisible(false)} />
    </div>
  );
};

export default SystemModalContent;
