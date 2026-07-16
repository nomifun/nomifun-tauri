/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('Guid collaboration policy', () => {
  test('shares one Nomi-only policy control between home and conversation', () => {
    const pageSource = readSource(new URL('./GuidPage.tsx', import.meta.url));
    const chatSource = readSource(new URL('../conversation/components/ChatConversation.tsx', import.meta.url));
    const controlSource = readSource(new URL('../../components/collaboration/CollaborationPolicyControl.tsx', import.meta.url));

    expect(pageSource.includes('<CollaborationPolicyControl')).toBe(true);
    expect(chatSource.includes('<CollaborationPolicyControl')).toBe(true);
    expect(controlSource.includes("if (runtimeType !== 'nomi') return null")).toBe(true);
    expect(controlSource.includes("const DELEGATION_OPTIONS: TDelegationPolicy[] = ['disabled', 'automatic', 'prefer_parallel']"))
      .toBe(true);
    expect(controlSource.includes("decisionPolicy: checked ? 'ask_user' : 'automatic'")).toBe(true);
    expect(controlSource.includes("role='radiogroup'")).toBe(true);
    expect(controlSource.includes("role='radio'")).toBe(true);
    expect(controlSource.includes('aria-checked={active}')).toBe(true);
  });

  test('stores canonical policy fields on the created Nomi conversation', () => {
    const sendSource = readSource(new URL('./hooks/useGuidSend.ts', import.meta.url));

    expect(sendSource.includes('delegation_policy: delegationPolicy')).toBe(true);
    expect(sendSource.includes('execution_model_pool: executionModelPool')).toBe(true);
    expect(sendSource.includes('decision_policy: decisionPolicy')).toBe(true);

    const dependencies = sendSource.slice(sendSource.indexOf('  }, ['), sendSource.indexOf('  ]);'));
    expect(dependencies.includes('delegationPolicy')).toBe(true);
    expect(dependencies.includes('executionModelPool')).toBe(true);
    expect(dependencies.includes('decisionPolicy')).toBe(true);
  });

  test('reconciles persisted collaborator models before creating a pool', () => {
    const pageSource = readSource(new URL('./GuidPage.tsx', import.meta.url));
    const selectorSource = readSource(new URL('./components/GuidCollaboratorSelector.tsx', import.meta.url));

    expect(pageSource.includes('import { reconcileModelRefs, sameModelRefs }')).toBe(true);
    expect(pageSource.includes('const activeCollaborators = collaboratorReconciliation?.active ?? []')).toBe(true);
    expect(pageSource.includes('...activeCollaborators.filter(')).toBe(true);
    expect(pageSource.includes('value={activeCollaborators}')).toBe(true);
    expect(pageSource.includes('sameModelRefs(collaborationModels, collaboratorReconciliation.retained)')).toBe(true);
    expect(selectorSource.includes('const availableKeys = useMemo(')).toBe(true);
    expect(selectorSource.includes('disabled={isLoading}')).toBe(true);
  });
});
