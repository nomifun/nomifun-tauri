/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IMessageThinking } from '@/common/chat/chatLib';
import { Spin } from '@arco-design/web-react';
import { Brain, Right } from '@icon-park/react';
import React, { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import styles from './MessageThinking.module.css';

interface MessageThinkingProps {
  message: IMessageThinking;
  variant?: 'standalone' | 'process';
}

const MessageThinking: React.FC<MessageThinkingProps> = ({ message, variant = 'standalone' }) => {
  const { t } = useTranslation();
  const isProcessVariant = variant === 'process';

  const formatElapsedTime = (seconds: number): string => {
    const sUnit = t('common.unit.second_short', { defaultValue: 's' });
    const mUnit = t('common.unit.minute_short', { defaultValue: 'm' });

    if (seconds < 60) return `${seconds}${sUnit}`;
    const minutes = Math.floor(seconds / 60);
    const remaining = seconds % 60;
    return `${minutes}${mUnit} ${remaining}${sUnit}`;
  };

  const { content: text, status, subject } = message.content;
  const isDone = status === 'done';
  const [expanded, setExpanded] = useState(true);
  const [elapsedTime, setElapsedTime] = useState(() => {
    const initialStartedAt = message.created_at ?? Date.now();
    return isDone ? 0 : Math.max(0, Math.floor((Date.now() - initialStartedAt) / 1000));
  });
  const startTimeRef = useRef<number>(message.created_at ?? Date.now());
  const bodyRef = useRef<HTMLDivElement>(null);

  // Elapsed timer for active thinking
  useEffect(() => {
    if (isDone) return;

    startTimeRef.current = message.created_at ?? Date.now();
    setElapsedTime(Math.max(0, Math.floor((Date.now() - startTimeRef.current) / 1000)));
    const timer = setInterval(() => {
      setElapsedTime(Math.floor((Date.now() - startTimeRef.current) / 1000));
    }, 1000);

    return () => clearInterval(timer);
  }, [isDone, message.created_at, message.msg_id]);

  // Auto-scroll to bottom during streaming
  useEffect(() => {
    if (!isDone && expanded && bodyRef.current) {
      bodyRef.current.scrollTop = bodyRef.current.scrollHeight;
    }
  }, [text, isDone, expanded]);

  const summaryText = isDone
    ? t('conversation.thinking.complete', { defaultValue: 'Thought complete' })
    : `${subject || t('conversation.thinking.label', { defaultValue: 'Thinking...' })} · ${formatElapsedTime(elapsedTime)}`;

  return (
    <div className={`${styles.container} ${isProcessVariant ? styles.containerProcess : ''}`}>
      <div
        className={`${styles.header} ${isProcessVariant ? styles.headerProcess : ''}`}
        onClick={() => setExpanded((v) => !v)}
      >
        <span className={styles.headerIcon}>{!isDone ? <Spin size={12} /> : <Brain theme='outline' size='14' />}</span>
        <span className={styles.summary}>{summaryText}</span>
        <span className={`${styles.arrow} ${expanded ? styles.arrowExpanded : ''}`}>
          <Right theme='outline' size='12' />
        </span>
      </div>
      <div
        ref={bodyRef}
        className={`${styles.body} ${isProcessVariant ? styles.bodyProcess : ''} ${!expanded ? styles.collapsed : ''}`}
      >
        {text}
      </div>
    </div>
  );
};

export default MessageThinking;
