/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useMemo, useState } from 'react';
import type { ChannelPluginId } from '@/common/types/ids';
import { useTranslation } from 'react-i18next';
import { Button, Modal, Spin, Switch, Tag } from '@arco-design/web-react';
import { Connection } from '@icon-park/react';
import { channel } from '@/common/adapter/ipcBridge';
import type { IPublicAgent } from '@/common/adapter/ipcBridge';
import { isBackendHttpError } from '@/common/adapter/httpBridge';
import type { IChannelPluginStatus } from '@/common/types/channel/channel';
import NomiModal from '@/renderer/components/base/NomiModal';
import type { ChannelPlatform } from '@renderer/components/settings/SettingsModal/contents/channels/channelTarget';
import {
  CHANNEL_PLATFORMS,
  CREDENTIALS_REQUIRED_KEY,
  PLUGIN_DISABLED_KEY,
  PLUGIN_ENABLED_KEY,
  PlatformConfigBody,
} from '@renderer/components/channels/PlatformConfigBody';
import {
  retargetConfigAfterStatus,
  statusOwnedBy,
  statusIsUnbound,
  type ChannelConfigTarget,
} from '@renderer/components/channels/channelStatusSelection';
import type { ArcoMessageInstance } from '@renderer/utils/ui/useArcoMessage';
import { SectionCard } from '../components';

interface Props {
  agent: IPublicAgent;
  message: ArcoMessageInstance;
}

/**
 * 渠道部署 —— 为这位对外伙伴接入真实的 IM 渠道机器人。复用桌面伙伴「远程连接」的
 * 多机器人机制,只是绑定对象换成对外伙伴:每平台可为本伙伴配置机器人凭据、启停、
 * 绑定/解绑与删除。一个机器人只服务一个对象(对外伙伴或桌面伙伴,互斥);绑定后
 * 陌生人经该渠道自动被安全接待。
 *
 * Per-public-agent multi-bot manager over the same channel model as the desktop
 * companion's RemoteConnectSection. Each bot is one channel plugin entity; a bot
 * belongs to this agent when
 * `publicAgentId === agent.public_agent_id`. Pairing badges are
 * scoped by the plugin business UUID.
 */
const ChannelsSection: React.FC<Props> = ({ agent, message }) => {
  const { t } = useTranslation();

  // All channel plugins, indexed by business UUID (not platform type).
  const [statuses, setStatuses] = useState<Record<string, IChannelPluginStatus>>({});
  const [pendingCounts, setPendingCounts] = useState<Record<string, number>>({});
  const [busyPluginId, setBusyPluginId] = useState<ChannelPluginId | null>(null);
  const [loaded, setLoaded] = useState(false);
  // Config modal target: with channelPluginId = edit; without = create mode.
  const [configTarget, setConfigTarget] = useState<ChannelConfigTarget>(null);

  // ── Channel plugin statuses (REST snapshot + WS live updates) ──
  const refreshStatuses = useCallback(async () => {
    try {
      const plugins = await channel.getPluginStatus.invoke();
      if (!plugins) return;
      setStatuses(() => {
        const next: Record<string, IChannelPluginStatus> = {};
        for (const plugin of plugins) next[plugin.plugin_id] = plugin;
        return next;
      });
    } catch (error) {
      console.error('[PublicAgentChannels] Failed to load plugin statuses:', error);
    } finally {
      setLoaded(true);
    }
  }, []);

  useEffect(() => {
    void refreshStatuses();
    const unsubscribe = channel.pluginStatusChanged.on(({ status }) => {
      setStatuses((prev) =>
        prev[status.plugin_id]
          ? { ...prev, [status.plugin_id]: { ...prev[status.plugin_id], ...status } }
          : prev
      );
      void refreshStatuses();
    });
    return () => unsubscribe();
  }, [refreshStatuses]);

  // ── Pending pairing requests (badge per channel row) ──
  const refreshPendings = useCallback(async () => {
    try {
      const pairings = await channel.getPendingPairings.invoke();
      setPendingCounts(() => {
        const next: Record<string, number> = {};
        for (const pairing of pairings ?? []) {
          if (!pairing.channel_plugin_id) continue;
          next[pairing.channel_plugin_id] = (next[pairing.channel_plugin_id] ?? 0) + 1;
        }
        return next;
      });
    } catch (error) {
      console.error('[PublicAgentChannels] Failed to load pending pairings:', error);
    }
  }, []);

  useEffect(() => {
    void refreshPendings();
    const unsubs = [
      channel.pairingRequested.on(() => void refreshPendings()),
      channel.userAuthorized.on(() => void refreshPendings()),
    ];
    return () => unsubs.forEach((unsubscribe) => unsubscribe());
  }, [refreshPendings]);

  // Adopt the row created from inside a create-mode modal: once a row of that
  // platform bound to this agent shows up, retarget the modal so the enable
  // switch and the form address the new row instead of creating another one.
  useEffect(() => {
    if (!configTarget || configTarget.channelPluginId) return;
    const created = Object.values(statuses).find(
      (s) =>
        s.type === configTarget.platform &&
        statusOwnedBy(s, { publicAgentId: agent.public_agent_id })
    );
    if (created) setConfigTarget((prev) => retargetConfigAfterStatus(prev, created));
  }, [statuses, configTarget, agent.public_agent_id]);

  // ── Row actions ──
  const handleToggleEnabled = useCallback(
    async (row: IChannelPluginStatus, platform: ChannelPlatform, enabled: boolean) => {
      setBusyPluginId(row.plugin_id);
      try {
        if (enabled) {
          // The outer card has no credential inputs — point the user at the form instead.
          if (!row.hasToken) {
            message.warning(t(CREDENTIALS_REQUIRED_KEY[platform]));
            return;
          }
          const result = await channel.enablePlugin.invoke({ plugin_id: row.plugin_id, config: {} });
          if (!result.success) {
            throw new Error(
              result.error ||
                t('nomi.settings.remoteEnableFailed', { defaultValue: 'Failed to enable channel' })
            );
          }
          message.success(t(PLUGIN_ENABLED_KEY[platform]));
        } else {
          await channel.disablePlugin.invoke({ plugin_id: row.plugin_id });
          message.success(t(PLUGIN_DISABLED_KEY[platform]));
        }
        await refreshStatuses();
      } catch (error: unknown) {
        message.error(error instanceof Error ? error.message : String(error));
      } finally {
        setBusyPluginId(null);
      }
    },
    [message, refreshStatuses, t]
  );

  const applyPluginBinding = useCallback(
    async (pluginId: ChannelPluginId, bind: boolean) => {
      setBusyPluginId(pluginId);
      try {
        // Backend contract: null public_agent_id clears the binding. Atomic — persists
        // the binding AND resets only this channel plugin's sessions.
        await channel.setChannelPublicAgent.invoke({
          plugin_id: pluginId,
          public_agent_id: bind ? agent.public_agent_id : null,
        });
        message.success(
          bind
            ? t('publicCompanion.channels.bindSuccess', {
                defaultValue: '已改由「{{name}}」接待,该渠道会话已重置',
                name: agent.name,
              })
            : t('nomi.settings.remoteUnbindSuccess')
        );
        await refreshStatuses();
      } catch (error) {
        console.error(`[PublicAgentChannels] Failed to update binding for ${pluginId}:`, error);
        // Conflict (bot already bound elsewhere) carries the other owner's name in the
        // backend message — surface it verbatim.
        if (isBackendHttpError(error) && error.backendMessage) {
          message.error(error.backendMessage);
        } else {
          message.error(t('nomi.settings.remoteBindFailed'));
        }
      } finally {
        setBusyPluginId(null);
      }
    },
    [agent.public_agent_id, agent.name, message, refreshStatuses, t]
  );

  const confirmBind = useCallback(
    (row: IChannelPluginStatus) => {
      Modal.confirm({
        title: t('publicCompanion.channels.bindRow', { defaultValue: '绑定到此对外伙伴' }),
        content: t('publicCompanion.channels.bindConfirm', {
          defaultValue: '绑定后该机器人由「{{name}}」接待,并重置该渠道的活跃会话。',
          name: agent.name,
        }),
        onOk: () => applyPluginBinding(row.plugin_id, true),
      });
    },
    [applyPluginBinding, agent.name, t]
  );

  const confirmUnbind = useCallback(
    (row: IChannelPluginStatus) => {
      Modal.confirm({
        title: t('nomi.settings.remoteUnbindRow'),
        content: t('publicCompanion.channels.unbindConfirm', {
          defaultValue: '解绑后该机器人不再由此对外伙伴接待,并重置该渠道的活跃会话。',
        }),
        onOk: () => applyPluginBinding(row.plugin_id, false),
      });
    },
    [applyPluginBinding, t]
  );

  // Move (rebind) a bot that currently belongs to another object onto this
  // public agent — one bot serves one owner at a time, but moving is free.
  const confirmMove = useCallback(
    (row: IChannelPluginStatus) => {
      Modal.confirm({
        title: t('nomi.settings.remoteMoveHere'),
        content: t('nomi.settings.remoteMoveConfirm', {
          from: t('publicCompanion.channels.otherOwner', { defaultValue: '其他对象' }),
          to: agent.name,
        }),
        onOk: () => applyPluginBinding(row.plugin_id, true),
      });
    },
    [applyPluginBinding, agent.name, t]
  );

  const confirmDelete = useCallback(
    (row: IChannelPluginStatus) => {
      Modal.confirm({
        title: t('nomi.settings.remoteDeleteBot'),
        content: t('nomi.settings.remoteDeleteConfirm'),
        okButtonProps: { status: 'danger' },
        onOk: async () => {
          try {
            await channel.deletePlugin.invoke({ plugin_id: row.plugin_id });
            await refreshStatuses();
          } catch (error: unknown) {
            message.error(error instanceof Error ? error.message : String(error));
          }
        },
      });
    },
    [message, refreshStatuses, t]
  );

  // ── Row presentation helpers ──
  const statusTag = (row: IChannelPluginStatus | null) => {
    if (!row?.hasToken) {
      return (
        <Tag size='small' color='gray'>
          {t('nomi.settings.remoteStatusNotConfigured')}
        </Tag>
      );
    }
    if (row.enabled && row.connected) {
      return (
        <Tag size='small' color='green'>
          {t('nomi.settings.remoteStatusRunning')}
        </Tag>
      );
    }
    if (row.enabled) {
      return (
        <Tag size='small' bordered={false} className='!bg-primary-1 !text-primary-6'>
          {t('nomi.settings.remoteStatusEnabled')}
        </Tag>
      );
    }
    return (
      <Tag size='small' color='gray'>
        {t('nomi.settings.remoteStatusDisabled')}
      </Tag>
    );
  };

  /** Bot identity line (botUsername preferred over raw botKey), empty when unknown. */
  const botIdentityOf = (row: IChannelPluginStatus | null) => {
    const bot = row?.botUsername || row?.botKey;
    return bot ? t('nomi.settings.remoteBotIdentity', { bot }) : '';
  };

  const allRows = useMemo(() => Object.values(statuses), [statuses]);

  const configChannel = useMemo(
    () => CHANNEL_PLATFORMS.find((p) => p.id === configTarget?.platform),
    [configTarget?.platform]
  );

  return (
    <SectionCard
      icon={<Connection theme='outline' size='16' fill='currentColor' className='block' style={{ lineHeight: 0 }} />}
      title={t('publicCompanion.channels.title', { defaultValue: '渠道部署' })}
      desc={t('publicCompanion.channels.desc', {
        defaultValue:
          '为这位对外伙伴接入 IM 渠道机器人:填写机器人凭据、启停并绑定后,陌生人经该渠道会被自动安全接待。一个渠道机器人只服务一个对象。',
      })}
    >
      {!loaded ? (
        <div className='flex justify-center py-32px'>
          <Spin />
        </div>
      ) : (
        <div className='flex flex-col gap-10px'>
          {CHANNEL_PLATFORMS.map(({ id, logo, titleKey, fallback }) => {
            const title = t(titleKey, fallback);
            // Ignore unconfigured platform placeholders; configured plugins
            // carry credentials (`hasToken=true`).
            const rows = allRows.filter((s) => s.type === id && s.hasToken);
            const myRow = rows.find((r) =>
              statusOwnedBy(r, { publicAgentId: agent.public_agent_id })
            );
            const unboundRows = rows.filter((r) => statusIsUnbound(r));
            const otherRows = rows.filter(
              (r) =>
                !statusIsUnbound(r) &&
                !statusOwnedBy(r, {
                  publicAgentId: agent.public_agent_id,
                })
            );
            // The row this card talks about: this agent's bot, else a bindable one.
            const focusRow = myRow ?? unboundRows[0] ?? null;
            const pending = focusRow ? (pendingCounts[focusRow.plugin_id] ?? 0) : 0;

            let subtitle = '';
            let actions: React.ReactNode;
            if (myRow) {
              subtitle = botIdentityOf(myRow);
              actions = (
                <>
                  <Switch
                    checked={myRow.enabled}
                    loading={busyPluginId === myRow.plugin_id}
                    onChange={(checked: boolean) => void handleToggleEnabled(myRow, id, checked)}
                  />
                  <Button
                    size='small'
                    onClick={() => setConfigTarget({ platform: id, channelPluginId: myRow.plugin_id })}
                  >
                    {t('nomi.settings.remoteConfigure')}
                  </Button>
                  <Button size='small' onClick={() => confirmUnbind(myRow)}>
                    {t('nomi.settings.remoteUnbindRow')}
                  </Button>
                  <Button size='small' status='danger' onClick={() => confirmDelete(myRow)}>
                    {t('nomi.settings.remoteDeleteBot')}
                  </Button>
                </>
              );
            } else if (unboundRows.length > 0) {
              const bindable = unboundRows[0];
              subtitle = [t('nomi.settings.remoteUnboundBot'), botIdentityOf(bindable)].filter(Boolean).join(' · ');
              actions = (
                <>
                  <Button
                    size='small'
                    type='primary'
                    loading={busyPluginId === bindable.plugin_id}
                    onClick={() => confirmBind(bindable)}
                  >
                    {t('publicCompanion.channels.bindRow', { defaultValue: '绑定到此对外伙伴' })}
                  </Button>
                  <Button
                    size='small'
                    onClick={() => setConfigTarget({ platform: id, channelPluginId: bindable.plugin_id })}
                  >
                    {t('nomi.settings.remoteConfigure')}
                  </Button>
                </>
              );
            } else if (otherRows.length > 0) {
              const movable = otherRows[0];
              subtitle = t('publicCompanion.channels.otherBots', {
                defaultValue: '{{num}} 个机器人已绑定其他对象',
                num: otherRows.length,
              });
              actions = (
                <>
                  <Button
                    size='small'
                    type='primary'
                    loading={busyPluginId === movable.plugin_id}
                    onClick={() => confirmMove(movable)}
                  >
                    {t('nomi.settings.remoteMoveHere')}
                  </Button>
                  <Button size='small' onClick={() => setConfigTarget({ platform: id })}>
                    {t('publicCompanion.channels.createBot', { defaultValue: '连接机器人' })}
                  </Button>
                </>
              );
            } else {
              actions = (
                <Button size='small' type='primary' onClick={() => setConfigTarget({ platform: id })}>
                  {t('publicCompanion.channels.createBot', { defaultValue: '连接机器人' })}
                </Button>
              );
            }

            return (
              <div
                key={id}
                className='flex items-center gap-16px rd-10px border border-solid border-[var(--color-border-2)] bg-fill-1 px-14px py-12px flex-wrap'
              >
                <div className='flex items-center gap-10px w-200px shrink-0 min-w-0'>
                  <img src={logo} alt={title} className='w-18px h-18px object-contain shrink-0' />
                  <div className='min-w-0'>
                    <div className='flex items-center gap-6px'>
                      <span className='text-14px text-t-primary font-500 truncate'>{title}</span>
                      {statusTag(focusRow)}
                    </div>
                    {pending > 0 && (
                      <Tag size='small' color='orangered' className='mt-4px'>
                        {t('nomi.settings.remotePending', { num: pending })}
                      </Tag>
                    )}
                  </div>
                </div>
                <div className='flex-1 min-w-0 text-12px text-t-tertiary'>{subtitle}</div>
                <div className='flex items-center gap-8px shrink-0'>{actions}</div>
              </div>
            );
          })}
        </div>
      )}

      <div className='mt-12px text-11px text-t-tertiary leading-16px'>
        {t('publicCompanion.channels.footerHint', {
          defaultValue: '为某个平台连接并绑定机器人后,该渠道的陌生人将由这位对外伙伴接待。机器人使用该对外伙伴的对话模型。',
        })}
      </div>

      <NomiModal
        visible={Boolean(configTarget)}
        onCancel={() => {
          setConfigTarget(null);
          // Pairings may have been approved/rejected inside the form.
          void refreshPendings();
          void refreshStatuses();
        }}
        header={{
          title: t('nomi.settings.remoteConfigTitle', {
            channel: configChannel ? t(configChannel.titleKey, configChannel.fallback) : '',
          }),
          showClose: true,
        }}
        footer={null}
        style={{ width: 720 }}
        contentStyle={{ maxHeight: 'calc(80vh - 80px)', padding: '0 2px' }}
      >
        {configTarget && (
          <PlatformConfigBody
            key={configTarget.channelPluginId ?? `${configTarget.platform}:new`}
            platform={configTarget.platform}
            status={configTarget.channelPluginId ? (statuses[configTarget.channelPluginId] ?? null) : null}
            channelTarget={{
              channelPluginId: configTarget.channelPluginId,
              publicAgentId: agent.public_agent_id,
            }}
            onStatusChange={(status) => {
              // Forms report the plugin they saved; merge by business UUID.
              if (status) {
                setStatuses((prev) => ({ ...prev, [status.plugin_id]: status }));
                setConfigTarget((prev) => retargetConfigAfterStatus(prev, status));
              }
              void refreshStatuses();
            }}
            refreshStatuses={refreshStatuses}
          />
        )}
      </NomiModal>
    </SectionCard>
  );
};

export default ChannelsSection;
