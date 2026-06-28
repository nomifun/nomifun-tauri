/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import type { IConversationArtifact } from '@/common/adapter/ipcBridge';
import type { IMessageAcpToolCall, IMessageToolCall, IMessageToolGroup, TMessage } from '@/common/chat/chatLib';
import { useConversationContextSafe } from '@/renderer/hooks/context/ConversationContext';
import { iconColors } from '@/renderer/styles/colors';
import { CHAT_MESSAGE_JUMP_EVENT, type ChatMessageJumpDetail } from '@/renderer/utils/chat/chatMinimapEvents';
import { Image } from '@arco-design/web-react';
import { Down } from '@icon-park/react';
import MessageAcpPermission from '@renderer/pages/conversation/Messages/acp/MessageAcpPermission';
import MessagePermission from './components/MessagePermission';
import MessageAcpToolCall from '@renderer/pages/conversation/Messages/acp/MessageAcpToolCall';
import classNames from 'classnames';
import React, { createContext, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useLocation } from 'react-router-dom';
import { uuid } from '@renderer/utils/common';
import './messages.css';
import HOC from '@renderer/utils/ui/HOC';
import type { FileChangeInfo } from './MessageFileChanges';
import MessageFileChanges, { parseDiff } from './MessageFileChanges';
import { useConversationArtifacts } from './artifacts';
import { useMessageList, useMessageListLoading } from './hooks';
import MessageAgentStatus from './components/MessageAgentStatus';
import MessageTips from './components/MessageTips';
import MessageToolCall from './components/MessageToolCall';
import MessageToolGroup from './components/MessageToolGroup';
import MessageToolGroupSummary from './components/MessageToolGroupSummary';
import MessageCronTrigger from './components/MessageCronTrigger';
import MessageSkillSuggest from './components/MessageSkillSuggest';
import MessageText from './components/MessageText';
import MessageThinking from './components/MessageThinking';
import type { WriteFileResult } from './types';
import { useAutoScroll } from './useAutoScroll';
import { useAutoPreviewOfficeFiles } from '@/renderer/hooks/file/useAutoPreviewOfficeFiles';
import SelectionReplyButton from './components/SelectionReplyButton';

type IMessageVO =
  | TMessage
  | { type: 'file_summary'; id: string; diffs: FileChangeInfo[]; sourceMessageIds: string[]; created_at: number }
  | {
      type: 'tool_summary';
      id: string;
      messages: Array<IMessageToolGroup | IMessageAcpToolCall | IMessageToolCall>;
      sourceMessageIds: string[];
      created_at: number;
    };
type IArtifactVO = { type: 'artifact'; id: string; artifact: IConversationArtifact; created_at: number };
type IProcessedItem = IMessageVO | IArtifactVO;

type ConversationLocationState = {
  targetMessageId?: string;
  fromConversationSearch?: boolean;
};

const getProcessedItemSourceMessageIds = (item: IProcessedItem): string[] => {
  if ('type' in item && item.type === 'artifact') {
    return [item.id];
  }
  if ('type' in item && item.type === 'tool_summary') {
    return item.sourceMessageIds;
  }
  if ('type' in item && item.type === 'file_summary') {
    return item.sourceMessageIds;
  }
  return 'id' in item ? [item.id] : [];
};

const matchesTargetMessage = (item: IProcessedItem, targetMessageId?: string): boolean => {
  if (!targetMessageId) {
    return false;
  }
  return getProcessedItemSourceMessageIds(item).includes(targetMessageId);
};

const getProcessedItemAnchorId = (item: IProcessedItem): string => {
  const sourceIds = getProcessedItemSourceMessageIds(item);
  return sourceIds[0] || ('id' in item ? item.id : uuid());
};

const getProcessedItemCreatedAt = (item: IProcessedItem): number => {
  if ('type' in item && ['file_summary', 'tool_summary', 'artifact'].includes(item.type)) {
    // `includes` doesn't narrow the union, so `created_at` is still typed
    // `number | undefined`; the synthetic VO types always carry a number, so
    // `?? 0` is a no-op fallback (mirrors the branch below).
    return item.created_at ?? 0;
  }
  return item.created_at ?? 0;
};

const highlightStyle: React.CSSProperties = {
  backgroundColor: 'var(--color-aou-1)',
  boxShadow: '0 0 0 1px var(--color-aou-6-brand) inset',
  borderRadius: '12px',
};

const getUnhandledMessageType = (_message: never): string => 'unknown';

/** Scroll-up zone (px from top) that triggers loading the next older window. */
const TOP_LOAD_THRESHOLD_PX = 96;

// Image preview context
export const ImagePreviewContext = createContext<{ inPreviewGroup: boolean }>({ inPreviewGroup: false });

const MessageListSkeleton: React.FC = () => {
  const rows = [
    { align: 'left', bubbleWidth: '100%', lines: [72, 58, 64] },
    { align: 'right', bubbleWidth: '82%', lines: [54, 48] },
    { align: 'left', bubbleWidth: '100%', lines: [68, 76, 44] },
    { align: 'left', bubbleWidth: '100%', lines: [46, 52] },
    { align: 'right', bubbleWidth: '78%', lines: [60, 42, 36] },
    { align: 'left', bubbleWidth: '100%', lines: [74, 62] },
    { align: 'right', bubbleWidth: '84%', lines: [52, 66] },
    { align: 'left', bubbleWidth: '100%', lines: [64, 56, 40] },
    { align: 'right', bubbleWidth: '80%', lines: [58, 46] },
  ] as const;

  return (
    <div
      className='flex-1 h-full overflow-y-auto pb-10px box-border'
      data-testid='message-list-skeleton'
      style={{ minHeight: '100%' }}
    >
      <div className='min-h-full flex flex-col justify-between py-10px box-border'>
        {rows.map((row, index) => (
          <div
            key={index}
            className={classNames(
              'w-full min-w-0 flex items-start message-item px-8px m-t-10px max-w-full md:max-w-780px mx-auto',
              {
                'justify-start': row.align === 'left',
                'justify-end': row.align === 'right',
              }
            )}
          >
            <div
              className='flex-none min-w-0 rd-16px p-14px'
              style={{
                width: row.bubbleWidth,
                maxWidth: '100%',
                background: 'var(--color-fill-1)',
                border: '1px solid var(--color-border-2)',
              }}
            >
              <div className='flex flex-col gap-10px'>
                {row.lines.map((width, lineIndex) => (
                  <div
                    key={lineIndex}
                    className='h-12px rd-999px'
                    style={{
                      width: `${width}%`,
                      background:
                        'linear-gradient(90deg, var(--color-fill-2) 0%, var(--color-fill-3) 50%, var(--color-fill-2) 100%)',
                      backgroundSize: '200% 100%',
                      animation: 'message-list-skeleton-shimmer 1.4s ease-in-out infinite',
                    }}
                  />
                ))}
              </div>
            </div>
          </div>
        ))}
      </div>
      <style>{`
        @keyframes message-list-skeleton-shimmer {
          0% { background-position: 200% 0; }
          100% { background-position: -200% 0; }
        }
      `}</style>
    </div>
  );
};

const MessageItem: React.FC<{ message: TMessage; highlighted?: boolean }> = React.memo(
  HOC((props) => {
    const { message, highlighted } = props as { message: TMessage; highlighted?: boolean };
    return (
      <div
        id={`message-${message.id}`}
        data-testid={`message-${message.type}-${message.position}`}
        data-message-type={message.type}
        data-message-position={message.position}
        className={classNames(
          'min-w-0 flex items-start message-item [&>div]:max-w-full px-8px m-t-10px max-w-full md:max-w-780px mx-auto',
          message.type,
          {
            'justify-center': message.position === 'center',
            'justify-end': message.position === 'right',
            'justify-start': message.position === 'left',
          }
        )}
        style={highlighted ? highlightStyle : undefined}
      >
        {props.children}
      </div>
    );
  })(({ message }) => {
    const { t } = useTranslation();
    switch (message.type) {
      case 'text':
        return <MessageText message={message}></MessageText>;
      case 'tips':
        return <MessageTips message={message}></MessageTips>;
      case 'tool_call':
        return <MessageToolCall message={message}></MessageToolCall>;
      case 'tool_group':
        return <MessageToolGroup message={message}></MessageToolGroup>;
      case 'agent_status':
        return <MessageAgentStatus message={message}></MessageAgentStatus>;
      case 'permission':
        return <MessagePermission message={message}></MessagePermission>;
      case 'acp_permission':
        return <MessageAcpPermission message={message}></MessageAcpPermission>;
      case 'acp_tool_call':
        return <MessageAcpToolCall message={message}></MessageAcpToolCall>;
      case 'plan':
        // Plans render in the docked PinnedPlan bar, not inline — they're
        // filtered out of processedList above. This guard keeps the switch
        // exhaustive (the `never` default below would otherwise error).
        return null;
      case 'thinking':
        return <MessageThinking message={message}></MessageThinking>;
      case 'available_commands':
        return null;
      default:
        return <div>{t('messages.unknownMessageType', { type: getUnhandledMessageType(message) })}</div>;
    }
  }),
  (prev, next) =>
    prev.message.id === next.message.id &&
    prev.message.content === next.message.content &&
    prev.message.position === next.message.position &&
    prev.message.type === next.message.type &&
    prev.highlighted === next.highlighted
);

const MessageList: React.FC<{
  className?: string;
  emptySlot?: React.ReactNode;
  /** Windowed-history paging (nomi surfaces): prepend the next older message
   *  window when the user scrolls to the top. Omitted on chats that still load
   *  their whole transcript at once. */
  onLoadOlder?: () => void | Promise<void>;
  hasMoreOlder?: boolean;
  loadingOlder?: boolean;
}> = ({ emptySlot, onLoadOlder, hasMoreOlder, loadingOlder }) => {
  const list = useMessageList();
  const isMessageListLoading = useMessageListLoading();
  const artifacts = useConversationArtifacts();
  const conversationContext = useConversationContextSafe();
  useAutoPreviewOfficeFiles(conversationContext);
  const { t } = useTranslation();
  const location = useLocation();
  const locationState = (location.state || {}) as ConversationLocationState;
  const targetMessageId = locationState.targetMessageId;
  const [highlightedMessageId, setHighlightedMessageId] = useState<string | undefined>();
  const handledTargetKeyRef = useRef<string>('');

  // Pre-process message list to group tool outputs into summary cards
  const processedList = useMemo(() => {
    const result: Array<IMessageVO> = [];
    let diffsChanges: FileChangeInfo[] = [];
    let diffsSourceMessageIds: string[] = [];
    let toolList: Array<IMessageToolGroup | IMessageAcpToolCall | IMessageToolCall> = [];
    let toolSourceMessageIds: string[] = [];

    const pushFileDffChanges = (changes: FileChangeInfo, sourceMessageId: string, created_at: number) => {
      if (!diffsChanges.length) {
        diffsSourceMessageIds = [];
        result.push({
          type: 'file_summary',
          id: `summary-${sourceMessageId}`,
          diffs: diffsChanges,
          sourceMessageIds: diffsSourceMessageIds,
          created_at,
        });
      }
      diffsChanges.push(changes);
      diffsSourceMessageIds.push(sourceMessageId);
      toolList = [];
      toolSourceMessageIds = [];
    };
    const pushToolList = (message: IMessageToolGroup | IMessageAcpToolCall | IMessageToolCall) => {
      if (!toolList.length) {
        toolSourceMessageIds = [];
        result.push({
          type: 'tool_summary',
          id: `tool-summary-${message.id}`,
          messages: toolList,
          sourceMessageIds: toolSourceMessageIds,
          created_at: message.created_at ?? 0,
        });
      }
      toolList.push(message);
      toolSourceMessageIds.push(message.id);
      diffsChanges = [];
      diffsSourceMessageIds = [];
    };

    for (let i = 0, len = list.length; i < len; i++) {
      const message = list[i];
      // Skip hidden and available_commands messages
      if (message.hidden) continue;
      if (message.type === 'available_commands') continue;
      // Plans are no longer rendered inline — they surface in the docked
      // PinnedPlan bar above the composer, which reads the raw list directly.
      if (message.type === 'plan') continue;
      // Connection-handshake status banners (connecting/connected/authenticated/
      // session_active) are implementation noise: never render them as chat
      // items, and never let them fragment the tool-execution trace below.
      // Actionable 'error' status still surfaces. (Phase 3 UX)
      if (message.type === 'agent_status') {
        const st = (message.content as { status?: string })?.status;
        if (st === 'connecting' || st === 'connected' || st === 'authenticated' || st === 'session_active') {
          continue;
        }
      }
      if (message.type === 'tool_group') {
        if (message.content.length === 1) {
          const writeFileResults = message.content
            .filter(
              (item) =>
                item.name === 'WriteFile' &&
                item.result_display &&
                typeof item.result_display === 'object' &&
                'file_diff' in item.result_display
            )
            .map((item) => item.result_display as WriteFileResult);
          if (writeFileResults.length && writeFileResults[0].file_diff) {
            pushFileDffChanges(
              parseDiff(writeFileResults[0].file_diff, writeFileResults[0].file_name),
              message.id,
              message.created_at ?? 0
            );
            continue;
          }
        }
        pushToolList(message);
        continue;
      }
      if (message.type === 'acp_tool_call') {
        pushToolList(message);
        continue;
      }
      if (message.type === 'tool_call') {
        pushToolList(message);
        continue;
      }
      toolList = [];
      toolSourceMessageIds = [];
      diffsChanges = [];
      diffsSourceMessageIds = [];
      result.push(message);
    }
    const visibleArtifacts = artifacts
      .filter((artifact) => {
        if (artifact.kind === 'cron_trigger') return artifact.status === 'active';
        if (artifact.kind === 'skill_suggest') return artifact.status === 'pending';
        return false;
      })
      .map<IArtifactVO>((artifact) => ({
        type: 'artifact',
        id: `artifact_${artifact.id}`,
        artifact,
        created_at: artifact.created_at,
      }));

    if (visibleArtifacts.length === 0) {
      // Common streaming case: nothing to interleave, and `result` is already in
      // arrival (created_at) order — skip the O(n log n) re-sort that otherwise
      // runs on every streamed token and janks long conversations.
      return result;
    }
    return [...result, ...visibleArtifacts].toSorted(
      (a, b) => getProcessedItemCreatedAt(a) - getProcessedItemCreatedAt(b)
    );
  }, [artifacts, list]);

  // Use auto-scroll hook
  const {
    handleScrollerRef,
    handleContentRef,
    handleScroll,
    handleWheel,
    handlePointerDown,
    showScrollButton,
    scrollToBottom,
    scrollElementIntoView,
    hideScrollButton,
  } = useAutoScroll({
    messages: list,
    itemCount: processedList.length,
  });

  // ── Windowed history: load older messages on scroll-up with a scroll-anchor ──
  const scrollerElRef = useRef<HTMLDivElement | null>(null);
  const lastScrollTopRef = useRef(0);
  // Set when a load-older was triggered; the layout effect below restores the
  // viewport once the prepend grows the content so the position doesn't jump.
  const prependAnchorRef = useRef<{ height: number; top: number } | null>(null);

  const handleScrollWithPaging = useCallback(
    (e: React.UIEvent<HTMLDivElement>) => {
      const el = e.currentTarget;
      scrollerElRef.current = el;
      handleScroll(e);
      const prevTop = lastScrollTopRef.current;
      lastScrollTopRef.current = el.scrollTop;
      // Fire only while actively scrolling UP into the top zone. The initial
      // mount auto-scroll-to-bottom moves scrollTop downward, so it can't trip
      // this; `prependAnchorRef` guards against re-entrancy mid-load.
      if (
        onLoadOlder &&
        hasMoreOlder &&
        !loadingOlder &&
        !prependAnchorRef.current &&
        el.scrollTop <= TOP_LOAD_THRESHOLD_PX &&
        prevTop > el.scrollTop
      ) {
        prependAnchorRef.current = { height: el.scrollHeight, top: el.scrollTop };
        void onLoadOlder();
      }
    },
    [handleScroll, onLoadOlder, hasMoreOlder, loadingOlder]
  );

  // Restore the viewport after an older window prepends (content grew at the
  // top). Keyed on the raw `list.length` (always grows by the prepended count,
  // even when the grouping transform merges cards). `overflowAnchor: none` on
  // the scroller keeps the browser from fighting this. Only acts while a
  // load-older is pending; ordinary bottom growth (streaming) leaves the anchor
  // null and is untouched.
  useLayoutEffect(() => {
    const anchor = prependAnchorRef.current;
    if (!anchor) return;
    const el = scrollerElRef.current;
    if (el) {
      const delta = el.scrollHeight - anchor.height;
      if (delta > 0) {
        el.scrollTop = anchor.top + delta;
        lastScrollTopRef.current = el.scrollTop;
      }
    }
    prependAnchorRef.current = null;
  }, [list.length]);

  useEffect(() => {
    if (!targetMessageId || processedList.length === 0) {
      return;
    }

    const targetKey = `${location.key}:${targetMessageId}`;
    if (handledTargetKeyRef.current === targetKey) {
      return;
    }

    const targetIndex = processedList.findIndex((item) => matchesTargetMessage(item, targetMessageId));
    if (targetIndex === -1) {
      return;
    }

    handledTargetKeyRef.current = targetKey;
    setHighlightedMessageId(targetMessageId);
    hideScrollButton();

    requestAnimationFrame(() => {
      const targetElement = document.getElementById(`message-${getProcessedItemAnchorId(processedList[targetIndex])}`);
      scrollElementIntoView(targetElement, {
        behavior: 'smooth',
        block: 'center',
      });
    });

    const timer = window.setTimeout(() => {
      setHighlightedMessageId((current) => (current === targetMessageId ? undefined : current));
    }, 2400);

    return () => window.clearTimeout(timer);
  }, [hideScrollButton, location.key, processedList, scrollElementIntoView, targetMessageId]);

  useEffect(() => {
    const handleMessageJump = (event: Event) => {
      const detail = (event as CustomEvent<ChatMessageJumpDetail>).detail;
      if (!detail || !detail.conversation_id) return;
      // detail.conversation_id arrives as a route/event string; coerce to the
      // numeric conversation id before comparing against the context id.
      if (!conversationContext?.conversation_id || Number(detail.conversation_id) !== conversationContext.conversation_id)
        return;

      const targetIndex = processedList.findIndex((item) => {
        if (
          (item as { type?: string }).type === 'file_summary' ||
          (item as { type?: string }).type === 'tool_summary' ||
          (item as { type?: string }).type === 'artifact'
        ) {
          return false;
        }
        const message = item as TMessage;
        if (detail.messageId && message.id === detail.messageId) return true;
        if (detail.msgId && message.msg_id === detail.msgId) return true;
        return false;
      });
      if (targetIndex < 0) return;

      hideScrollButton();
      requestAnimationFrame(() => {
        const targetElement = document.getElementById(
          `message-${getProcessedItemAnchorId(processedList[targetIndex])}`
        );
        scrollElementIntoView(targetElement, {
          block: detail.align || 'start',
          behavior: detail.behavior || 'smooth',
        });
      });
    };

    window.addEventListener(CHAT_MESSAGE_JUMP_EVENT, handleMessageJump);
    return () => {
      window.removeEventListener(CHAT_MESSAGE_JUMP_EVENT, handleMessageJump);
    };
  }, [conversationContext?.conversation_id, hideScrollButton, processedList, scrollElementIntoView]);

  // Click scroll button
  const handleScrollButtonClick = () => {
    hideScrollButton();
    scrollToBottom('smooth');
  };

  const renderItem = (_index: number, item: (typeof processedList)[0]) => {
    const highlighted = matchesTargetMessage(item, highlightedMessageId);
    if ('type' in item && item.type === 'artifact') {
      return (
        <div
          key={item.id}
          id={`message-${getProcessedItemAnchorId(item)}`}
          data-conversation-artifact-kind={item.artifact.kind}
          data-testid={`conversation-artifact-${item.artifact.kind}`}
          className='min-w-0 message-item px-8px m-t-10px max-w-full md:max-w-780px mx-auto'
          style={highlighted ? highlightStyle : undefined}
        >
          {item.artifact.kind === 'cron_trigger' ? (
            <MessageCronTrigger artifact={item.artifact} />
          ) : (
            <MessageSkillSuggest artifact={item.artifact} />
          )}
        </div>
      );
    }
    if ('type' in item && ['file_summary', 'tool_summary'].includes(item.type)) {
      return (
        <div
          key={item.id}
          id={`message-${getProcessedItemAnchorId(item)}`}
          className={'min-w-0 message-item px-8px m-t-10px max-w-full md:max-w-780px mx-auto ' + item.type}
          style={highlighted ? highlightStyle : undefined}
        >
          {item.type === 'file_summary' && <MessageFileChanges diffsChanges={item.diffs} />}
          {item.type === 'tool_summary' && <MessageToolGroupSummary messages={item.messages}></MessageToolGroupSummary>}
        </div>
      );
    }
    return <MessageItem message={item as TMessage} key={(item as TMessage).id} highlighted={highlighted}></MessageItem>;
  };

  if (processedList.length === 0 && isMessageListLoading) {
    return <MessageListSkeleton />;
  }

  if (processedList.length === 0 && emptySlot) {
    return <div className='relative flex-1 h-full flex items-center justify-center'>{emptySlot}</div>;
  }

  return (
    <div className='relative flex-1 h-full'>
      {/* Use PreviewGroup to wrap all messages for cross-message image preview */}
      <Image.PreviewGroup actionsLayout={['zoomIn', 'zoomOut', 'originalSize', 'rotateLeft', 'rotateRight']}>
        <ImagePreviewContext.Provider value={{ inPreviewGroup: true }}>
          <div
            ref={handleScrollerRef}
            data-testid='message-list-scroller'
            className='flex-1 h-full overflow-y-auto pb-10px box-border'
            style={{ overflowAnchor: 'none' }}
            onPointerDown={handlePointerDown}
            onScroll={handleScrollWithPaging}
            onWheel={handleWheel}
          >
            <div ref={handleContentRef} data-testid='message-list-content' style={{ overflowAnchor: 'none' }}>
              <div className='h-10px' />
              {processedList.map((item, index) => (
                <React.Fragment key={getProcessedItemAnchorId(item) || index}>{renderItem(index, item)}</React.Fragment>
              ))}
              <div className='h-20px' />
            </div>
          </div>
        </ImagePreviewContext.Provider>
      </Image.PreviewGroup>

      {showScrollButton && (
        <>
          {/* Gradient mask */}
          <div className='absolute bottom-0 left-0 right-0 h-100px pointer-events-none' />
          {/* Scroll button */}
          <div className='absolute bottom-20px left-50% transform -translate-x-50% z-100'>
            <div
              className='flex items-center justify-center w-40px h-40px rd-full bg-base shadow-lg cursor-pointer hover:bg-1 transition-all hover:scale-110 border-1 border-solid border-3'
              onClick={handleScrollButtonClick}
              title={t('messages.scrollToBottom')}
              style={{ lineHeight: 0 }}
            >
              <Down theme='filled' size='20' fill={iconColors.secondary} style={{ display: 'block' }} />
            </div>
          </div>
        </>
      )}

      <SelectionReplyButton messages={list} />
    </div>
  );
};

export default MessageList;
