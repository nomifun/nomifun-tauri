/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * New API 网关平台标识
 * New API gateway platform identifier
 */
export const NEW_API_PLATFORM_ID = 'new-api';

/**
 * 检查平台是否为 New API 网关类型
 * Check if platform is New API gateway type
 */
export const isNewApiPlatform = (platform: string): boolean => {
  return platform === NEW_API_PLATFORM_ID;
};

/**
 * 订阅制「套餐」网关（Coding Plan / Agent Plan 等）只暴露 chat/completions，
 * 其 Base URL 没有 `/models` 目录端点（例如 GET .../api/plan/v3/models → 404）。
 * Subscription "plan" gateways (Coding Plan / Agent Plan style) expose ONLY the
 * chat-completions path — their base URL has no `/models` catalog.
 *
 * 添加/编辑模型弹窗保存前会用 `/models` 列表来校验 Key（detectProtocol 的 OpenAI
 * 探针），对这些端点会得到 404，从而把一个「合法的套餐 Key」误判为不可用。故对这些
 * 平台跳过保存前的 Key 探测——按模型的「心跳检测」按钮（真实 chat completion）才是
 * 它们正确的校验方式。
 * The pre-save key probe lists `/models`, which 404s on these gateways and would
 * wrongly reject a valid subscription key. Skip that probe for these platforms;
 * the per-model health-check button (a real chat completion) validates them.
 */
export const PLATFORMS_WITHOUT_MODELS_ENDPOINT: ReadonlySet<string> = new Set([
  'ark-coding-plan',
  'ark-agent-plan',
  'minimax-coding-plan',
  'stepfun-plan',
  'dashscope-coding',
  'glm-coding-plan',
  'qianfan-coding-plan',
]);

/**
 * 平台的 Base URL 是否没有 `/models` 目录端点（订阅套餐网关）。
 * Whether the platform's base URL has no `/models` catalog (subscription plan gateway).
 */
export const platformHasNoModelsEndpoint = (platform?: string | null): boolean => {
  return !!platform && PLATFORMS_WITHOUT_MODELS_ENDPOINT.has(platform);
};
