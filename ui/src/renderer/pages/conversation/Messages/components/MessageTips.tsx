/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IMessageTips } from '@/common/chat/chatLib';
import { Collapse, Tag } from '@arco-design/web-react';
import { Attention, CheckOne } from '@icon-park/react';
import { theme } from '@/platform';
import classNames from 'classnames';
import React, { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import MarkdownView from '@renderer/components/Markdown';
import FeedbackButton from '@renderer/components/base/FeedbackButton';
import CollapsibleContent from '@renderer/components/chat/CollapsibleContent';
import { MESSAGE_BODY_FONT_SIZE, MESSAGE_BODY_LINE_HEIGHT } from '../typography';

const icon = {
  success: <CheckOne theme='filled' size='16' fill={theme.Color.FunctionalColor.success} className='m-t-2px' />,
  warning: (
    <Attention
      theme='filled'
      size='16'
      strokeLinejoin='bevel'
      className='m-t-2px'
      fill={theme.Color.FunctionalColor.warn}
    />
  ),
  error: (
    <Attention
      theme='filled'
      size='16'
      strokeLinejoin='bevel'
      className='m-t-2px'
      fill={theme.Color.FunctionalColor.error}
    />
  ),
};

const useFormatContent = (content: string) => {
  return useMemo(() => {
    try {
      const json = JSON.parse(content);
      return {
        json: true,
        data: json,
      };
    } catch {
      return { data: content };
    }
  }, [content]);
};

const MessageTips: React.FC<{ message: IMessageTips }> = ({ message }) => {
  const { t } = useTranslation();
  const { content, type } = message.content;
  const structuredError = type === 'error' ? message.content.error : undefined;
  const { json, data } = useFormatContent(content);

  const displayContent = json ? '' : content;
  const shouldShowFeedback = type === 'error';

  if (structuredError) {
    const code = structuredError.code;
    const ownership = structuredError.ownership;
    const title = code
      ? t(`conversation.agentError.codes.${code}.title`, {
          defaultValue: t('conversation.agentError.fallbackTitle'),
        })
      : t('conversation.agentError.fallbackTitle');
    const body = code
      ? t(
          structuredError.workspacePath
            ? `conversation.agentError.codes.${code}.bodyWithPath`
            : `conversation.agentError.codes.${code}.body`,
          {
            workspacePath: structuredError.workspacePath,
            defaultValue: structuredError.message || content,
          }
        )
      : structuredError.message || content;
    const ownershipLabel = ownership
      ? t(`conversation.agentError.ownership.${ownership}`, {
          defaultValue: t('conversation.agentError.ownership.unknown_upstream'),
        })
      : null;
    const retryHint =
      structuredError.retryable === undefined
        ? null
        : structuredError.retryable
          ? t('conversation.agentError.retryable')
          : t('conversation.agentError.notRetryable');
    const resolutionText = structuredError.resolution
      ? t(`conversation.agentError.resolution.${structuredError.resolution.kind}`)
      : null;
    const detailParts = [
      code ? `${t('conversation.agentError.errorCode')}: ${code}` : '',
      structuredError.detail || structuredError.message,
    ].filter(Boolean);
    const feedbackTags: Record<string, string> = {};
    if (code) {
      feedbackTags.agent_error_code = code;
    }
    if (ownership) {
      feedbackTags.agent_error_ownership = ownership;
    }
    if (structuredError.retryable !== undefined) {
      feedbackTags.agent_error_retryable = String(structuredError.retryable);
    }
    if (structuredError.resolution?.kind) {
      feedbackTags.agent_error_resolution = structuredError.resolution.kind;
    }
    const feedbackExtra = {
      agent_error: {
        ...(code ? { code } : {}),
        ...(ownership ? { ownership } : {}),
        ...(structuredError.retryable !== undefined ? { retryable: structuredError.retryable } : {}),
        ...(structuredError.feedback_recommended !== undefined
          ? { feedback_recommended: structuredError.feedback_recommended }
          : {}),
        ...(structuredError.resolution ? { resolution: structuredError.resolution } : {}),
      },
    };

    return (
      <div className='w-full'>
        <div className={classNames('message-error-note', ownership && `message-error-note--${ownership}`)}>
          <div className='message-error-note__rail' aria-hidden='true' />
          <div className='message-error-note__content'>
            <div className='message-error-note__header'>
              <div className='message-error-note__status'>
                <span className='message-error-note__icon'>{icon.error}</span>
                {ownershipLabel && <span className='message-error-note__owner'>{ownershipLabel}</span>}
              </div>
              <div className='message-error-note__meta'>
                {retryHint && (
                  <Tag
                    size='small'
                    color={structuredError.retryable ? 'green' : 'gray'}
                    className='message-error-note__tag'
                  >
                    {retryHint}
                  </Tag>
                )}
                {code && <span className='message-error-note__code'>{code}</span>}
              </div>
            </div>
            <div className='message-error-note__main'>
              <div className='message-error-note__title'>{title}</div>
              <div className='message-error-note__body'>{body}</div>
              {resolutionText && (
                <div className='message-error-note__resolution'>
                  <span className='message-error-note__resolution-label'>
                    {t('conversation.agentError.resolutionPrefix')}
                  </span>
                  <span>{resolutionText}</span>
                </div>
              )}
              <div className='message-error-note__footer'>
                <div className='message-error-note__footer-main'>
                  {detailParts.length > 0 && (
                    <Collapse bordered={false} className='message-error-note__details'>
                      <Collapse.Item
                        name='technical-details'
                        header={
                          <span className='message-error-note__details-label'>{t('common.technical_details')}</span>
                        }
                      >
                        <div className='message-error-note__detail-body'>{detailParts.join('\n')}</div>
                      </Collapse.Item>
                    </Collapse>
                  )}
                  {shouldShowFeedback && (
                    <div className='message-error-note__actions'>
                      <FeedbackButton
                        module='conversation-session'
                        feedbackTags={feedbackTags}
                        feedbackExtra={feedbackExtra}
                        className='message-error-note__feedback'
                      />
                    </div>
                  )}
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
    );
  }

  if (json)
    return (
      <div className='w-full'>
        <div className={classNames('bg-message-tips rd-8px p-x-12px p-y-8px flex flex-col gap-4px')}>
          <div className='flex items-start gap-4px'>
            {icon[type] || icon.warning}
            <div className='flex-1 min-w-0'>
              <MarkdownView fontSize={MESSAGE_BODY_FONT_SIZE} lineHeight={MESSAGE_BODY_LINE_HEIGHT}>
                {`\`\`\`json\n${JSON.stringify(data, null, 2)}\n\`\`\``}
              </MarkdownView>
            </div>
          </div>
          {type === 'error' && (
            <div className='flex justify-end'>
              <FeedbackButton module='conversation-session' />
            </div>
          )}
        </div>
      </div>
    );
  return (
    <div className='w-full'>
      <div className={classNames('bg-message-tips rd-8px  p-x-12px p-y-8px flex flex-col gap-4px')}>
        <div className='flex items-start gap-4px'>
          {icon[type] || icon.warning}
          <div className='flex-1 min-w-0'>
            <CollapsibleContent maxHeight={48} defaultCollapsed={true} useMask={true}>
              <span className='whitespace-break-spaces text-t-primary [word-break:break-word]'>{displayContent}</span>
            </CollapsibleContent>
          </div>
        </div>
        {shouldShowFeedback && (
          <div className='flex justify-end'>
            <FeedbackButton module='conversation-session' />
          </div>
        )}
      </div>
    </div>
  );
};

export default MessageTips;
