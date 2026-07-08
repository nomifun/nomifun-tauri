/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IProvider, ModelType } from '@/common/config/storage';

/**
 * Capability matching regex patterns
 *
 * `image_generation` / `video_generation` are the Creative Workshop generator
 * signals — kept in sync with the backend engine
 * `nomifun_api_types::infer_generation_capabilities`. They double as the source
 * for "which models can generate images/videos" queried by the workshop.
 */
export const CAPABILITY_PATTERNS: Record<ModelType, RegExp> = {
  text: /gpt|claude|gemini|qwen|llama|mistral|deepseek|mimo/i,
  vision: /4o|claude-3|gemini-.*-pro|gemini-.*-flash|gemini-2\.0|qwen-vl|llava|vision|mimo-v2\.5$/i,
  function_calling: /gpt-4|claude-3|gemini|qwen|deepseek|mimo/i,
  image_generation:
    /gpt-image|dall-e|dall|seedream|flux|stable-diffusion|sd-|sdxl|imagen|midjourney|mj-|nano-banana|kolors|hidream|janus|cogview|diffusion|stabilityai/i,
  video_generation: /sora|veo|kling|seedance|wanx|wan2|hailuo|vidu|cogvideo|pixverse|runway|luma|dream-machine/i,
  web_search: /search|perplexity/i,
  reasoning: /o1-|reasoning|think|mimo-v2\.5/i,
  embedding: /(?:^text-|embed|bge-|e5-|LLM2Vec|retrieval|uae-|gte-|jina-clip|jina-embeddings|voyage-)/i,
  rerank: /(?:rerank|re-rank|re-ranker|re-ranking|retrieval|retriever)/i,
  excludeFromPrimary:
    /dall-e|flux|stable-diffusion|midjourney|flash-image|image|embed|rerank|sora|veo|kling|seedance|wanx|wan2|hailuo|vidu|cogvideo|pixverse|runway|dream-machine/i,
};

/**
 * Explicit exclusion lists (blacklist) for capabilities
 */
export const CAPABILITY_EXCLUSIONS: Record<ModelType, RegExp[]> = {
  text: [],
  // Generators (image/video) must never be mis-tagged as vision-understanding.
  vision: [
    /embed|rerank|dall-e|flux|stable-diffusion/i,
    /gpt-image|seedream|sdxl|imagen|midjourney|nano-banana|kolors|hidream|cogview/i,
    /sora|veo|kling|seedance|wanx|wan2|hailuo|vidu|cogvideo|pixverse|runway|dream-machine/i,
  ],
  function_calling: [
    /aqa(?:-[\w-]+)?/i,
    /imagen(?:-[\w-]+)?/i,
    /o1-mini/i,
    /o1-preview/i,
    /gemini-1(?:\\.[\w-]+)?/i,
    /dall-e/i,
    /embed/i,
    /rerank/i,
  ],
  image_generation: [],
  video_generation: [],
  web_search: [],
  reasoning: [],
  embedding: [],
  rerank: [],
  excludeFromPrimary: [],
};

/**
 * Get the lowercase, normalized base model name for matching.
 */
export const getBaseModelName = (modelName: string): string => {
  return modelName
    .toLowerCase()
    .replace(/[^a-z0-9./-]/g, '-')
    .replace(/-+/g, '-')
    .replace(/^-|-$/g, '');
};

/**
 * Check whether a specific model within a provider has a given capability.
 * Returns true (supported), false (excluded), or undefined (unknown).
 */
export const hasSpecificModelCapability = (
  _platformModel: IProvider,
  modelName: string,
  type: ModelType
): boolean | undefined => {
  const baseModelName = getBaseModelName(modelName);
  const exclusions = CAPABILITY_EXCLUSIONS[type];
  const pattern = CAPABILITY_PATTERNS[type];

  const isExcluded = exclusions.some((excludePattern) => excludePattern.test(baseModelName));
  if (isExcluded) return false;

  return pattern.test(baseModelName) ? true : undefined;
};
