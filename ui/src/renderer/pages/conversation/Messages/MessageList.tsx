/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IConversationArtifact } from '@/common/adapter/ipcBridge';
import type { IMessageAcpToolCall, IMessageToolCall, IMessageToolGroup, TMessage } from '@/common/chat/chatLib';
import { normalizeToolMessages } from '@/common/chat/normalizeToolCall';
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
import { parseDiff } from './MessageFileChanges';
import { useConversationArtifacts } from './artifacts';
import { useMessageList, useMessageListLoading } from './hooks';
import MessageAgentStatus from './components/MessageAgentStatus';
import MessageTips from './components/MessageTips';
import MessageToolCall from './components/MessageToolCall';
import MessageToolGroup from './components/MessageToolGroup';
import MessageCronTrigger from './components/MessageCronTrigger';
import MessageSkillSuggest from './components/MessageSkillSuggest';
import MessageText from './components/MessageText';
import MessageThinking from './components/MessageThinking';
import MessageListSkeleton from './components/MessageListSkeleton';
import TurnProcessDisclosure from './components/TurnProcessDisclosure';
import TurnProcessReceipt, { type TurnProcessReceiptIcon } from './components/TurnProcessReceipt';
import {
  buildToolReceiptSummaryParts,
  buildToolSummaryDescriptor,
  getToolReceiptIconFromSummaryParts,
  type ToolReceiptSummaryPart,
} from './components/toolGroupSummaryModel';
import ProcessTraceItem, { type ProcessTraceItemExpansionControls } from './components/ProcessTraceItem';
import { isContextCompressionTip } from './processTipModel';
import { formatFileTargetPreview, splitToolReceiptTargets } from './processFileTargetLabel';
import type { WriteFileResult } from './types';
import { useAutoScroll } from './useAutoScroll';
import { useAutoPreviewOfficeFiles } from '@/renderer/hooks/file/useAutoPreviewOfficeFiles';
import SelectionReplyButton from './components/SelectionReplyButton';
import {
  assignTurnIdsFromUserRequests,
  buildTurnDisclosureItems,
  type TurnDisclosureProcessState,
  type TurnDisclosureInputItem,
  type TurnDisclosureOutputItem,
} from './turnDisclosureModel';
import { getProcessItemState } from './turnProcessState';

type IMessageVO =
  | TMessage
  | {
      type: 'file_summary';
      id: string;
      msg_id?: string;
      diffs: FileChangeInfo[];
      sourceMessageIds: string[];
      created_at: number;
    }
  | {
      type: 'tool_summary';
      id: string;
      msg_id?: string;
      messages: Array<IMessageToolGroup | IMessageAcpToolCall | IMessageToolCall>;
      sourceMessageIds: string[];
      created_at: number;
    };
type IArtifactVO = { type: 'artifact'; id: string; artifact: IConversationArtifact; created_at: number };
type IRenderableItem = IMessageVO | IArtifactVO;
type ITurnProcessDisclosureVO = {
  type: 'turn_process_disclosure';
  id: string;
  msg_id: string;
  processItems: IRenderableItem[];
  processItemStates: Record<string, TurnDisclosureProcessState>;
  sourceMessageIds: string[];
  created_at: number;
  startAt: number;
  endAt: number;
  state: TurnDisclosureProcessState;
  running: boolean;
  defaultCollapsed: boolean;
};
type IProcessReceiptVO = {
  type: 'process_receipt';
  id: string;
  msg_id?: string;
  item: IRenderableItem;
  sourceMessageIds: string[];
  created_at: number;
  state: TurnDisclosureProcessState;
  label: string;
  icon: TurnProcessReceiptIcon;
  defaultExpanded: boolean;
  hasDetail?: boolean;
};
type IProcessedItem = IRenderableItem | ITurnProcessDisclosureVO | IProcessReceiptVO;

type ConversationLocationState = {
  targetMessageId?: string;
  fromConversationSearch?: boolean;
};

const getProcessedItemSourceMessageIds = (item: IProcessedItem): string[] => {
  if ('type' in item && (item.type === 'turn_process_disclosure' || item.type === 'process_receipt')) {
    return item.sourceMessageIds;
  }
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
  if (
    'type' in item &&
    ['file_summary', 'tool_summary', 'artifact', 'turn_process_disclosure', 'process_receipt'].includes(item.type)
  ) {
    // `includes` doesn't narrow the union, so `created_at` is still typed
    // `number | undefined`; the synthetic VO types always carry a number, so
    // `?? 0` is a no-op fallback (mirrors the branch below).
    return item.created_at ?? 0;
  }
  return item.created_at ?? 0;
};

const getThinkingDurationMs = (item: IRenderableItem): number | undefined => {
  if (!('type' in item) || item.type !== 'thinking') return undefined;
  const duration = item.content.duration;
  if (typeof duration !== 'number' || !Number.isFinite(duration) || duration <= 0) return undefined;
  return duration;
};

const getProcessedItemProcessStartedAt = (item: IRenderableItem): number => getProcessedItemCreatedAt(item);

const getProcessedItemProcessEndedAt = (item: IRenderableItem): number => {
  const createdAt = getProcessedItemCreatedAt(item);
  const duration = getThinkingDurationMs(item);
  if (duration === undefined) return createdAt;
  return createdAt + duration;
};

const getProcessedItemMsgId = (item: IRenderableItem): string | undefined => {
  if ('type' in item && (item.type === 'file_summary' || item.type === 'tool_summary')) {
    return item.msg_id;
  }
  if ('type' in item && item.type === 'artifact') {
    return undefined;
  }
  return item.msg_id;
};

const getProcessedItemRole = (item: IRenderableItem): TurnDisclosureInputItem['role'] => {
  if ('type' in item && (item.type === 'file_summary' || item.type === 'tool_summary')) {
    return 'process';
  }
  if ('type' in item && item.type === 'artifact') {
    return 'other';
  }

  switch (item.type) {
    case 'text':
      return item.position === 'right' ? 'user' : 'assistant';
    case 'tips':
      if (isContextCompressionTip(item)) return 'process';
      return 'assistant';
    case 'thinking':
      return 'process_content';
    case 'tool_call':
    case 'tool_group':
    case 'agent_status':
    case 'permission':
    case 'acp_permission':
    case 'acp_tool_call':
      return 'process';
    default:
      return 'other';
  }
};

type TranslationFn = ReturnType<typeof useTranslation>['t'];

const defaultToolSummaryByState: Record<TurnDisclosureProcessState, string> = {
  completed: 'Ran {{target}}',
  running: 'Running {{target}}',
  waiting: 'Waiting to confirm {{target}}',
  failed: 'Failed {{target}}',
  canceled: 'Canceled {{target}}',
};

const compactReceiptText = (value: unknown, fallback: string): string => {
  if (typeof value !== 'string') return fallback;
  const compacted = value.replace(/\s+/g, ' ').trim();
  return compacted || fallback;
};

const getToolReceiptDisplayTarget = (part: ToolReceiptSummaryPart, workspaceRoots: string[]): string | undefined => {
  if (!part.target) return undefined;
  if (part.action !== 'read_files' && part.action !== 'edit_files') return part.target;
  const targets = splitToolReceiptTargets(part.target);
  return targets.length ? formatFileTargetPreview(targets, { workspaceRoots }) : part.target;
};

const formatToolReceiptPart = (
  part: ToolReceiptSummaryPart,
  t: TranslationFn,
  workspaceRoots: string[]
): string => {
  const displayTarget = getToolReceiptDisplayTarget(part, workspaceRoots);

  if ((part.state === 'failed' || part.state === 'canceled') && displayTarget) {
    return t(`messages.toolSummary.${part.state}`, {
      target: displayTarget,
      defaultValue: defaultToolSummaryByState[part.state],
    });
  }

  switch (part.action) {
    case 'read_files':
      if (displayTarget) {
        return part.state === 'running'
          ? t('messages.processReceipt.readingTargets', {
              count: part.count,
              target: displayTarget,
              defaultValue: 'Reading {{count}} files: {{target}}',
            })
          : t('messages.processReceipt.readTargets', {
              count: part.count,
              target: displayTarget,
              defaultValue: 'Read {{count}} files: {{target}}',
            });
      }
      return part.state === 'running'
        ? t('messages.processReceipt.readingFiles', {
            count: part.count,
            defaultValue: 'Reading {{count}} files',
          })
        : t('messages.processReceipt.readFiles', {
            count: part.count,
            defaultValue: 'Read {{count}} files',
          });
    case 'edit_files':
      if (displayTarget) {
        return part.state === 'running'
          ? t('messages.processReceipt.editingFileTargets', {
              count: part.count,
              target: displayTarget,
              defaultValue: 'Editing {{count}} files: {{target}}',
            })
          : t('messages.processReceipt.fileEditTargets', {
              count: part.count,
              target: displayTarget,
              defaultValue: 'Edited {{count}} files: {{target}}',
            });
      }
      return part.state === 'running'
        ? t('messages.processReceipt.editingFiles', {
            count: part.count,
            defaultValue: 'Editing {{count}} files',
          })
        : t('messages.processReceipt.fileEdits', {
            count: part.count,
            defaultValue: 'Edited {{count}} files',
          });
    case 'run_commands':
      if (part.count === 1 && part.target) {
        return t(`messages.toolSummary.${part.state}`, {
          target: part.target,
          defaultValue: defaultToolSummaryByState[part.state],
        });
      }
      return part.state === 'running'
        ? t('messages.processReceipt.runningCommands', {
            count: part.count,
            defaultValue: 'Running {{count}} commands',
          })
        : t('messages.processReceipt.runCommands', {
            count: part.count,
            defaultValue: 'Ran {{count}} commands',
          });
    case 'search_code':
      return part.state === 'running'
        ? t('messages.processReceipt.searchingCode', { defaultValue: 'Searching code' })
        : t('messages.processReceipt.searchedCode', { defaultValue: 'Searched code' });
    case 'list_files':
      return part.state === 'running'
        ? t('messages.processReceipt.listingFiles', { defaultValue: 'Listing files' })
        : t('messages.processReceipt.listedFiles', { defaultValue: 'Listed files' });
    case 'load_tools':
      return part.state === 'running'
        ? t('messages.processReceipt.loadingTools', {
            count: part.count,
            defaultValue: 'Loading {{count}} tools',
          })
        : t('messages.processReceipt.loadedTools', {
            count: part.count,
            defaultValue: 'Loaded {{count}} tools',
          });
    case 'generic':
    default:
      if (displayTarget) {
        return t(`messages.toolSummary.${part.state}`, {
          target: displayTarget,
          defaultValue: defaultToolSummaryByState[part.state],
        });
      }
      return t('messages.processReceipt.tools', {
        count: part.count,
        defaultValue: '{{count}} tools',
      });
  }
};

const getToolReceiptIcon = (
  messages: Array<IMessageToolGroup | IMessageAcpToolCall | IMessageToolCall>
): TurnProcessReceiptIcon => {
  const latestMessage = messages.findLast(Boolean);
  if (!latestMessage) return 'tool';

  if (latestMessage.type === 'acp_tool_call') {
    const kind = latestMessage.content.update?.kind;
    if (kind === 'edit') return 'edit';
    if (kind === 'read') return 'file';
    return 'tool';
  }

  if (latestMessage.type === 'tool_group') {
    const latestTool = latestMessage.content.findLast(Boolean);
    const confirmationType = latestTool?.confirmationDetails?.type;
    if (confirmationType === 'edit') return 'edit';
    if (confirmationType === 'info') return 'file';
    return 'tool';
  }

  const toolName = `${latestMessage.content.name ?? ''} ${latestMessage.content.description ?? ''}`.toLowerCase();
  if (/\b(write|edit|patch|update|modify)\b/.test(toolName)) return 'edit';
  if (/\b(read|list|ls|glob|search|grep|find)\b/.test(toolName)) return 'file';
  return 'tool';
};

const buildProcessReceiptSummary = (
  item: IRenderableItem,
  state: TurnDisclosureProcessState,
  t: TranslationFn,
  workspaceRoots: string[] = []
): { label: string; icon: TurnProcessReceiptIcon; defaultExpanded: boolean; hasDetail?: boolean } => {
  if ('type' in item && item.type === 'tool_summary') {
    const tools = normalizeToolMessages(item.messages);
    const receiptParts = buildToolReceiptSummaryParts(tools, state);
    const descriptor = buildToolSummaryDescriptor(tools, state);
    const label = receiptParts.length
      ? receiptParts.map((part) => formatToolReceiptPart(part, t, workspaceRoots)).join(' ')
      : descriptor
        ? t(`messages.toolSummary.${state}`, {
            target: descriptor.target,
            defaultValue: defaultToolSummaryByState[state],
          })
        : t('messages.processReceipt.tools', {
            count: item.messages.length,
            defaultValue: '{{count}} tools',
          });
    return {
      label,
      icon: getToolReceiptIconFromSummaryParts(receiptParts) ?? getToolReceiptIcon(item.messages),
      defaultExpanded: state === 'waiting',
      hasDetail: true,
    };
  }

  if ('type' in item && item.type === 'file_summary') {
    const targets = item.diffs
      .map((file) => file.fullPath || file.file_name)
      .filter((target): target is string => Boolean(target));
    const targetPreview = targets.length ? formatFileTargetPreview(targets, { workspaceRoots }) : '';
    return {
      label: targetPreview
        ? t('messages.processReceipt.fileEditTargets', {
            count: item.diffs.length,
            target: targetPreview,
            defaultValue: 'Edited {{count}} files: {{target}}',
          })
        : t('messages.processReceipt.fileEdits', {
            count: item.diffs.length,
            defaultValue: 'Edited {{count}} files',
          }),
      icon: 'edit',
      defaultExpanded: false,
      hasDetail: item.diffs.length > 1,
    };
  }

  if ('type' in item && item.type === 'artifact') {
    const target =
      item.artifact.kind === 'cron_trigger' ? item.artifact.payload.cron_job_name : item.artifact.payload.name;
    return {
      label: t('messages.processReceipt.status', { target, defaultValue: '{{target}}' }),
      icon: 'status',
      defaultExpanded: false,
      hasDetail: false,
    };
  }

  switch (item.type) {
    case 'permission':
      return {
        label: t('messages.processReceipt.waitingPermission', {
          target: compactReceiptText(item.content.title || item.content.description, t('messages.permissionRequest')),
          defaultValue: 'Waiting to confirm {{target}}',
        }),
        icon: 'permission',
        defaultExpanded: true,
        hasDetail: true,
      };
    case 'acp_permission':
      return {
        label: t('messages.processReceipt.waitingPermission', {
          target: compactReceiptText(
            item.content.tool_call?.title ||
              item.content.tool_call?.raw_input?.command ||
              item.content.tool_call?.raw_input?.description,
            t('messages.permissionRequest')
          ),
          defaultValue: 'Waiting to confirm {{target}}',
        }),
        icon: 'permission',
        defaultExpanded: true,
        hasDetail: true,
      };
    case 'agent_status':
      return {
        label:
          item.content.status === 'preparing'
            ? t('messages.processReceipt.preparingAction', { defaultValue: 'Preparing next action' })
            : item.content.status === 'prepared'
              ? t('messages.processReceipt.preparedAction', { defaultValue: 'Prepared next action' })
            : state === 'failed'
            ? t('messages.processReceipt.agentFailed', {
                target: item.content.agent_name || item.content.backend,
                defaultValue: '{{target}} failed',
              })
            : t('messages.processReceipt.agentConnecting', {
                target: item.content.agent_name || item.content.backend,
                defaultValue: 'Connecting {{target}}',
              }),
        icon: 'status',
        defaultExpanded: false,
        hasDetail: false,
      };
    case 'tips':
      if (isContextCompressionTip(item)) {
        return {
          label: t('messages.processReceipt.contextCompressed', { defaultValue: 'Context compressed' }),
          icon: 'status',
          defaultExpanded: false,
          hasDetail: false,
        };
      }
      return {
        label: compactReceiptText(
          item.content.content,
          t('messages.processReceipt.status', { target: t('messages.processing'), defaultValue: '{{target}}' })
        ),
        icon: state === 'failed' ? 'permission' : 'status',
        defaultExpanded: state === 'failed',
        hasDetail: false,
      };
    case 'tool_call':
    case 'tool_group':
    case 'acp_tool_call':
      return buildProcessReceiptSummary(
        {
          type: 'tool_summary',
          id: `tool-summary-${item.id}`,
          msg_id: item.msg_id,
          messages: [item],
          sourceMessageIds: [item.id],
          created_at: item.created_at ?? 0,
        },
        state,
        t,
        workspaceRoots
      );
    default:
      return {
        label: t('messages.processReceipt.status', {
          target: t('messages.processing'),
          defaultValue: '{{target}}',
        }),
        icon: 'status',
        defaultExpanded: false,
        hasDetail: false,
      };
  }
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

const renderProcessTraceItem = (
  item: IRenderableItem,
  variant: 'list' | 'receipt' = 'list',
  workspaceRoots: string[] = [],
  stateOverride?: TurnDisclosureProcessState,
  thinkingExpansion?: ProcessTraceItemExpansionControls
) => (
  <ProcessTraceItem
    item={item}
    variant={variant}
    workspaceRoots={workspaceRoots}
    stateOverride={stateOverride}
    thinkingExpansion={thinkingExpansion}
  />
);

const isCompletedThinkingProcessItem = (item: IRenderableItem): boolean =>
  'type' in item && item.type === 'thinking' && item.content.status === 'done';

const getProcessItemLayoutKind = (item: IRenderableItem): string => {
  if ('type' in item && item.type === 'text') return 'text';
  if ('type' in item && item.type === 'thinking') return 'thinking';
  if (
    'type' in item &&
    ['tool_summary', 'file_summary', 'tool_call', 'tool_group', 'acp_tool_call'].includes(item.type)
  ) {
    return 'tool';
  }
  if ('type' in item && (item.type === 'permission' || item.type === 'acp_permission')) return 'permission';
  if ('type' in item && (item.type === 'agent_status' || item.type === 'tips' || item.type === 'artifact')) return 'status';
  return 'other';
};

const MessageItem: React.FC<{ message: TMessage; highlighted?: boolean; hideActions?: boolean }> = React.memo(
  HOC((props) => {
    const { message, highlighted } = props as { message: TMessage; highlighted?: boolean; hideActions?: boolean };
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
  })(({ message, hideActions }) => {
    const { t } = useTranslation();
    switch (message.type) {
      case 'text':
        return <MessageText message={message} hideActions={hideActions}></MessageText>;
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
    prev.highlighted === next.highlighted &&
    prev.hideActions === next.hideActions
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
  const workspaceRoots = useMemo(
    () => (conversationContext?.workspace ? [conversationContext.workspace] : []),
    [conversationContext?.workspace]
  );
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

    const pushFileDffChanges = (
      changes: FileChangeInfo,
      sourceMessageId: string,
      created_at: number,
      msg_id?: string
    ) => {
      if (!diffsChanges.length) {
        diffsSourceMessageIds = [];
        result.push({
          type: 'file_summary',
          id: `summary-${sourceMessageId}`,
          msg_id,
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
          msg_id: message.msg_id,
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
              message.created_at ?? 0,
              message.msg_id
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

  const displayList = useMemo<IProcessedItem[]>(() => {
    const itemById = new Map<string, IRenderableItem>();
    const rawModelInput: TurnDisclosureInputItem[] = processedList.map((item) => {
      const id = getProcessedItemAnchorId(item);
      const role = getProcessedItemRole(item);
      itemById.set(id, item);
      return {
        id,
        turnId: role === 'user' ? getProcessedItemMsgId(item) : undefined,
        role,
        createdAt: getProcessedItemCreatedAt(item),
        processState: getProcessItemState(item),
        processStartedAt: getProcessedItemProcessStartedAt(item),
        processEndedAt: getProcessedItemProcessEndedAt(item),
        sourceMessageIds: getProcessedItemSourceMessageIds(item),
      };
    });
    const modelInput = assignTurnIdsFromUserRequests(rawModelInput);

    const disclosureItems = buildTurnDisclosureItems(modelInput, { tailClosed: conversationContext?.isProcessing !== true })
      .map<IProcessedItem | undefined>((entry: TurnDisclosureOutputItem) => {
        if (entry.type === 'item') {
          return itemById.get(entry.id);
        }

        if (entry.type === 'process_receipt') {
          const item = itemById.get(entry.itemId);
          if (!item) return undefined;
          const state = getProcessItemState(item);
          const summary = buildProcessReceiptSummary(item, state, t, workspaceRoots);
          return {
            type: 'process_receipt',
            id: entry.id,
            msg_id: getProcessedItemMsgId(item),
            item,
            sourceMessageIds: getProcessedItemSourceMessageIds(item),
            created_at: getProcessedItemCreatedAt(item),
            state,
            label: summary.label,
            icon: summary.icon,
            defaultExpanded: summary.defaultExpanded,
            hasDetail: summary.hasDetail,
          };
        }

        const processItems = entry.processItemIds
          .map((id) => itemById.get(id))
          .filter((item): item is IRenderableItem => Boolean(item));

        return {
          type: 'turn_process_disclosure',
          id: entry.id,
          msg_id: entry.turnId,
          processItems,
          processItemStates: entry.processItemStates,
          sourceMessageIds: entry.sourceMessageIds,
          created_at: entry.endAt,
          startAt: entry.startAt,
          endAt: entry.endAt,
          state: entry.state,
          running: entry.running,
          defaultCollapsed: entry.defaultCollapsed,
        };
      })
      .filter((item): item is IProcessedItem => Boolean(item));

    return disclosureItems;
  }, [conversationContext?.isProcessing, processedList, t, workspaceRoots]);

  const lastUserTextIndex = useMemo(
    () =>
      displayList.findLastIndex(
        (item) => !('type' in item && ['turn_process_disclosure', 'process_receipt', 'artifact'].includes(item.type)) &&
          (item as TMessage).type === 'text' &&
          (item as TMessage).position === 'right'
      ),
    [displayList]
  );

  const isActiveProcessTextItem = useCallback(
    (item: IProcessedItem, index: number): boolean =>
      conversationContext?.isProcessing === true &&
      index > lastUserTextIndex &&
      !('type' in item && ['turn_process_disclosure', 'process_receipt', 'artifact'].includes(item.type)) &&
      (item as TMessage).type === 'text' &&
      (item as TMessage).position === 'left',
    [conversationContext?.isProcessing, lastUserTextIndex]
  );

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
    itemCount: displayList.length,
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
    if (!targetMessageId || displayList.length === 0) {
      return;
    }

    const targetKey = `${location.key}:${targetMessageId}`;
    if (handledTargetKeyRef.current === targetKey) {
      return;
    }

    const targetIndex = displayList.findIndex((item) => matchesTargetMessage(item, targetMessageId));
    if (targetIndex === -1) {
      return;
    }

    handledTargetKeyRef.current = targetKey;
    setHighlightedMessageId(targetMessageId);
    hideScrollButton();

    requestAnimationFrame(() => {
      const targetElement = document.getElementById(`message-${getProcessedItemAnchorId(displayList[targetIndex])}`);
      scrollElementIntoView(targetElement, {
        behavior: 'smooth',
        block: 'center',
      });
    });

    const timer = window.setTimeout(() => {
      setHighlightedMessageId((current) => (current === targetMessageId ? undefined : current));
    }, 2400);

    return () => window.clearTimeout(timer);
  }, [displayList, hideScrollButton, location.key, scrollElementIntoView, targetMessageId]);

  useEffect(() => {
    const handleMessageJump = (event: Event) => {
      const detail = (event as CustomEvent<ChatMessageJumpDetail>).detail;
      if (!detail || !detail.conversation_id) return;
      // detail.conversation_id arrives as a route/event string; coerce to the
      // numeric conversation id before comparing against the context id.
      if (!conversationContext?.conversation_id || Number(detail.conversation_id) !== conversationContext.conversation_id)
        return;

      const targetIndex = displayList.findIndex((item) => {
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
          `message-${getProcessedItemAnchorId(displayList[targetIndex])}`
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
  }, [conversationContext?.conversation_id, displayList, hideScrollButton, scrollElementIntoView]);

  // Click scroll button
  const handleScrollButtonClick = () => {
    hideScrollButton();
    scrollToBottom('smooth');
  };

  const renderTurnDisclosure = (item: ITurnProcessDisclosureVO, highlighted: boolean) => {
    const getDisclosureProcessItemState = (processItem: IRenderableItem): TurnDisclosureProcessState =>
      item.processItemStates[getProcessedItemAnchorId(processItem)] ?? getProcessItemState(processItem);

    return (
      <TurnProcessDisclosure
        item={item}
        highlighted={highlighted}
        renderProcessItem={(processItem, expansionControls) =>
          renderProcessTraceItem(
            processItem,
            'list',
            workspaceRoots,
            getDisclosureProcessItemState(processItem),
            expansionControls
          )
        }
        getProcessItemKey={getProcessedItemAnchorId}
        getProcessItemState={getDisclosureProcessItemState}
        getProcessItemLayoutKind={getProcessItemLayoutKind}
        getProcessItemCanExpandAll={isCompletedThinkingProcessItem}
      />
    );
  };

  const renderProcessReceipt = (item: IProcessReceiptVO, highlighted: boolean) => {
    return (
      <TurnProcessReceipt
        receipt={item}
        highlighted={highlighted}
        renderProcessItem={(processItem) => renderProcessTraceItem(processItem, 'receipt', workspaceRoots)}
      />
    );
  };

  const renderItem = (_index: number, item: (typeof displayList)[0]) => {
    const highlighted = matchesTargetMessage(item, highlightedMessageId);
    if ('type' in item && item.type === 'turn_process_disclosure') {
      return (
        <div
          key={item.id}
          id={`message-${getProcessedItemAnchorId(item)}`}
          data-testid='turn-process-disclosure'
          className='min-w-0 message-item px-8px m-t-10px max-w-full md:max-w-780px mx-auto turn_process_disclosure'
          style={highlighted ? highlightStyle : undefined}
        >
          {renderTurnDisclosure(item, highlighted)}
        </div>
      );
    }
    if ('type' in item && item.type === 'process_receipt') {
      return (
        <div
          key={item.id}
          id={`message-${getProcessedItemAnchorId(item)}`}
          data-testid='turn-process-receipt'
          className='min-w-0 message-item px-8px m-t-10px max-w-full md:max-w-780px mx-auto process_receipt'
          style={highlighted ? highlightStyle : undefined}
        >
          {renderProcessReceipt(item, highlighted)}
        </div>
      );
    }
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
          {renderProcessTraceItem(item, 'list', workspaceRoots)}
        </div>
      );
    }
    return (
      <MessageItem
        message={item as TMessage}
        key={(item as TMessage).id}
        highlighted={highlighted}
        hideActions={isActiveProcessTextItem(item, _index)}
      ></MessageItem>
    );
  };

  if (displayList.length === 0 && isMessageListLoading) {
    return <MessageListSkeleton />;
  }

  if (displayList.length === 0 && emptySlot) {
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
              {displayList.map((item, index) => (
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
