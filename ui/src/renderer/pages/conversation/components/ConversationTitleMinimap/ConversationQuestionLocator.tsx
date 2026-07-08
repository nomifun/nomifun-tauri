/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useMessageList } from '@renderer/pages/conversation/Messages/hooks';
import { dispatchChatMessageJump } from '@/renderer/utils/chat/chatMinimapEvents';
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import styles from './ConversationQuestionLocator.module.css';
import { buildTurnPreview, truncate } from './minimapUtils';

type ConversationQuestionLocatorProps = {
  conversation_id?: number;
};

export const pickActiveQuestionIndex = (questionTopOffsets: number[], anchorY: number): number => {
  if (!questionTopOffsets.length) return -1;
  if (questionTopOffsets[0] > anchorY) return 0;

  let activeIndex = 0;
  for (let index = 0; index < questionTopOffsets.length; index += 1) {
    if (questionTopOffsets[index] <= anchorY) {
      activeIndex = index;
      continue;
    }
    break;
  }
  return activeIndex;
};

const ConversationQuestionLocator: React.FC<ConversationQuestionLocatorProps> = ({ conversation_id }) => {
  const { t } = useTranslation();
  const list = useMessageList();
  const rootRef = useRef<HTMLDivElement | null>(null);
  const rafRef = useRef<number | null>(null);
  const turns = useMemo(() => buildTurnPreview(list), [list]);
  const [activeIndex, setActiveIndex] = useState(0);
  const [hoverIndex, setHoverIndex] = useState<number | null>(null);
  const activeItem = turns[Math.min(activeIndex, turns.length - 1)];
  const previewIndex = Math.min(hoverIndex ?? activeIndex, turns.length - 1);
  const previewItem = turns[previewIndex];

  const getScroller = useCallback(() => {
    const host = rootRef.current?.parentElement;
    return host?.querySelector<HTMLElement>('[data-testid="message-list-scroller"]') ?? null;
  }, []);

  const syncActiveQuestionFromScroll = useCallback(() => {
    const scroller = getScroller();
    if (!scroller || !turns.length) return;

    const scrollerRect = scroller.getBoundingClientRect();
    const anchorY = Math.min(180, Math.max(96, scrollerRect.height * 0.34));
    const questionTopOffsets = turns.map((item) => {
      const messageElement = item.messageId ? document.getElementById(`message-${item.messageId}`) : null;
      if (!messageElement) return Number.POSITIVE_INFINITY;
      return messageElement.getBoundingClientRect().top - scrollerRect.top;
    });
    const nextIndex = pickActiveQuestionIndex(questionTopOffsets, anchorY);
    if (nextIndex >= 0) {
      setActiveIndex((current) => (current === nextIndex ? current : nextIndex));
    }
  }, [getScroller, turns]);

  const scheduleActiveQuestionSync = useCallback(() => {
    if (rafRef.current !== null) return;
    rafRef.current = window.requestAnimationFrame(() => {
      rafRef.current = null;
      syncActiveQuestionFromScroll();
    });
  }, [syncActiveQuestionFromScroll]);

  useLayoutEffect(() => {
    setActiveIndex(0);
    setHoverIndex(null);
  }, [conversation_id]);

  useEffect(() => {
    const scroller = getScroller();
    if (!scroller || !turns.length) return;

    syncActiveQuestionFromScroll();
    scroller.addEventListener('scroll', scheduleActiveQuestionSync, { passive: true });
    window.addEventListener('resize', scheduleActiveQuestionSync);
    return () => {
      scroller.removeEventListener('scroll', scheduleActiveQuestionSync);
      window.removeEventListener('resize', scheduleActiveQuestionSync);
      if (rafRef.current !== null) {
        window.cancelAnimationFrame(rafRef.current);
        rafRef.current = null;
      }
    };
  }, [getScroller, scheduleActiveQuestionSync, syncActiveQuestionFromScroll, turns.length]);

  const jumpToQuestion = useCallback((index: number) => {
    const item = turns[index];
    if (!conversation_id || !item) return;
    dispatchChatMessageJump({
      conversation_id,
      messageId: item.messageId,
      msgId: item.msgId,
      align: 'start',
      behavior: 'smooth',
    });
  }, [conversation_id, turns]);

  if (!conversation_id || !activeItem || !previewItem) return null;

  return (
    <div
      ref={rootRef}
      className={styles.root}
      data-testid='conversation-question-locator'
      onMouseLeave={() => setHoverIndex(null)}
    >
      <div
        className={styles.track}
        data-testid='conversation-question-locator-track'
        role='list'
        aria-label={t('conversation.minimap.locatorAria', { defaultValue: 'Open question history' })}
      >
        {turns.map((item, index) => {
          const isSelected = (hoverIndex ?? activeIndex) === index;
          return (
            <button
              key={item.messageId || item.msgId || item.index}
              type='button'
              className={`${styles.bar} ${isSelected ? styles.barActive : ''}`}
              data-testid='conversation-question-locator-bar'
              data-active={isSelected ? 'true' : undefined}
              aria-current={isSelected ? 'true' : undefined}
              aria-label={t('conversation.minimap.locatorItemAria', {
                defaultValue: 'Jump to question {{index}}',
                index: item.index,
              })}
              title={truncate(item.questionRaw || item.question, 72)}
              onClick={() => jumpToQuestion(index)}
              onFocus={() => setHoverIndex(index)}
              onMouseEnter={() => setHoverIndex(index)}
            >
              <span className={styles.barLine} aria-hidden='true' />
            </button>
          );
        })}
      </div>
      <button
        type='button'
        className={styles.previewCard}
        data-testid='conversation-question-locator-card'
        onClick={() => jumpToQuestion(previewIndex)}
      >
        <span className={styles.previewTitle}>{truncate(previewItem.questionRaw || previewItem.question, 72)}</span>
        {previewItem.answer ? (
          <span className={styles.previewExcerpt}>{truncate(previewItem.answerRaw || previewItem.answer, 156)}</span>
        ) : null}
      </button>
    </div>
  );
};

export default ConversationQuestionLocator;
