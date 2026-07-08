/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IProvider, ModelType } from '@/common/config/storage';

export { hasSpecificModelCapability } from '@/common/utils/modelCapabilities';

// 能力判断缓存
const modelCapabilitiesCache = new Map<string, boolean | undefined>();

/**
 * 特定 provider 的能力规则
 */
const PROVIDER_CAPABILITY_RULES: Record<string, Record<ModelType, boolean | null>> = {
  anthropic: {
    text: true,
    vision: true,
    function_calling: true,
    image_generation: false,
    video_generation: false,
    web_search: false,
    reasoning: false,
    embedding: false,
    rerank: false,
    excludeFromPrimary: false,
  },
  deepseek: {
    text: true,
    vision: null,
    function_calling: true,
    image_generation: false,
    video_generation: false,
    web_search: false,
    reasoning: null,
    embedding: false,
    rerank: false,
    excludeFromPrimary: false,
  },
};

/**
 * 检查用户是否手动配置了某个能力类型
 * @param model - 模型对象
 * @param type - 能力类型
 * @returns true/false 如果用户有明确配置，undefined 如果未配置
 */
const getUserSelectedCapability = (model: IProvider, type: ModelType): boolean | undefined => {
  const capability = model.capabilities?.find((cap) => cap.type === type);
  return capability?.is_user_selected;
};

/**
 * 根据 provider 获取特定能力的规则
 * @param provider - 提供商名称
 * @param type - 能力类型
 * @returns true/false/null (null表示使用默认逻辑)
 */
const getProviderCapabilityRule = (provider: string, type: ModelType): boolean | null => {
  const rules = PROVIDER_CAPABILITY_RULES[provider?.toLowerCase()];
  return rules?.[type] ?? null;
};
