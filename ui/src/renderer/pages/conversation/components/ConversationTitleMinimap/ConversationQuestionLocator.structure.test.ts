/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('conversation question locator structure', () => {
  test('mounts a side locator from MessageList using the current conversation id', () => {
    const locatorSource = readSource(new URL('./ConversationQuestionLocator.tsx', import.meta.url));
    const messageListSource = readSource(new URL('../../Messages/MessageList.tsx', import.meta.url));

    expect(locatorSource.includes("data-testid='conversation-question-locator'")).toBe(true);
    expect(locatorSource.includes('conversation_id?: number')).toBe(true);
    expect(locatorSource.includes('if (!conversation_id || !activeItem || !previewItem) return null')).toBe(true);
    expect(messageListSource.includes('ConversationQuestionLocator')).toBe(true);
    expect(messageListSource.includes('conversationContext?.conversation_id')).toBe(true);
  });

  test('uses Codex-style hover bars instead of the title search minimap panel', () => {
    const locatorSource = readSource(new URL('./ConversationQuestionLocator.tsx', import.meta.url));

    expect(locatorSource.includes("from '@renderer/pages/conversation/Messages/hooks'")).toBe(true);
    expect(locatorSource.includes('buildTurnPreview')).toBe(true);
    expect(locatorSource.includes('dispatchChatMessageJump')).toBe(true);
    expect(locatorSource.includes('ConversationTitleMinimap')).toBe(false);
    expect(locatorSource.includes("data-testid='conversation-question-locator-track'")).toBe(true);
    expect(locatorSource.includes("data-testid='conversation-question-locator-bar'")).toBe(true);
    expect(locatorSource.includes("data-testid='conversation-question-locator-card'")).toBe(true);
    expect(locatorSource.includes('hoverIndex')).toBe(true);
    expect(locatorSource.includes('previewIndex')).toBe(true);
    expect(locatorSource.includes('locatorItemAria')).toBe(true);
    expect(locatorSource.includes('locatorMeta')).toBe(false);
    expect(locatorSource.includes('openSearchPanel')).toBe(false);
    expect(locatorSource.includes('togglePanel')).toBe(false);
  });

  test('message jump events scroll to the target without adding a message highlight background', () => {
    const messageListSource = readSource(new URL('../../Messages/MessageList.tsx', import.meta.url));
    const jumpHandlerSource = messageListSource.slice(
      messageListSource.indexOf('const handleMessageJump'),
      messageListSource.indexOf('window.addEventListener(CHAT_MESSAGE_JUMP_EVENT')
    );

    expect(jumpHandlerSource.includes('scrollElementIntoView')).toBe(true);
    expect(jumpHandlerSource.includes('const targetHighlightId =')).toBe(false);
    expect(jumpHandlerSource.includes('setHighlightedMessageId')).toBe(false);
    expect(jumpHandlerSource.includes('jumpHighlightTimerRef')).toBe(false);
  });

  test('message highlight styles use existing theme tokens', () => {
    const messageListSource = readSource(new URL('../../Messages/MessageList.tsx', import.meta.url));

    expect(messageListSource.includes("backgroundColor: 'var(--aou-1)'")).toBe(true);
    expect(messageListSource.includes("boxShadow: '0 0 0 1px var(--aou-6) inset'")).toBe(true);
    expect(messageListSource.includes('var(--color-aou-')).toBe(false);
  });

  test('hover bars are hidden at rest and reveal a content card by hover or keyboard focus', () => {
    const cssSource = readSource(new URL('./ConversationQuestionLocator.module.css', import.meta.url));

    expect(cssSource.includes('.track')).toBe(true);
    expect(cssSource.includes('.bar')).toBe(true);
    expect(cssSource.includes('.barActive')).toBe(true);
    expect(cssSource.includes('min-width: 44px')).toBe(true);
    expect(cssSource.includes('width: 32px')).toBe(true);
    expect(cssSource.includes('width: 14px')).toBe(true);
    expect(cssSource.includes('width: 28px')).toBe(true);
    expect(cssSource.includes('height: 2px')).toBe(true);
    expect(cssSource.includes('left: 46px')).toBe(true);
    expect(cssSource.includes('width: max-content')).toBe(true);
    expect(cssSource.includes('max-width: min(520px')).toBe(true);
    expect(cssSource.includes('padding: 12px 16px')).toBe(true);
    expect(cssSource.includes('.previewMeta')).toBe(false);
    expect(cssSource.includes('.root:hover .previewCard')).toBe(true);
    expect(cssSource.includes('.root:focus-within .previewCard')).toBe(true);
    expect(cssSource.includes('pointer-events: none')).toBe(true);
    expect(cssSource.includes('pointer-events: auto')).toBe(true);
    expect(cssSource.includes('opacity: 0')).toBe(true);
    expect(cssSource.includes('box-shadow: 0 12px 30px')).toBe(true);
  });
});
