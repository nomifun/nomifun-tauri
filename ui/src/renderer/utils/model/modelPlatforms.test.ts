/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { MODEL_PLATFORMS, getPlatformByValue } from './modelPlatforms';

const platform = (value: string) => {
  const found = getPlatformByValue(value);
  if (!found) throw new Error(`Missing model platform: ${value}`);
  return found;
};

describe('MODEL_PLATFORMS coding plan presets', () => {
  test('exposes Doubao/Ark as both ordinary API and Coding Plan choices', () => {
    expect(platform('Ark').name.includes('Doubao')).toBe(true);
    expect(platform('Ark').platform).toBe('ark');

    const coding = platform('Ark-Coding-Plan');
    expect(coding.name.includes('Coding Plan')).toBe(true);
    expect(coding.platform).toBe('ark-coding-plan');
    expect(coding.base_url).toBe('https://ark.cn-beijing.volces.com/api/coding/v3');

    const agent = platform('Ark-Agent-Plan');
    expect(agent.name.includes('Agent Plan')).toBe(true);
    expect(agent.platform).toBe('ark-agent-plan');
    // Agent Plan has its OWN endpoint: /api/plan/v3 (OpenAI-compat) — distinct
    // from Coding Plan's /api/coding/v3 and from pay-as-you-go /api/v3.
    // The quota is determined by the endpoint + key; the wrong path fails auth
    // or bills the wrong plan.
    expect(agent.base_url).toBe('https://ark.cn-beijing.volces.com/api/plan/v3');
  });

  test('uses dedicated platform keys for domestic coding plan endpoints', () => {
    expect(platform('MiMo').platform).toBe('mimo');
    expect(platform('MiMo').base_url).toBe('https://api.xiaomimimo.com/v1');

    expect(platform('MiMo-Token-Plan-CN').platform).toBe('mimo-token-plan-cn');
    expect(platform('MiMo-Token-Plan-CN').base_url).toBe('https://token-plan-cn.xiaomimimo.com/v1');

    expect(platform('MiMo-Token-Plan-SGP').platform).toBe('mimo-token-plan-sgp');
    expect(platform('MiMo-Token-Plan-SGP').base_url).toBe('https://token-plan-sgp.xiaomimimo.com/v1');

    expect(platform('MiMo-Token-Plan-AMS').platform).toBe('mimo-token-plan-ams');
    expect(platform('MiMo-Token-Plan-AMS').base_url).toBe('https://token-plan-ams.xiaomimimo.com/v1');

    expect(platform('MiniMax-Code').platform).toBe('minimax-code');
    expect(platform('MiniMax-Code').base_url).toBe('https://api.minimax.io/v1');

    expect(platform('MiniMax-Coding-Plan').platform).toBe('minimax-coding-plan');
    expect(platform('MiniMax-Coding-Plan').base_url).toBe('https://api.minimaxi.com/v1');

    expect(platform('StepFun-Plan').platform).toBe('stepfun-plan');
    expect(platform('StepFun-Plan').base_url).toBe('https://api.stepfun.com/step_plan/v1');

    expect(platform('Dashscope-Coding').platform).toBe('dashscope-coding');
    expect(platform('Dashscope-Coding').base_url).toBe('https://coding.dashscope.aliyuncs.com/v1');

    expect(platform('GLM-Coding-Plan').platform).toBe('glm-coding-plan');
    expect(platform('GLM-Coding-Plan').base_url).toBe('https://open.bigmodel.cn/api/coding/paas/v4');

    expect(platform('Qianfan-Coding-Plan').platform).toBe('qianfan-coding-plan');
    expect(platform('Qianfan-Coding-Plan').base_url).toBe('https://qianfan.baidubce.com/v2/coding');
  });

  test('keeps ordinary API presets distinct from coding plan presets', () => {
    const byValue = new Map(MODEL_PLATFORMS.map((item) => [item.value, item]));

    expect(byValue.get('Dashscope')?.platform).toBe('dashscope');
    expect(byValue.get('Dashscope')?.base_url).toBe('https://dashscope.aliyuncs.com/compatible-mode/v1');

    expect(byValue.get('MiniMax')?.platform).toBe('minimax');
    expect(byValue.get('MiniMax')?.base_url).toBe('https://api.minimaxi.com/v1');

    expect(byValue.get('Zhipu')?.platform).toBe('zhipu');
    expect(byValue.get('Zhipu')?.base_url).toBe('https://open.bigmodel.cn/api/paas/v4');

    expect(byValue.get('Qianfan')?.platform).toBe('qianfan');
    expect(byValue.get('Qianfan')?.base_url).toBe('https://qianfan.baidubce.com/v2');
  });
});
