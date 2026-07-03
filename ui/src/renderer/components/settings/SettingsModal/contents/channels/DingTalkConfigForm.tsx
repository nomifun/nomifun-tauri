/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IChannelPairingRequest, IChannelPluginStatus, IChannelUser } from '@/common/types/channel/channel';
import { channel } from '@/common/adapter/ipcBridge';
import { openExternalUrl } from '@/renderer/utils/platform';
import { Button, Empty, Input, Message, Spin, Tooltip } from '@arco-design/web-react';
import { CheckOne, CloseOne, Copy, Delete, Refresh } from '@icon-park/react';
import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { ChannelTarget } from './channelTarget';

/**
 * Preference row component
 */
const PreferenceRow: React.FC<{
  label: string;
  description?: React.ReactNode;
  extra?: React.ReactNode;
  required?: boolean;
  children: React.ReactNode;
}> = ({ label, description, extra, required, children }) => (
  <div className='flex items-center justify-between gap-24px py-12px'>
    <div className='flex-1'>
      <div className='flex items-center gap-8px'>
        <span className='text-14px text-t-primary'>
          {label}
          {required && <span className='text-red-500 ml-2px'>*</span>}
        </span>
        {extra}
      </div>
      {description && <div className='text-12px text-t-tertiary mt-2px'>{description}</div>}
    </div>
    <div className='flex items-center'>{children}</div>
  </div>
);

/**
 * Section header component
 */
const SectionHeader: React.FC<{ title: string; action?: React.ReactNode }> = ({ title, action }) => (
  <div className='flex items-center justify-between mb-12px'>
    <h3 className='text-14px font-500 text-t-primary m-0'>{title}</h3>
    {action}
  </div>
);

interface DingTalkConfigFormProps {
  pluginStatus: IChannelPluginStatus | null;
  /** 多机器人模式下寻址的渠道行；缺省 = 全局设置页 legacy 单行行为。 */
  channelTarget?: ChannelTarget;
  onStatusChange: (status: IChannelPluginStatus | null) => void;
}

const DINGTALK_DEV_DOCS_URL = 'https://github.com/nomifun/nomifun-app/wiki/DingTalk-Bot-Setup-Guide';

const DingTalkConfigForm: React.FC<DingTalkConfigFormProps> = ({
  pluginStatus,
  channelTarget,
  onStatusChange,
}) => {
  const { t } = useTranslation();

  // DingTalk credentials
  const [clientId, setClientId] = useState('');
  const [clientSecret, setClientSecret] = useState('');

  const [testLoading, setTestLoading] = useState(false);
  const [_credentialsTested, setCredentialsTested] = useState(false);
  const [touched, setTouched] = useState({ clientId: false, clientSecret: false });
  const [pairingLoading, setPairingLoading] = useState(false);
  const [usersLoading, setUsersLoading] = useState(false);
  const [pendingPairings, setPendingPairings] = useState<IChannelPairingRequest[]>([]);
  const [authorizedUsers, setAuthorizedUsers] = useState<IChannelUser[]>([]);

  // Load pending pairings
  const loadPendingPairings = useCallback(async () => {
    setPairingLoading(true);
    try {
      const pairings = await channel.getPendingPairings.invoke();
      if (pairings) {
        setPendingPairings(
          pairings.filter(
            (p) => p.platformType === 'dingtalk' && (!channelTarget?.channelId || p.channelId === channelTarget.channelId)
          )
        );
      }
    } catch (error) {
      console.error('[DingTalkConfig] Failed to load pending pairings:', error);
    } finally {
      setPairingLoading(false);
    }
  }, [channelTarget?.channelId]);

  // Load authorized users
  const loadAuthorizedUsers = useCallback(async () => {
    setUsersLoading(true);
    try {
      const users = await channel.getAuthorizedUsers.invoke();
      if (users) {
        setAuthorizedUsers(
          users.filter(
            (u) => u.platformType === 'dingtalk' && (!channelTarget?.channelId || u.channelId === channelTarget.channelId)
          )
        );
      }
    } catch (error) {
      console.error('[DingTalkConfig] Failed to load authorized users:', error);
    } finally {
      setUsersLoading(false);
    }
  }, [channelTarget?.channelId]);

  // Initial load
  useEffect(() => {
    void loadPendingPairings();
    void loadAuthorizedUsers();
  }, [loadPendingPairings, loadAuthorizedUsers]);

  // Listen for pairing requests
  useEffect(() => {
    const unsubscribe = channel.pairingRequested.on((request) => {
      if (request.platformType !== 'dingtalk') return;
      if (channelTarget?.channelId && request.channelId !== channelTarget.channelId) return;
      setPendingPairings((prev) => {
        const exists = prev.some((p) => p.code === request.code);
        if (exists) return prev;
        return [request, ...prev];
      });
    });
    return () => unsubscribe();
  }, [channelTarget?.channelId]);

  // Listen for user authorization
  useEffect(() => {
    const unsubscribe = channel.userAuthorized.on((user) => {
      if (user.platformType !== 'dingtalk') return;
      if (channelTarget?.channelId && user.channelId !== channelTarget.channelId) return;
      setAuthorizedUsers((prev) => {
        const exists = prev.some((u) => u.id === user.id);
        if (exists) return prev;
        return [user, ...prev];
      });
      setPendingPairings((prev) => prev.filter((p) => p.platformUserId !== user.platformUserId));
    });
    return () => unsubscribe();
  }, [channelTarget?.channelId]);

  // Test DingTalk connection
  const handleTestConnection = async () => {
    setTouched({ clientId: true, clientSecret: true });

    if (!clientId.trim() || !clientSecret.trim()) {
      Message.warning(t('settings.dingtalk.credentialsRequired', 'Please enter Client ID and Client Secret'));
      return;
    }

    setTestLoading(true);
    setCredentialsTested(false);
    try {
      // testPlugin returns { success, botUsername?, error? } directly
      const result = await channel.testPlugin.invoke({
        plugin_id: 'dingtalk',
        token: clientId.trim(),
        extra_config: {
          app_secret: clientSecret.trim(),
        },
      });

      if (result.success) {
        setCredentialsTested(true);
        Message.success(t('settings.dingtalk.connectionSuccess', 'Connected to DingTalk API!'));
        await handleAutoEnable();
      } else {
        setCredentialsTested(false);
        Message.error(result.error || t('settings.dingtalk.connectionFailed', 'Connection failed'));
      }
    } catch (error: any) {
      setCredentialsTested(false);
      Message.error(error.message || t('settings.dingtalk.connectionFailed', 'Connection failed'));
    } finally {
      setTestLoading(false);
    }
  };

  // Auto-enable plugin after successful test
  const handleAutoEnable = async () => {
    try {
      const config = {
        credentials: {
          client_id: clientId.trim(),
          client_secret: clientSecret.trim(),
        },
      };
      await channel.enablePlugin.invoke(
        channelTarget
          ? { plugin_id: channelTarget.channelId, plugin_type: 'dingtalk', ...(channelTarget.publicAgentId ? { public_agent_id: channelTarget.publicAgentId } : { companion_id: channelTarget.companionId }), config }
          : { plugin_id: 'dingtalk', config }
      );

      Message.success(t('settings.dingtalk.pluginEnabled', 'DingTalk bot enabled'));
      const plugins = await channel.getPluginStatus.invoke();
      if (plugins) {
        // Multi-row model: resolve by row id (or this companion's freshly created
        // row in create mode); legacy path keeps the by-type lookup.
        const dingtalkPlugin = channelTarget
          ? channelTarget.channelId
            ? plugins.find((p) => p.id === channelTarget.channelId)
            : plugins.find((p) => p.type === 'dingtalk' && p.companionId === channelTarget.companionId)
          : plugins.find((p) => p.type === 'dingtalk');
        onStatusChange(dingtalkPlugin || null);
      }
    } catch (error: unknown) {
      console.error('[DingTalkConfig] Auto-enable failed:', error);
      Message.error(
        (error instanceof Error ? error.message : String(error)) ||
          t('settings.dingtalk.enableFailed', 'Failed to enable DingTalk plugin')
      );
    }
  };

  // Reset credentials tested state when credentials change
  const handleCredentialsChange = () => {
    setCredentialsTested(false);
  };

  // Approve pairing
  const handleApprovePairing = async (code: string) => {
    try {
      await channel.approvePairing.invoke({ code });
      Message.success(t('settings.assistant.pairingApproved', 'Pairing approved'));
      await loadPendingPairings();
      await loadAuthorizedUsers();
    } catch (error: unknown) {
      Message.error(error instanceof Error ? error.message : String(error));
    }
  };

  // Reject pairing
  const handleRejectPairing = async (code: string) => {
    try {
      await channel.rejectPairing.invoke({ code });
      Message.info(t('settings.assistant.pairingRejected', 'Pairing rejected'));
      await loadPendingPairings();
    } catch (error: unknown) {
      Message.error(error instanceof Error ? error.message : String(error));
    }
  };

  // Revoke user
  const handleRevokeUser = async (user_id: string) => {
    try {
      await channel.revokeUser.invoke({ user_id });
      Message.success(t('settings.assistant.userRevoked', 'User access revoked'));
      await loadAuthorizedUsers();
    } catch (error: unknown) {
      Message.error(error instanceof Error ? error.message : String(error));
    }
  };

  // Copy to clipboard
  const copyToClipboard = (text: string) => {
    void navigator.clipboard.writeText(text);
    Message.success(t('common.copySuccess', 'Copied'));
  };

  // Format timestamp
  const formatTime = (timestamp: number) => {
    return new Date(timestamp).toLocaleString();
  };

  // Calculate remaining time
  const getRemainingTime = (expiresAt: number) => {
    const remaining = Math.max(0, Math.ceil((expiresAt - Date.now()) / 1000 / 60));
    return `${remaining} min`;
  };

  // Row-scoped credential lock — see LarkConfigForm for the rationale (was a
  // global per-platform `authorizedUsers.length > 0`, which froze a second
  // companion's create form). Lock only when THIS bot row is live.
  const credentialsLocked = !!pluginStatus?.connected;

  return (
    <div className='flex flex-col gap-24px'>
      {/* Client ID */}
      <PreferenceRow
        label={t('settings.dingtalk.clientId', 'Client ID')}
        description={
          <span>
            <a
              className='text-primary hover:underline cursor-pointer text-12px'
              href={DINGTALK_DEV_DOCS_URL}
              onClick={(e) => {
                e.preventDefault();
                openExternalUrl(DINGTALK_DEV_DOCS_URL).catch(console.error);
              }}
            >
              {t('settings.dingtalk.devConsoleLink', 'DingTalk Open Platform')}
            </a>{' '}
            {t('settings.dingtalk.clientIdDescSuffix', 'to get your Client ID')}
          </span>
        }
        required
      >
        {credentialsLocked ? (
          <Tooltip
            content={t(
              'settings.assistant.tokenLocked',
              'Please close the Channel and delete all authorized users before modifying'
            )}
          >
            <span>
              <Input
                value={clientId}
                onChange={(value) => {
                  setClientId(value);
                  handleCredentialsChange();
                }}
                onBlur={() => setTouched((prev) => ({ ...prev, clientId: true }))}
                placeholder={pluginStatus?.hasToken ? '••••••••••••••••' : 'dingxxxxxxxxxx'}
                style={{ width: 240 }}
                status={touched.clientId && !clientId.trim() && !pluginStatus?.hasToken ? 'error' : undefined}
                disabled={credentialsLocked}
              />
            </span>
          </Tooltip>
        ) : (
          <Input
            value={clientId}
            onChange={(value) => {
              setClientId(value);
              handleCredentialsChange();
            }}
            onBlur={() => setTouched((prev) => ({ ...prev, clientId: true }))}
            placeholder={pluginStatus?.hasToken ? '••••••••••••••••' : 'dingxxxxxxxxxx'}
            style={{ width: 240 }}
            status={touched.clientId && !clientId.trim() && !pluginStatus?.hasToken ? 'error' : undefined}
            disabled={credentialsLocked}
          />
        )}
      </PreferenceRow>

      {/* Client Secret */}
      <PreferenceRow
        label={t('settings.dingtalk.clientSecret', 'Client Secret')}
        description={
          <span>
            <a
              className='text-primary hover:underline cursor-pointer text-12px'
              href={DINGTALK_DEV_DOCS_URL}
              onClick={(e) => {
                e.preventDefault();
                openExternalUrl(DINGTALK_DEV_DOCS_URL).catch(console.error);
              }}
            >
              {t('settings.dingtalk.devConsoleLink', 'DingTalk Open Platform')}
            </a>{' '}
            {t('settings.dingtalk.clientSecretDescSuffix', 'to get Client Secret')}
          </span>
        }
        required
      >
        {credentialsLocked ? (
          <Tooltip
            content={t(
              'settings.assistant.tokenLocked',
              'Please close the Channel and delete all authorized users before modifying'
            )}
          >
            <span>
              <Input.Password
                value={clientSecret}
                onChange={(value) => {
                  setClientSecret(value);
                  handleCredentialsChange();
                }}
                onBlur={() => setTouched((prev) => ({ ...prev, clientSecret: true }))}
                placeholder={pluginStatus?.hasToken ? '••••••••••••••••' : 'xxxxxxxxxxxxxxxxxx'}
                style={{ width: 240 }}
                status={touched.clientSecret && !clientSecret.trim() && !pluginStatus?.hasToken ? 'error' : undefined}
                visibilityToggle
                disabled={credentialsLocked}
              />
            </span>
          </Tooltip>
        ) : (
          <Input.Password
            value={clientSecret}
            onChange={(value) => {
              setClientSecret(value);
              handleCredentialsChange();
            }}
            onBlur={() => setTouched((prev) => ({ ...prev, clientSecret: true }))}
            placeholder={pluginStatus?.hasToken ? '••••••••••••••••' : 'xxxxxxxxxxxxxxxxxx'}
            style={{ width: 240 }}
            status={touched.clientSecret && !clientSecret.trim() && !pluginStatus?.hasToken ? 'error' : undefined}
            visibilityToggle
            disabled={credentialsLocked}
          />
        )}
      </PreferenceRow>

      {/* Test Connection Button */}
      {!pluginStatus?.connected && (
        <div className='flex justify-end'>
          {pluginStatus?.hasToken && !clientId.trim() && !clientSecret.trim() ? (
            <span className='text-12px text-t-tertiary mr-12px self-center'>
              {t('settings.dingtalk.credentialsSaved', 'Credentials already configured. Enter new values to update.')}
            </span>
          ) : null}
          <Button
            type='primary'
            loading={testLoading}
            onClick={handleTestConnection}
            disabled={pluginStatus?.hasToken && !clientId.trim() && !clientSecret.trim()}
          >
            {t('settings.dingtalk.testAndConnect', 'Test & Connect')}
          </Button>
        </div>
      )}


      {/* Connection Status */}
      {pluginStatus?.enabled && authorizedUsers.length === 0 && (
        <div
          className={`rd-12px p-16px border ${pluginStatus?.connected ? 'bg-green-50 dark:bg-green-900/20 border-green-200 dark:border-green-800' : pluginStatus?.error ? 'bg-red-50 dark:bg-red-900/20 border-red-200 dark:border-red-800' : 'bg-yellow-50 dark:bg-yellow-900/20 border-yellow-200 dark:border-yellow-800'}`}
        >
          <SectionHeader
            title={t('settings.dingtalk.connectionStatus', 'Connection Status')}
            action={
              <span
                className={`text-12px px-8px py-2px rd-4px ${pluginStatus?.connected ? 'bg-green-100 text-green-700 dark:bg-green-900 dark:text-green-300' : pluginStatus?.error ? 'bg-red-100 text-red-700 dark:bg-red-900 dark:text-red-300' : 'bg-yellow-100 text-yellow-700 dark:bg-yellow-900 dark:text-yellow-300'}`}
              >
                {pluginStatus?.connected
                  ? t('settings.dingtalk.statusConnected', 'Connected')
                  : pluginStatus?.error
                    ? t('settings.dingtalk.statusError', 'Error')
                    : t('settings.dingtalk.statusConnecting', 'Connecting...')}
              </span>
            }
          />
          {pluginStatus?.error && (
            <div className='text-14px text-red-600 dark:text-red-400 mb-12px'>{pluginStatus.error}</div>
          )}
          {pluginStatus?.connected && (
            <div className='text-14px text-t-secondary space-y-8px'>
              <p className='m-0 font-500'>{t('settings.assistant.nextSteps', 'Next Steps')}:</p>
              <p className='m-0'>
                <strong>1.</strong> {t('settings.dingtalk.step1', 'Open DingTalk and find your bot application')}
              </p>
              <p className='m-0'>
                <strong>2.</strong> {t('settings.dingtalk.step2', 'Send any message to initiate pairing')}
              </p>
              <p className='m-0'>
                <strong>3.</strong>{' '}
                {t(
                  'settings.dingtalk.step3',
                  'A pairing request will appear below. Click "Approve" to authorize the user.'
                )}
              </p>
              <p className='m-0'>
                <strong>4.</strong>{' '}
                {t(
                  'settings.dingtalk.step4',
                  'Once approved, you can start chatting with the AI assistant through DingTalk!'
                )}
              </p>
            </div>
          )}
          {!pluginStatus?.connected && !pluginStatus?.error && (
            <div className='text-14px text-t-secondary'>
              {t('settings.dingtalk.waitingConnection', 'Connection is being established. Please wait...')}
            </div>
          )}
        </div>
      )}

      {/* Pending Pairings */}
      {pluginStatus?.enabled && authorizedUsers.length === 0 && (
        <div className='bg-fill-1 rd-12px pt-16px pr-16px pb-16px pl-0'>
          <SectionHeader
            title={t('settings.assistant.pendingPairings', 'Pending Pairing Requests')}
            action={
              <Button
                size='mini'
                type='text'
                icon={<Refresh size={14} />}
                loading={pairingLoading}
                onClick={loadPendingPairings}
              >
                {t('conversation.workspace.refresh', 'Refresh')}
              </Button>
            }
          />

          {pairingLoading ? (
            <div className='flex justify-center py-24px'>
              <Spin />
            </div>
          ) : pendingPairings.length === 0 ? (
            <Empty description={t('settings.assistant.noPendingPairings', 'No pending pairing requests')} />
          ) : (
            <div className='flex flex-col gap-12px'>
              {pendingPairings.map((pairing) => (
                <div key={pairing.code} className='flex items-center justify-between bg-fill-2 rd-8px p-12px'>
                  <div className='flex-1'>
                    <div className='flex items-center gap-8px'>
                      <span className='text-14px font-500 text-t-primary'>
                        {pairing.display_name || 'Unknown User'}
                      </span>
                      <Tooltip content={t('settings.assistant.copyCode', 'Copy pairing code')}>
                        <button
                          className='p-4px bg-transparent border-none text-t-tertiary hover:text-t-primary cursor-pointer'
                          onClick={() => copyToClipboard(pairing.code)}
                        >
                          <Copy size={14} />
                        </button>
                      </Tooltip>
                    </div>
                    <div className='text-12px text-t-tertiary mt-4px'>
                      {t('settings.assistant.pairingCode', 'Code')}:{' '}
                      <code className='bg-fill-3 px-4px rd-2px'>{pairing.code}</code>
                      <span className='mx-8px'>|</span>
                      {t('settings.assistant.expiresIn', 'Expires in')}: {getRemainingTime(pairing.expiresAt)}
                    </div>
                  </div>
                  <div className='flex items-center gap-8px'>
                    <Button
                      type='primary'
                      size='small'
                      icon={<CheckOne size={14} />}
                      onClick={() => handleApprovePairing(pairing.code)}
                    >
                      {t('settings.assistant.approve', 'Approve')}
                    </Button>
                    <Button
                      type='secondary'
                      size='small'
                      status='danger'
                      icon={<CloseOne size={14} />}
                      onClick={() => handleRejectPairing(pairing.code)}
                    >
                      {t('settings.assistant.reject', 'Reject')}
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>
      )}

      {/* Authorized Users */}
      {pluginStatus?.enabled && authorizedUsers.length > 0 && (
        <div className='bg-fill-1 rd-12px pt-16px pr-16px pb-16px pl-0'>
          <SectionHeader
            title={t('settings.assistant.authorizedUsers', 'Authorized Users')}
            action={
              <Button
                size='mini'
                type='text'
                icon={<Refresh size={14} />}
                loading={usersLoading}
                onClick={loadAuthorizedUsers}
              >
                {t('common.refresh', 'Refresh')}
              </Button>
            }
          />

          {usersLoading ? (
            <div className='flex justify-center py-24px'>
              <Spin />
            </div>
          ) : authorizedUsers.length === 0 ? (
            <Empty description={t('settings.assistant.noAuthorizedUsers', 'No authorized users yet')} />
          ) : (
            <div className='flex flex-col gap-12px'>
              {authorizedUsers.map((user) => (
                <div key={user.id} className='flex items-center justify-between bg-fill-2 rd-8px p-12px'>
                  <div className='flex-1'>
                    <div className='text-14px font-500 text-t-primary'>{user.display_name || 'Unknown User'}</div>
                    <div className='text-12px text-t-tertiary mt-4px'>
                      {t('settings.assistant.platform', 'Platform')}: {user.platformType}
                      <span className='mx-8px'>|</span>
                      {t('settings.assistant.authorizedAt', 'Authorized')}: {formatTime(user.authorizedAt)}
                    </div>
                  </div>
                  <Tooltip content={t('settings.assistant.revokeAccess', 'Revoke access')}>
                    <Button
                      type='text'
                      status='danger'
                      size='small'
                      icon={<Delete size={16} />}
                      onClick={() => handleRevokeUser(user.id)}
                    />
                  </Tooltip>
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
};

export default DingTalkConfigForm;
