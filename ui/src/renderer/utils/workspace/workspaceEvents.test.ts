/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { conversationTarget, terminalTarget } from '@/common/types/ids';
import { isWorkspacePanelEventForTarget } from '@/renderer/pages/conversation/components/ChatLayout/WorkspaceToolRail';

describe('workspace event targets', () => {
  test('uses both kind and id when matching a session', () => {
    const conversation = conversationTarget('0190f5fe-7c00-7a00-8000-000000000001');
    expect(isWorkspacePanelEventForTarget(conversation, conversation)).toBe(true);
    expect(
      isWorkspacePanelEventForTarget(
        terminalTarget('0190f5fe-7c00-7a00-8000-000000000001'),
        conversation,
      ),
    ).toBe(false);
    expect(
      isWorkspacePanelEventForTarget(
        conversationTarget('0190f5fe-7c00-7a00-8000-000000000002'),
        conversation,
      ),
    ).toBe(false);
  });

  test('rejects unscoped events', () => {
    expect(
      isWorkspacePanelEventForTarget(
        undefined,
        conversationTarget('0190f5fe-7c00-7a00-8000-000000000001'),
      ),
    ).toBe(false);
  });
});
