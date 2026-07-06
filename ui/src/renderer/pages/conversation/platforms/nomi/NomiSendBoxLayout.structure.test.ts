/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('Nomi sendbox control layout', () => {
  test('moves context usage into the sendbox top row and removes turn metrics copy', () => {
    const source = readSource(new URL('./NomiSendBox.tsx', import.meta.url));
    const sendBoxIndex = source.indexOf('<SendBox');
    const topRightToolsIndex = source.indexOf('topRightTools={', sendBoxIndex);
    const contextPillIndex = source.indexOf('<ContextUsagePill', topRightToolsIndex);
    const rightToolsIndex = source.indexOf('rightTools={');

    expect(sendBoxIndex).toBeGreaterThan(-1);
    expect(topRightToolsIndex).toBeGreaterThan(sendBoxIndex);
    expect(contextPillIndex).toBeGreaterThan(topRightToolsIndex);
    expect(rightToolsIndex).toBeGreaterThan(sendBoxIndex);
    expect(source.indexOf('<ContextUsagePill', rightToolsIndex)).toBe(-1);
    expect(source.includes("data-testid='nomi-context-usage-slot'")).toBe(false);
    expect(source.includes("data-testid='nomi-turn-metrics'")).toBe(false);
    expect(source.includes('formatTurnDuration')).toBe(false);
    expect(source.includes('formatTokenCount(tokenUsage.total_tokens)')).toBe(false);
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
    const modelIndex = sendBoxSource.indexOf('<NomiModelSelector', rightToolsIndex);
    const collaboratorIndex = sendBoxSource.indexOf('{collaboratorSelectorNode}', rightToolsIndex);
    const clusterIndex = sendBoxSource.indexOf('{extraRightTools}', rightToolsIndex);
    const permissionIndex = sendBoxSource.indexOf('<AgentModeSelector', rightToolsIndex);

    expect(modelIndex).toBeGreaterThan(rightToolsIndex);
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
