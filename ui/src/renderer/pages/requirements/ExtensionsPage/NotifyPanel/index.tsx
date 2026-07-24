import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ipcBridge } from '@/common';
import { isHandledAuthExpiredHttpError } from '@/common/adapter/httpBridge';
import type { IWebhook } from '@/common/adapter/ipcBridge';
import ChannelList from './ChannelList';
import RoutingRuleList from './RoutingRuleList';

/**
 * NotifyPanel — the unified notification surface of the requirements platform.
 *
 * Brings together two formerly-split concerns in a single scrollable pane:
 *  1. 通知渠道 (CHANNELS): webhook endpoint CRUD (`ChannelList`).
 *  2. 触发规则 (ROUTING RULES): which tag's requirements, on which events,
 *     notify which channel (`RoutingRuleList`).
 */
const NotifyPanel: React.FC = () => {
  const { t } = useTranslation();
  const [channels, setChannels] = useState<IWebhook[]>([]);
  const [channelsLoading, setChannelsLoading] = useState(false);
  const [channelsError, setChannelsError] = useState<string | null>(null);
  const latestChannelRequest = useRef(0);

  /**
   * Channels are shared by the CRUD table and every routing-rule picker.
   * Keeping the request and state here guarantees that a successful mutation
   * updates both views from the same snapshot without requiring a remount.
   */
  const loadChannels = useCallback(async () => {
    const requestId = ++latestChannelRequest.current;
    setChannelsLoading(true);
    setChannelsError(null);
    try {
      const list = await ipcBridge.webhook.list.invoke();
      if (requestId === latestChannelRequest.current) {
        setChannels(list);
      }
    } catch (error) {
      if (isHandledAuthExpiredHttpError(error)) return;
      if (requestId === latestChannelRequest.current) {
        setChannelsError(String(error));
      }
    } finally {
      if (requestId === latestChannelRequest.current) {
        setChannelsLoading(false);
      }
    }
  }, []);

  useEffect(() => {
    void loadChannels();
  }, [loadChannels]);

  return (
    <div className='flex h-full flex-col gap-24px overflow-y-auto p-4px'>
      <section className='flex flex-col gap-12px'>
        <div className='flex flex-col gap-2px'>
          <h2 className='m-0 text-18px font-bold text-t-primary'>{t('requirements.notify.channelsTitle')}</h2>
          <p className='m-0 text-13px text-t-tertiary'>{t('requirements.notify.channelsHint')}</p>
        </div>
        <ChannelList
          channels={channels}
          loading={channelsLoading}
          error={channelsError}
          reloadChannels={loadChannels}
        />
      </section>

      <section className='flex flex-col gap-12px'>
        <div className='flex flex-col gap-2px'>
          <h2 className='m-0 text-18px font-bold text-t-primary'>{t('requirements.notify.rulesTitle')}</h2>
          <p className='m-0 text-13px text-t-tertiary'>{t('requirements.notify.rulesHint')}</p>
        </div>
        <RoutingRuleList channels={channels} />
      </section>
    </div>
  );
};

export default NotifyPanel;
