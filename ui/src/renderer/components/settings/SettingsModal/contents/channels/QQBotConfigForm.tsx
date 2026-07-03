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

interface QQBotConfigFormProps {
  pluginStatus: IChannelPluginStatus | null;
  channelTarget?: ChannelTarget;
  onStatusChange: (status: IChannelPluginStatus | null) => void;
}

/** QQ Bot config: AppID + ClientSecret (OAuth2 client-credentials). */
const QQBotConfigForm: React.FC<QQBotConfigFormProps> = ({ pluginStatus, channelTarget, onStatusChange }) => {
  const { t } = useTranslation();
  const [appId, setAppId] = useState('');
  const [clientSecret, setClientSecret] = useState('');
  const [testLoading, setTestLoading] = useState(false);
  const [pairingLoading, setPairingLoading] = useState(false);
  const [usersLoading, setUsersLoading] = useState(false);
  const [pendingPairings, setPendingPairings] = useState<IChannelPairingRequest[]>([]);
  const [authorizedUsers, setAuthorizedUsers] = useState<IChannelUser[]>([]);

  const loadPendingPairings = useCallback(async () => {
    setPairingLoading(true);
    try {
      const pairings = await channel.getPendingPairings.invoke();
      if (pairings) setPendingPairings(pairings.filter((p) => p.platformType === 'qqbot' && (!channelTarget?.channelId || p.channelId === channelTarget.channelId)));
    } finally {
      setPairingLoading(false);
    }
  }, [channelTarget?.channelId]);

  const loadAuthorizedUsers = useCallback(async () => {
    setUsersLoading(true);
    try {
      const users = await channel.getAuthorizedUsers.invoke();
      if (users) setAuthorizedUsers(users.filter((u) => u.platformType === 'qqbot' && (!channelTarget?.channelId || u.channelId === channelTarget.channelId)));
    } finally {
      setUsersLoading(false);
    }
  }, [channelTarget?.channelId]);

  useEffect(() => {
    void loadPendingPairings();
    void loadAuthorizedUsers();
  }, [loadPendingPairings, loadAuthorizedUsers]);

  useEffect(() => {
    const unsub = channel.pairingRequested.on((request) => {
      if (request.platformType !== 'qqbot') return;
      if (channelTarget?.channelId && request.channelId !== channelTarget.channelId) return;
      setPendingPairings((prev) => (prev.some((p) => p.code === request.code) ? prev : [request, ...prev]));
    });
    return () => unsub();
  }, [channelTarget?.channelId]);

  useEffect(() => {
    const unsub = channel.userAuthorized.on((user) => {
      if (user.platformType !== 'qqbot') return;
      if (channelTarget?.channelId && user.channelId !== channelTarget.channelId) return;
      setAuthorizedUsers((prev) => (prev.some((u) => u.id === user.id) ? prev : [user, ...prev]));
      setPendingPairings((prev) => prev.filter((p) => p.platformUserId !== user.platformUserId));
    });
    return () => unsub();
  }, [channelTarget?.channelId]);

  const handleAutoEnable = async () => {
    const config = { credentials: { client_id: appId.trim(), client_secret: clientSecret.trim() } };
    await channel.enablePlugin.invoke(channelTarget ? { plugin_id: channelTarget.channelId, plugin_type: 'qqbot', ...(channelTarget.publicAgentId ? { public_agent_id: channelTarget.publicAgentId } : { companion_id: channelTarget.companionId }), config } : { plugin_id: 'qqbot', config });
    Message.success(t('settings.qqbot.pluginEnabled', 'QQ bot enabled'));
    const plugins = await channel.getPluginStatus.invoke();
    if (plugins) {
      const row = channelTarget ? (channelTarget.channelId ? plugins.find((p) => p.id === channelTarget.channelId) : plugins.find((p) => p.type === 'qqbot' && p.companionId === channelTarget.companionId)) : plugins.find((p) => p.type === 'qqbot');
      onStatusChange(row || null);
    }
  };

  const handleTestConnection = async () => {
    if (!appId.trim() || !clientSecret.trim()) {
      Message.warning(t('settings.qqbot.credentialsRequired', 'Please enter the AppID and ClientSecret'));
      return;
    }
    setTestLoading(true);
    try {
      const result = await channel.testPlugin.invoke({ plugin_id: 'qqbot', token: appId.trim(), extra_config: { app_secret: clientSecret.trim() } });
      if (result.success) {
        Message.success(t('settings.qqbot.connectionSuccess', { defaultValue: 'Connected! AppID: {{appId}}', appId: result.bot_username || appId.trim() }));
        await handleAutoEnable();
      } else {
        Message.error(result.error || t('settings.qqbot.connectionFailed', 'Connection failed'));
      }
    } catch (error: any) {
      Message.error(error.message || t('settings.qqbot.connectionFailed', 'Connection failed'));
    } finally {
      setTestLoading(false);
    }
  };

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
  const handleRejectPairing = async (code: string) => {
    try {
      await channel.rejectPairing.invoke({ code });
      await loadPendingPairings();
    } catch (error: unknown) {
      Message.error(error instanceof Error ? error.message : String(error));
    }
  };
  const handleRevokeUser = async (user_id: string) => {
    try {
      await channel.revokeUser.invoke({ user_id });
      await loadAuthorizedUsers();
    } catch (error: unknown) {
      Message.error(error instanceof Error ? error.message : String(error));
    }
  };

  const getRemainingTime = (expiresAt: number) => `${Math.max(0, Math.ceil((expiresAt - Date.now()) / 1000 / 60))} min`;
  const credentialsLocked = !!pluginStatus?.connected;

  return (
    <div className='flex flex-col gap-16px'>
      <div className='flex flex-col gap-8px'>
        <span className='text-14px text-t-primary'>{t('settings.qqbot.appId', 'AppID')}</span>
        <Input value={appId} onChange={setAppId} placeholder='102xxxxxx' disabled={credentialsLocked} />
        <span className='text-14px text-t-primary mt-4px'>{t('settings.qqbot.clientSecret', 'ClientSecret')}</span>
        <Input.Password value={clientSecret} onChange={setClientSecret} placeholder={pluginStatus?.hasToken ? '••••••••••••••••' : 'client secret'} visibilityToggle disabled={credentialsLocked} />
        <div className='text-12px text-t-tertiary'>{t('settings.qqbot.tokensDesc', 'Create a bot on the QQ Open Platform (q.qq.com) and copy its AppID + ClientSecret.')}</div>
        <div>
          <Button type='outline' loading={testLoading} onClick={handleTestConnection} disabled={credentialsLocked}>
            {t('settings.assistant.testConnection', 'Test')}
          </Button>
        </div>
      </div>

      {/* Intent reminder: GROUP_AND_C2C needs console approval. */}
      <div className='bg-[rgba(var(--primary-rgb),0.08)] rd-12px p-12px border border-[rgba(var(--primary-rgb),0.2)] text-12px text-t-secondary'>
        {t('settings.qqbot.intentNote', 'In the QQ Open Platform console → bot management → permissions, apply for the "GROUP_AND_C2C" intent, otherwise the bot cannot receive group/private messages.')}
      </div>

      {pluginStatus?.enabled && authorizedUsers.length === 0 && (
        <div className='bg-fill-1 rd-12px pt-16px pr-16px pb-16px pl-0'>
          <div className='flex items-center justify-between mb-12px'>
            <h3 className='text-14px font-500 text-t-primary m-0'>{t('settings.assistant.pendingPairings', 'Pending Pairing Requests')}</h3>
            <Button size='mini' type='text' icon={<Refresh size={14} />} loading={pairingLoading} onClick={loadPendingPairings}>{t('common.refresh', 'Refresh')}</Button>
          </div>
          {pairingLoading ? (
            <div className='flex justify-center py-24px'><Spin /></div>
          ) : pendingPairings.length === 0 ? (
            <Empty description={t('settings.assistant.noPendingPairings', 'No pending pairing requests')} />
          ) : (
            <div className='flex flex-col gap-12px'>
              {pendingPairings.map((pairing) => (
                <div key={pairing.code} className='flex items-center justify-between bg-fill-2 rd-8px p-12px'>
                  <div className='flex-1'>
                    <div className='text-14px font-500 text-t-primary'>{pairing.display_name || 'Unknown User'}</div>
                    <div className='text-12px text-t-tertiary mt-4px'>{t('settings.assistant.pairingCode', 'Code')}: <code className='bg-fill-3 px-4px rd-2px'>{pairing.code}</code> · {t('settings.assistant.expiresIn', 'Expires in')}: {getRemainingTime(pairing.expiresAt)}</div>
                  </div>
                  <div className='flex items-center gap-8px'>
                    <Button type='primary' size='small' icon={<CheckOne size={14} />} onClick={() => handleApprovePairing(pairing.code)}>{t('settings.assistant.approve', 'Approve')}</Button>
                    <Button type='secondary' size='small' status='danger' icon={<CloseOne size={14} />} onClick={() => handleRejectPairing(pairing.code)}>{t('settings.assistant.reject', 'Reject')}</Button>
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
            <h3 className='text-14px font-500 text-t-primary m-0'>{t('settings.assistant.authorizedUsers', 'Authorized Users')}</h3>
            <Button size='mini' type='text' icon={<Refresh size={14} />} loading={usersLoading} onClick={loadAuthorizedUsers}>{t('common.refresh', 'Refresh')}</Button>
          </div>
          <div className='flex flex-col gap-12px'>
            {authorizedUsers.map((user) => (
              <div key={user.id} className='flex items-center justify-between bg-fill-2 rd-8px p-12px'>
                <div className='text-14px font-500 text-t-primary'>{user.display_name || 'Unknown User'}</div>
                <Tooltip content={t('settings.assistant.revokeAccess', 'Revoke access')}>
                  <Button type='text' status='danger' size='small' icon={<Delete size={16} />} onClick={() => handleRevokeUser(user.id)} />
                </Tooltip>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
};

export default QQBotConfigForm;
