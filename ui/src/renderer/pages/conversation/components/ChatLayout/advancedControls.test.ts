/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('ChatLayout advanced controls', () => {
  test('keeps the stable header controls but removes the multi-agent entry icon', () => {
    const source = readSource(new URL('./index.tsx', import.meta.url));

    expect(source.includes("<AutoWorkControl target={{ kind: 'conversation', id: conversation_id }} />")).toBe(true);
    expect(source.includes("<IdmmControl target={{ kind: 'conversation', id: conversation_id }} />")).toBe(true);
    expect(source.includes("<KnowledgeControl target={{ kind: 'conversation', id: conversation_id }} />")).toBe(true);
    expect(source.includes('MultiAgentControl')).toBe(false);
  });

  test('does not let workspace file-tree events auto-expand the conversation right rail', () => {
    const source = readSource(new URL('./index.tsx', import.meta.url));

    expect(source.includes('autoExpandOnFiles: false')).toBe(true);
  });

  test('keeps the workspace tool rail at the far right of the expanded panel', () => {
    const source = readSource(new URL('./index.tsx', import.meta.url));
    const panelIndex = source.indexOf("className={classNames('!bg-1 relative chat-layout-right-sider layout-sider')}");
    const railIndex = source.indexOf('<WorkspaceToolRail');

    expect(panelIndex >= 0).toBe(true);
    expect(railIndex >= 0).toBe(true);
    expect(panelIndex < railIndex).toBe(true);
  });
});
