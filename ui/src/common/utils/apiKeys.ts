/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

export function parseApiKeyList(value?: string | null): string[] {
  if (!value) return [];
  return value
    .split(/[,\n]/)
    .map((key) => key.trim())
    .filter(Boolean);
}

export function normalizeApiKeyList(value?: string | null): string {
  return parseApiKeyList(value).join(',');
}

export interface ApiKeySaveValidationResult {
  keys: string[];
  normalized: string;
  invalidIndexes: number[];
  valid: boolean;
}

export async function validateApiKeysForSave(
  value: string | null | undefined,
  testKey: (key: string) => Promise<boolean>
): Promise<ApiKeySaveValidationResult> {
  const keys = parseApiKeyList(value);
  const invalidIndexes: number[] = [];

  for (const [index, key] of keys.entries()) {
    let isValid = false;
    try {
      isValid = await testKey(key);
    } catch {
      isValid = false;
    }
    if (!isValid) {
      invalidIndexes.push(index);
    }
  }

  return {
    keys,
    normalized: keys.join(','),
    invalidIndexes,
    valid: invalidIndexes.length === 0,
  };
}
