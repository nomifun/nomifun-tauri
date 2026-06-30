/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { IConversationMcpStatus } from '@/common/config/storage';
import AgentModeSelector from '@/renderer/components/agent/AgentModeSelector';
import CommandQueuePanel from '@/renderer/components/chat/CommandQueuePanel';
import MobileActionSheet, {
  type MobileActionSheetEntry,
  type MobileActionSheetOption,
  useAttachEntry,
} from '@/renderer/components/chat/MobileActionSheet';
import SendBox from '@/renderer/components/chat/SendBox';
import ThoughtDisplay from '@/renderer/components/chat/ThoughtDisplay';
import { useProcessingStartedAt } from '@/renderer/pages/conversation/platforms/useProcessingStartedAt';
import FileAttachButton from '@/renderer/components/media/FileAttachButton';
import FilePreview from '@/renderer/components/media/FilePreview';
import HorizontalFileList from '@/renderer/components/media/HorizontalFileList';
import { useConversationContextSafe } from '@/renderer/hooks/context/ConversationContext';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { useAutoTitle } from '@/renderer/hooks/chat/useAutoTitle';
import { getSendBoxDraftHook, type FileOrFolderItem } from '@/renderer/hooks/chat/useSendBoxDraft';
import { createSetUploadFile, useSendBoxFiles } from '@/renderer/hooks/chat/useSendBoxFiles';
import { useSlashCommands } from '@/renderer/hooks/chat/useSlashCommands';
import { useOpenFileSelector } from '@/renderer/hooks/file/useOpenFileSelector';
import { useLatestRef } from '@/renderer/hooks/ui/useLatestRef';
import { useAddOrUpdateMessage, useRemoveMessageByMsgId, useRemoveMessagesFrom } from '@/renderer/pages/conversation/Messages/hooks';
import { savePreferredMode } from '@/renderer/pages/guid/hooks/agentSelectionUtils';
import {
  shouldEnqueueConversationCommand,
  useConversationCommandQueue,
  type ConversationCommandQueueItem,
} from '@/renderer/pages/conversation/platforms/useConversationCommandQueue';
import { getConversationOrNull } from '@/renderer/pages/conversation/utils/conversationCache';
import { getConversationRuntimeWorkspaceErrorMessage } from '@/renderer/pages/conversation/utils/conversationCreateError';
import { warmupConversation } from '@/renderer/pages/conversation/utils/warmupConversation';
import { usePreviewContext } from '@/renderer/pages/conversation/Preview';
import { allSupportedExts } from '@/renderer/services/FileService';
import { iconColors } from '@/renderer/styles/colors';
import { emitter, useAddEventListener } from '@/renderer/utils/emitter';
import { mergeFileSelectionItems } from '@/renderer/utils/file/fileSelection';
import { buildDisplayMessage, collectSelectedFiles } from '@/renderer/utils/file/messageFiles';
import { mergeWithCapabilities, type AgentModeOption } from '@/renderer/utils/model/agentModes';
import { Message, Tag, Tooltip } from '@arco-design/web-react';
import { Brain, ChartHistogram, MagicHat, Shield, Time } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNomiMessage } from './useNomiMessage';
import { formatTokenCount, formatTurnDuration } from './turnMetrics';
import NomiModelSelector from './NomiModelSelector';
import { ContextUsagePill } from './ContextUsagePill';
import type { NomiModelSelection } from './useNomiModelSelection';

const useNomiSendBoxDraft = getSendBoxDraftHook('nomi', {
  _type: 'nomi',
  atPath: [],
  content: '',
  uploadFile: [],
});

const EMPTY_AT_PATH: Array<string | FileOrFolderItem> = [];
const EMPTY_UPLOAD_FILES: string[] = [];

const useSendBoxDraft = (conversation_id: number) => {
  const { data, mutate } = useNomiSendBoxDraft(String(conversation_id));

  const atPath = data?.atPath ?? EMPTY_AT_PATH;
  const uploadFile = data?.uploadFile ?? EMPTY_UPLOAD_FILES;
  const content = data?.content ?? '';

  const setAtPath = useCallback(
    (nextAtPath: Array<string | FileOrFolderItem>) => {
      mutate((prev) => ({ ...prev, atPath: nextAtPath }));
    },
    [data, mutate]
  );

  const setUploadFile = createSetUploadFile(mutate, data);

  const setContent = useCallback(
    (nextContent: string) => {
      mutate((prev) => ({ ...prev, content: nextContent }));
    },
    [data, mutate]
  );

  return {
    atPath,
    uploadFile,
    setAtPath,
    setUploadFile,
    content,
    setContent,
  };
};

const NomiSendBox: React.FC<{
  conversation_id: number;
  modelSelection: NomiModelSelection;
  session_mode?: string;
  agent_name?: string;
  /**
   * Hide the permission/agent-mode selector (and the mobile action-sheet
   * model + permission entries). Used by locked surfaces like the desktop
   * companion chat, which runs in a fixed yolo mode with a locked model.
   */
  hideModeSelector?: boolean;
}> = ({ conversation_id, modelSelection, session_mode, agent_name, hideModeSelector }) => {
  const [workspacePath, setWorkspacePath] = useState('');
  const [dynamicModes, setDynamicModes] = useState<AgentModeOption[]>([]);
  const [currentMode, setCurrentMode] = useState<string | undefined>(session_mode);
  const [isMobileSheetOpen, setIsMobileSheetOpen] = useState(false);
  const layout = useLayoutContext();
  const isMobile = Boolean(layout?.isMobile);
  const conversationContext = useConversationContextSafe();
  const loadedSkills = conversationContext?.loadedSkills ?? [];
  const loadedMcpStatuses =
    conversationContext?.loadedMcpStatuses ??
    (conversationContext?.loadedMcpServers ?? []).map<IConversationMcpStatus>((name) => ({
      id: 0,
      name,
      status: 'loaded',
    }));
  const { t } = useTranslation();
  const { checkAndUpdateTitle } = useAutoTitle();
  const { current_model } = modelSelection;

  const { thought, running, hasHydratedRunningState, tokenUsage, setActiveMsgId, setWaitingResponse, resetState } =
    useNomiMessage(conversation_id, {
      onConfigChanged: (capabilities) => {
        const modes = (capabilities as { modes?: string[] })?.modes;
        if (modes && modes.length > 0) {
          setDynamicModes(mergeWithCapabilities('nomi', modes));
        }
      },
    });

  const { atPath, uploadFile, setAtPath, setUploadFile, content, setContent } = useSendBoxDraft(conversation_id);

  const handleContentChange = useCallback(
    (val: string) => {
      setContent(val);
    },
    [setContent]
  );

  const [agentWarmed, setAgentWarmed] = useState(false);
  const prepareRuntimeSync = useCallback(async () => {
    await warmupConversation(conversation_id);
  }, [conversation_id]);

  useEffect(() => {
    void getConversationOrNull(conversation_id).then((res) => {
      if (!res?.extra?.workspace) return;
      setWorkspacePath(res.extra.workspace);
    });
  }, [conversation_id]);

  useEffect(() => {
    if (!conversation_id) return;
    setAgentWarmed(false);
    void prepareRuntimeSync()
      .then(() => {
        setAgentWarmed(true);
      })
      .catch((error) => {
        Message.error(getConversationRuntimeWorkspaceErrorMessage(error, t));
      });
  }, [conversation_id, prepareRuntimeSync, t]);

  const slash_commands = useSlashCommands(conversation_id, {
    conversation_type: 'nomi',
    agentStatus: agentWarmed ? 'active' : null,
  });

  const addOrUpdateMessage = useAddOrUpdateMessage();
  const removeMessageByMsgId = useRemoveMessageByMsgId();
  const removeMessagesFrom = useRemoveMessagesFrom();
  const { setSendBoxHandler } = usePreviewContext();
  const isBusy = running;
  const processingStartedAt = useProcessingStartedAt(conversation_id, running);

  const setContentRef = useLatestRef(setContent);
  const contentRef = useLatestRef(content);
  const atPathRef = useLatestRef(atPath);

  // Register handler for adding text from preview panel to sendbox
  useEffect(() => {
    const handler = (text: string) => {
      const new_content = content ? `${content}\n${text}` : text;
      setContentRef.current(new_content);
    };
    setSendBoxHandler(handler);
  }, [setSendBoxHandler, content]);

  // Listen for sendbox.fill event to append text to sendbox
  useAddEventListener(
    'sendbox.fill',
    (text: string) => {
      const prev = contentRef.current;
      setContentRef.current(prev ? `${prev}${text}` : text);
    },
    []
  );

  // Shared file handling logic
  const { handleFilesAdded, clearFiles } = useSendBoxFiles({
    atPath,
    uploadFile,
    setAtPath,
    setUploadFile,
  });

  const executeCommand = useCallback(
    async ({ input, files }: Pick<ConversationCommandQueueItem, 'input' | 'files'>) => {
      if (!current_model?.use_model) {
        Message.warning(t('conversation.chat.noModelSelected'));
        throw new Error('No model selected');
      }

      setWaitingResponse(true);

      const displayMessage = buildDisplayMessage(input, files, workspacePath);
      let msg_id: string | null = null;
      try {
        void checkAndUpdateTitle(conversation_id, input);
        // Wait for the server-assigned msg_id before rendering the optimistic
        // user bubble so the local row uses the same id as the DB row and
        // subsequent WebSocket stream events — avoids duplicate bubbles when
        // useMessageLstCache reloads.
        const res = await ipcBridge.conversation.sendMessage.invoke({
          input: displayMessage,
          conversation_id,
          files,
        });
        msg_id = res.msg_id;
        setActiveMsgId(msg_id);
        // Use add=false (compose mode) so composeMessageWithIndex can de-dup
        // by msg_id — this prevents a duplicate bubble if useMessageLstCache
        // already inserted the DB row for this same msg_id.
        addOrUpdateMessage({
          id: msg_id,
          msg_id,
          type: 'text',
          position: 'right',
          conversation_id,
          content: {
            content: displayMessage,
          },
          created_at: Date.now(),
        });
        emitter.emit('chat.history.refresh');
        if (files.length > 0) {
          emitter.emit('nomi.workspace.refresh');
        }
      } catch (error) {
        if (msg_id) removeMessageByMsgId(msg_id);
        Message.error(getConversationRuntimeWorkspaceErrorMessage(error, t));
        throw error;
      }
    },
    [
      addOrUpdateMessage,
      checkAndUpdateTitle,
      conversation_id,
      current_model?.use_model,
      setActiveMsgId,
      removeMessageByMsgId,
      setWaitingResponse,
      t,
      workspacePath,
    ]
  );

  const {
    items: queuedCommands,
    isPaused: isQueuePaused,
    isInteractionLocked: isQueueInteractionLocked,
    hasPendingCommands,
    enqueue,
    remove,
    clear,
    reorder,
    pause,
    resume,
    lockInteraction,
    unlockInteraction,
    resetActiveExecution,
  } = useConversationCommandQueue({
    conversation_id: conversation_id,
    enabled: true,
    isBusy,
    isHydrated: hasHydratedRunningState,
    onExecute: executeCommand,
  });

  // Handle initial message from Guid page — wait until model is ready
  useEffect(() => {
    if (!conversation_id || !current_model?.use_model) return;

    const storageKey = `nomi_initial_message_${conversation_id}`;
    const processedKey = `nomi_initial_processed_${conversation_id}`;

    const processInitialMessage = async () => {
      if (sessionStorage.getItem(processedKey)) return;
      const storedMessage = sessionStorage.getItem(storageKey);
      if (!storedMessage) return;

      sessionStorage.setItem(processedKey, '1');
      sessionStorage.removeItem(storageKey);

      try {
        const { input, files: initialFiles } = JSON.parse(storedMessage);
        await executeCommand({ input, files: initialFiles || [] });
      } catch (error) {
        console.error('[NomiSendBox] Failed to send initial message:', error);
        sessionStorage.removeItem(processedKey);
      }
    };

    void processInitialMessage();
  }, [conversation_id, current_model?.use_model, executeCommand]);

  const onSendHandler = async (message: string) => {
    const filesToSend = collectSelectedFiles(uploadFile, atPath);
    clearFiles();
    emitter.emit('nomi.selected.file.clear');

    if (
      shouldEnqueueConversationCommand({
        enabled: true,
        isBusy,
        hasPendingCommands,
      })
    ) {
      enqueue({ input: message, files: filesToSend });
      return;
    }

    await executeCommand({ input: message, files: filesToSend });
  };

  // 编辑最近一条用户消息并截断重跑：先本地移除被编辑消息及其后内容（在新 turn 流式
  // 开始之前，避免被流式新消息误删），调用 editResubmit 接口（后端回退引擎 turn +
  // 删除该条及其后的 DB 消息），再乐观插入新的用户气泡（与 executeCommand 一致，
  // 因为消息列表不会随 chat.history.refresh 重载，气泡只能靠乐观插入渲染）。
  const handleEditResubmit = useCallback(
    async (msgId: string, createdAt: number, message: string) => {
      const filesToSend = collectSelectedFiles(uploadFile, atPath);
      clearFiles();
      emitter.emit('nomi.selected.file.clear');
      // 在新 turn 开始流式之前移除旧消息（旧用户消息 + 其后被截断的内容）。
      removeMessagesFrom(createdAt);
      setWaitingResponse(true);
      const displayMessage = buildDisplayMessage(message, filesToSend, workspacePath);
      try {
        const res = await ipcBridge.conversation.editResubmit.invoke({
          conversation_id,
          msg_id: msgId,
          input: displayMessage,
          files: filesToSend,
        });
        // 乐观插入新用户气泡（compose 模式按 msg_id 去重，避免 DB 行重复）。
        addOrUpdateMessage({
          id: res.msg_id,
          msg_id: res.msg_id,
          type: 'text',
          position: 'right',
          conversation_id,
          content: {
            content: displayMessage,
          },
          created_at: Date.now(),
        });
        setActiveMsgId(res.msg_id);
        emitter.emit('chat.history.refresh');
        if (filesToSend.length > 0) emitter.emit('nomi.workspace.refresh');
      } catch (error) {
        setWaitingResponse(false);
        Message.error(getConversationRuntimeWorkspaceErrorMessage(error, t));
        throw error;
      }
    },
    [
      atPath,
      conversation_id,
      uploadFile,
      workspacePath,
      clearFiles,
      removeMessagesFrom,
      addOrUpdateMessage,
      setActiveMsgId,
      setWaitingResponse,
      t,
    ]
  );

  const isSteerUnsupportedError = (error: unknown): boolean => {
    const msg = error instanceof Error ? error.message : String(error ?? '');
    return /not supported for this agent type|steer_unsupported/i.test(msg);
  };

  // Steering injects into the turn that is ALREADY running — it does NOT start a
  // new turn, so we deliberately skip setWaitingResponse(true) (unlike
  // executeCommand). Renders the optimistic user bubble the same way so the
  // interjection shows immediately.
  const executeSteer = useCallback(
    async ({ input, files }: Pick<ConversationCommandQueueItem, 'input' | 'files'>) => {
      const displayMessage = buildDisplayMessage(input, files, workspacePath);
      let msg_id: string | null = null;
      try {
        const res = await ipcBridge.conversation.steer.invoke({
          input: displayMessage,
          conversation_id,
          files,
        });
        msg_id = res.msg_id;
        setActiveMsgId(msg_id);
        addOrUpdateMessage({
          id: msg_id,
          msg_id,
          type: 'text',
          position: 'right',
          conversation_id,
          content: {
            content: displayMessage,
          },
          created_at: Date.now(),
        });
        emitter.emit('chat.history.refresh');
        if (files.length > 0) {
          emitter.emit('nomi.workspace.refresh');
        }
      } catch (error) {
        if (msg_id) removeMessageByMsgId(msg_id);
        // Engine can't steer (non-Nomi) or the turn just ended → fall back to the
        // pending queue so the interjection is never lost.
        if (isSteerUnsupportedError(error)) {
          enqueue({ input, files });
          Message.info(t('conversation.steer.fallbackQueued'));
          return;
        }
        Message.error(getConversationRuntimeWorkspaceErrorMessage(error, t));
      }
    },
    [addOrUpdateMessage, conversation_id, enqueue, removeMessageByMsgId, setActiveMsgId, t, workspacePath]
  );

  const onSteerHandler = async (message: string) => {
    const filesToSend = collectSelectedFiles(uploadFile, atPath);
    clearFiles();
    emitter.emit('nomi.selected.file.clear');
    await executeSteer({ input: message, files: filesToSend });
  };

  const handleEditQueuedCommand = useCallback(
    (item: ConversationCommandQueueItem) => {
      remove(item.id);
      setContent(item.input);
      setUploadFile(Array.from(new Set(item.files)));
      setAtPath([]);
      emitter.emit('nomi.selected.file.clear');
    },
    [remove, setAtPath, setContent, setUploadFile]
  );

  const appendSelectedFiles = useCallback(
    (files: string[]) => {
      setUploadFile((prev) => [...prev, ...files]);
    },
    [setUploadFile]
  );
  const { openFileSelector, onSlashBuiltinCommand } = useOpenFileSelector({
    onFilesSelected: appendSelectedFiles,
  });

  const { entries: attachEntries, hiddenFileInput: attachHiddenInput } = useAttachEntry({
    openFileSelector,
    onLocalFilesAdded: handleFilesAdded,
    dividerBefore: true,
  });

  // Mode switching for the mobile action sheet — mirrors AgentModeSelector's
  // setMode call so the bottom-sheet path stays in lockstep with the desktop dropdown.
  const handleSheetModeChange = useCallback(
    async (mode: string) => {
      if (mode === currentMode) return;
      try {
        await prepareRuntimeSync();
        await ipcBridge.acpConversation.setMode.invoke({ conversation_id, mode });
        setCurrentMode(mode);
        void savePreferredMode('nomi', mode);
        Message.success(t('agentMode.switchSuccess'));
      } catch (error) {
        console.error('[NomiSendBox] Failed to switch mode via sheet:', error);
        Message.error(t('agentMode.switchFailed'));
      }
    },
    [conversation_id, currentMode, prepareRuntimeSync, t]
  );

  // Sync currentMode from backend when the sheet first opens / conversation switches
  useEffect(() => {
    if (!isMobile || !isMobileSheetOpen) return;
    if (!conversation_id) return;
    let cancelled = false;
    void prepareRuntimeSync()
      .then(() => ipcBridge.acpConversation.getMode.invoke({ conversation_id }))
      .then((result) => {
        if (cancelled || !result) return;
        if (result.initialized !== false) {
          setCurrentMode(result.mode);
        }
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [conversation_id, isMobile, isMobileSheetOpen, prepareRuntimeSync]);

  const handleSheetModelSelect = useCallback(
    (value: string) => {
      // value format: `${providerId}::${modelName}`
      const [providerId, modelName] = value.split('::');
      const provider = modelSelection.providers.find((p) => p.id === providerId);
      if (!provider || !modelName) return;
      void modelSelection.handleSelectModel(provider, modelName);
    },
    [modelSelection]
  );

  const sheetEntries = useMemo<MobileActionSheetEntry[]>(() => {
    if (!isMobile) return [];

    const availableModes: AgentModeOption[] =
      dynamicModes.length > 0
        ? dynamicModes
        : [
            { value: 'default', label: 'Default' },
            { value: 'auto_edit', label: 'Auto-Accept Edits' },
            { value: 'yolo', label: 'YOLO' },
          ];
    const modeOptions: MobileActionSheetOption[] = availableModes.map((mode) => ({
      key: mode.value,
      label: t(`agentMode.${mode.value}`, { defaultValue: mode.label }),
      description: mode.description,
      active: currentMode === mode.value,
    }));

    const modelOptions: MobileActionSheetOption[] = modelSelection.providers.flatMap((provider) =>
      modelSelection.getAvailableModels(provider).map((modelName) => ({
        key: `${provider.id}::${modelName}`,
        label: modelName,
        description: provider.name,
        active:
          modelSelection.current_model?.id === provider.id && modelSelection.current_model?.use_model === modelName,
      }))
    );

    const currentModeLabel =
      modeOptions.find((opt) => opt.active)?.label ?? t('agentMode.default', { defaultValue: 'Default' });
    const currentModelLabel = modelSelection.current_model?.use_model || t('conversation.welcome.selectModel');

    const entries: MobileActionSheetEntry[] = [
      // Locked surfaces (companion) hide the model + permission entries: model is
      // pinned to the companion profile and permission is fixed to yolo.
      ...(hideModeSelector
        ? []
        : [
            {
              key: 'model',
              icon: <Brain theme='outline' size='16' />,
              label: t('common.model', { defaultValue: 'Model' }),
              meta: currentModelLabel,
              submenu: {
                title: t('common.model', { defaultValue: 'Model' }),
                options: modelOptions,
                onSelect: handleSheetModelSelect,
                emptyText: t('conversation.welcome.selectModel'),
              },
            },
            {
              key: 'permission',
              icon: <Shield theme='outline' size='16' />,
              label: t('agentMode.permission', { defaultValue: 'Permission' }),
              meta: currentModeLabel,
              submenu: {
                title: t('agentMode.permission', { defaultValue: 'Permission' }),
                options: modeOptions,
                onSelect: (key: string) => void handleSheetModeChange(key),
              },
            },
          ]),
      ...attachEntries,
    ];

    if (loadedSkills.length > 0) {
      const skillOptions: MobileActionSheetOption[] = loadedSkills.map((name) => ({
        key: name,
        label: `/${name}`,
      }));
      entries.push({
        key: 'skills',
        icon: <MagicHat theme='outline' size='16' />,
        label: t('common.skills', { defaultValue: 'Skills' }),
        variant: 'muted',
        submenu: {
          title: t('common.skills', { defaultValue: 'Skills' }),
          selectable: false,
          options: skillOptions,
          onSelect: (name) => {
            setContent(`/${name} `);
          },
        },
      });
    }

    if (loadedMcpStatuses.length > 0) {
      const mcpOptions: MobileActionSheetOption[] = loadedMcpStatuses.map((item) => ({
        key: item.name,
        label: item.name,
        description:
          item.status === 'loaded'
            ? undefined
            : item.reason
              ? `${t(`conversation.mcp.status.${item.status}` as const)} · ${item.reason}`
              : t(`conversation.mcp.status.${item.status}` as const),
      }));
      entries.push({
        key: 'mcp',
        icon: <Shield theme='outline' size='16' />,
        label: t('conversation.mcp.loaded', { defaultValue: 'Loaded MCP' }),
        variant: 'muted',
        submenu: {
          title: t('conversation.mcp.loaded', { defaultValue: 'Loaded MCP' }),
          selectable: false,
          options: mcpOptions,
          onSelect: () => undefined,
        },
      });
    }

    return entries;
  }, [
    attachEntries,
    currentMode,
    dynamicModes,
    handleSheetModeChange,
    handleSheetModelSelect,
    hideModeSelector,
    isMobile,
    loadedMcpStatuses,
    loadedSkills,
    modelSelection,
    setContent,
    t,
  ]);

  useAddEventListener('nomi.selected.file', setAtPath);
  useAddEventListener('nomi.selected.file.append', (selectedItems: Array<string | FileOrFolderItem>) => {
    const merged = mergeFileSelectionItems(atPathRef.current, selectedItems);
    if (merged !== atPathRef.current) {
      setAtPath(merged as Array<string | FileOrFolderItem>);
    }
  });

  // Stop conversation handler
  const handleStop = async (): Promise<void> => {
    // Best-effort cancel: swallow rejections so they don't bubble up as
    // unhandled rejections. UI state is still reset via finally.
    try {
      await ipcBridge.conversation.stop.invoke({ conversation_id });
    } catch (error) {
      console.warn('[NomiSendBox] stop request failed', error);
    } finally {
      resetState();
      resetActiveExecution('stop');
    }
  };

  // Clear conversation context (release model context); keeps message records.
  const handleClearContext = async (): Promise<void> => {
    try {
      await ipcBridge.conversation.clearContext.invoke({ conversation_id });
      Message.success({
        content: t('conversation.clearContext.success', { defaultValue: 'Context cleared' }),
        duration: 2000,
        closable: true,
      });
    } catch (error) {
      console.warn('[NomiSendBox] clear context failed', error);
      Message.error({
        content: t('conversation.clearContext.failed', { defaultValue: 'Failed to clear context' }),
        closable: true,
      });
    }
  };

  return (
    <div className='max-w-800px w-full mx-auto flex flex-col mt-auto mb-16px'>
      <CommandQueuePanel
        items={queuedCommands}
        paused={isQueuePaused}
        interactionLocked={isQueueInteractionLocked}
        onPause={pause}
        onResume={resume}
        onInteractionLock={lockInteraction}
        onInteractionUnlock={unlockInteraction}
        onEdit={handleEditQueuedCommand}
        onReorder={reorder}
        onRemove={remove}
        onClear={clear}
      />
      <ThoughtDisplay thought={thought} running={running} startedAt={processingStartedAt} onStop={handleStop} />

      {!running && tokenUsage && (tokenUsage.total_tokens > 0 || (tokenUsage.elapsed_ms ?? 0) > 0) && (
        <div
          className='flex items-center justify-end gap-3 px-1 pb-1 text-[11px] text-3 select-none'
          data-testid='nomi-turn-metrics'
        >
          {typeof tokenUsage.elapsed_ms === 'number' && tokenUsage.elapsed_ms > 0 && (
            <Tooltip content={t('conversation.chat.turnMetrics.durationTooltip')} mini>
              <span className='inline-flex items-center gap-1'>
                <Time theme='outline' size='12' fill={iconColors.secondary} />
                {formatTurnDuration(tokenUsage.elapsed_ms)}
              </span>
            </Tooltip>
          )}
          {tokenUsage.total_tokens > 0 && (
            <Tooltip content={t('conversation.chat.turnMetrics.tokensTooltip')} mini>
              <span className='inline-flex items-center gap-1'>
                <ChartHistogram theme='outline' size='12' fill={iconColors.secondary} />
                {formatTokenCount(tokenUsage.total_tokens)}
              </span>
            </Tooltip>
          )}
        </div>
      )}

      <SendBox
        data-testid='nomi-sendbox'
        onMobilePlusClick={isMobile ? () => setIsMobileSheetOpen(true) : undefined}
        value={content}
        onChange={handleContentChange}
        selectedWorkspaceItems={atPath}
        onSelectedWorkspaceItemsChange={(items) => {
          emitter.emit('nomi.selected.file', items);
          setAtPath(items);
        }}
        loading={isBusy}
        disabled={!current_model?.use_model}
        placeholder={
          current_model?.use_model
            ? t('acp.sendbox.placeholder', {
                backend: agent_name || 'Nomi',
                defaultValue: `Send message to {{backend}}...`,
              })
            : t('conversation.chat.noModelSelected')
        }
        onStop={handleStop}
        onClearContext={handleClearContext}
        className='z-10'
        onFilesAdded={handleFilesAdded}
        hasPendingAttachments={uploadFile.length > 0 || atPath.length > 0}
        supportedExts={allSupportedExts}
        defaultMultiLine={!isMobile}
        lockMultiLine={!isMobile}
        tools={
          <FileAttachButton
            openFileSelector={openFileSelector}
            onLocalFilesAdded={handleFilesAdded}
            loadedMcpStatuses={loadedMcpStatuses}
          />
        }
        rightTools={
          hideModeSelector ? undefined : (
            <div className='flex items-center gap-2 min-w-0' data-testid='nomi-sendbox-config-group'>
              <ContextUsagePill used={tokenUsage?.context_tokens} max={tokenUsage?.context_window} />
              <NomiModelSelector selection={modelSelection} className='nomi-sendbox-model-btn' />
              <AgentModeSelector
                backend='nomi'
                conversation_id={conversation_id}
                compact
                initialMode={session_mode}
                dynamicModes={dynamicModes}
                compactLeadingIcon={<Shield theme='outline' size='14' fill={iconColors.secondary} />}
                modeLabelFormatter={(mode) => t(`agentMode.${mode.value}`, { defaultValue: mode.label })}
                compactLabelPrefix={t('agentMode.permission')}
                hideCompactLabelPrefixOnMobile
                beforeRuntimeSync={prepareRuntimeSync}
              />
            </div>
          )
        }
        prefix={
          <>
            {uploadFile.length > 0 && (
              <HorizontalFileList>
                {uploadFile.map((path) => (
                  <FilePreview
                    key={path}
                    data-testid={`nomi-file-tag-${uploadFile.indexOf(path)}`}
                    path={path}
                    onRemove={() => setUploadFile(uploadFile.filter((v) => v !== path))}
                  />
                ))}
              </HorizontalFileList>
            )}
            {atPath.some((item) => (typeof item === 'string' ? false : !item.isFile)) && (
              <div className='flex flex-wrap items-center gap-8px mb-8px'>
                {atPath.map((item) => {
                  if (typeof item === 'string') return null;
                  if (!item.isFile) {
                    const folderIndex = atPath.filter((v) => typeof v !== 'string' && !v.isFile).indexOf(item);
                    return (
                      <Tag
                        key={item.path}
                        data-testid={`nomi-folder-tag-${folderIndex}`}
                        bordered={false}
                        className='!bg-primary-1 !text-primary-6'
                        closable
                        onClose={() => {
                          const newAtPath = atPath.filter((v) => (typeof v === 'string' ? true : v.path !== item.path));
                          emitter.emit('nomi.selected.file', newAtPath);
                          setAtPath(newAtPath);
                        }}
                      >
                        {item.name}
                      </Tag>
                    );
                  }
                  return null;
                })}
              </div>
            )}
          </>
        }
        onSend={onSendHandler}
        onSteer={onSteerHandler}
        steerAvailable
        onEditResubmit={handleEditResubmit}
        slash_commands={slash_commands}
        onSlashBuiltinCommand={onSlashBuiltinCommand}
        allowSendWhileLoading
      />
      {isMobile && (
        <>
          <MobileActionSheet
            open={isMobileSheetOpen}
            onClose={() => setIsMobileSheetOpen(false)}
            title={t('common.more', { defaultValue: 'More' })}
            entries={sheetEntries}
          />
          {attachHiddenInput}
        </>
      )}
    </div>
  );
};

export default NomiSendBox;
