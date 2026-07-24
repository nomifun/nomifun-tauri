/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IChannelPairingRequest, IChannelPluginStatus, IChannelUser } from '@/common/types/channel/channel';
import { channel } from '@/common/adapter/ipcBridge';
import { Button, Empty, Input, Message, Spin, Tooltip } from '@arco-design/web-react';
import { CheckOne, CloseOne, Delete, Refresh } from '@icon-park/react';
import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { ChannelTarget } from './channelTarget';

interface SlackConfigFormProps {
  pluginStatus: IChannelPluginStatus | null;
  channelTarget?: ChannelTarget;
  onStatusChange: (status: IChannelPluginStatus | null) => void;
}

/**
 * Slack channel config (Socket Mode). Needs two tokens: a bot token (`xoxb-`)
 * and an app-level token (`xapp-`, scope `connections:write`).
 */
const SlackConfigForm: React.FC<SlackConfigFormProps> = ({ pluginStatus, channelTarget, onStatusChange }) => {
  const { t } = useTranslation();
  const [botToken, setBotToken] = useState('');
  const [appToken, setAppToken] = useState('');
  const [testLoading, setTestLoading] = useState(false);
  const [pairingLoading, setPairingLoading] = useState(false);
  const [usersLoading, setUsersLoading] = useState(false);
  const [pendingPairings, setPendingPairings] = useState<IChannelPairingRequest[]>([]);
  const [authorizedUsers, setAuthorizedUsers] = useState<IChannelUser[]>([]);

  const loadPendingPairings = useCallback(async () => {
    setPairingLoading(true);
    try {
      const pairings = await channel.getPendingPairings.invoke();
      if (pairings) setPendingPairings(pairings.filter((p) => p.platformType === 'slack' && (!channelTarget?.channelPluginId || p.channel_plugin_id === channelTarget.channelPluginId)));
    } finally {
      setPairingLoading(false);
    }
  }, [channelTarget?.channelPluginId]);

  const loadAuthorizedUsers = useCallback(async () => {
    setUsersLoading(true);
    try {
      const users = await channel.getAuthorizedUsers.invoke();
      if (users) setAuthorizedUsers(users.filter((u) => u.platformType === 'slack' && (!channelTarget?.channelPluginId || u.channel_plugin_id === channelTarget.channelPluginId)));
    } finally {
      setUsersLoading(false);
    }
  }, [channelTarget?.channelPluginId]);

  useEffect(() => {
    void loadPendingPairings();
    void loadAuthorizedUsers();
  }, [loadPendingPairings, loadAuthorizedUsers]);

  useEffect(() => {
    const unsub = channel.pairingRequested.on((request) => {
      if (request.platformType !== 'slack') return;
      if (channelTarget?.channelPluginId && request.channel_plugin_id !== channelTarget.channelPluginId) return;
      setPendingPairings((prev) => (prev.some((p) => p.code === request.code) ? prev : [request, ...prev]));
    });
    return () => unsub();
  }, [channelTarget?.channelPluginId]);

  useEffect(() => {
    const unsub = channel.userAuthorized.on((user) => {
      if (user.platformType !== 'slack') return;
      if (channelTarget?.channelPluginId && user.channel_plugin_id !== channelTarget.channelPluginId) return;
      setAuthorizedUsers((prev) => (prev.some((u) => u.channel_user_id === user.channel_user_id) ? prev : [user, ...prev]));
      setPendingPairings((prev) => prev.filter((p) => p.platformUserId !== user.platformUserId));
    });
    return () => unsub();
  }, [channelTarget?.channelPluginId]);

  const handleAutoEnable = async () => {
    const config = { credentials: { token: botToken.trim(), app_token: appToken.trim() } };
    const result = await channel.enablePlugin.invoke(channelTarget ? { plugin_id: channelTarget.channelPluginId, plugin_type: 'slack', ...(channelTarget.publicAgentId ? { public_agent_id: channelTarget.publicAgentId } : { companion_id: channelTarget.companionId }), config } : { plugin_type: 'slack', config });
    if (!result.success) {
      throw new Error(result.error || t('nomi.settings.remoteEnableFailed', { defaultValue: 'Failed to enable channel' }));
    }
    Message.success(t('settings.slack.pluginEnabled', 'Slack bot enabled'));
    const plugins = await channel.getPluginStatus.invoke();
    if (plugins) {
      const row = channelTarget ? (channelTarget.channelPluginId ? plugins.find((p) => p.plugin_id === channelTarget.channelPluginId) : plugins.find((p) => p.type === 'slack' && p.companionId === channelTarget.companionId)) : plugins.find((p) => p.type === 'slack');
      onStatusChange(row || null);
    }
  };

  const handleTestConnection = async () => {
    if (!botToken.trim() || !appToken.trim()) {
      Message.warning(t('settings.slack.credentialsRequired', 'Please enter both the bot token and the app-level token'));
      return;
    }
    setTestLoading(true);
    try {
      const result = await channel.testPlugin.invoke({ plugin_type: 'slack', token: botToken.trim(), extra_config: { app_token: appToken.trim() } });
      if (result.success) {
        Message.success(t('settings.slack.connectionSuccess', { defaultValue: 'Connected! Bot: {{username}}', username: result.bot_username || 'unknown' }));
        await handleAutoEnable();
      } else {
        Message.error(result.error || t('settings.slack.connectionFailed', 'Connection failed'));
      }
    } catch (error: any) {
      Message.error(error.message || t('settings.slack.connectionFailed', 'Connection failed'));
    } finally {
      setTestLoading(false);
    }
  };

  const handleApprovePairing = async (code: string) => {
    try {
      await channel.approvePairing.invoke({ code });
      Message.success(t('settings.channels.pairingApproved', 'Pairing approved'));
      await loadPendingPairings();
      await loadAuthorizedUsers();
    } catch (error: unknown) {
      Message.error(error instanceof Error ? error.message : String(error));
    }
  };
  const handleRejectPairing = async (code: string) => {
    try {
      await channel.rejectPairing.invoke({ code });
      await loadPendingPairings();
    } catch (error: unknown) {
      Message.error(error instanceof Error ? error.message : String(error));
    }
  };
  const handleRevokeUser = async (channel_user_id: import('@/common/types/ids').ChannelUserId) => {
    try {
      await channel.revokeUser.invoke({ channel_user_id });
      await loadAuthorizedUsers();
    } catch (error: unknown) {
      Message.error(error instanceof Error ? error.message : String(error));
    }
  };

  const getRemainingTime = (expiresAt: number) => `${Math.max(0, Math.ceil((expiresAt - Date.now()) / 1000 / 60))} ${t('common.unit.minute_short')}`;
  const credentialsLocked = !!pluginStatus?.connected;

  return (
    <div className='flex flex-col gap-16px'>
      <div className='flex flex-col gap-8px'>
        <span className='text-14px text-t-primary'>{t('settings.slack.botToken', 'Bot Token (xoxb-)')}</span>
        <Input.Password value={botToken} onChange={setBotToken} placeholder={pluginStatus?.hasToken ? '••••••••••••••••' : 'xoxb-...'} visibilityToggle disabled={credentialsLocked} />
        <span className='text-14px text-t-primary mt-4px'>{t('settings.slack.appToken', 'App-Level Token (xapp-)')}</span>
        <Input.Password value={appToken} onChange={setAppToken} placeholder={pluginStatus?.hasToken ? '••••••••••••••••' : 'xapp-...'} visibilityToggle disabled={credentialsLocked} />
        <div className='text-12px text-t-tertiary'>{t('settings.slack.tokensDesc', 'In Slack API → your app: enable Socket Mode (gives the xapp- app token with connections:write), and install the app to get the xoxb- bot token.')}</div>
        <div>
          <Button type='outline' loading={testLoading} onClick={handleTestConnection} disabled={credentialsLocked}>
            {t('settings.channels.testConnection', 'Test')}
          </Button>
        </div>
      </div>

      {pluginStatus?.enabled && authorizedUsers.length === 0 && (
        <div className='bg-fill-1 rd-12px pt-16px pr-16px pb-16px pl-0'>
          <div className='flex items-center justify-between mb-12px'>
            <h3 className='text-14px font-500 text-t-primary m-0'>{t('settings.channels.pendingPairings', 'Pending Pairing Requests')}</h3>
            <Button size='mini' type='text' icon={<Refresh size={14} />} loading={pairingLoading} onClick={loadPendingPairings}>{t('common.refresh', 'Refresh')}</Button>
          </div>
          {pairingLoading ? (
            <div className='flex justify-center py-24px'><Spin /></div>
          ) : pendingPairings.length === 0 ? (
            <Empty description={t('settings.channels.noPendingPairings', 'No pending pairing requests')} />
          ) : (
            <div className='flex flex-col gap-12px'>
              {pendingPairings.map((pairing) => (
                <div key={pairing.code} className='flex items-center justify-between bg-fill-2 rd-8px p-12px'>
                  <div className='flex-1'>
                    <div className='text-14px font-500 text-t-primary'>{pairing.display_name || t('common.unknownUser')}</div>
                    <div className='text-12px text-t-tertiary mt-4px'>{t('settings.channels.pairingCode', 'Code')}: <code className='bg-fill-3 px-4px rd-2px'>{pairing.code}</code> · {t('settings.channels.expiresIn', 'Expires in')}: {getRemainingTime(pairing.expiresAt)}</div>
                  </div>
                  <div className='flex items-center gap-8px'>
                    <Button type='primary' size='small' icon={<CheckOne size={14} />} onClick={() => handleApprovePairing(pairing.code)}>{t('settings.channels.approve', 'Approve')}</Button>
                    <Button type='secondary' size='small' status='danger' icon={<CloseOne size={14} />} onClick={() => handleRejectPairing(pairing.code)}>{t('settings.channels.reject', 'Reject')}</Button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>
      )}

      {pluginStatus?.enabled && authorizedUsers.length > 0 && (
        <div className='bg-fill-1 rd-12px pt-16px pr-16px pb-16px pl-0'>
          <div className='flex items-center justify-between mb-12px'>
            <h3 className='text-14px font-500 text-t-primary m-0'>{t('settings.channels.authorizedUsers', 'Authorized Users')}</h3>
            <Button size='mini' type='text' icon={<Refresh size={14} />} loading={usersLoading} onClick={loadAuthorizedUsers}>{t('common.refresh', 'Refresh')}</Button>
          </div>
          <div className='flex flex-col gap-12px'>
            {authorizedUsers.map((user) => (
              <div key={user.channel_user_id} className='flex items-center justify-between bg-fill-2 rd-8px p-12px'>
                <div className='text-14px font-500 text-t-primary'>{user.display_name || t('common.unknownUser')}</div>
                <Tooltip content={t('settings.channels.revokeAccess', 'Revoke access')}>
                  <Button type='text' status='danger' size='small' icon={<Delete size={16} />} onClick={() => handleRevokeUser(user.channel_user_id)} />
                </Tooltip>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
};

export default SlackConfigForm;
