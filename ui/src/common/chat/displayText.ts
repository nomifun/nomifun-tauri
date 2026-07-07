/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

export const toDisplayText = (value: unknown, fallback = ''): string => {
  if (typeof value === 'string') return value;
  if (value == null) return fallback;
  if (typeof value === 'number' || typeof value === 'boolean' || typeof value === 'bigint') {
    return String(value);
  }

  try {
    const json = JSON.stringify(value, null, 2);
    return json === undefined ? fallback : json;
  } catch {
    return String(value);
  }
};

export const optionalDisplayText = (value: unknown): string | undefined => {
  if (value == null) return undefined;
  return toDisplayText(value);
};

export const compactDisplayText = (value: unknown, fallback = ''): string => {
  const compacted = toDisplayText(value, fallback).replace(/\s+/g, ' ').trim();
  return compacted || fallback;
};

export const extractResponseTextChunk = (data: unknown): string => {
  if (typeof data === 'string') return data;
  if (data && typeof data === 'object' && 'content' in data) {
    return toDisplayText((data as { content?: unknown }).content);
  }
  return '';
};
