/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import { WEBUI_DEFAULT_PORT } from '@/common/config/constants';
import { shell, webui } from '@/common/adapter/ipcBridge';
import { isBackendHttpError } from '@/common/adapter/httpBridge';
import NomiModal from '@/renderer/components/base/NomiModal';
import { useWebuiServer } from '@/renderer/hooks/context/WebuiServerContext';
import { Button, Form, Input, Message, Select, Switch, Tooltip } from '@arco-design/web-react';
import { Copy, Earth, EditTwo, Info, Refresh } from '@icon-park/react';
import React, { Suspense, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { buildWebuiQrLoginUrl, getWebuiQrBaseUrls } from './webuiQrLinks';

const QRCodeSVGLazy = React.lazy(async () => {
  const mod = await import('qrcode.react');
  return { default: mod.QRCodeSVG };
});

/**
 * WebuiControlPanel — the full WebUI remote-access control body, shared by the
 * Open Capabilities page and the compact sider footer popover.
 *
 * Consumes the shared `useWebuiServer` state (single status subscription / single
 * start-stop path) and adds the credential modals + QR login, ported verbatim
 * from the former `WebuiModalContent` desktop panel. This panel only ever renders
 * inside the desktop shell, so it drops the browser-mode "channels moved" hint.
 */
interface WebuiControlPanelProps {
  mode?: 'popover' | 'page';
}

const WebuiControlPanel: React.FC<WebuiControlPanelProps> = ({ mode = 'popover' }) => {
  const { t } = useTranslation();

  // Shared WebUI server state (single subscription, single start/stop path).
  const { status, running, starting, lifecycleSupported, initialPassword, accessUrls, start, stop, clearInitialPassword } = useWebuiServer();
  const port = WEBUI_DEFAULT_PORT;

  // 用户名即时覆盖（改密后调 clearInitialPassword 收起明文）。
  // Optimistic username override for immediate display after a change.
  const [usernameOverride, setUsernameOverride] = useState<string | null>(null);

  // 设置新密码 / 用户名弹窗 / Password & username modals
  const [setPasswordModalVisible, setSetPasswordModalVisible] = useState(false);
  const [passwordLoading, setPasswordLoading] = useState(false);
  const [setUsernameModalVisible, setSetUsernameModalVisible] = useState(false);
  const [usernameLoading, setUsernameLoading] = useState(false);
  const [form] = Form.useForm();
  const [usernameForm] = Form.useForm();

  // 二维码登录相关状态 / QR code login related state
  const [qrToken, setQrToken] = useState<string | null>(null);
  const [qrExpiresAt, setQrExpiresAt] = useState<number | null>(null);
  const [qrLoading, setQrLoading] = useState(false);
  const [selectedQrBaseUrl, setSelectedQrBaseUrl] = useState<string | null>(null);
  const qrRefreshTimerRef = useRef<NodeJS.Timeout | null>(null);

  // 复制内容 / Copy content
  const handleCopy = (text: string) => {
    void navigator.clipboard.writeText(text);
    Message.success(t('common.copySuccess'));
  };

  // Use `||` (not `??`) so an empty-string username from a not-yet-populated
  // status falls back to the persisted value / 'admin' instead of rendering blank.
  const displayUsername = usernameOverride || status?.adminUsername || 'admin';
  const qrBaseUrls = useMemo(() => getWebuiQrBaseUrls(status, accessUrls, port), [status, accessUrls, port]);
  const qrUrl = useMemo(() => {
    if (!qrToken || !selectedQrBaseUrl) return null;
    return buildWebuiQrLoginUrl(selectedQrBaseUrl, qrToken);
  }, [qrToken, selectedQrBaseUrl]);
  const selectedQrDisplayUrl = selectedQrBaseUrl ?? qrBaseUrls[0] ?? '';

  // 打开设置新密码弹窗 / Open set new password modal
  const handleResetPassword = () => {
    form.resetFields();
    setSetPasswordModalVisible(true);
  };

  const handleResetUsername = () => {
    usernameForm.setFieldsValue({ newUsername: displayUsername });
    setSetUsernameModalVisible(true);
  };

  // 提交新密码 / Submit new password
  const handleSetNewPassword = async () => {
    try {
      const values = await form.validate();
      setPasswordLoading(true);

      // changePassword goes through httpBridge; on 4xx/5xx it throws
      // BackendHttpError, caught below and translated via errorCodeMap.
      await webui.changePassword.invoke({
        newPassword: values.newPassword,
      });
      Message.success(t('settings.webui.passwordChanged'));
      setSetPasswordModalVisible(false);
      form.resetFields();
      // 改密后清除一次性明文（标题栏抽屉与本页同时收起）。
      // After a change, forget the one-time plaintext (hides it everywhere).
      clearInitialPassword();
    } catch (error) {
      console.error('Set new password error:', error);
      const errorCodeMap: Record<string, string> = {
        PASSWORD_TOO_SHORT: t('settings.webui.passwordTooShort'),
        PASSWORD_TOO_LONG: t('settings.webui.passwordTooLong'),
        PASSWORD_TOO_COMMON: t('settings.webui.passwordTooCommon'),
      };
      const rawMsg =
        isBackendHttpError(error) && error.backendMessage
          ? error.backendMessage
          : error instanceof Error
            ? error.message
            : '';
      const codes = rawMsg.split('; ');
      const translated = codes.map((code) => errorCodeMap[code]).filter(Boolean);
      Message.error(translated.length > 0 ? translated.join('; ') : rawMsg || t('settings.webui.passwordChangeFailed'));
    } finally {
      setPasswordLoading(false);
    }
  };

  const handleSetNewUsername = async () => {
    try {
      const values = await usernameForm.validate();
      setUsernameLoading(true);

      // HTTP bridge: changeUsername returns { username: string } directly;
      // httpBridge throws BackendHttpError on 4xx/5xx — caught below.
      const result = await webui.changeUsername.invoke({
        newUsername: values.newUsername,
      });
      const nextUsername = result?.username ?? values.newUsername.trim();
      Message.success(t('settings.webui.usernameChanged'));
      setSetUsernameModalVisible(false);
      usernameForm.resetFields();
      setUsernameOverride(nextUsername);
    } catch (error) {
      console.error('Set new username error:', error);
      const fallback = t('settings.webui.usernameChangeFailed');
      const msg = isBackendHttpError(error) && error.backendMessage ? error.backendMessage : fallback;
      Message.error(msg);
    } finally {
      setUsernameLoading(false);
    }
  };

  // 生成二维码 / Generate QR code
  const generateQRCode = useCallback(async () => {
    if (!status?.running) return;

    setQrLoading(true);
    try {
      // Backend returns only { token, expires_at_ms }; the scannable URL is
      // composed here from the selected access URL so multi-homed/VPN hosts can
      // switch the QR code to whichever LAN address a phone can actually reach.
      const qrData = await webui.generateQRToken.invoke();

      if (qrData) {
        setQrToken(qrData.token);
        setSelectedQrBaseUrl((current) => (current && qrBaseUrls.includes(current) ? current : (qrBaseUrls[0] ?? null)));
        setQrExpiresAt(qrData.expires_at_ms);

        // 设置自动刷新定时器（4分钟后自动刷新，因为 token 5分钟过期）
        // Set auto-refresh timer (refresh after 4 minutes, as token expires in 5 minutes)
        if (qrRefreshTimerRef.current) {
          clearTimeout(qrRefreshTimerRef.current);
        }
        qrRefreshTimerRef.current = setTimeout(
          () => {
            void generateQRCode();
          },
          4 * 60 * 1000
        );
      } else {
        console.error('Generate QR code failed: no data returned');
        Message.error(t('settings.webui.qrGenerateFailed'));
      }
    } catch (error) {
      console.error('Generate QR code error:', error);
      Message.error(t('settings.webui.qrGenerateFailed'));
    } finally {
      setQrLoading(false);
    }
  }, [status?.running, qrBaseUrls, t]);

  useEffect(() => {
    if (qrBaseUrls.length === 0) {
      setSelectedQrBaseUrl(null);
      return;
    }
    setSelectedQrBaseUrl((current) => (current && qrBaseUrls.includes(current) ? current : qrBaseUrls[0]));
  }, [qrBaseUrls]);

  // 当服务器启动且允许远程访问时自动生成二维码 / Auto-generate QR code when server starts and remote access is allowed
  useEffect(() => {
    if (status?.running && status.allowRemote && !qrToken) {
      void generateQRCode();
    }
  }, [status?.allowRemote, status?.running, generateQRCode, qrToken]);

  // 清理定时器 / Cleanup timer
  useEffect(() => {
    return () => {
      if (qrRefreshTimerRef.current) {
        clearTimeout(qrRefreshTimerRef.current);
      }
    };
  }, []);

  // 服务器停止或关闭远程访问时清除二维码 / Clear QR code when server stops or remote access is disabled
  useEffect(() => {
    if (!status?.running || !status.allowRemote) {
      setQrToken(null);
      setQrExpiresAt(null);
      setSelectedQrBaseUrl(null);
      if (qrRefreshTimerRef.current) {
        clearTimeout(qrRefreshTimerRef.current);
        qrRefreshTimerRef.current = null;
      }
    }
  }, [status?.allowRemote, status?.running]);

  // 格式化过期时间 / Format expiration time
  const formatExpiresAt = (timestamp: number) => {
    const date = new Date(timestamp);
    return date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
  };

  // 密码默认显示 ******，仅首次启动返回的一次性明文可见；改密后由 clearInitialPassword 收起。
  // Password shows ****** by default; only the one-time plaintext from first start
  // is visible, and a change forgets it (clearInitialPassword). When the backend
  // reports no password has ever been set (passwordSet === false), show a
  // "not set yet" hint instead of ****** so a fresh install does not imply a
  // hidden credential. Defaults to "set" while status is loading to avoid a flash.
  const passwordIsSet = status?.passwordSet ?? true;
  const displayPassword = initialPassword ? initialPassword : passwordIsSet ? t('settings.webui.passwordHidden') : t('settings.webui.passwordNotSet');
  const containerClass =
    mode === 'page'
      ? 'w-full overflow-visible flex flex-col gap-12px'
      : 'w-320px max-h-[min(560px,70vh)] overflow-y-auto -mx-4px px-4px py-4px flex flex-col gap-12px';

  return (
    <div className={containerClass}>
      {/* 标题 / Title */}
      <div className='flex items-center gap-8px px-2px'>
        <Earth theme='outline' size='16' className='text-[rgb(var(--primary-6))] shrink-0' />
        <span className='text-14px font-500 text-t-primary'>{t('settings.webui')}</span>
      </div>

      {/* WebUI 引导提示 / WebUI hint */}
      <div className='rd-10px border border-line bg-fill-1 px-10px py-8px text-12px text-t-secondary leading-relaxed'>
        {t('settings.webui.featureRemoteDesc')}
      </div>

      {/* 桌面端生命周期未实现提示 / Desktop lifecycle-unavailable notice */}
      {!lifecycleSupported && (
        <div className='rd-10px border border-line bg-fill-1 px-10px py-8px text-12px text-warning leading-relaxed'>
          {t('settings.webui.desktopLifecycleUnavailable')}
        </div>
      )}

      {/* 启用 WebUI / Enable WebUI */}
      <div className='flex items-center justify-between gap-12px'>
        <div className='min-w-0 flex items-center gap-8px'>
          <span className='text-14px text-t-primary'>{t('settings.webui.enable')}</span>
          {starting ? (
            <span className='text-12px text-warning'>{t('settings.webui.starting')}</span>
          ) : running ? (
            <span className='text-12px text-success'>✓ {t('settings.webui.running')}</span>
          ) : null}
        </div>
        <Switch
          checked={running}
          loading={starting}
          disabled={!lifecycleSupported}
          onChange={(checked) => (checked ? void start() : void stop())}
        />
      </div>

      {/* 访问地址：本机 + 每个网卡真实 IP 的快捷 URL / Access URLs */}
      {lifecycleSupported && running && accessUrls.length > 0 && (
        <div className='flex flex-col gap-6px'>
          <div className='flex items-center gap-4px px-2px'>
            <span className='text-12px font-500 text-t-tertiary'>{t('settings.webui.accessUrl')}</span>
            {/* 浏览器缓存提示收拢进 tips / Browser-cache hint tucked into a tips tooltip */}
            <Tooltip
              position='top'
              content={<div className='max-w-260px text-12px leading-relaxed'>{t('settings.webui.cacheHint')}</div>}
            >
              <span className='inline-flex items-center text-t-tertiary hover:text-t-primary cursor-help leading-none'>
                <Info theme='outline' size='13' fill='currentColor' />
              </span>
            </Tooltip>
          </div>
          <div className='flex flex-col gap-6px'>
            {accessUrls.map((url) => (
              <div key={url} className='flex items-center gap-8px min-w-0'>
                <button
                  className='text-13px text-primary font-mono hover:underline cursor-pointer bg-transparent border-none p-0 truncate'
                  onClick={() => shell.openExternal.invoke(url).catch(console.error)}
                >
                  {url}
                </button>
                <Tooltip content={t('common.copy')}>
                  <button
                    className='p-4px text-t-tertiary hover:text-t-primary cursor-pointer bg-transparent border-none'
                    onClick={() => handleCopy(url)}
                  >
                    <Copy size={15} />
                  </button>
                </Tooltip>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* 一次性初始密码块 / One-time initial password block */}
      {running && initialPassword && (
        <div className='flex flex-col gap-6px'>
          <div className='text-12px font-500 text-t-tertiary px-2px'>{t('settings.webui.initialPassword')}</div>
          <div className='inline-flex items-center gap-8px rd-100px border border-line bg-fill-1 px-10px py-4px min-w-0'>
            <span className='text-13px text-t-primary truncate flex-1'>{initialPassword}</span>
            <Tooltip content={t('common.copy')}>
              <Button
                type='text'
                size='mini'
                className='rd-100px !px-6px inline-flex items-center !h-24px'
                onClick={() => handleCopy(initialPassword)}
              >
                <Copy size={14} />
              </Button>
            </Tooltip>
          </div>
        </div>
      )}

      {/* 登录信息 / Login Info */}
      <div className='flex flex-col gap-6px'>
        <div className='text-12px font-500 text-t-tertiary px-2px'>{t('settings.webui.loginInfo')}</div>

        {/* 账号 / Account */}
        <div className='flex items-center justify-between gap-12px'>
          <span className='text-13px text-t-secondary shrink-0'>{t('settings.webui.username')}:</span>
          <div className='inline-flex items-center gap-6px rd-100px border border-line bg-fill-1 px-10px py-4px min-w-0'>
            <span className='text-13px text-t-primary truncate'>{displayUsername}</span>
            <Tooltip content={t('common.copy')}>
              <Button
                type='text'
                size='mini'
                className='rd-100px !px-6px inline-flex items-center !h-24px'
                onClick={() => handleCopy(displayUsername)}
              >
                <Copy size={14} />
              </Button>
            </Tooltip>
            <Tooltip content={t('settings.webui.editUsernameTooltip')}>
              <Button
                type='text'
                size='mini'
                className='rd-100px !px-6px inline-flex items-center !h-24px'
                onClick={handleResetUsername}
              >
                <EditTwo size={14} />
              </Button>
            </Tooltip>
          </div>
        </div>

        {/* 密码 / Password */}
        <div className='flex items-center justify-between gap-12px'>
          <span className='text-13px text-t-secondary shrink-0'>{t('settings.webui.initialPassword')}:</span>
          <div className='inline-flex items-center gap-6px rd-100px border border-line bg-fill-1 px-10px py-4px min-w-0'>
            <span className='text-13px text-t-primary truncate'>{displayPassword}</span>
            <Tooltip content={t('settings.webui.resetPasswordTooltip')}>
              <Button
                type='text'
                size='mini'
                className='rd-100px !px-6px inline-flex items-center !h-24px'
                onClick={handleResetPassword}
              >
                <EditTwo size={14} />
              </Button>
            </Tooltip>
          </div>
        </div>
      </div>

      {/* 二维码登录（仅生命周期可用、服务器运行且允许远程访问时显示）/ QR Code Login */}
      {lifecycleSupported && status?.running && status.allowRemote && (
        <div className='flex flex-col gap-6px'>
          <div className='border-t border-line' />
          <div className='text-12px font-500 text-t-tertiary px-2px'>{t('settings.webui.qrLogin')}</div>
          <div className='text-12px text-t-tertiary px-2px'>{t('settings.webui.qrLoginHint')}</div>

          <div className='flex flex-col items-center gap-12px'>
            {qrBaseUrls.length > 0 && (
              <div className='qr-address-picker w-full rd-12px border border-[rgba(var(--primary-6),0.30)] bg-[rgba(var(--primary-6),0.07)] p-10px shadow-[0_8px_22px_rgba(var(--primary-6),0.08)]'>
                <div className='flex items-start gap-8px'>
                  <span className='mt-1px flex size-24px shrink-0 items-center justify-center rd-8px bg-primary-1 text-primary-6'>
                    <Earth size={14} />
                  </span>
                  <div className='min-w-0 flex-1'>
                    <div className='text-12px font-600 leading-18px text-t-primary'>
                      {t('settings.webui.qrAddressPickerTitle')}
                    </div>
                    <div className='mt-2px text-11px leading-16px text-t-secondary'>
                      {t('settings.webui.qrAddressPickerDesc')}
                    </div>
                  </div>
                </div>
                {qrBaseUrls.length > 1 ? (
                  <Select
                    size='mini'
                    value={selectedQrBaseUrl ?? qrBaseUrls[0]}
                    onChange={(value) => setSelectedQrBaseUrl(value)}
                    className='mt-8px w-full'
                  >
                    {qrBaseUrls.map((url) => (
                      <Select.Option key={url} value={url}>
                        {url}
                      </Select.Option>
                    ))}
                  </Select>
                ) : (
                  <div className='mt-8px flex min-h-28px items-center rd-8px border border-border-2 bg-fill-0 px-9px'>
                    <code className='truncate font-mono text-11px text-t-primary'>{selectedQrDisplayUrl}</code>
                  </div>
                )}
              </div>
            )}

            {/* 二维码显示区域 / QR Code display area */}
            <div className='p-12px bg-fill-1 border border-line rd-10px'>
              {qrLoading ? (
                <div className='w-140px h-140px flex items-center justify-center'>
                  <span className='text-13px text-t-tertiary'>{t('common.loading')}</span>
                </div>
              ) : qrUrl ? (
                <div className='p-8px bg-white rd-8px'>
                  <Suspense
                    fallback={
                      <div className='w-140px h-140px flex items-center justify-center'>
                        <span className='text-13px text-t-tertiary'>{t('common.loading')}</span>
                      </div>
                    }
                  >
                    <QRCodeSVGLazy value={qrUrl} size={140} level='M' />
                  </Suspense>
                </div>
              ) : (
                <div className='w-140px h-140px flex items-center justify-center'>
                  <span className='text-13px text-t-tertiary'>{t('settings.webui.qrGenerateFailed')}</span>
                </div>
              )}
            </div>

            {/* 过期时间、复制链接和刷新按钮 / Expiration time, copy link and refresh button */}
            <div className='flex items-center gap-8px'>
              {qrExpiresAt && (
                <span className='text-12px text-t-tertiary'>
                  {t('settings.webui.qrExpires', { time: formatExpiresAt(qrExpiresAt) })}
                </span>
              )}
              {qrUrl && (
                <Tooltip content={t('settings.webui.copyQrLink')}>
                  <button
                    className='p-4px bg-transparent border-none text-t-tertiary hover:text-t-primary cursor-pointer'
                    onClick={() => handleCopy(qrUrl)}
                  >
                    <Copy size={15} />
                  </button>
                </Tooltip>
              )}
              <Tooltip content={t('settings.webui.refreshQr')}>
                <button
                  className='p-4px bg-transparent border-none text-t-tertiary hover:text-t-primary cursor-pointer'
                  onClick={() => void generateQRCode()}
                  disabled={qrLoading}
                >
                  <Refresh size={15} className={qrLoading ? 'animate-spin' : ''} />
                </button>
              </Tooltip>
            </div>
          </div>
        </div>
      )}

      {/* 设置新用户名弹窗 / Set New Username Modal */}
      <NomiModal
        visible={setUsernameModalVisible}
        onCancel={() => setSetUsernameModalVisible(false)}
        onOk={handleSetNewUsername}
        confirmLoading={usernameLoading}
        title={t('settings.webui.setNewUsername')}
        size='small'
      >
        <Form form={usernameForm} layout='vertical' className='pt-16px'>
          <Form.Item
            label={t('settings.webui.newUsername')}
            field='newUsername'
            rules={[
              { required: true, message: t('settings.webui.newUsernameRequired') },
              {
                validator: (value, callback) => {
                  if (typeof value !== 'string') {
                    callback();
                    return;
                  }

                  const trimmed = value.trim();
                  if (trimmed.length < 3) {
                    callback(t('settings.webui.usernameMinLength'));
                    return;
                  }

                  if (trimmed.length > 32) {
                    callback(t('settings.webui.usernameMaxLength'));
                    return;
                  }

                  if (!/^[a-zA-Z0-9_-]+$/.test(trimmed)) {
                    callback(t('settings.webui.usernameFormatError'));
                    return;
                  }

                  if (/^[_-]|[_-]$/.test(trimmed)) {
                    callback(t('settings.webui.usernameEdgeError'));
                    return;
                  }

                  callback();
                },
              },
            ]}
          >
            <Input placeholder={t('settings.webui.newUsernamePlaceholder')} />
          </Form.Item>
        </Form>
      </NomiModal>

      {/* 设置新密码弹窗 / Set New Password Modal */}
      <NomiModal
        visible={setPasswordModalVisible}
        onCancel={() => setSetPasswordModalVisible(false)}
        onOk={handleSetNewPassword}
        confirmLoading={passwordLoading}
        title={t('settings.webui.setNewPassword')}
        size='small'
      >
        <Form form={form} layout='vertical' className='pt-16px'>
          <Form.Item
            label={t('settings.webui.newPassword')}
            field='newPassword'
            rules={[
              { required: true, message: t('settings.webui.newPasswordRequired') },
              { minLength: 8, message: t('settings.webui.passwordMinLength') },
            ]}
          >
            <Input.Password placeholder={t('settings.webui.newPasswordPlaceholder')} />
          </Form.Item>
          <Form.Item
            label={t('settings.webui.confirmPassword')}
            field='confirmPassword'
            rules={[
              { required: true, message: t('settings.webui.confirmPasswordRequired') },
              {
                validator: (value, callback) => {
                  if (value !== form.getFieldValue('newPassword')) {
                    callback(t('settings.webui.passwordMismatch'));
                  } else {
                    callback();
                  }
                },
              },
            ]}
          >
            <Input.Password placeholder={t('settings.webui.confirmPasswordPlaceholder')} />
          </Form.Item>
        </Form>
      </NomiModal>
    </div>
  );
};

export default WebuiControlPanel;
