/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { getNomiToolGroupRuntimeState } from './useNomiMessage';

describe('getNomiToolGroupRuntimeState', () => {
  test('treats malformed tool_group data as inactive instead of calling array methods on it', () => {
    expect(getNomiToolGroupRuntimeState({ status: 'Executing' })).toEqual({
      tools: [],
      hasActive: false,
      hasAny: false,
      confirmingDescription: undefined,
      executingDescription: undefined,
    });
  });

  test('stringifies structured tool descriptions used in thought hints', () => {
    expect(
      getNomiToolGroupRuntimeState([
        {
          status: 'Confirming',
          name: { label: 'Edit' },
          description: { file_path: 'src/App.tsx' },
        },
      ])
    ).toEqual({
      tools: [
        {
          status: 'Confirming',
          name: '{\n  "label": "Edit"\n}',
          description: '{\n  "file_path": "src/App.tsx"\n}',
        },
      ],
      hasActive: true,
      hasAny: true,
      confirmingDescription: '{\n  "file_path": "src/App.tsx"\n}',
      executingDescription: undefined,
    });
  });
});
