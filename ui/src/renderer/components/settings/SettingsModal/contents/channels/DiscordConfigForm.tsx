/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IChannelPairingRequest, IChannelPluginStatus, IChannelUser } from '@/common/types/channel/channel';
import { channel } from '@/common/adapter/ipcBridge';
import { Button, Empty, Input, Message, Spin, Tooltip } from '@arco-design/web-react';
import { CheckOne, CloseOne, Copy, Delete, Refresh } from '@icon-park/react';
import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { ChannelTarget } from './channelTarget';

/** Preference row */
const PreferenceRow: React.FC<{
  label: string;
  description?: React.ReactNode;
  children: React.ReactNode;
}> = ({ label, description, children }) => (
  <div className='flex items-center justify-between gap-24px py-12px'>
    <div className='flex-1'>
      <span className='text-14px text-t-primary'>{label}</span>
      {description && <div className='text-12px text-t-tertiary mt-2px'>{description}</div>}
    </div>
    <div className='flex items-center'>{children}</div>
  </div>
);

const SectionHeader: React.FC<{ title: string; action?: React.ReactNode }> = ({ title, action }) => (
  <div className='flex items-center justify-between mb-12px'>
    <h3 className='text-14px font-500 text-t-primary m-0'>{title}</h3>
    {action}
  </div>
);

interface DiscordConfigFormProps {
  pluginStatus: IChannelPluginStatus | null;
  /** 多机器人模式下寻址的渠道行；缺省 = 全局设置页 legacy 单行行为。 */
  channelTarget?: ChannelTarget;
  onStatusChange: (status: IChannelPluginStatus | null) => void;
  onTokenChange?: (token: string) => void;
}

const DiscordConfigForm: React.FC<DiscordConfigFormProps> = ({ pluginStatus, channelTarget, onStatusChange, onTokenChange }) => {
  const { t } = useTranslation();

  const [discordToken, setDiscordToken] = useState('');
  const [testLoading, setTestLoading] = useState(false);
  const [pairingLoading, setPairingLoading] = useState(false);
  const [usersLoading, setUsersLoading] = useState(false);
  const [pendingPairings, setPendingPairings] = useState<IChannelPairingRequest[]>([]);
  const [authorizedUsers, setAuthorizedUsers] = useState<IChannelUser[]>([]);

  const loadPendingPairings = useCallback(async () => {
    setPairingLoading(true);
    try {
      const pairings = await channel.getPendingPairings.invoke();
      if (pairings) {
        setPendingPairings(pairings.filter((p) => p.platformType === 'discord' && (!channelTarget?.channelId || p.channelId === channelTarget.channelId)));
      }
    } catch (error) {
      console.error('[ChannelSettings] Failed to load pending pairings:', error);
    } finally {
      setPairingLoading(false);
    }
  }, [channelTarget?.channelId]);

  const loadAuthorizedUsers = useCallback(async () => {
    setUsersLoading(true);
    try {
      const users = await channel.getAuthorizedUsers.invoke();
      if (users) {
        setAuthorizedUsers(users.filter((u) => u.platformType === 'discord' && (!channelTarget?.channelId || u.channelId === channelTarget.channelId)));
      }
    } catch (error) {
      console.error('[ChannelSettings] Failed to load authorized users:', error);
    } finally {
      setUsersLoading(false);
    }
  }, [channelTarget?.channelId]);

  useEffect(() => {
    void loadPendingPairings();
    void loadAuthorizedUsers();
  }, [loadPendingPairings, loadAuthorizedUsers]);

  useEffect(() => {
    const unsubscribe = channel.pairingRequested.on((request) => {
      if (request.platformType !== 'discord') return;
      if (channelTarget?.channelId && request.channelId !== channelTarget.channelId) return;
      setPendingPairings((prev) => (prev.some((p) => p.code === request.code) ? prev : [request, ...prev]));
    });
    return () => unsubscribe();
  }, [channelTarget?.channelId]);

  useEffect(() => {
    const unsubscribe = channel.userAuthorized.on((user) => {
      if (user.platformType !== 'discord') return;
      if (channelTarget?.channelId && user.channelId !== channelTarget.channelId) return;
      setAuthorizedUsers((prev) => (prev.some((u) => u.id === user.id) ? prev : [user, ...prev]));
      setPendingPairings((prev) => prev.filter((p) => p.platformUserId !== user.platformUserId));
    });
    return () => unsubscribe();
  }, [channelTarget?.channelId]);

  const handleAutoEnable = async () => {
    try {
      const config = { credentials: { token: discordToken.trim() } };
      await channel.enablePlugin.invoke(channelTarget ? { plugin_id: channelTarget.channelId, plugin_type: 'discord', ...(channelTarget.publicAgentId ? { public_agent_id: channelTarget.publicAgentId } : { companion_id: channelTarget.companionId }), config } : { plugin_id: 'discord', config });
      Message.success(t('settings.discord.pluginEnabled', 'Discord bot enabled'));
      const plugins = await channel.getPluginStatus.invoke();
      if (plugins) {
        const discordPlugin = channelTarget ? (channelTarget.channelId ? plugins.find((p) => p.id === channelTarget.channelId) : plugins.find((p) => p.type === 'discord' && p.companionId === channelTarget.companionId)) : plugins.find((p) => p.type === 'discord');
        onStatusChange(discordPlugin || null);
      }
    } catch (error: unknown) {
      console.error('[ChannelSettings] Auto-enable failed:', error);
    }
  };

  const handleTestConnection = async () => {
    if (!discordToken.trim()) {
      Message.warning(t('settings.discord.tokenRequired', 'Please enter a bot token'));
      return;
    }
    setTestLoading(true);
    try {
      const result = await channel.testPlugin.invoke({ plugin_id: 'discord', token: discordToken.trim() });
      if (result.success) {
        Message.success(t('settings.discord.connectionSuccess', { defaultValue: 'Connected! Bot: {{username}}', username: result.bot_username || 'unknown' }));
        await handleAutoEnable();
      } else {
        Message.error(result.error || t('settings.discord.connectionFailed', 'Connection failed'));
      }
    } catch (error: any) {
      Message.error(error.message || t('settings.discord.connectionFailed', 'Connection failed'));
    } finally {
      setTestLoading(false);
    }
  };

  const handleTokenChange = (value: string) => {
    setDiscordToken(value);
    onTokenChange?.(value);
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
      Message.info(t('settings.assistant.pairingRejected', 'Pairing rejected'));
      await loadPendingPairings();
    } catch (error: unknown) {
      Message.error(error instanceof Error ? error.message : String(error));
    }
  };

  const handleRevokeUser = async (user_id: string) => {
    try {
      await channel.revokeUser.invoke({ user_id });
      Message.success(t('settings.assistant.userRevoked', 'User access revoked'));
      await loadAuthorizedUsers();
    } catch (error: unknown) {
      Message.error(error instanceof Error ? error.message : String(error));
    }
  };

  const copyToClipboard = (text: string) => {
    void navigator.clipboard.writeText(text);
    Message.success(t('common.copySuccess', 'Copied'));
  };

  const formatTime = (timestamp: number) => new Date(timestamp).toLocaleString();
  const getRemainingTime = (expiresAt: number) => `${Math.max(0, Math.ceil((expiresAt - Date.now()) / 1000 / 60))} min`;

  // Row-scoped credential lock — lock only when THIS bot row is live.
  const credentialsLocked = !!pluginStatus?.connected;

  return (
    <div className='flex flex-col gap-24px'>
      <PreferenceRow label={t('settings.discord.botToken', 'Bot Token')} description={t('settings.discord.botTokenDesc', 'Create an application at the Discord Developer Portal, add a Bot, and copy its token.')}>
        <div className='flex items-center gap-8px'>
          <Input.Password value={discordToken} onChange={handleTokenChange} placeholder={pluginStatus?.hasToken ? '••••••••••••••••' : 'MTxxxxxxxx.Gxxxxx.xxxxxxxx'} style={{ width: 240 }} visibilityToggle disabled={credentialsLocked} />
          {credentialsLocked ? (
            <Tooltip content={t('settings.discord.tokenLocked', 'Disable this channel before modifying the configuration')}>
              <span>
                <Button type='outline' loading={testLoading} onClick={handleTestConnection} disabled={credentialsLocked}>
                  {t('settings.assistant.testConnection', 'Test')}
                </Button>
              </span>
            </Tooltip>
          ) : (
            <Button type='outline' loading={testLoading} onClick={handleTestConnection} disabled={credentialsLocked}>
              {t('settings.assistant.testConnection', 'Test')}
            </Button>
          )}
        </div>
      </PreferenceRow>

      {/* Privileged-intent reminder: Discord requires the Message Content Intent. */}
      <div className='bg-[rgba(var(--primary-rgb),0.08)] rd-12px p-12px border border-[rgba(var(--primary-rgb),0.2)] text-12px text-t-secondary'>
        {t('settings.discord.intentNote', 'In the Developer Portal → Bot → Privileged Gateway Intents, enable "Message Content Intent", otherwise the bot cannot read message text. Invite the bot to your server (or DM it) to start.')}
      </div>

      {/* Pending Pairings */}
      {pluginStatus?.enabled && authorizedUsers.length === 0 && (
        <div className='bg-fill-1 rd-12px pt-16px pr-16px pb-16px pl-0'>
          <SectionHeader title={t('settings.assistant.pendingPairings', 'Pending Pairing Requests')} action={<Button size='mini' type='text' icon={<Refresh size={14} />} loading={pairingLoading} onClick={loadPendingPairings}>{t('common.refresh', 'Refresh')}</Button>} />
          {pairingLoading ? (
            <div className='flex justify-center py-24px'><Spin /></div>
          ) : pendingPairings.length === 0 ? (
            <Empty description={t('settings.assistant.noPendingPairings', 'No pending pairing requests')} />
          ) : (
            <div className='flex flex-col gap-12px'>
              {pendingPairings.map((pairing) => (
                <div key={pairing.code} className='flex items-center justify-between bg-fill-2 rd-8px p-12px'>
                  <div className='flex-1'>
                    <div className='flex items-center gap-8px'>
                      <span className='text-14px font-500 text-t-primary'>{pairing.display_name || 'Unknown User'}</span>
                      <Tooltip content={t('settings.assistant.copyCode', 'Copy pairing code')}>
                        <button className='p-4px bg-transparent border-none text-t-tertiary hover:text-t-primary cursor-pointer' onClick={() => copyToClipboard(pairing.code)}>
                          <Copy size={14} />
                        </button>
                      </Tooltip>
                    </div>
                    <div className='text-12px text-t-tertiary mt-4px'>
                      {t('settings.assistant.pairingCode', 'Code')}: <code className='bg-fill-3 px-4px rd-2px'>{pairing.code}</code>
                      <span className='mx-8px'>|</span>
                      {t('settings.assistant.expiresIn', 'Expires in')}: {getRemainingTime(pairing.expiresAt)}
                    </div>
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

      {/* Authorized Users */}
      {pluginStatus?.enabled && authorizedUsers.length > 0 && (
        <div className='bg-fill-1 rd-12px pt-16px pr-16px pb-16px pl-0'>
          <SectionHeader title={t('settings.assistant.authorizedUsers', 'Authorized Users')} action={<Button size='mini' type='text' icon={<Refresh size={14} />} loading={usersLoading} onClick={loadAuthorizedUsers}>{t('common.refresh', 'Refresh')}</Button>} />
          {usersLoading ? (
            <div className='flex justify-center py-24px'><Spin /></div>
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
                    <Button type='text' status='danger' size='small' icon={<Delete size={16} />} onClick={() => handleRevokeUser(user.id)} />
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

export default DiscordConfigForm;
