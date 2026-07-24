/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import {
  preferKnowledgeWritebackState,
  type IMessageText,
  type KnowledgeWritebackState,
  type KnowledgeWritebackStatus,
} from '@/common/chat/chatLib';
import { ipcBridge } from '@/common';
import { toDisplayText } from '@/common/chat/displayText';
import { useConversationContextSafe } from '@/renderer/hooks/context/ConversationContext';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { iconColors } from '@/renderer/styles/colors';
import { Alert, Message, Tooltip } from '@arco-design/web-react';
import { CheckOne, CloseOne, Copy, Edit, Info, Loading } from '@icon-park/react';
import classNames from 'classnames';
import React, { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { copyText } from '@/renderer/utils/ui/clipboard';
import { emitter } from '@/renderer/utils/emitter';
import { useMessageList } from '../hooks';
import CollapsibleContent from '@renderer/components/chat/CollapsibleContent';
import FilePreview from '@renderer/components/media/FilePreview';
import HorizontalFileList from '@renderer/components/media/HorizontalFileList';
import MarkdownView from '@renderer/components/Markdown';
import { stripThinkTags, hasThinkTags } from '@renderer/utils/chat/thinkTagFilter';
import { stripSkillSuggest, hasSkillSuggest } from '@renderer/utils/chat/skillSuggestParser';
import { MESSAGE_BODY_CLASS_NAME, MESSAGE_BODY_FONT_SIZE, MESSAGE_BODY_LINE_HEIGHT } from '../typography';
import { parseMessageFileMarker } from './messageFileMarker';

/**
 * Format a timestamp for message display.
 * Today: "HH:mm", older: "MM-DD HH:mm".
 */
export const formatMessageTime = (timestamp: number): string => {
  const date = new Date(timestamp);
  const now = new Date();
  const hours = date.getHours().toString().padStart(2, '0');
  const minutes = date.getMinutes().toString().padStart(2, '0');
  const time = `${hours}:${minutes}`;

  if (
    date.getFullYear() !== now.getFullYear() ||
    date.getMonth() !== now.getMonth() ||
    date.getDate() !== now.getDate()
  ) {
    const month = (date.getMonth() + 1).toString().padStart(2, '0');
    const day = date.getDate().toString().padStart(2, '0');
    return `${month}-${day} ${time}`;
  }
  return time;
};
import MessageCronBadge from './MessageCronBadge';
import { getAgentLogo } from '@/renderer/utils/model/agentLogo';
import AgentMessageAvatar from './AgentMessageAvatar';

const CODE_STYLE = { marginTop: 4, marginBlock: 4 };

const RUNNING_WRITEBACK_STATUSES = new Set<KnowledgeWritebackStatus>(['started', 'extracting', 'writing']);
const SUCCESS_WRITEBACK_STATUSES = new Set<KnowledgeWritebackStatus>(['written']);
const WARNING_WRITEBACK_STATUSES = new Set<KnowledgeWritebackStatus>(['partial', 'interrupted']);
const FAILURE_WRITEBACK_STATUSES = new Set<KnowledgeWritebackStatus>(['failed']);

const compactWritebackFiles = (state: KnowledgeWritebackState): string | undefined => {
  const paths = (state.written ?? [])
    .map((file) => file.rel_path)
    .filter((path): path is string => Boolean(path));
  if (!paths.length) return undefined;
  const visible = paths.slice(0, 2).join(', ');
  return paths.length > 2 ? `${visible} +${paths.length - 2}` : visible;
};

const getWritebackTextKey = (status: KnowledgeWritebackStatus): string => {
  switch (status) {
    case 'started':
      return 'messages.knowledgeWriteback.started';
    case 'extracting':
      return 'messages.knowledgeWriteback.extracting';
    case 'writing':
      return 'messages.knowledgeWriteback.writing';
    case 'written':
      return 'messages.knowledgeWriteback.written';
    case 'partial':
      return 'messages.knowledgeWriteback.partial';
    case 'failed':
      return 'messages.knowledgeWriteback.failed';
    case 'no_candidate':
      return 'messages.knowledgeWriteback.noCandidate';
    case 'no_completer':
      return 'messages.knowledgeWriteback.noCompleter';
    case 'disabled':
      return 'messages.knowledgeWriteback.disabled';
    case 'interrupted':
      return 'messages.knowledgeWriteback.interrupted';
  }
};

const MessageKnowledgeWriteback: React.FC<{
  state: KnowledgeWritebackState;
  conversationId: IMessageText['conversation_id'];
  messageId?: IMessageText['message_id'];
}> = ({ state, conversationId, messageId }) => {
  const { t } = useTranslation();
  const [retrying, setRetrying] = useState(false);
  const [watchingRetry, setWatchingRetry] = useState(false);
  const [polledState, setPolledState] = useState<KnowledgeWritebackState>();
  const retryFromAttemptRef = useRef<string | undefined>(undefined);
  const displayState = useMemo(
    () => preferKnowledgeWritebackState(state, polledState) ?? state,
    [polledState, state]
  );
  const failureCount = displayState.failures?.length ?? 0;
  const writtenCount = displayState.written?.length ?? 0;
  const firstFailure = displayState.failures?.find((failure) => failure.error)?.error;
  const fileSummary = compactWritebackFiles(displayState);
  const detail = firstFailure ?? fileSummary;
  const canRetry =
    displayState.retryable === true &&
    !RUNNING_WRITEBACK_STATUSES.has(displayState.status) &&
    Boolean(messageId);

  useEffect(() => {
    if (
      retrying &&
      (RUNNING_WRITEBACK_STATUSES.has(displayState.status) ||
        displayState.attempt_id !== retryFromAttemptRef.current)
    ) {
      setRetrying(false);
    }
    if (
      watchingRetry &&
      displayState.attempt_id !== retryFromAttemptRef.current &&
      !RUNNING_WRITEBACK_STATUSES.has(displayState.status)
    ) {
      setWatchingRetry(false);
    }
  }, [
    displayState.attempt_id,
    displayState.status,
    retrying,
    watchingRetry,
  ]);

  // Realtime fan-out is bounded and may drop a frame without disconnecting.
  // Poll this exact durable owner row while it is running (or while a manual
  // retry is crossing the HTTP/event gap), including old messages outside the
  // newest keyset-history window.
  useEffect(() => {
    if (
      !messageId ||
      (!watchingRetry &&
        !RUNNING_WRITEBACK_STATUSES.has(displayState.status))
    ) {
      return;
    }
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      try {
        const message =
          await ipcBridge.database.getConversationMessage.invoke({
            conversation_id: conversationId,
            message_id: messageId,
          });
        if (
          message.type === 'text' &&
          typeof message.content === 'object' &&
          message.content !== null
        ) {
          const incoming = (message.content as IMessageText['content'])
            .knowledge_writeback;
          if (incoming && !stopped) {
            setPolledState((existing) =>
              preferKnowledgeWritebackState(existing, incoming)
            );
          }
        }
      } catch (error) {
        if (!stopped) {
          console.warn(
            '[MessageKnowledgeWriteback] Failed to refresh durable write-back state:',
            error
          );
        }
      } finally {
        if (!stopped) timer = setTimeout(poll, 1_000);
      }
    };
    timer = setTimeout(poll, 250);
    return () => {
      stopped = true;
      if (timer) clearTimeout(timer);
    };
  }, [
    conversationId,
    displayState.status,
    messageId,
    watchingRetry,
  ]);

  const handleRetry = async () => {
    if (!messageId || retrying) return;
    retryFromAttemptRef.current = displayState.attempt_id;
    setRetrying(true);
    setWatchingRetry(true);
    try {
      await ipcBridge.conversation.retryKnowledgeWriteback.invoke({
        conversation_id: conversationId,
        message_id: messageId,
        attempt_id: displayState.attempt_id ?? '',
      });
      Message.success(t('messages.knowledgeWriteback.retryStarted'));
      // Keep the action disabled until the fresh attempt arrives. This closes
      // the response/event gap without inventing a frontend job state.
    } catch (error) {
      setRetrying(false);
      setWatchingRetry(false);
      Message.error(
        error instanceof Error && error.message
          ? error.message
          : t('messages.knowledgeWriteback.retryFailed')
      );
    }
  };

  const toneClass = RUNNING_WRITEBACK_STATUSES.has(displayState.status)
    ? 'border-line bg-fill-1 text-t-secondary'
    : SUCCESS_WRITEBACK_STATUSES.has(displayState.status)
      ? 'border-success-4 bg-success-light-1 text-success-6'
      : WARNING_WRITEBACK_STATUSES.has(displayState.status)
        ? 'border-warning-3 bg-warning-1 text-warning-7'
        : FAILURE_WRITEBACK_STATUSES.has(displayState.status)
          ? 'border-danger-4 bg-danger-light-1 text-danger-6'
          : 'border-line bg-fill-1 text-t-secondary';

  const icon = RUNNING_WRITEBACK_STATUSES.has(displayState.status) ? (
    <Loading theme='outline' size='13' className='block shrink-0 animate-spin' />
  ) : SUCCESS_WRITEBACK_STATUSES.has(displayState.status) ? (
    <CheckOne theme='filled' size='13' className='block shrink-0' />
  ) : FAILURE_WRITEBACK_STATUSES.has(displayState.status) ? (
    <CloseOne theme='filled' size='13' className='block shrink-0' />
  ) : (
    <Info theme='outline' size='13' className='block shrink-0' />
  );

  return (
    <div
      className={classNames(
        'mt-6px inline-flex max-w-full items-center gap-6px rd-6px border px-8px py-4px text-12px leading-18px',
        toneClass
      )}
      title={detail}
    >
      <span className='flex h-14px w-14px shrink-0 items-center justify-center self-center leading-none'>{icon}</span>
      <span className='min-w-0 truncate'>
        {t(getWritebackTextKey(displayState.status), {
          count: writtenCount,
          failures: failureCount,
        })}
      </span>
      {detail && <span className='min-w-0 truncate opacity-75'>{detail}</span>}
      {canRetry && (
        <button
          type='button'
          className='ml-2px shrink-0 rd-4px border-0 bg-transparent px-4px py-0 text-inherit font-medium hover:bg-fill-2 disabled:cursor-wait disabled:opacity-60'
          disabled={retrying}
          aria-label={t('messages.knowledgeWriteback.retry')}
          onClick={(event) => {
            event.stopPropagation();
            void handleRetry();
          }}
        >
          {retrying
            ? t('messages.knowledgeWriteback.retrying')
            : t('messages.knowledgeWriteback.retry')}
        </button>
      )}
    </div>
  );
};

const isAbsoluteMessageFilePath = (file_path: string): boolean =>
  file_path.startsWith('/') || /^[A-Za-z]:/.test(file_path);

export const resolveMessageFilePath = (file_path: string, workspace?: string): string => {
  if (!file_path || isAbsoluteMessageFilePath(file_path) || !workspace) {
    return file_path;
  }

  const normalizedWorkspace = workspace.replace(/[\\/]+$/, '').replace(/\\/g, '/');
  const normalizedFilePath = file_path.replace(/^\.?[\\/]+/, '').replace(/\\/g, '/');
  return `${normalizedWorkspace}/${normalizedFilePath}`.replace(/\/+/g, '/');
};

const useFormatContent = (content: string) => {
  return useMemo(() => {
    try {
      const json = JSON.parse(content);
      const isJson = typeof json === 'object';
      return {
        json: isJson,
        data: isJson ? json : content,
      };
    } catch {
      return { data: content };
    }
  }, [content]);
};

const MessageText: React.FC<{ message: IMessageText; hideActions?: boolean }> = ({ message, hideActions = false }) => {
  // Filter think tags from content before rendering
  // 在渲染前过滤 think 标签
  const contentToRender = useMemo(() => {
    let content = toDisplayText(message.content.content);
    if (hasThinkTags(content)) {
      content = stripThinkTags(content);
    }
    // Strip any inline [SKILL_SUGGEST] blocks (now handled via separate skill_suggest message type)
    if (hasSkillSuggest(content)) {
      content = stripSkillSuggest(content);
    }
    return content;
  }, [message.content.content]);

  const { text, files } = parseMessageFileMarker(contentToRender, message.position);
  const { data, json } = useFormatContent(text);
  const { t } = useTranslation();
  const [showCopyAlert, setShowCopyAlert] = useState(false);
  const isUserMessage = message.position === 'right';
  const isAgentMessage = message.position === 'left' && message.content.agentMessage === true;
  const writebackState = !isUserMessage ? message.content.knowledge_writeback : undefined;
  const shouldRenderPlainText = isUserMessage;
  const conversationContext = useConversationContextSafe();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const shouldShowActions = !hideActions && !isMobile;
  const resolvedFiles = useMemo(
    () => files.map((file_path) => resolveMessageFilePath(file_path, conversationContext?.workspace)),
    [conversationContext?.workspace, files]
  );

  // 仅 Nomi、且为最近一条用户文本消息时可编辑（与后端"仅最近一条"对齐）。
  const messageList = useMessageList();
  const isLatestUserMessage = useMemo(() => {
    if (!isUserMessage) return false;
    const lastRight = [...messageList].reverse().find((m) => m.position === 'right' && m.type === 'text');
    return lastRight?.msg_id != null && lastRight.msg_id === message.msg_id;
  }, [isUserMessage, messageList, message.msg_id]);

  // 过滤空内容，避免渲染空DOM
  const hasRenderableContent = contentToRender.trim().length > 0;

  if (!hasRenderableContent && !writebackState) {
    return null;
  }

  const handleCopy = () => {
    const baseText = shouldRenderPlainText ? text : json ? JSON.stringify(data, null, 2) : text;
    const fileList = files.length ? `Files:\n${files.map((path) => `- ${path}`).join('\n')}\n\n` : '';
    const textToCopy = fileList + baseText;
    copyText(textToCopy)
      .then(() => {
        setShowCopyAlert(true);
        setTimeout(() => setShowCopyAlert(false), 2000);
      })
      .catch(() => {
        Message.error(t('common.copyFailed'));
      });
  };

  const copyButton = (
    <Tooltip content={t('common.copy', { defaultValue: 'Copy' })}>
      <div
        className='p-4px rd-4px cursor-pointer hover:bg-3 transition-colors opacity-0 pointer-events-none group-hover:opacity-100 group-hover:pointer-events-auto focus-within:opacity-100 focus-within:pointer-events-auto'
        onClick={handleCopy}
        style={{ lineHeight: 0 }}
      >
        <Copy theme='outline' size='16' fill={iconColors.secondary} />
      </div>
    </Tooltip>
  );

  // 编辑（仅 Nomi 原生、且为最近一条用户文本消息）：把原文回填输入框并截断本地后续消息。
  const canEdit = conversationContext?.type === 'nomi' && isUserMessage && message.type === 'text' && isLatestUserMessage;

  const handleEdit = () => {
    if (!message.msg_id || !message.created_at) return;
    const rawContent = typeof message.content?.content === 'string' ? message.content.content : '';
    const { text: editText } = parseMessageFileMarker(rawContent, 'right');
    emitter.emit('sendbox.edit', { msgId: message.msg_id, createdAt: message.created_at, content: editText });
  };

  const editButton = canEdit ? (
    <Tooltip content={t('conversation.editMessage.action', { defaultValue: 'Edit' })}>
      <div
        className='p-4px rd-4px cursor-pointer hover:bg-3 transition-colors opacity-0 pointer-events-none group-hover:opacity-100 group-hover:pointer-events-auto focus-within:opacity-100 focus-within:pointer-events-auto'
        onClick={handleEdit}
        style={{ lineHeight: 0 }}
      >
        <Edit theme='outline' size='16' fill={iconColors.secondary} />
      </div>
    </Tooltip>
  ) : null;

  const cronMeta = message.content.cronMeta;
  const senderName = message.content.senderName;
  const senderAgentType = message.content.senderAgentType;
  const senderConversationId = message.content.senderConversationId;
  const fallbackBackendLogo = senderAgentType ? getAgentLogo(senderAgentType) : null;

  return (
    <>
      <div className={classNames('min-w-0 flex flex-col group', isUserMessage ? 'items-end' : 'items-start')}>
        {cronMeta && <MessageCronBadge meta={cronMeta} />}
        {isAgentMessage && senderName && (
          <div className='flex items-center gap-6px mb-4px'>
            <AgentMessageAvatar
              senderName={senderName}
              senderConversationId={senderConversationId}
              backendLogo={fallbackBackendLogo}
            />
            <span className='text-12px text-t-secondary'>{senderName}</span>
          </div>
        )}
        {files.length > 0 && (
          <div className={classNames('mt-6px', { 'self-end': isUserMessage })}>
            {resolvedFiles.length === 1 ? (
              <div className='flex items-center'>
                <FilePreview path={resolvedFiles[0]} onRemove={() => undefined} readonly />
              </div>
            ) : (
              <HorizontalFileList>
                {resolvedFiles.map((path) => (
                  <FilePreview key={path} path={path} onRemove={() => undefined} readonly />
                ))}
              </HorizontalFileList>
            )}
          </div>
        )}
        {hasRenderableContent && (
          <div
            className={classNames('min-w-0 [&>p:first-child]:mt-0px [&>p:last-child]:mb-0px md:max-w-780px', {
              'bg-aou-2 p-6px md:p-8px': isUserMessage || cronMeta,
              'bg-3 p-6px md:p-8px': isAgentMessage,
              'w-full': !(isUserMessage || cronMeta || isAgentMessage),
            })}
            style={{
              ...(isUserMessage || cronMeta
                ? { borderRadius: '8px 0 8px 8px', color: 'var(--text-primary)' }
                : isAgentMessage
                  ? { borderRadius: '0 8px 8px 8px' }
                  : undefined),
            }}
          >
            {/* JSON 内容使用折叠组件 Use CollapsibleContent for JSON content */}
            {shouldRenderPlainText ? (
              <div className={MESSAGE_BODY_CLASS_NAME} data-testid='message-text-content'>
                {text}
              </div>
            ) : json ? (
              <CollapsibleContent maxHeight={200} defaultCollapsed={true}>
                <div data-testid='message-text-content'>
                  <MarkdownView
                    codeStyle={CODE_STYLE}
                    fontSize={MESSAGE_BODY_FONT_SIZE}
                    lineHeight={MESSAGE_BODY_LINE_HEIGHT}
                    allowUnverifiedImages={isUserMessage}
                  >{`\`\`\`json\n${JSON.stringify(data, null, 2)}\n\`\`\``}</MarkdownView>
                </div>
              </CollapsibleContent>
            ) : (
              <div data-testid='message-text-content'>
                <MarkdownView
                  codeStyle={CODE_STYLE}
                  fontSize={MESSAGE_BODY_FONT_SIZE}
                  lineHeight={MESSAGE_BODY_LINE_HEIGHT}
                  allowUnverifiedImages={isUserMessage}
                >
                  {data}
                </MarkdownView>
              </div>
            )}
          </div>
        )}
        {writebackState && (
          <MessageKnowledgeWriteback
            state={writebackState}
            conversationId={message.conversation_id}
            messageId={message.message_id ?? message.msg_id}
          />
        )}
        {/* Hover-revealed copy + timestamp row. Mobile has no hover affordance,
            so we drop the row entirely — system-level long-press still copies. */}
        {shouldShowActions && (
          <div
            className={classNames('h-32px flex items-center mt-4px gap-8px', {
              'flex-row-reverse': isUserMessage,
            })}
          >
            {copyButton}
            {editButton}
            {message.created_at && (
              <span className='text-12px text-t-secondary opacity-0 group-hover:opacity-100 transition-opacity select-none'>
                {formatMessageTime(message.created_at)}
              </span>
            )}
          </div>
        )}
      </div>
      {showCopyAlert && (
        <Alert
          type='success'
          content={t('messages.copySuccess')}
          showIcon
          className='fixed top-20px left-50% transform -translate-x-50% z-9999 w-max max-w-[80%]'
          style={{ boxShadow: '0px 2px 12px rgba(0,0,0,0.12)' }}
          closable={false}
        />
      )}
    </>
  );
};

export default MessageText;
