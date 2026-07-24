/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (file: string) => readFileSync(new URL(file, import.meta.url), 'utf8');

describe('notification channel state synchronization', () => {
  test('the channel table and routing rules consume the same panel-owned snapshot', () => {
    const panel = readSource('./index.tsx');
    const channelList = readSource('./ChannelList.tsx');
    const routingRules = readSource('./RoutingRuleList.tsx');

    expect(panel.includes('const [channels, setChannels] = useState<IWebhook[]>([])')).toBe(true);
    expect(panel.includes('<ChannelList')).toBe(true);
    expect(panel.includes('channels={channels}')).toBe(true);
    expect(panel.includes('reloadChannels={loadChannels}')).toBe(true);
    expect(panel.includes('<RoutingRuleList channels={channels} />')).toBe(true);

    expect(channelList.includes('reloadChannels: () => Promise<void>')).toBe(true);
    expect(channelList.includes('await reloadChannels()')).toBe(true);

    expect(routingRules.includes('type RoutingRuleListProps')).toBe(true);
    expect(routingRules.includes('ipcBridge.webhook.list.invoke()')).toBe(false);
    expect(routingRules.includes('useState<IWebhook[]>([])')).toBe(false);
  });
});
