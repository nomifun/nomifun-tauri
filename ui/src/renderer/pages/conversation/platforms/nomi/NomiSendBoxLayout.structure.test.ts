/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('Nomi sendbox control layout', () => {
  test('renders context usage as a click ring before the model selector and removes turn metrics copy', () => {
    const source = readSource(new URL('./NomiSendBox.tsx', import.meta.url));
    const sendBoxSource = readSource(new URL('../../../../components/chat/SendBox/index.tsx', import.meta.url));
    const contextRingSource = readSource(new URL('./ContextUsageRing.tsx', import.meta.url));
    const sendBoxIndex = source.indexOf('<SendBox');
    const rightToolsIndex = source.indexOf('rightTools={');
    const modelIndex = source.indexOf('<NomiModelSelector', rightToolsIndex);
    const contextRingIndex = source.indexOf('<ContextUsageRing', rightToolsIndex);
    const collaboratorIndex = source.indexOf('{collaboratorSelectorNode}', rightToolsIndex);

    expect(sendBoxIndex).toBeGreaterThan(-1);
    expect(rightToolsIndex).toBeGreaterThan(sendBoxIndex);
    expect(contextRingIndex).toBeGreaterThan(rightToolsIndex);
    expect(modelIndex).toBeGreaterThan(contextRingIndex);
    expect(collaboratorIndex).toBeGreaterThan(modelIndex);
    expect(source.includes('topRightTools={')).toBe(false);
    expect(source.includes('ContextUsagePill')).toBe(false);
    expect(source.includes("data-testid='nomi-context-usage-slot'")).toBe(false);
    expect(source.includes("data-testid='nomi-turn-metrics'")).toBe(false);
    expect(source.includes('formatTurnDuration')).toBe(false);
    expect(source.includes('formatTokenCount(tokenUsage.total_tokens)')).toBe(false);
    expect(sendBoxSource.includes("data-testid='sendbox-internal-status-row'")).toBe(true);
    expect(sendBoxSource.includes("data-testid='sendbox-top-right-tools'")).toBe(false);
    expect(contextRingSource.includes("data-testid='nomi-context-usage-ring'")).toBe(true);
    expect(contextRingSource.includes("data-testid='nomi-context-usage-popover'")).toBe(true);
    expect(contextRingSource.includes("trigger='click'")).toBe(true);
    expect(contextRingSource.includes('conic-gradient')).toBe(true);
    expect(contextRingSource.includes('h-22px w-22px')).toBe(true);
    expect(contextRingSource.includes('formatTokenCount(used)')).toBe(true);
    expect(contextRingSource.includes('formatTokenCount(max)')).toBe(true);
    expect(contextRingSource.includes("data-testid='nomi-context-usage'")).toBe(false);
    expect(contextRingSource.includes('rd-999px b b-solid px-10px')).toBe(false);
  });

  test('keeps collaborator controls next to the main model and moves cluster next to permission', () => {
    const chatSource = readSource(new URL('../../components/ChatConversation.tsx', import.meta.url));
    const sendBoxSource = readSource(new URL('./NomiSendBox.tsx', import.meta.url));

    const collaboratorBlock = chatSource.slice(
      chatSource.indexOf('const collaboratorSelectorNode'),
      chatSource.indexOf('const { providers: healProviders')
    );
    expect(collaboratorBlock.includes('<GuidCollaboratorSelector')).toBe(true);
    expect(collaboratorBlock.includes('<ClusterModePill')).toBe(false);
    expect(chatSource.includes('extraRightTools={<ClusterModePill conversation={conversation} />}')).toBe(true);

    const rightToolsIndex = sendBoxSource.indexOf('rightTools={');
    const contextRingIndex = sendBoxSource.indexOf('<ContextUsageRing', rightToolsIndex);
    const modelIndex = sendBoxSource.indexOf('<NomiModelSelector', rightToolsIndex);
    const collaboratorIndex = sendBoxSource.indexOf('{collaboratorSelectorNode}', rightToolsIndex);
    const clusterIndex = sendBoxSource.indexOf('{extraRightTools}', rightToolsIndex);
    const permissionIndex = sendBoxSource.indexOf('<AgentModeSelector', rightToolsIndex);

    expect(contextRingIndex).toBeGreaterThan(rightToolsIndex);
    expect(modelIndex).toBeGreaterThan(contextRingIndex);
    expect(collaboratorIndex).toBeGreaterThan(modelIndex);
    expect(clusterIndex).toBeGreaterThan(collaboratorIndex);
    expect(permissionIndex).toBeGreaterThan(clusterIndex);
  });

  test('keeps the cluster trigger icon-only instead of showing status text in the toolbar', () => {
    const source = readSource(new URL('../../components/ClusterModePill.tsx', import.meta.url));

    expect(source.includes("data-testid='cluster-mode-pill'")).toBe(true);
    expect(source.includes('sendbox-cluster-pill__dot')).toBe(true);
    expect(source.includes('conversation.cluster.pill,')).toBe(false);
    expect(source.includes('conversation.cluster.approvalShort')).toBe(false);
  });
});
