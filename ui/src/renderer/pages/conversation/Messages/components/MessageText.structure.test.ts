/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./MessageText.tsx', import.meta.url), 'utf8');
const typographySource = readFileSync(new URL('../typography.ts', import.meta.url), 'utf8');

describe('MessageText process action chrome', () => {
  test('can hide the hover copy and timestamp row for process text', () => {
    expect(source.includes('hideActions?: boolean')).toBe(true);
    expect(source.includes('const shouldShowActions = !hideActions && !isMobile;')).toBe(true);
    expect(source.includes('{shouldShowActions && (')).toBe(true);
  });

  test('uses one body typography contract for plain text and markdown text', () => {
    expect(typographySource.includes("export const MESSAGE_BODY_FONT_SIZE = 'var(--conversation-message-font-size)';")).toBe(
      true
    );
    expect(
      typographySource.includes("export const MESSAGE_BODY_LINE_HEIGHT = 'var(--conversation-message-line-height)';")
    ).toBe(true);
    expect(typographySource.includes("export const MESSAGE_BODY_CLASS_NAME = 'message-text-body whitespace-pre-wrap break-words';")).toBe(
      true
    );
    expect(source.includes("from '../typography'")).toBe(true);
    expect(source.includes('className={MESSAGE_BODY_CLASS_NAME}')).toBe(true);
    expect(source.includes('fontSize={MESSAGE_BODY_FONT_SIZE}')).toBe(true);
    expect(source.includes('lineHeight={MESSAGE_BODY_LINE_HEIGHT}')).toBe(true);
  });

  test('keeps the knowledge writeback icon optically centered with the status text', () => {
    expect(source.includes('h-14px w-14px shrink-0 items-center justify-center self-center leading-none')).toBe(true);
    expect(source.includes("className='block shrink-0'")).toBe(true);
  });

  test('offers one explicit retry action only for retryable terminal writeback state', () => {
    expect(source.includes('displayState.retryable === true')).toBe(true);
    expect(source.includes('!RUNNING_WRITEBACK_STATUSES.has(displayState.status)')).toBe(true);
    expect(source.includes('ipcBridge.conversation.retryKnowledgeWriteback.invoke')).toBe(true);
    expect(source.includes('messageId={message.message_id ?? message.msg_id}')).toBe(true);
    expect(source.includes('disabled={retrying}')).toBe(true);
    expect(source.includes("event.stopPropagation();")).toBe(true);
  });

  test('routes file marker parsing through the message-side trust boundary', () => {
    expect(source.includes("import { parseMessageFileMarker } from './messageFileMarker';")).toBe(true);
    expect(source.includes('parseMessageFileMarker(contentToRender, message.position)')).toBe(true);
    expect(source.includes('const parseFileMarker')).toBe(false);
  });
});
