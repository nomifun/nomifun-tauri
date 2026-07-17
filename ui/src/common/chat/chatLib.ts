/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type {
  AcpPermissionRequest,
  PlanUpdate,
  ToolCallContentItem,
  ToolCallUpdate,
} from '@/common/types/platform/acpTypes';
import type { IKnowledgeWritebackEvent, IResponseMessage, IUserMessageCreatedEvent } from '../adapter/ipcBridge';
import {
  parseConversationId,
  parseCronJobId,
  parseKnowledgeBaseId,
  type ConversationId,
  type CronJobId,
  type KnowledgeBaseId,
  type MessageId,
} from '../types/ids';
import { uuid } from '../utils';
import { optionalDisplayText, toDisplayText } from './displayText';

declare const confirmationCorrelationBrand: unique symbol;

/** ACP confirmation correlation key; transient protocol identity, never a DB entity ID. */
export type ConfirmationCorrelationId = string & {
  readonly [confirmationCorrelationBrand]: true;
};

export const parseConfirmationCorrelationId = (value: unknown): ConfirmationCorrelationId => {
  if (typeof value !== 'string' || value.length === 0 || value.trim() !== value) {
    throw new TypeError('confirmation correlation id must be non-empty canonical text');
  }
  return value as ConfirmationCorrelationId;
};

/**
 * 安全的路径拼接函数，兼容Windows和Mac
 * @param basePath 基础路径
 * @param relativePath 相对路径
 * @returns 拼接后的绝对路径
 */
export const joinPath = (basePath: string, relativePath: string): string => {
  // 标准化路径分隔符为 /
  const normalizePath = (path: string) => path.replace(/\\/g, '/');

  const base = normalizePath(basePath);
  const relative = normalizePath(relativePath);

  // 去掉base路径末尾的斜杠
  const cleanBase = base.replace(/\/+$/, '');

  // 处理相对路径中的 ./ 和 ../
  const parts = relative.split('/');
  const resultParts = [];

  for (const part of parts) {
    if (part === '.' || part === '') {
      continue; // 跳过 . 和空字符串
    } else if (part === '..') {
      // 处理上级目录
      if (resultParts.length > 0) {
        resultParts.pop(); // 移除最后一个部分
      }
    } else {
      resultParts.push(part);
    }
  }

  // 拼接路径
  const result = cleanBase + '/' + resultParts.join('/');

  // 确保路径格式正确
  return result.replace(/\/+/g, '/'); // 将多个连续的斜杠替换为单个
};

/**
 * @description 跟对话相关的消息类型申明 及相关处理
 */

type TMessageType =
  | 'text'
  | 'tips'
  | 'tool_call'
  | 'tool_group'
  | 'agent_status'
  | 'permission'
  | 'acp_permission'
  | 'acp_tool_call'
  | 'plan'
  | 'thinking'
  | 'available_commands';

interface IMessage<T extends TMessageType, Content extends Record<string, any>> {
  /**
   * 唯一ID — frontend-local message key (uuid), NOT a backend entity id.
   */
  id: string;
  /**
   * 消息来源ID，— backend messages.id, stays TEXT (`msg_…`).
   */
  msg_id?: MessageId;

  /** Owning canonical Conversation entity id. */
  conversation_id: ConversationId;
  /**
   * 消息类型
   */
  type: T;
  /**
   * 消息内容
   */
  content: Content;
  /**
   * 消息创建时间
   */
  created_at?: number;
  /**
   * 消息位置
   */
  position?: 'left' | 'right' | 'center' | 'pop';
  /**
   * 消息状态
   */
  status?: 'finish' | 'pending' | 'error' | 'work';
  /**
   * Hidden from UI display but persisted to DB and sent to agent.
   */
  hidden?: boolean;
}

export type CronMessageMeta = {
  source: 'cron';
  cron_job_id: CronJobId;
  cron_job_name: string;
  triggered_at: number;
};

export type KnowledgeWritebackStatus =
  | 'started'
  | 'extracting'
  | 'writing'
  | 'written'
  | 'partial'
  | 'failed'
  | 'no_candidate'
  | 'no_completer'
  | 'disabled'
  | 'interrupted';

export type KnowledgeWritebackFile = {
  kb_id?: KnowledgeBaseId | null;
  rel_path?: string | null;
  staged?: boolean;
};

export type KnowledgeWritebackFailure = {
  kb_id?: KnowledgeBaseId | null;
  rel_path?: string | null;
  error?: string;
};

export type KnowledgeWritebackState = {
  status: KnowledgeWritebackStatus;
  attempt_id?: string;
  started_at?: number;
  updated_at?: number;
  finished_at?: number | null;
  retryable?: boolean;
  candidates?: number;
  written?: KnowledgeWritebackFile[];
  failures?: KnowledgeWritebackFailure[];
  interrupted_at?: number;
};

export type IMessageText = IMessage<
  'text',
  {
    content: string;
    /** Backend explicitly replaced the accumulated text for this msg_id. */
    replace?: boolean;
    cronMeta?: CronMessageMeta;
    /** True when this reply was sent by another Agent participating in the task. */
    agentMessage?: boolean;
    senderName?: string;
    senderAgentType?: string;
    /** Sender Agent's conversation id — lets the renderer resolve preset avatars via conversation extras. */
    senderConversationId?: ConversationId;
    /** Turn-final knowledge write-back state, rendered under the assistant message. */
    knowledge_writeback?: KnowledgeWritebackState;
  }
>;

export type AgentErrorOwnership = 'nomifun' | 'user_agent' | 'user_llm_provider' | 'unknown_upstream';

export type AgentErrorResolutionKind =
  | 'retry'
  | 'wait_for_current_response'
  | 'start_new_session'
  | 'reconnect_agent'
  | 'check_agent_login'
  | 'check_agent_installation'
  | 'check_agent_version'
  | 'check_local_command'
  | 'check_provider_credentials'
  | 'check_provider_billing'
  | 'check_provider_base_url'
  | 'change_model'
  | 'reduce_context'
  | 'send_feedback';

export type AgentErrorResolutionTarget = 'provider_settings' | 'agent_settings' | 'new_conversation' | 'feedback';

export type AgentErrorResolution = {
  kind: AgentErrorResolutionKind;
  target?: AgentErrorResolutionTarget;
};

export type AgentStreamErrorInfo = {
  message: string;
  code?: string;
  ownership?: AgentErrorOwnership;
  detail?: string;
  workspacePath?: string;
  retryable?: boolean;
  feedback_recommended?: boolean;
  resolution?: AgentErrorResolution;
};

export type IMessageTips = IMessage<
  'tips',
  {
    content: string;
    type: 'error' | 'success' | 'warning';
    error?: AgentStreamErrorInfo;
  }
>;

export type IMessageToolCall = IMessage<
  'tool_call',
  {
    call_id: string;
    name: string;
    /**
     * Provider arguments when they were decoded as a JSON object. Local
     * pre-execution validation failures can legitimately persist `null` here
     * because no valid argument object reached the tool.
     */
    args?: Record<string, unknown> | null;
    error?: string;
    status?: 'running' | 'completed' | 'error';
    input?: Record<string, unknown>;
    output?: string;
    description?: string;
  }
>;

type IMessageToolGroupConfirmationDetailsBase<Type, Extra extends Record<string, any>> = {
  type: Type;
  title: string;
} & Extra;

export type IMessageToolGroup = IMessage<
  'tool_group',
  Array<{
    call_id: string;
    description: string;
    name: string;
    render_output_as_markdown: boolean;
    result_display?:
      | string
      | {
          file_diff: string;
          file_name: string;
        }
      | {
          img_url: string;
          relative_path: string;
        };
    status: 'Executing' | 'Success' | 'Error' | 'Canceled' | 'Pending' | 'Confirming';
    confirmationDetails?:
      | IMessageToolGroupConfirmationDetailsBase<
          'edit',
          {
            file_name: string;
            file_diff: string;
            isModifying?: boolean;
          }
        >
      | IMessageToolGroupConfirmationDetailsBase<
          'exec',
          {
            rootCommand: string;
            command: string;
          }
        >
      | IMessageToolGroupConfirmationDetailsBase<
          'info',
          {
            urls?: string[];
            prompt: string;
          }
        >
      | IMessageToolGroupConfirmationDetailsBase<
          'mcp',
          {
            tool_name: string;
            tool_display_name: string;
            server_name: string;
          }
        >;
  }>
>;

// Unified agent status message type for all ACP-based agents (Claude, Qwen, Codex, etc.)
export type IMessageAgentStatus = IMessage<
  'agent_status',
  {
    backend: string; // Agent identifier: 'claude', 'qwen', 'codex', 'remote', etc.
    status:
      | 'connecting'
      | 'connected'
      | 'authenticated'
      | 'session_active'
      | 'preparing'
      | 'prepared'
      | 'disconnected'
      | 'error';
    /** Display name for the agent (e.g. extension-contributed adapter name) / Agent 显示名称 */
    agent_name?: string;
    // Optional runtime metadata supplied by some ACP agents.
    session_id?: string;
    is_connected?: boolean;
    has_active_session?: boolean;
  }
>;

export type IMessageAcpPermission = IMessage<'acp_permission', AcpPermissionRequest>;

export type IMessagePermission = IMessage<'permission', IConfirmation>;

export type IMessageAcpToolCall = IMessage<'acp_tool_call', ToolCallUpdate>;

export const mergeAcpToolCallContent = (
  existing: IMessageAcpToolCall['content'],
  incoming: IMessageAcpToolCall['content']
): IMessageAcpToolCall['content'] => ({
  ...existing,
  ...incoming,
  update: {
    ...existing.update,
    ...incoming.update,
  },
});

type ResponseTextData = {
  content: unknown;
  replace?: boolean;
  cronMeta?: CronMessageMeta;
  knowledge_writeback?: unknown;
  teammate_message?: unknown;
  sender_name?: unknown;
  sender_backend?: unknown;
  /**
   * Untrusted ACP wire field. It is validated into `ConversationId` by
   * `normalizeWireAgentMessageMetadata` before entering renderer state.
   */
  sender_conversation_id?: string;
};

type AgentMessageMetadata = Pick<
  IMessageText['content'],
  'agentMessage' | 'senderName' | 'senderAgentType' | 'senderConversationId'
>;

const normalizeCronMessageMeta = (value: unknown): CronMessageMeta | undefined => {
  if (!isObject(value) || value.source !== 'cron') return undefined;
  if (typeof value.cron_job_name !== 'string' || typeof value.triggered_at !== 'number') return undefined;
  return {
    source: value.source,
    cron_job_id: parseCronJobId(value.cron_job_id),
    cron_job_name: value.cron_job_name,
    triggered_at: value.triggered_at,
  };
};

/** External ACP event name; normalized immediately at the message boundary. */
export const ACP_AGENT_MESSAGE_EVENT = 'teammate_message' as const;

/**
 * Translate the external ACP collaboration wire fields into the renderer's
 * single Agent message shape. The legacy protocol name must not propagate
 * beyond message-ingress adapters.
 */
export const normalizeWireAgentMessageMetadata = (
  data: Record<string, unknown>
): Partial<AgentMessageMetadata> => {
  let senderConversationId: ConversationId | undefined;
  if (typeof data.sender_conversation_id === 'string') {
    try {
      senderConversationId = parseConversationId(data.sender_conversation_id);
    } catch {
      // Malformed external metadata must not poison an otherwise valid message.
      senderConversationId = undefined;
    }
  }
  return {
    ...(data.teammate_message ? { agentMessage: true } : {}),
    ...(typeof data.sender_name === 'string' ? { senderName: data.sender_name } : {}),
    ...(typeof data.sender_backend === 'string' ? { senderAgentType: data.sender_backend } : {}),
    ...(senderConversationId ? { senderConversationId } : {}),
  };
};

const isObject = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

const KNOWLEDGE_WRITEBACK_STATUSES = new Set<KnowledgeWritebackStatus>([
  'started',
  'extracting',
  'writing',
  'written',
  'partial',
  'failed',
  'no_candidate',
  'no_completer',
  'disabled',
  'interrupted',
]);

const normalizeKnowledgeWritebackFiles = (value: unknown): KnowledgeWritebackFile[] | undefined => {
  if (!Array.isArray(value)) return undefined;
  return value
    .filter(isObject)
    .map((file) => ({
      ...(file.kb_id === null
        ? { kb_id: null }
        : typeof file.kb_id === 'string'
          ? { kb_id: parseKnowledgeBaseId(file.kb_id) }
          : {}),
      ...(typeof file.rel_path === 'string' || file.rel_path === null ? { rel_path: file.rel_path } : {}),
      ...(typeof file.staged === 'boolean' ? { staged: file.staged } : {}),
    }));
};

const normalizeKnowledgeWritebackFailures = (value: unknown): KnowledgeWritebackFailure[] | undefined => {
  if (!Array.isArray(value)) return undefined;
  return value
    .filter(isObject)
    .map((failure) => ({
      ...(failure.kb_id === null
        ? { kb_id: null }
        : typeof failure.kb_id === 'string'
          ? { kb_id: parseKnowledgeBaseId(failure.kb_id) }
          : {}),
      ...(typeof failure.rel_path === 'string' || failure.rel_path === null ? { rel_path: failure.rel_path } : {}),
      ...(typeof failure.error === 'string' ? { error: failure.error } : {}),
    }));
};

export const normalizeKnowledgeWritebackState = (value: unknown): KnowledgeWritebackState | undefined => {
  if (!isObject(value) || typeof value.status !== 'string') return undefined;
  if (!KNOWLEDGE_WRITEBACK_STATUSES.has(value.status as KnowledgeWritebackStatus)) return undefined;
  const written = normalizeKnowledgeWritebackFiles(value.written);
  const failures = normalizeKnowledgeWritebackFailures(value.failures);
  return {
    status: value.status as KnowledgeWritebackStatus,
    ...(typeof value.attempt_id === 'string' ? { attempt_id: value.attempt_id } : {}),
    ...(typeof value.started_at === 'number' ? { started_at: value.started_at } : {}),
    ...(typeof value.updated_at === 'number' ? { updated_at: value.updated_at } : {}),
    ...(typeof value.finished_at === 'number' || value.finished_at === null ? { finished_at: value.finished_at } : {}),
    ...(typeof value.retryable === 'boolean' ? { retryable: value.retryable } : {}),
    ...(typeof value.candidates === 'number' ? { candidates: value.candidates } : {}),
    ...(written ? { written } : {}),
    ...(failures ? { failures } : {}),
    ...(typeof value.interrupted_at === 'number' ? { interrupted_at: value.interrupted_at } : {}),
  };
};

const knowledgeWritebackTime = (state: KnowledgeWritebackState | undefined): number | undefined => {
  if (!state) return undefined;
  return state.updated_at ?? state.finished_at ?? state.interrupted_at ?? state.started_at;
};

const preferKnowledgeWritebackState = (
  existing: KnowledgeWritebackState | undefined,
  incoming: KnowledgeWritebackState | undefined
): KnowledgeWritebackState | undefined => {
  if (!existing) return incoming;
  if (!incoming) return existing;
  const existingTime = knowledgeWritebackTime(existing);
  const incomingTime = knowledgeWritebackTime(incoming);
  if (existingTime === undefined && incomingTime === undefined) return incoming;
  if (existingTime === undefined) return incoming;
  if (incomingTime === undefined) return existing;
  return incomingTime >= existingTime ? incoming : existing;
};

const isResponseTextData = (data: unknown): data is ResponseTextData =>
  typeof data === 'object' &&
  data !== null &&
  'content' in data &&
  !Array.isArray(data);

export const isTextContentReplacement = (content: IMessageText['content'] | undefined): boolean =>
  content?.replace === true;

export const mergeTextMessageContent = (
  existing: IMessageText['content'],
  incoming: IMessageText['content']
): IMessageText['content'] => {
  const { replace: _existingReplace, knowledge_writeback: existingWriteback, ...existingRest } = existing;
  const { replace: incomingReplace, knowledge_writeback: incomingWriteback, ...incomingRest } = incoming;
  const knowledgeWriteback = preferKnowledgeWritebackState(existingWriteback, incomingWriteback);

  return {
    ...existingRest,
    ...incomingRest,
    content: incomingReplace ? incoming.content : existing.content + incoming.content,
    ...(incomingReplace ? { replace: true } : {}),
    ...(knowledgeWriteback ? { knowledge_writeback: knowledgeWriteback } : {}),
  };
};

export const preferTextMessageVersion = (primary: IMessageText, secondary: IMessageText): IMessageText => {
  const primaryIsReplace = isTextContentReplacement(primary.content);
  const secondaryIsReplace = isTextContentReplacement(secondary.content);
  const mergePreferredWriteback = (preferred: IMessageText, fallback: IMessageText): IMessageText => {
    const knowledgeWriteback = preferKnowledgeWritebackState(
      fallback.content.knowledge_writeback,
      preferred.content.knowledge_writeback
    );
    if (!knowledgeWriteback) return preferred;
    return {
      ...preferred,
      content: {
        ...preferred.content,
        knowledge_writeback: knowledgeWriteback,
      },
    };
  };

  if (primaryIsReplace !== secondaryIsReplace) {
    return primaryIsReplace ? mergePreferredWriteback(primary, secondary) : mergePreferredWriteback(secondary, primary);
  }

  return secondary.content.content.length > primary.content.content.length
    ? mergePreferredWriteback(secondary, primary)
    : mergePreferredWriteback(primary, secondary);
};

export type IMessagePlan = IMessage<
  'plan',
  {
    session_id: string;
    entries: PlanUpdate['update']['entries'];
  }
>;

export type IMessageThinking = IMessage<
  'thinking',
  {
    content: string;
    subject?: string;
    duration?: number;
    status: 'thinking' | 'done';
  }
>;

// Available commands from ACP agents (Claude, etc.)
export type AvailableCommand = {
  name: string;
  description: string;
  hint?: string;
};

export type IMessageAvailableCommands = IMessage<
  'available_commands',
  {
    commands: AvailableCommand[];
  }
>;

// eslint-disable-next-line max-len
export type TMessage =
  | IMessageText
  | IMessageTips
  | IMessageToolCall
  | IMessageToolGroup
  | IMessageAgentStatus
  | IMessagePermission
  | IMessageAcpPermission
  | IMessageAcpToolCall
  | IMessagePlan
  | IMessageThinking
  | IMessageAvailableCommands;

// 统一所有需要用户交互的用户类型
export interface IConfirmation<Option extends any = any> {
  title?: string;
  id: string;
  action?: string;
  description: string;
  call_id: string;
  options: Array<{
    label: string;
    value: Option;
    params?: Record<string, string>; // Translation interpolation parameters
  }>;
  /**
   * Command type for exec confirmations (e.g., 'curl', 'npm', 'git')
   * Used for "always allow" permission memory
   */
  command_type?: string;
  /**
   * Optional inline page preview (`data:image/png;base64,...`) for a browser
   * takeover approval. Lets a silent (headless) session show the user the current
   * page they're approving an irreversible action on. Absent for non-browser prompts.
   */
  screenshot?: string;
}

const AGENT_ERROR_OWNERSHIPS = new Set<AgentErrorOwnership>([
  'nomifun',
  'user_agent',
  'user_llm_provider',
  'unknown_upstream',
]);

const AGENT_ERROR_RESOLUTION_KINDS = new Set<AgentErrorResolutionKind>([
  'retry',
  'wait_for_current_response',
  'start_new_session',
  'reconnect_agent',
  'check_agent_login',
  'check_agent_installation',
  'check_agent_version',
  'check_local_command',
  'check_provider_credentials',
  'check_provider_billing',
  'check_provider_base_url',
  'change_model',
  'reduce_context',
  'send_feedback',
]);

const AGENT_ERROR_RESOLUTION_TARGETS = new Set<AgentErrorResolutionTarget>([
  'provider_settings',
  'agent_settings',
  'new_conversation',
  'feedback',
]);

export const normalizeAgentErrorResolution = (value: unknown): AgentErrorResolution | undefined => {
  if (!isObject(value) || typeof value.kind !== 'string') {
    return undefined;
  }

  if (!AGENT_ERROR_RESOLUTION_KINDS.has(value.kind as AgentErrorResolutionKind)) {
    return undefined;
  }

  const target =
    typeof value.target === 'string' && AGENT_ERROR_RESOLUTION_TARGETS.has(value.target as AgentErrorResolutionTarget)
      ? (value.target as AgentErrorResolutionTarget)
      : undefined;

  return {
    kind: value.kind as AgentErrorResolutionKind,
    ...(target ? { target } : {}),
  };
};

export const normalizeAgentStreamError = (value: unknown): AgentStreamErrorInfo | undefined => {
  if (!isObject(value) || typeof value.message !== 'string') {
    return undefined;
  }

  const code = typeof value.code === 'string' ? value.code : undefined;
  const ownership =
    typeof value.ownership === 'string' && AGENT_ERROR_OWNERSHIPS.has(value.ownership as AgentErrorOwnership)
      ? (value.ownership as AgentErrorOwnership)
      : undefined;
  const detail = typeof value.detail === 'string' ? value.detail : undefined;
  const workspacePath = typeof value.workspacePath === 'string' ? value.workspacePath : undefined;
  const retryable = typeof value.retryable === 'boolean' ? value.retryable : undefined;
  const feedback_recommended = typeof value.feedback_recommended === 'boolean' ? value.feedback_recommended : undefined;
  const resolution = normalizeAgentErrorResolution(value.resolution);

  if (
    !code &&
    !ownership &&
    !detail &&
    !workspacePath &&
    retryable === undefined &&
    feedback_recommended === undefined &&
    !resolution
  ) {
    return undefined;
  }

  return {
    message: value.message,
    ...(code ? { code } : {}),
    ...(ownership ? { ownership } : {}),
    ...(detail ? { detail } : {}),
    ...(workspacePath ? { workspacePath } : {}),
    ...(retryable !== undefined ? { retryable } : {}),
    ...(feedback_recommended !== undefined ? { feedback_recommended } : {}),
    ...(resolution ? { resolution } : {}),
  };
};

const normalizeTipType = (value: unknown): IMessageTips['content']['type'] =>
  value === 'success' || value === 'warning' || value === 'error' ? value : 'warning';

const normalizeThinkingStatus = (value: unknown): IMessageThinking['content']['status'] =>
  value === 'done' ? 'done' : 'thinking';

const finiteNumber = (value: unknown): number | undefined =>
  typeof value === 'number' && Number.isFinite(value) ? value : undefined;

const normalizeToolGroupStatus = (value: unknown): IMessageToolGroup['content'][number]['status'] => {
  switch (value) {
    case 'Success':
    case 'Error':
    case 'Canceled':
    case 'Pending':
    case 'Confirming':
    case 'Executing':
      return value;
    default:
      return 'Executing';
  }
};

const normalizeToolGroupResultDisplay = (
  value: unknown
): IMessageToolGroup['content'][number]['result_display'] | undefined => {
  if (value == null) return undefined;
  if (typeof value === 'string') return value;
  if (!isObject(value)) return toDisplayText(value);

  if ('file_diff' in value || 'file_name' in value) {
    return {
      file_diff: toDisplayText(value.file_diff),
      file_name: toDisplayText(value.file_name),
    };
  }
  if ('img_url' in value || 'relative_path' in value) {
    return {
      img_url: toDisplayText(value.img_url),
      relative_path: toDisplayText(value.relative_path),
    };
  }

  return toDisplayText(value);
};

const normalizeToolGroupConfirmationDetails = (
  value: unknown
): IMessageToolGroup['content'][number]['confirmationDetails'] | undefined => {
  if (!isObject(value)) return undefined;
  const type = value.type;
  const title = toDisplayText(value.title);

  if (type === 'edit') {
    return {
      type,
      title,
      file_name: toDisplayText(value.file_name),
      file_diff: toDisplayText(value.file_diff),
      ...(typeof value.isModifying === 'boolean' ? { isModifying: value.isModifying } : {}),
    };
  }
  if (type === 'exec') {
    return {
      type,
      title,
      rootCommand: toDisplayText(value.rootCommand),
      command: toDisplayText(value.command),
    };
  }
  if (type === 'info') {
    return {
      type,
      title,
      prompt: toDisplayText(value.prompt),
      ...(Array.isArray(value.urls) ? { urls: value.urls.map((url) => toDisplayText(url)) } : {}),
    };
  }
  if (type === 'mcp') {
    return {
      type,
      title,
      tool_name: toDisplayText(value.tool_name),
      tool_display_name: toDisplayText(value.tool_display_name),
      server_name: toDisplayText(value.server_name),
    };
  }

  return undefined;
};

const normalizeToolGroupContent = (value: unknown): IMessageToolGroup['content'] => {
  if (!Array.isArray(value)) return [];

  return value
    .filter(isObject)
    .map((item) => {
      const resultDisplay = normalizeToolGroupResultDisplay(item.result_display);
      const confirmationDetails = normalizeToolGroupConfirmationDetails(item.confirmationDetails);
      return {
        call_id: optionalDisplayText(item.call_id) ?? optionalDisplayText(item.id) ?? uuid(),
        description: toDisplayText(item.description),
        name: toDisplayText(item.name, 'Tool'),
        render_output_as_markdown:
          typeof item.render_output_as_markdown === 'boolean' ? item.render_output_as_markdown : false,
        status: normalizeToolGroupStatus(item.status),
        ...(resultDisplay !== undefined ? { result_display: resultDisplay } : {}),
        ...(confirmationDetails ? { confirmationDetails } : {}),
      };
    });
};

const normalizePermissionParams = (params: unknown): Record<string, string> | undefined => {
  if (!isObject(params)) return undefined;
  return Object.fromEntries(Object.entries(params).map(([key, value]) => [key, toDisplayText(value)]));
};

const normalizePermissionContent = (value: unknown): IConfirmation => {
  const data = isObject(value) ? value : {};
  const options = Array.isArray(data.options)
    ? data.options.filter(isObject).map((option, index) => {
        const params = normalizePermissionParams(option.params);
        return {
          label: toDisplayText(option.label, `Option ${index + 1}`),
          value: option.value,
          ...(params ? { params } : {}),
        };
      })
    : [];

  return {
    id: toDisplayText(data.id, uuid()),
    description: toDisplayText(data.description ?? data.title, ''),
    call_id: toDisplayText(data.call_id ?? data.id, ''),
    options,
    ...(data.title != null ? { title: toDisplayText(data.title) } : {}),
    ...(data.action != null ? { action: toDisplayText(data.action) } : {}),
    ...(data.command_type != null ? { command_type: toDisplayText(data.command_type) } : {}),
    ...(data.screenshot != null ? { screenshot: toDisplayText(data.screenshot) } : {}),
  };
};

const normalizeAcpPermissionOptionKind = (
  value: unknown
): AcpPermissionRequest['options'][number]['kind'] => {
  switch (value) {
    case 'allow_once':
    case 'allow_always':
    case 'reject_once':
    case 'reject_always':
      return value;
    default:
      return 'allow_once';
  }
};

const normalizeAcpPermissionContent = (value: unknown): AcpPermissionRequest => {
  const data = isObject(value) ? value : {};
  const toolCall = isObject(data.tool_call) ? data.tool_call : {};
  const rawInput = isObject(toolCall.raw_input)
    ? Object.fromEntries(Object.entries(toolCall.raw_input).map(([key, entry]) => [key, entry]))
    : undefined;

  return {
    session_id: toDisplayText(data.session_id),
    options: Array.isArray(data.options)
      ? data.options.filter(isObject).map((option, index) => ({
          option_id: toDisplayText(option.option_id, `option_${index}`),
          name: toDisplayText(option.name ?? option.label, `Option ${index + 1}`),
          kind: normalizeAcpPermissionOptionKind(option.kind),
        }))
      : [],
    tool_call: {
      tool_call_id: toDisplayText(toolCall.tool_call_id ?? data.call_id),
      ...(rawInput ? { raw_input: rawInput } : {}),
      ...(toolCall.status != null ? { status: toDisplayText(toolCall.status) } : {}),
      ...(toolCall.title != null ? { title: toDisplayText(toolCall.title) } : {}),
      ...(toolCall.kind != null ? { kind: toDisplayText(toolCall.kind) } : {}),
    },
  };
};

const normalizeAcpToolCallContent = (value: unknown): IMessageAcpToolCall['content'] => {
  if (!isObject(value)) return value as IMessageAcpToolCall['content'];
  const update = isObject(value.update) ? value.update : undefined;
  if (!update) return value as unknown as IMessageAcpToolCall['content'];
  const content = Array.isArray(update.content)
    ? update.content.filter(isObject).map((item): ToolCallContentItem => {
        const normalized: ToolCallContentItem = {
          ...(item as unknown as ToolCallContentItem),
          type: item.type === 'diff' ? 'diff' : 'content',
          ...(item.path != null ? { path: toDisplayText(item.path) } : {}),
          ...(item.old_text != null ? { old_text: toDisplayText(item.old_text) } : {}),
          ...(item.new_text != null ? { new_text: toDisplayText(item.new_text) } : {}),
        };
        if (isObject(item.content)) {
          normalized.content = {
            type: 'text',
            text: toDisplayText(item.content.text),
          };
        }
        return normalized;
      })
    : undefined;

  return ({
    ...value,
    update: {
      ...update,
      tool_call_id: toDisplayText(update.tool_call_id),
      status: toDisplayText(update.status, 'pending') as IMessageAcpToolCall['content']['update']['status'],
      title: toDisplayText(update.title, 'Tool'),
      kind: toDisplayText(update.kind, 'execute') as IMessageAcpToolCall['content']['update']['kind'],
      ...(content ? { content } : {}),
    },
  } as unknown) as IMessageAcpToolCall['content'];
};

const normalizeAgentStatusContent = (value: unknown): IMessageAgentStatus['content'] => {
  const data = isObject(value) ? value : {};
  const status =
    data.status === 'connecting' ||
    data.status === 'connected' ||
    data.status === 'authenticated' ||
    data.status === 'session_active' ||
    data.status === 'preparing' ||
    data.status === 'prepared' ||
    data.status === 'disconnected' ||
    data.status === 'error'
      ? data.status
      : 'error';

  return {
    backend: toDisplayText(data.backend, 'agent'),
    status,
    ...(data.agent_name != null ? { agent_name: toDisplayText(data.agent_name) } : {}),
    ...(data.session_id != null ? { session_id: toDisplayText(data.session_id) } : {}),
    ...(typeof data.is_connected === 'boolean' ? { is_connected: data.is_connected } : {}),
    ...(typeof data.has_active_session === 'boolean' ? { has_active_session: data.has_active_session } : {}),
  };
};

/**
 * @description 将后端返回的消息转换为前端消息
 * */
export const transformMessage = (message: IResponseMessage): TMessage | undefined => {
  const created_at = message.created_at ?? Date.now();
  switch (message.type) {
    case 'error': {
      const errorData = message.data;
      const structuredError = normalizeAgentStreamError(errorData);
      const errorText =
        (isObject(errorData) ? optionalDisplayText(errorData.message) : undefined) ?? toDisplayText(errorData);
      return {
        id: uuid(),
        type: 'tips',
        msg_id: message.msg_id,
        position: 'center',
        conversation_id: message.conversation_id,
        created_at,
        content: {
          content: errorText,
          type: 'error',
          ...(structuredError ? { error: structuredError } : {}),
        },
      };
    }
    case 'tips': {
      const data = isObject(message.data) ? message.data : { content: message.data };
      const content = toDisplayText(data.content);
      const tipType = normalizeTipType(data.type);
      const structuredError =
        tipType === 'error'
          ? (normalizeAgentStreamError(data.error) ?? normalizeAgentStreamError({ ...data, message: content }))
          : undefined;
      return {
        id: uuid(),
        type: 'tips',
        msg_id: message.msg_id,
        position: 'center',
        conversation_id: message.conversation_id,
        created_at,
        content: {
          content,
          type: tipType,
          ...(structuredError ? { error: structuredError } : {}),
        },
      };
    }
    case 'text':
    case 'content':
    case 'user_content': {
      const data = message.data;
      const isRichData = isResponseTextData(data);
      const shouldReplace = message.replace === true || (isRichData && data.replace === true);
      const persistedWriteback = isRichData ? normalizeKnowledgeWritebackState(data.knowledge_writeback) : undefined;
      return {
        id: uuid(),
        type: 'text',
        msg_id: message.msg_id,
        position: message.type === 'user_content' ? 'right' : 'left',
        conversation_id: message.conversation_id,
        created_at,
        content: isRichData
          ? {
              content: toDisplayText(data.content),
              cronMeta: normalizeCronMessageMeta(data.cronMeta),
              ...(shouldReplace ? { replace: true } : {}),
              ...(persistedWriteback ? { knowledge_writeback: persistedWriteback } : {}),
              ...normalizeWireAgentMessageMetadata(data as Record<string, unknown>),
            }
          : {
              content: toDisplayText(data),
              ...(shouldReplace ? { replace: true } : {}),
            },
        ...(message.hidden && { hidden: true }),
      };
    }
    case 'tool_call': {
      return {
        id: uuid(),
        type: 'tool_call',
        msg_id: message.msg_id,
        conversation_id: message.conversation_id,
        position: 'left',
        created_at,
        content: message.data as any,
      };
    }
    case 'tool_group': {
      return {
        type: 'tool_group',
        id: uuid(),
        msg_id: message.msg_id,
        conversation_id: message.conversation_id,
        created_at,
        content: normalizeToolGroupContent(message.data),
      };
    }
    case 'agent_status': {
      return {
        id: uuid(),
        type: 'agent_status',
        msg_id: message.msg_id,
        position: 'center',
        conversation_id: message.conversation_id,
        created_at,
        content: normalizeAgentStatusContent(message.data),
      };
    }
    case 'permission': {
      return {
        id: uuid(),
        type: 'permission',
        msg_id: message.msg_id,
        position: 'left',
        conversation_id: message.conversation_id,
        created_at,
        content: normalizePermissionContent(message.data),
      };
    }
    case 'acp_permission': {
      return {
        id: uuid(),
        type: 'acp_permission',
        msg_id: message.msg_id,
        position: 'left',
        conversation_id: message.conversation_id,
        created_at,
        content: normalizeAcpPermissionContent(message.data),
      };
    }
    case 'acp_tool_call': {
      return {
        id: uuid(),
        type: 'acp_tool_call',
        msg_id: message.msg_id,
        position: 'left',
        conversation_id: message.conversation_id,
        created_at,
        content: normalizeAcpToolCallContent(message.data),
      };
    }
    case 'plan': {
      return {
        id: uuid(),
        type: 'plan',
        msg_id: message.msg_id,
        position: 'left',
        conversation_id: message.conversation_id,
        created_at,
        content: message.data as any,
      };
    }
    case 'thinking': {
      const data = isObject(message.data) ? message.data : { content: message.data };
      const duration = finiteNumber(data.duration) ?? finiteNumber(data.duration_ms);
      return {
        id: uuid(),
        type: 'thinking',
        msg_id: message.msg_id,
        position: 'left',
        conversation_id: message.conversation_id,
        created_at,
        content: {
          content: toDisplayText(data.content),
          ...(data.subject != null ? { subject: toDisplayText(data.subject) } : {}),
          ...(duration !== undefined ? { duration } : {}),
          status: normalizeThinkingStatus(data.status),
        },
      };
    }
    // Disabled: available_commands messages are too noisy and distracting in the chat UI
    case 'available_commands':
      return undefined;
    case 'start':
    case 'finish':
    case 'thought':
    case 'skill_suggest':
    case 'cron_trigger':
    case 'info': // Stream retry notifications and similar transient agent updates
    case 'system': // Cron system responses, ignored
    case 'acp_model_info': // Model info updates, handled by AcpModelSelector
    case 'codex_model_info': // Legacy Codex model info updates
    case 'acp_context_usage': // Context usage updates, handled by AcpSendBox
    case 'request_trace': // Request trace events, logged to F12 console (not persisted)
      return undefined;
    default: {
      console.warn(
        `[transformMessage] Unsupported message type '${message.type}'. All non-standard message types should be pre-processed by respective AgentManagers.`
      );
      return undefined;
    }
  }
};

export const transformKnowledgeWritebackEvent = (event: IKnowledgeWritebackEvent): IMessageText | undefined => {
  if (!event.msg_id) return undefined;
  return {
    id: uuid(),
    type: 'text',
    msg_id: event.msg_id,
    position: 'left',
    conversation_id: event.conversation_id,
    content: {
      content: '',
      knowledge_writeback: {
        status: event.status,
        attempt_id: event.attempt_id,
        started_at: event.started_at,
        updated_at: event.updated_at,
        finished_at: event.finished_at,
        retryable: event.retryable,
        candidates: event.candidates,
        written: event.written,
        failures: event.failures,
      },
    },
  };
};

const normalizeMessageStatus = (value: string | undefined): TMessage['status'] => {
  if (value === 'finish' || value === 'pending' || value === 'error' || value === 'work') return value;
  return 'finish';
};

export const transformUserCreatedEvent = (
  event: IUserMessageCreatedEvent,
  conversationId: ConversationId
): IMessageText | undefined => {
  if (event.hidden || event.conversation_id !== conversationId || !event.msg_id) return undefined;
  return {
    id: event.msg_id,
    type: 'text',
    msg_id: event.msg_id,
    position: 'right',
    status: normalizeMessageStatus(event.status),
    conversation_id: event.conversation_id,
    created_at: event.created_at,
    content: {
      content: event.content,
    },
  };
};

/**
 * @description 将消息合并到消息列表中
 * */
export const composeMessage = (
  message: TMessage | undefined,
  list: TMessage[] | undefined,
  messageHandler: (type: 'update' | 'insert', message: TMessage) => void = () => {}
): TMessage[] => {
  if (!message) return list || [];
  if (!list?.length) {
    messageHandler('insert', message);
    return [message];
  }
  const last = list[list.length - 1];

  const updateMessage = (index: number, message: TMessage, change = true) => {
    message.id = list[index].id;
    list[index] = message;
    if (change) messageHandler('update', message);
    return list.slice();
  };
  const pushMessage = (message: TMessage) => {
    list.push(message);
    messageHandler('insert', message);
    return list.slice();
  };

  if (message.type === 'tool_group') {
    if (!Array.isArray(message.content)) return list;
    const remainingToolsMap = new Map(message.content.map((t) => [t.call_id, t] as const));
    if (remainingToolsMap.size === 0) return list;

    const updatesToReport: TMessage[] = [];

    const updatedList = list.map((existingMessage) => {
      if (existingMessage.type !== 'tool_group') return existingMessage;
      if (existingMessage.msg_id !== message.msg_id) return existingMessage;
      if (!existingMessage.content.length) return existingMessage;

      let didMergeIntoThisMessage = false;
      const new_content = existingMessage.content.map((tool) => {
        const newToolData = remainingToolsMap.get(tool.call_id);
        if (!newToolData) return tool;
        didMergeIntoThisMessage = true;
        remainingToolsMap.delete(tool.call_id);
        // Create new object instead of mutating original
        return { ...tool, ...newToolData };
      });

      if (!didMergeIntoThisMessage) return existingMessage;
      const updatedMessage = { ...existingMessage, content: new_content } as TMessage;
      updatesToReport.push(updatedMessage);
      return updatedMessage;
    });

    const didUpdateExisting = updatesToReport.length > 0;
    for (const updatedMessage of updatesToReport) {
      messageHandler('update', updatedMessage);
    }

    const baseList = didUpdateExisting ? updatedList : list;

    // If there are new tool calls, append them as a new tool_group message (without mutating inputs)
    if (remainingToolsMap.size > 0) {
      const newTools = Array.from(remainingToolsMap.values());
      const insertMessage = { ...message, content: newTools } as TMessage;
      messageHandler('insert', insertMessage);
      return baseList.concat(insertMessage);
    }
    // No new tools appended; return a new list only if something was updated
    return didUpdateExisting ? baseList : list;
  }

  // Handle Gemini tool_call message merging
  if (message.type === 'tool_call') {
    for (let i = 0, len = list.length; i < len; i++) {
      const msg = list[i];
      if (
        msg.type === 'tool_call' &&
        msg.msg_id === message.msg_id &&
        msg.content.call_id === message.content.call_id
      ) {
        // Create new object instead of mutating original
        return updateMessage(i, { ...msg, ...message, content: { ...msg.content, ...message.content } });
      }
    }
    // If no existing tool call found, add new one
    return pushMessage(message);
  }

  // Handle acp_tool_call message merging
  if (message.type === 'acp_tool_call') {
    for (let i = 0, len = list.length; i < len; i++) {
      const msg = list[i];
      if (
        msg.type === 'acp_tool_call' &&
        msg.msg_id === message.msg_id &&
        msg.content.update?.tool_call_id === message.content.update?.tool_call_id
      ) {
        // Create new object instead of mutating original
        const merged = mergeAcpToolCallContent(msg.content, message.content);
        return updateMessage(i, { ...msg, ...message, content: merged });
      }
    }
    // If no existing tool call found, add new one
    return pushMessage(message);
  }

  if (message.type === 'plan') {
    for (let i = 0, len = list.length; i < len; i++) {
      const msg = list[i];
      if (msg.type === 'plan' && msg.content.session_id === message.content.session_id) {
        // Create new object instead of mutating original
        const merged = { ...msg.content, ...message.content };
        return updateMessage(i, { ...msg, content: merged });
      }
    }
    return pushMessage(message);
    // If no existing plan found, add new one
  }

  // Handle thinking message merging — only merge contiguous streaming chunks
  if (message.type === 'thinking') {
    if (message.content.status === 'done') {
      for (let i = list.length - 1; i >= 0; i--) {
        const msg = list[i];
        if (msg.type !== 'thinking' || msg.msg_id !== message.msg_id) continue;

        const merged = {
          ...msg.content,
          status: 'done' as const,
          duration: message.content.duration,
          subject: message.content.subject || msg.content.subject,
        };
        return updateMessage(i, { ...msg, content: merged });
      }
    }

    if (last.type === 'thinking' && last.msg_id === message.msg_id) {
      // Otherwise append content
      const merged = {
        ...last.content,
        content: last.content.content + message.content.content,
        subject: message.content.subject || last.content.subject,
      };
      return updateMessage(list.length - 1, { ...last, content: merged });
    }
    return pushMessage(message);
  }

  if (last.msg_id !== message.msg_id || last.type !== message.type) {
    return pushMessage(message);
  }
  if (message.type === 'text' && last.type === 'text') {
    message.content = mergeTextMessageContent(last.content, message.content);
  }
  return updateMessage(list.length - 1, Object.assign({}, last, message));
};
