/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('Guid agent cluster controls', () => {
  test('shows collaborator and approval controls in the bottom config row when cluster mode is active', () => {
    const pageSource = readSource(new URL('./GuidPage.tsx', import.meta.url));
    const actionRowSource = readSource(new URL('./components/GuidActionRow.tsx', import.meta.url));

    expect(pageSource.includes("import GuidCollaboratorSelector from './components/GuidCollaboratorSelector'")).toBe(true);
    expect(pageSource.includes("import GuidClusterApprovalSelector from './components/GuidClusterApprovalSelector'")).toBe(true);
    expect(pageSource.includes('clusterMode ? collaboratorSelectorNode : undefined')).toBe(true);
    expect(pageSource.includes('clusterMode ? clusterApprovalSelectorNode : undefined')).toBe(true);

    const configGroup = actionRowSource.slice(
      actionRowSource.indexOf('<div className={styles.actionConfigGroup}'),
      actionRowSource.indexOf('</div>', actionRowSource.indexOf('<div className={styles.actionConfigGroup}'))
    );
    expect(actionRowSource.includes('collaboratorSelectorNode?: React.ReactNode')).toBe(true);
    expect(actionRowSource.includes('clusterApprovalSelectorNode?: React.ReactNode')).toBe(true);
    expect(configGroup.indexOf('{modelSelectorNode}')).toBeGreaterThan(-1);
    expect(configGroup.indexOf('{collaboratorSelectorNode}')).toBeGreaterThan(configGroup.indexOf('{modelSelectorNode}'));
    expect(configGroup.indexOf('{clusterApprovalSelectorNode}')).toBeGreaterThan(
      configGroup.indexOf('{collaboratorSelectorNode}')
    );
    expect(configGroup.indexOf('<AgentModeSelector')).toBeGreaterThan(configGroup.indexOf('{clusterApprovalSelectorNode}'));
  });

  test('stores the homepage cluster model range and approval mode on the created Nomi conversation', () => {
    const pageSource = readSource(new URL('./GuidPage.tsx', import.meta.url));
    const sendSource = readSource(new URL('./hooks/useGuidSend.ts', import.meta.url));

    expect(pageSource.includes('orchestratorModelRange,')).toBe(true);
    expect(pageSource.includes('orchestratorApprovalMode,')).toBe(true);
    expect(sendSource.includes('orchestratorModelRange?: TModelRange')).toBe(true);
    expect(sendSource.includes("orchestratorApprovalMode?: 'auto' | 'manual'")).toBe(true);
    expect(sendSource.includes('orchestrator_model_range: clusterMode ? orchestratorModelRange : undefined')).toBe(true);
    expect(sendSource.includes('orchestrator_approval_mode: clusterMode ? orchestratorApprovalMode : undefined')).toBe(true);
  });
});
