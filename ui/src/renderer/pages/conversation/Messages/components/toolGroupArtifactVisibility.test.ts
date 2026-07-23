/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { IMessageToolGroup } from '@/common/chat/chatLib';
import {
  enforceToolGroupArtifactTrust,
  getSuccessfulLegacyImage,
  isSuccessfulWriteFileResult,
} from './toolGroupArtifactVisibility';

const imageItem = (status: IMessageToolGroup['content'][number]['status']) =>
  ({
    call_id: 'image-call',
    name: 'ImageGeneration',
    description: 'generate',
    render_output_as_markdown: false,
    status,
    result_display: { img_url: '/workspace/old.png', relative_path: 'old.png' },
  }) satisfies IMessageToolGroup['content'][number];

describe('legacy tool-group artifact visibility', () => {
  test('receipt-less terminal Success is downgraded instead of projecting an image URL', () => {
    const downgraded = enforceToolGroupArtifactTrust(imageItem('Success'));
    expect(getSuccessfulLegacyImage(imageItem('Success'))).toBeUndefined();
    expect(downgraded.status).toBe('Error');
    expect(downgraded.result_display).toBeUndefined();
    expect(downgraded.description.includes('not backed by a committed artifact receipt')).toBe(true);
    for (const status of ['Executing', 'Confirming', 'Pending', 'Error', 'Canceled'] as const) {
      expect(getSuccessfulLegacyImage(imageItem(status))).toBeUndefined();
      expect(enforceToolGroupArtifactTrust(imageItem(status)).result_display).toBeUndefined();
    }
  });

  test('a committed full receipt supplies the canonical display path', () => {
    const verified = {
      ...imageItem('Success'),
      artifact_delivery_committed: true,
      artifacts: [
        {
          id: '019b0000-0000-7000-8000-000000000002',
          kind: 'image',
          mime_type: 'image/png',
          path: '/workspace/old.png',
          relative_path: 'old.png',
          size_bytes: 42,
          sha256: 'a'.repeat(64),
        },
      ],
    } as unknown as IMessageToolGroup['content'][number];

    expect(getSuccessfulLegacyImage(verified)).toEqual({
      imgUrl: '/workspace/old.png',
      relativePath: 'old.png',
    });
    expect(enforceToolGroupArtifactTrust(verified)).toBe(verified);
  });

  test('write-file result cards also require terminal Success', () => {
    const item = {
      ...imageItem('Success'),
      name: 'WriteFile',
      result_display: { file_diff: '@@ -1 +1 @@', file_name: 'report.txt' },
    } satisfies IMessageToolGroup['content'][number];
    expect(isSuccessfulWriteFileResult(item)).toBe(true);
    expect(isSuccessfulWriteFileResult({ ...item, status: 'Error' })).toBe(false);
    expect(isSuccessfulWriteFileResult({ ...item, status: 'Executing' })).toBe(false);
  });
});
