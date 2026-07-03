/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IChannelPluginStatus } from '@/common/types/channel/channel';
import { channel, webui, type IWebUIStatus } from '@/common/adapter/ipcBridge';
import ChannelDingTalkLogo from '@/renderer/assets/channel-logos/dingtalk.svg';
import ChannelDiscordLogo from '@/renderer/assets/channel-logos/discord.svg';
import ChannelLarkLogo from '@/renderer/assets/channel-logos/lark.svg';
import ChannelMatrixLogo from '@/renderer/assets/channel-logos/matrix.svg';
import ChannelMattermostLogo from '@/renderer/assets/channel-logos/mattermost.svg';
import ChannelNostrLogo from '@/renderer/assets/channel-logos/nostr.svg';
import ChannelQQBotLogo from '@/renderer/assets/channel-logos/qqbot.svg';
import ChannelSlackLogo from '@/renderer/assets/channel-logos/slack.svg';
import ChannelTelegramLogo from '@/renderer/assets/channel-logos/telegram.svg';
import ChannelTwitchLogo from '@/renderer/assets/channel-logos/twitch.svg';
import ChannelWecomLogo from '@/renderer/assets/channel-logos/wecom.svg';
import ChannelWeixinLogo from '@/renderer/assets/channel-logos/weixin.svg';
import type { ChannelTarget, MasterAgentPlatform } from '@/renderer/components/settings/SettingsModal/contents/channels/channelTarget';
import DingTalkConfigForm from '@/renderer/components/settings/SettingsModal/contents/channels/DingTalkConfigForm';
import DiscordConfigForm from '@/renderer/components/settings/SettingsModal/contents/channels/DiscordConfigForm';
import LarkConfigForm from '@/renderer/components/settings/SettingsModal/contents/channels/LarkConfigForm';
import MatrixConfigForm from '@/renderer/components/settings/SettingsModal/contents/channels/MatrixConfigForm';
import MattermostConfigForm from '@/renderer/components/settings/SettingsModal/contents/channels/MattermostConfigForm';
import NostrConfigForm from '@/renderer/components/settings/SettingsModal/contents/channels/NostrConfigForm';
import QQBotConfigForm from '@/renderer/components/settings/SettingsModal/contents/channels/QQBotConfigForm';
import SlackConfigForm from '@/renderer/components/settings/SettingsModal/contents/channels/SlackConfigForm';
import TwitchConfigForm from '@/renderer/components/settings/SettingsModal/contents/channels/TwitchConfigForm';
import TelegramConfigForm from '@/renderer/components/settings/SettingsModal/contents/channels/TelegramConfigForm';
import WecomConfigForm from '@/renderer/components/settings/SettingsModal/contents/channels/WecomConfigForm';
import WeixinConfigForm from '@/renderer/components/settings/SettingsModal/contents/channels/WeixinConfigForm';
import { Message, Switch } from '@arco-design/web-react';
import React, { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

/**
 * Shared channel-config machinery for the multi-bot flows. Both the desktop
 * companion's 「远程连接」 (RemoteConnectSection) and the 对外伙伴 console's
 * ChannelsSection render this body inside their own NomiModal — the only
 * difference is who the bot is bound to (`channelTarget.companionId` vs
 * `channelTarget.publicAgentId`).
 */

/** Builtin IM platforms a bot can greet (same set as channel master-agent mode). */
export const CHANNEL_PLATFORMS: ReadonlyArray<{ id: MasterAgentPlatform; logo: string; titleKey: string; fallback: string }> = [
  { id: 'weixin', logo: ChannelWeixinLogo, titleKey: 'settings.channels.weixinTitle', fallback: 'WeChat' },
  { id: 'lark', logo: ChannelLarkLogo, titleKey: 'settings.channels.larkTitle', fallback: 'Lark / Feishu' },
  { id: 'wecom', logo: ChannelWecomLogo, titleKey: 'settings.channels.wecomTitle', fallback: 'WeCom' },
  { id: 'dingtalk', logo: ChannelDingTalkLogo, titleKey: 'settings.channels.dingtalkTitle', fallback: 'DingTalk' },
  { id: 'qqbot', logo: ChannelQQBotLogo, titleKey: 'settings.channels.qqbotTitle', fallback: 'QQ Bot' },
  { id: 'telegram', logo: ChannelTelegramLogo, titleKey: 'settings.channels.telegramTitle', fallback: 'Telegram' },
  { id: 'discord', logo: ChannelDiscordLogo, titleKey: 'settings.channels.discordTitle', fallback: 'Discord' },
  { id: 'slack', logo: ChannelSlackLogo, titleKey: 'settings.channels.slackTitle', fallback: 'Slack' },
  { id: 'matrix', logo: ChannelMatrixLogo, titleKey: 'settings.channels.matrixTitle', fallback: 'Matrix' },
  { id: 'mattermost', logo: ChannelMattermostLogo, titleKey: 'settings.channels.mattermostTitle', fallback: 'Mattermost' },
  { id: 'twitch', logo: ChannelTwitchLogo, titleKey: 'settings.channels.twitchTitle', fallback: 'Twitch' },
  { id: 'nostr', logo: ChannelNostrLogo, titleKey: 'settings.channels.nostrTitle', fallback: 'Nostr' },
];

/** Per-platform i18n keys reused from the legacy channel settings page. */
export const CREDENTIALS_REQUIRED_KEY: Record<MasterAgentPlatform, string> = {
  telegram: 'settings.assistant.tokenRequired',
  discord: 'settings.discord.tokenRequired',
  slack: 'settings.slack.credentialsRequired',
  matrix: 'settings.matrix.credentialsRequired',
  mattermost: 'settings.mattermost.credentialsRequired',
  twitch: 'settings.twitch.credentialsRequired',
  nostr: 'settings.nostr.credentialsRequired',
  qqbot: 'settings.qqbot.credentialsRequired',
  lark: 'settings.lark.credentialsRequired',
  dingtalk: 'settings.dingtalk.credentialsRequired',
  weixin: 'settings.weixin.loginRequired',
  wecom: 'settings.wecom.configureFirst',
};

export const PLUGIN_ENABLED_KEY: Record<MasterAgentPlatform, string> = {
  telegram: 'settings.assistant.pluginEnabled',
  discord: 'settings.discord.pluginEnabled',
  slack: 'settings.slack.pluginEnabled',
  matrix: 'settings.matrix.pluginEnabled',
  mattermost: 'settings.mattermost.pluginEnabled',
  twitch: 'settings.twitch.pluginEnabled',
  nostr: 'settings.nostr.pluginEnabled',
  qqbot: 'settings.qqbot.pluginEnabled',
  lark: 'settings.lark.pluginEnabled',
  dingtalk: 'settings.dingtalk.pluginEnabled',
  weixin: 'settings.weixin.pluginEnabled',
  wecom: 'settings.wecom.pluginEnabled',
};

export const PLUGIN_DISABLED_KEY: Record<MasterAgentPlatform, string> = {
  telegram: 'settings.assistant.pluginDisabled',
  discord: 'settings.discord.pluginDisabled',
  slack: 'settings.slack.pluginDisabled',
  matrix: 'settings.matrix.pluginDisabled',
  mattermost: 'settings.mattermost.pluginDisabled',
  twitch: 'settings.twitch.pluginDisabled',
  nostr: 'settings.nostr.pluginDisabled',
  qqbot: 'settings.qqbot.pluginDisabled',
  lark: 'settings.lark.pluginDisabled',
  dingtalk: 'settings.dingtalk.pluginDisabled',
  weixin: 'settings.weixin.pluginDisabled',
  wecom: 'settings.wecom.pluginDisabled',
};

/**
 * Channel config modal body: enable/disable switch + the platform's full
 * config form (credentials, connection test, master-agent switch, pairing
 * approvals, authorized users).
 *
 * 模型选择器在这里**不渲染**:机器人复用所绑定对象(桌面伙伴 / 对外伙伴)的对话
 * 模型(后端按绑定对象的 profile.model 解析),故这里不向各平台表单传
 * `modelSelection`,表单据此隐藏「默认模型」下拉。
 *
 * `channelTarget` addresses one channel row (multi-bot model). When
 * `channelTarget.channelId` is missing the form runs in create mode: the
 * first enable creates a new row bound to `channelTarget.companionId` OR
 * `channelTarget.publicAgentId` (whichever is set — mutually exclusive).
 */
export const PlatformConfigBody: React.FC<{
  platform: MasterAgentPlatform;
  status: IChannelPluginStatus | null;
  channelTarget?: ChannelTarget;
  onStatusChange: (status: IChannelPluginStatus | null) => void;
  refreshStatuses: () => Promise<void>;
}> = ({ platform, status, channelTarget, onStatusChange, refreshStatuses }) => {
  const { t } = useTranslation();
  const [toggleLoading, setToggleLoading] = useState(false);
  // Telegram lets the user enable with a token typed in the form but not yet saved.
  const telegramTokenRef = useRef('');
  // WeCom's form surfaces callback URLs derived from the WebUI status (best-effort).
  const [webuiStatus, setWebuiStatus] = useState<IWebUIStatus | null>(null);

  useEffect(() => {
    if (platform !== 'wecom') return;
    let cancelled = false;
    webui.getStatus
      .invoke()
      .then((status) => {
        if (!cancelled && status) setWebuiStatus(status);
      })
      .catch(() => {
        // Best-effort only — the form degrades to localhost URLs.
      });
    return () => {
      cancelled = true;
    };
  }, [platform]);

  const handleToggleEnabled = async (enabled: boolean) => {
    setToggleLoading(true);
    try {
      if (enabled) {
        const pendingToken = platform === 'telegram' || platform === 'discord' ? telegramTokenRef.current.trim() : '';
        if (!status?.hasToken && !pendingToken) {
          Message.warning(t(CREDENTIALS_REQUIRED_KEY[platform]));
          return;
        }
        const config = pendingToken ? { credentials: { token: pendingToken } } : {};
        await channel.enablePlugin.invoke(
          channelTarget
            ? {
                plugin_id: channelTarget.channelId,
                plugin_type: platform,
                ...(channelTarget.publicAgentId
                  ? { public_agent_id: channelTarget.publicAgentId }
                  : { companion_id: channelTarget.companionId }),
                config,
              }
            : { plugin_id: platform, config }
        );
        Message.success(t(PLUGIN_ENABLED_KEY[platform]));
      } else {
        await channel.disablePlugin.invoke({ plugin_id: channelTarget?.channelId ?? platform });
        Message.success(t(PLUGIN_DISABLED_KEY[platform]));
      }
      await refreshStatuses();
    } catch (error: unknown) {
      Message.error(error instanceof Error ? error.message : String(error));
    } finally {
      setToggleLoading(false);
    }
  };

  return (
    <div className='flex flex-col gap-8px py-8px'>
      {/* 渠道启停 / Channel enable-disable */}
      <div className='flex items-center justify-between gap-12px bg-fill-2 rd-10px px-14px py-10px'>
        <div className='min-w-0'>
          <div className='text-14px text-t-primary font-500'>{t('nomi.settings.remoteEnableChannel')}</div>
          <div className='text-12px text-t-tertiary mt-2px'>{t('nomi.settings.remoteEnableChannelHint')}</div>
        </div>
        <Switch
          checked={status?.enabled || false}
          loading={toggleLoading}
          onChange={(checked: boolean) => void handleToggleEnabled(checked)}
        />
      </div>

      {/* 模型跟随绑定对象的对话模型 / Model follows the bound owner's chat model */}
      <div className='text-12px text-t-tertiary bg-fill-2 rd-10px px-14px py-10px'>
        {channelTarget?.publicAgentId
          ? t('publicCompanion.channels.modelFollowsAgent', {
              defaultValue: '机器人使用该对外伙伴的对话模型,在「概览」页配置',
            })
          : '机器人使用该伙伴的对话模型,在「对话」页配置'}
      </div>

      {platform === 'telegram' && (
        <TelegramConfigForm
          pluginStatus={status}
          channelTarget={channelTarget}
          onStatusChange={onStatusChange}
          onTokenChange={(token) => {
            telegramTokenRef.current = token;
          }}
        />
      )}
      {platform === 'discord' && (
        <DiscordConfigForm
          pluginStatus={status}
          channelTarget={channelTarget}
          onStatusChange={onStatusChange}
          onTokenChange={(token) => {
            telegramTokenRef.current = token;
          }}
        />
      )}
      {platform === 'slack' && <SlackConfigForm pluginStatus={status} channelTarget={channelTarget} onStatusChange={onStatusChange} />}
      {platform === 'matrix' && <MatrixConfigForm pluginStatus={status} channelTarget={channelTarget} onStatusChange={onStatusChange} />}
      {platform === 'mattermost' && <MattermostConfigForm pluginStatus={status} channelTarget={channelTarget} onStatusChange={onStatusChange} />}
      {platform === 'twitch' && <TwitchConfigForm pluginStatus={status} channelTarget={channelTarget} onStatusChange={onStatusChange} />}
      {platform === 'nostr' && <NostrConfigForm pluginStatus={status} channelTarget={channelTarget} onStatusChange={onStatusChange} />}
      {platform === 'qqbot' && <QQBotConfigForm pluginStatus={status} channelTarget={channelTarget} onStatusChange={onStatusChange} />}
      {platform === 'lark' && (
        <LarkConfigForm pluginStatus={status} channelTarget={channelTarget} onStatusChange={onStatusChange} />
      )}
      {platform === 'dingtalk' && (
        <DingTalkConfigForm pluginStatus={status} channelTarget={channelTarget} onStatusChange={onStatusChange} />
      )}
      {platform === 'weixin' && (
        <WeixinConfigForm pluginStatus={status} channelTarget={channelTarget} onStatusChange={onStatusChange} />
      )}
      {platform === 'wecom' && (
        <WecomConfigForm
          pluginStatus={status}
          channelTarget={channelTarget}
          onStatusChange={onStatusChange}
          webuiStatus={webuiStatus}
        />
      )}
    </div>
  );
};

export default PlatformConfigBody;
