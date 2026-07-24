/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { ReactElement } from 'react';

import type { TChatConversation } from '@/common/config/storage';
import { parseConversationId } from '@/common/types/ids';
import { renderConversationOption } from './renderConversationOption';

describe('renderConversationOption', () => {
  test('renders the shared compact UUID suffix and keeps the full UUID on hover', () => {
    const id = parseConversationId('0190f5fe-7c00-7a00-8000-000000000101');
    const option = renderConversationOption({
      id,
      name: '',
      type: 'acp',
      extra: { backend: 'claude', workspace: '/tmp/project' },
    } as TChatConversation) as ReactElement<{
      title: string;
      children: ReactElement<{ title: string; idLabel: string }>;
    }>;

    expect(option.props.title).toBe(id);
    expect(option.props.children.props.title).toBe('000000000101');
    expect(option.props.children.props.idLabel).toBe('000000000101');
    expect(option.props.children.props.idLabel.startsWith('#')).toBe(false);
  });
});
