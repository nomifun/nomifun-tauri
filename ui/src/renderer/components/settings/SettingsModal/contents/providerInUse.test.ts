/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { featureRoute, groupUsagesByFeature, parseProviderInUseDetails, type ProviderUsage } from './providerInUse';
import { parseCompanionId, parseConversationId, parseExecutionId, parsePublicAgentId } from '@/common/types/ids';

const COMPANION_1 = parseCompanionId('0190f5fe-7c00-7a00-8000-000000000001');
const COMPANION_2 = parseCompanionId('0190f5fe-7c00-7a00-8000-000000000002');
const PUBLIC_AGENT = parsePublicAgentId('0190f5fe-7c00-7a00-8000-000000000001');
const CONVERSATION = parseConversationId('0190f5fe-7c00-7a00-8000-000000000001');
const EXECUTION = parseExecutionId('0190f5fe-7c00-7a00-8000-000000000001');

describe('providerInUse helpers', () => {
  test('featureRoute maps each feature', () => {
    expect(featureRoute('desktopCompanion')).toBe('/nomi');
    expect(featureRoute('publicCompanion', PUBLIC_AGENT)).toBe(`/public-companions/${PUBLIC_AGENT}`);
    expect(featureRoute('publicCompanion')).toBe('/public-companions');
    expect(featureRoute('smartDecision')).toBe('/models?section=global');
    expect(featureRoute('conversation', CONVERSATION)).toBe(`/conversation/${CONVERSATION}`);
    expect(featureRoute('conversation')).toBe('/guid');
    expect(featureRoute('agentExecution')).toBe('/guid');
  });

  test('groupUsagesByFeature groups labels', () => {
    const usages: ProviderUsage[] = [
      { feature: 'desktopCompanion', label: '甲', targetId: COMPANION_1 },
      { feature: 'desktopCompanion', label: '乙', targetId: COMPANION_2 },
      { feature: 'conversation', label: '主会话', targetId: CONVERSATION },
      { feature: 'agentExecution', label: '协作任务', targetId: EXECUTION },
    ];
    const groups = groupUsagesByFeature(usages);
    expect(groups.find((g) => g.feature === 'desktopCompanion')?.labels).toEqual(['甲', '乙']);
    expect(groups.find((g) => g.feature === 'conversation')?.targetId).toBe(CONVERSATION);
    expect(groups.find((g) => g.feature === 'agentExecution')?.targetId).toBe(EXECUTION);
  });

  test('parseProviderInUseDetails extracts usages and tolerates junk', () => {
    expect(
      parseProviderInUseDetails({ usages: [{ feature: 'agentExecution', label: '协作任务', targetId: EXECUTION }] })
    ).toHaveLength(1);
    expect(parseProviderInUseDetails(undefined)).toEqual([]);
    expect(parseProviderInUseDetails({ nope: 1 })).toEqual([]);
    expect(parseProviderInUseDetails('string')).toEqual([]);
  });
});
