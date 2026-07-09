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
    expect(locatorSource.includes('if (!conversation_id || !activeItem) return null')).toBe(true);
    expect(messageListSource.includes('ConversationQuestionLocator')).toBe(true);
    expect(messageListSource.includes('conversationContext?.conversation_id')).toBe(true);
  });

  test('uses a dot rail instead of the title search minimap panel', () => {
    const locatorSource = readSource(new URL('./ConversationQuestionLocator.tsx', import.meta.url));

    expect(locatorSource.includes("from '@renderer/pages/conversation/Messages/hooks'")).toBe(true);
    expect(locatorSource.includes('buildTurnPreview')).toBe(true);
    expect(locatorSource.includes('dispatchChatMessageJump')).toBe(true);
    expect(locatorSource.includes('ConversationTitleMinimap')).toBe(false);
    expect(locatorSource.includes("data-testid='conversation-question-locator-track'")).toBe(true);
    expect(locatorSource.includes("data-testid='conversation-question-locator-dot'")).toBe(true);
    expect(locatorSource.includes("data-distance-level={getDotDistanceLevel(index, activeIndex)}")).toBe(true);
    expect(locatorSource.includes("data-testid='conversation-question-locator-tooltip'")).toBe(true);
    expect(locatorSource.includes('hoverIndex')).toBe(true);
    expect(locatorSource.includes('previewIndex')).toBe(true);
    expect(locatorSource.includes('previewItem.answer')).toBe(true);
    expect(locatorSource.includes('onBlur={() => setHoverIndex((current) => (current === index ? null : current))}')).toBe(true);
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

  test('dot rail stays on the left edge and scales nearby active dots with theme tokens', () => {
    const cssSource = readSource(new URL('./ConversationQuestionLocator.module.css', import.meta.url));

    expect(cssSource.includes('.track')).toBe(true);
    expect(cssSource.includes('.dotButton')).toBe(true);
    expect(cssSource.includes('.dot')).toBe(true);
    expect(cssSource.includes('left: -18px')).toBe(true);
    expect(cssSource.includes('right:')).toBe(false);
    expect(cssSource.includes('--locator-dot-size')).toBe(true);
    expect(cssSource.includes('--locator-dot-size: 5px')).toBe(true);
    expect(cssSource.includes('--locator-dot-size: 6px')).toBe(true);
    expect(cssSource.includes('--locator-dot-size: 8px')).toBe(true);
    expect(cssSource.includes('--locator-dot-size: 10px')).toBe(true);
    expect(cssSource.includes('--locator-dot-color')).toBe(true);
    expect(cssSource.includes('--locator-dot-opacity')).toBe(true);
    expect(cssSource.includes('[data-distance-level="0"]')).toBe(true);
    expect(cssSource.includes('[data-distance-level="1"]')).toBe(true);
    expect(cssSource.includes('[data-distance-level="2"]')).toBe(true);
    expect(cssSource.includes('rgb(var(--primary-6))')).toBe(true);
    expect(cssSource.includes('color-mix(in srgb')).toBe(true);
    expect(cssSource.includes('.tooltipBubble')).toBe(true);
    expect(cssSource.includes('left: 34px')).toBe(true);
    expect(cssSource.includes('transform-origin: left center')).toBe(true);
    expect(cssSource.includes('max-width: min(420px')).toBe(true);
    expect(cssSource.includes('.root[data-tooltip-visible="true"] .tooltipBubble')).toBe(true);
    expect(cssSource.includes('.tooltipTitle')).toBe(true);
    expect(cssSource.includes('.tooltipExcerpt')).toBe(true);
    expect(cssSource.includes('.barLine')).toBe(false);
  });
});
