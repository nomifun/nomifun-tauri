/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */
import type { ConversationId, MessageId } from '@/common/types/ids';

import EventEmitter from 'eventemitter3';
import type { DependencyList } from 'react';
import { useEffect } from 'react';
import type { FileOrFolderItem } from '@/renderer/utils/file/fileTypes';
import type { PreviewContentType } from '@/common/types/office/preview';
import type { TokenUsageData } from '@/common/config/storage';

export type ReplyQuote = {
  /** Durable message identity used only as quote metadata. */
  messageId: MessageId;
  content: string;
  position: 'left' | 'right' | 'center' | 'pop';
};

interface EventTypes {
  'nomi.selected.file': [Array<string | FileOrFolderItem>];
  'nomi.selected.file.append': [Array<string | FileOrFolderItem>];
  'nomi.selected.file.clear': void;
  'nomi.workspace.refresh': void;
  'nomi.usage.updated': [{ conversation_id: ConversationId; tokenUsage: TokenUsageData }];
  'acp.selected.file': [Array<string | FileOrFolderItem>];
  'acp.selected.file.append': [Array<string | FileOrFolderItem>];
  'acp.selected.file.clear': void;
  'acp.workspace.refresh': void;
  'openclaw-gateway.selected.file': [Array<string | FileOrFolderItem>];
  'openclaw-gateway.selected.file.append': [Array<string | FileOrFolderItem>];
  'openclaw-gateway.selected.file.clear': void;
  'openclaw-gateway.workspace.refresh': void;
  'nanobot.selected.file': [Array<string | FileOrFolderItem>];
  'nanobot.selected.file.append': [Array<string | FileOrFolderItem>];
  'nanobot.selected.file.clear': void;
  'nanobot.workspace.refresh': void;
  'remote.selected.file': [Array<string | FileOrFolderItem>];
  'remote.selected.file.append': [Array<string | FileOrFolderItem>];
  'remote.selected.file.clear': void;
  'remote.workspace.refresh': void;
  // 终端工作区栏选中文件事件（镜像会话栏键），由终端工作区栏发出、TerminalSendBox 消费
  // Terminal workspace rail file-selection events (mirror the conversation rail
  // keys). Emitted by the terminal workspace rail; consumed by TerminalSendBox.
  'terminal.selected.file': [Array<string | FileOrFolderItem>];
  'terminal.selected.file.append': [Array<string | FileOrFolderItem>];
  'terminal.selected.file.clear': void;
  'terminal.workspace.refresh': void;
  'chat.history.refresh': void;
  // 终端会话列表刷新事件 / Terminal session list refresh event
  'terminal.list.refresh': void;
  // 会话删除事件 / Conversation deletion event
  'conversation.deleted': [ConversationId];
  // 预览面板事件 / Preview panel events
  'preview.open': [
    { content: string; contentType: PreviewContentType; metadata?: { title?: string; file_name?: string } },
  ];
  // 填充输入框事件 / Fill sendbox input event
  'sendbox.fill': [string]; // prompt text to fill
  'sendbox.reply': [ReplyQuote]; // reply/quote a message
  'sendbox.reply.clear': void; // clear reply quote
  'sendbox.edit': [{ msgId: MessageId; createdAt: number; content: string }]; // edit a sent user message (recall into composer)
  'staroffice.install.request': [{ conversation_id: ConversationId; text: string; detectedUrl?: string | null }];
  'staroffice.install.finished': [{ conversation_id: ConversationId }];
}

export const emitter = new EventEmitter<EventTypes>();

export const addEventListener = <T extends EventEmitter.EventNames<EventTypes>>(
  event: T,
  fn: EventEmitter.EventListener<EventTypes, T>
) => {
  emitter.on(event, fn);
  return () => {
    emitter.off(event, fn);
  };
};

export const useAddEventListener = <T extends EventEmitter.EventNames<EventTypes>>(
  event: T,
  fn: EventEmitter.EventListener<EventTypes, T>,
  deps?: DependencyList
) => {
  useEffect(() => {
    return addEventListener(event, fn);
  }, deps || []);
};
