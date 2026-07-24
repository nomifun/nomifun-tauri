/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IChannelPluginStatus } from '@/common/types/channel/channel';
import { channel } from '@/common/adapter/ipcBridge';
import { isBackendHttpError } from '@/common/adapter/httpBridge';
import NomiModal from '@/renderer/components/base/NomiModal';
import type { ChannelPlatform } from '@/renderer/components/settings/SettingsModal/contents/channels/channelTarget';
import {
  CHANNEL_PLATFORMS,
  CREDENTIALS_REQUIRED_KEY,
  PLUGIN_DISABLED_KEY,
  PLUGIN_ENABLED_KEY,
  PlatformConfigBody,
} from '@/renderer/components/channels/PlatformConfigBody';
import {
  retargetConfigAfterStatus,
  statusOwnedBy,
  statusIsUnbound,
  type ChannelConfigTarget,
} from '@/renderer/components/channels/channelStatusSelection';
import { Button, Message, Modal, Switch, Tag } from '@arco-design/web-react';
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import type { ChannelPluginId, CompanionId } from '@/common/types/ids';
import { useTranslation } from 'react-i18next';
import { useCompanions } from '../useNomi';

/**
 * 伙伴设置页「远程连接」节：每伙伴视角的多机器人管理。
 * 每个机器人 = 一个 channel plugin 实体（companion_id 绑宠，UNIQUE(type,bot_key)
 * 保证同一机器人不绑多宠）。同一平台可以有多个实体：本宠的机器人直接启停/配置/
 * 解绑/删除；未绑定的机器人可以绑到本宠；他宠的机器人可迁移，也可另建机器人。
 *
 * Per-companion "Remote connect" section over the multi-bot channel model. Each bot
 * is one channel plugin entity; the card for a platform branches on whether
 * this companion owns one, an unbound plugin exists, or only other companions' plugins exist.
 * Pending pairing requests still surface as a platform-level badge.
 */
const RemoteConnectSection: React.FC<{ companionId: CompanionId; companionName: string }> = ({ companionId, companionName }) => {
  const { t } = useTranslation();
  const { companions } = useCompanions();

  // All channel plugins, indexed by business UUID (not platform type).
  const [statuses, setStatuses] = useState<Record<string, IChannelPluginStatus>>({});
  const [pendingCounts, setPendingCounts] = useState<Record<string, number>>({});
  const [busyPluginId, setBusyPluginId] = useState<ChannelPluginId | null>(null);
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
      console.error('[RemoteConnect] Failed to load plugin statuses:', error);
    }
  }, []);

  useEffect(() => {
    void refreshStatuses();
    const unsubscribe = channel.pluginStatusChanged.on(({ status }) => {
      // Merge known plugins by business UUID for fast feedback, then reconcile
      // with a REST snapshot for deleted or newly created entities.
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
      console.error('[RemoteConnect] Failed to load pending pairings:', error);
    }
  }, []);

  useEffect(() => {
    void refreshPendings();
    const unsubs = [
      // New request → badge appears even while the user never opens settings.
      channel.pairingRequested.on(() => void refreshPendings()),
      // Approval resolves a pairing into an authorized user → badge shrinks.
      channel.userAuthorized.on(() => void refreshPendings()),
    ];
    return () => unsubs.forEach((unsubscribe) => unsubscribe());
  }, [refreshPendings]);

  // Adopt the plugin created inside a create-mode modal.
  useEffect(() => {
    if (!configTarget || configTarget.channelPluginId) return;
    const created = Object.values(statuses).find(
      (s) => s.type === configTarget.platform && statusOwnedBy(s, { companionId })
    );
    if (created) setConfigTarget((prev) => retargetConfigAfterStatus(prev, created));
  }, [statuses, configTarget, companionId]);

  const companionNameOf = useCallback(
    (id: CompanionId | null | undefined) =>
      companions.find((p) => p.companion_id === id)?.name,
    [companions]
  );

  // ── Row actions ──
  const handleToggleEnabled = useCallback(
    async (row: IChannelPluginStatus, platform: ChannelPlatform, enabled: boolean) => {
      setBusyPluginId(row.plugin_id);
      try {
        if (enabled) {
          // The outer card has no credential inputs (unlike the config modal's
          // telegram token field) — point the user at the form instead.
          if (!row.hasToken) {
            Message.warning(t(CREDENTIALS_REQUIRED_KEY[platform]));
            return;
          }
          const result = await channel.enablePlugin.invoke({ plugin_id: row.plugin_id, config: {} });
          if (!result.success) {
            throw new Error(
              result.error ||
                t('nomi.settings.remoteEnableFailed', { defaultValue: 'Failed to enable channel' })
            );
          }
          Message.success(t(PLUGIN_ENABLED_KEY[platform]));
        } else {
          await channel.disablePlugin.invoke({ plugin_id: row.plugin_id });
          Message.success(t(PLUGIN_DISABLED_KEY[platform]));
        }
        await refreshStatuses();
      } catch (error: unknown) {
        Message.error(error instanceof Error ? error.message : String(error));
      } finally {
        setBusyPluginId(null);
      }
    },
    [refreshStatuses, t]
  );

  const applyPluginBinding = useCallback(
    async (pluginId: ChannelPluginId, bind: boolean) => {
      setBusyPluginId(pluginId);
      try {
        // Backend contract: empty companion_id clears the binding. The call atomically
        // persists the binding AND resets only this channel plugin's sessions.
        await channel.setChannelCompanion.invoke({ plugin_id: pluginId, companion_id: bind ? companionId : null });
        Message.success(
          bind ? t('nomi.settings.remoteBindSuccess', { companionName }) : t('nomi.settings.remoteUnbindSuccess')
        );
        await refreshStatuses();
      } catch (error) {
        console.error(`[RemoteConnect] Failed to update binding for ${pluginId}:`, error);
        // Conflict (bot already bound to another companion) carries the other companion's
        // name in the backend message — surface it verbatim.
        if (isBackendHttpError(error) && error.backendMessage) {
          Message.error(error.backendMessage);
        } else {
          Message.error(t('nomi.settings.remoteBindFailed'));
        }
      } finally {
        setBusyPluginId(null);
      }
    },
    [companionId, companionName, refreshStatuses, t]
  );

  const confirmBind = useCallback(
    (row: IChannelPluginStatus) => {
      Modal.confirm({
        title: t('nomi.settings.remoteBindRow'),
        content: t('nomi.settings.remoteBindConfirm', { companionName }),
        onOk: () => applyPluginBinding(row.plugin_id, true),
      });
    },
    [applyPluginBinding, companionName, t]
  );

  const confirmUnbind = useCallback(
    (row: IChannelPluginStatus) => {
      Modal.confirm({
        title: t('nomi.settings.remoteUnbindRow'),
        content: t('nomi.settings.remoteUnbindConfirm', { companionName }),
        onOk: () => applyPluginBinding(row.plugin_id, false),
      });
    },
    [applyPluginBinding, companionName, t]
  );

  // Move (rebind) a bot that currently belongs to ANOTHER owner onto this
  // companion. A bot serves exactly one owner at a time, but moving is free —
  // this reuses the same setChannelCompanion rebind as bind (clears the
  // channel's old sessions server-side).
  const confirmMove = useCallback(
    (row: IChannelPluginStatus) => {
      const fromName = companionNameOf(row.companionId) ?? row.companionId ?? row.publicAgentId ?? '';
      Modal.confirm({
        title: t('nomi.settings.remoteMoveHere'),
        content: t('nomi.settings.remoteMoveConfirm', { from: fromName, to: companionName }),
        onOk: () => applyPluginBinding(row.plugin_id, true),
      });
    },
    [applyPluginBinding, companionNameOf, companionName, t]
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
            Message.error(error instanceof Error ? error.message : String(error));
          }
        },
      });
    },
    [refreshStatuses, t]
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
    <>
      <div className='mt-8px text-13px font-600 text-t-secondary'>{t('nomi.settings.remoteTitle')}</div>
      <div className='text-12px text-t-tertiary -mt-6px'>{t('nomi.settings.remoteHint', { companionName })}</div>

      {CHANNEL_PLATFORMS.map(({ id, logo, titleKey, fallback }) => {
        const title = t(titleKey, fallback);
        // Only configured plugins: `GET /plugins` may pad a builtin platform
        // with an unconfigured placeholder. Configured plugins carry credentials
        // (hasToken=true) — without this filter an empty platform would be
        // misread as "an unbound bot exists" and offer a binding that 404s.
        const rows = allRows.filter((s) => s.type === id && s.hasToken);
        const myRow = rows.find((r) => statusOwnedBy(r, { companionId }));
        const unboundRows = rows.filter((r) => statusIsUnbound(r));
        const otherRows = rows.filter((r) => !statusIsUnbound(r) && !statusOwnedBy(r, { companionId }));
        // The row this card talks about: this companion's bot, else a bindable one.
        const focusRow = myRow ?? unboundRows[0] ?? null;
        // Pending-pairing badge is per channel plugin business UUID, so a
        // second bot of the same platform shows its own count, not the platform's.
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
              <Button size='small' onClick={() => setConfigTarget({ platform: id, channelPluginId: myRow.plugin_id })}>
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
                {t('nomi.settings.remoteBindRow')}
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
          subtitle = t('nomi.settings.remoteOtherBots', {
            num: otherRows.length,
            companions: otherRows.map((r) => companionNameOf(r.companionId) ?? r.companionId).join(', '),
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
                {t('nomi.settings.remoteCreateBot')}
              </Button>
            </>
          );
        } else {
          actions = (
            <Button size='small' type='primary' onClick={() => setConfigTarget({ platform: id })}>
              {t('nomi.settings.remoteCreateBot')}
            </Button>
          );
        }

        return (
          <div key={id} className='flex items-center gap-16px bg-fill-2 rd-10px px-14px py-12px flex-wrap'>
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
              companionId,
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
    </>
  );
};

export default RemoteConnectSection;
