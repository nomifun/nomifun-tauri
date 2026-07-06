/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('conversation orchestration canvas integration', () => {
  test('keeps run progress native to the canvas pane instead of the chat column', () => {
    const chatSource = readSource(new URL('../components/ChatConversation.tsx', import.meta.url));
    const panelSource = readSource(new URL('./OrchestrationTopPanel.tsx', import.meta.url));

    expect(chatSource.includes('ClusterProgressStrip')).toBe(false);
    expect(chatSource.includes('<ClusterProgressStrip')).toBe(false);
    expect(chatSource.includes("className='flex-1 min-w-0 min-h-0 flex flex-col'")).toBe(true);

    expect(panelSource.includes("data-testid='orchestration-canvas-progress'")).toBe(true);
    expect(panelSource.includes('taskStatusMeta(task.status)')).toBe(true);
    expect(panelSource.includes('openTask(task)')).toBe(true);
  });
});
