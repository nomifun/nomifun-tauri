/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import {
  conversationTarget,
  type ConversationId,
  type ExecutionTemplateId,
  type McpServerId,
} from '@/common/types/ids';
import { sessionStorageKey } from '@/common/utils/browserStorageKey';
import { uuidv7 } from '@/common/utils';
import { ipcBridge } from '@/common';
import type { IMcpServer, TProviderWithModel } from '@/common/config/storage';
import { buildAgentConversationParams } from '@/common/utils/buildAgentConversationParams';
import { toSessionMcpServer } from '@/renderer/hooks/mcp/catalog';
import { emitter } from '@/renderer/utils/emitter';
import { buildDisplayMessage } from '@/renderer/utils/file/messageFiles';
import { Message } from '@arco-design/web-react';
import { useCallback, useRef } from 'react';
import { type TFunction } from 'i18next';
import type { NavigateFunction } from 'react-router-dom';
import { getConversationCreateErrorMessage } from '@/renderer/pages/conversation/utils/conversationCreateError';
import { seedConversationCache } from '@/renderer/pages/conversation/utils/conversationCache';
import type { PendingConversation } from '@/renderer/pages/conversation/components/ConversationShell/PendingConversationContext';
import { planGuidEntry, isAutoWorkEntry } from './autoWorkEntry';
import type { AutoWorkDraftValue } from '@/renderer/pages/conversation/components/AutoWorkControl';
import type { AcpModelInfo, AvailableAgent, EffectiveAgentInfo } from '../types';
import type {
  TDecisionPolicy,
  TDelegationPolicy,
  TExecutionModelPool,
} from '@/common/types/agentExecution/agentExecutionTypes';
import {
  assertCreatedConversationPreset,
  presetIdFromSelectionKey,
} from './presetConversationContract';

export type GuidSendDeps = {
  // Input state
  input: string;
  setInput: React.Dispatch<React.SetStateAction<string>>;
  files: string[];
  setFiles: React.Dispatch<React.SetStateAction<string[]>>;
  dir: string;
  setDir: React.Dispatch<React.SetStateAction<string>>;
  setLoading: React.Dispatch<React.SetStateAction<boolean>>;
  loading: boolean;

  // Agent state
  selectedAgent: string;
  selectedAgentKey: string;
  selectedAgentInfo: AvailableAgent | undefined;
  selectedMode: string;
  selectedAcpModel: string | null;
  currentAcpCachedModelInfo: AcpModelInfo | null;
  current_model: TProviderWithModel | undefined;

  // Agent helpers
  findAgentByKey: (key: string) => AvailableAgent | undefined;
  getEffectiveAgentType: (
    agentInfo: { agent_type: string; backend?: string } | undefined,
  ) => EffectiveAgentInfo;
  guidDisabledBuiltinSkills: string[] | undefined;
  guidEnabledSkills: string[] | undefined;
  availableMcpServers: IMcpServer[];
  selectedMcpServerIds: McpServerId[] | undefined;
  currentEffectiveAgentInfo: EffectiveAgentInfo;
  isGoogleAuth: boolean;

  /** Applies the Guid page's advanced drafts (knowledge/AutoWork/IDMM) onto the
   * freshly created conversation, before navigation. Never throws. */
  applyAdvancedConfig?: (conversationId: ConversationId) => Promise<void>;

  /** Current AutoWork draft. When enabled with a tag, the entry starts an
   * AutoWork session (no initial message) instead of a normal chat send —
   * sending a first message would race the AutoWork turn and surface
   * "conversation N is already running". */
  autoWork: AutoWorkDraftValue;

  delegationPolicy: TDelegationPolicy;
  executionModelPool?: TExecutionModelPool;
  decisionPolicy: TDecisionPolicy;
  /** Optional reusable collaboration input selected in the composer. It is an
   * entry default only; the created Execution copies it and keeps no live FK. */
  executionTemplateId?: ExecutionTemplateId;

  // Mention state reset
  setMentionOpen: React.Dispatch<React.SetStateAction<boolean>>;
  setMentionQuery: React.Dispatch<React.SetStateAction<string | null>>;
  setMentionSelectorOpen: React.Dispatch<React.SetStateAction<boolean>>;
  setMentionActiveIndex: React.Dispatch<React.SetStateAction<number>>;

  // Navigation
  navigate: NavigateFunction;
  t: TFunction;

  /** Show the instant "creating conversation" loading overlay the moment the
   * user sends, before the create round-trip resolves. Optional so callers
   * outside the conversation shell degrade gracefully. */
  beginPending?: (payload: PendingConversation) => void;
  /** Tear the loading overlay down (on success after navigate, or on failure). */
  endPending?: () => void;
};

export type GuidSendResult = {
  handleSend: () => Promise<void>;
  sendMessageHandler: () => void;
  isButtonDisabled: boolean;
};

/**
 * Hook that manages the send logic for all conversation types (openclaw/nanobot/acp).
 */
export const useGuidSend = (deps: GuidSendDeps): GuidSendResult => {
  const {
    input,
    setInput,
    files,
    setFiles,
    dir,
    setDir,
    setLoading,
    loading,
    selectedAgent,
    selectedAgentKey,
    selectedAgentInfo,
    selectedMode,
    selectedAcpModel,
    currentAcpCachedModelInfo,
    current_model,
    findAgentByKey,
    getEffectiveAgentType,
    guidDisabledBuiltinSkills,
    guidEnabledSkills,
    availableMcpServers,
    selectedMcpServerIds,
    currentEffectiveAgentInfo,
    isGoogleAuth,
    applyAdvancedConfig,
    autoWork,
    delegationPolicy,
    executionModelPool,
    decisionPolicy,
    executionTemplateId,
    setMentionOpen,
    setMentionQuery,
    setMentionSelectorOpen,
    setMentionActiveIndex,
    navigate,
    t,
    beginPending,
    endPending,
  } = deps;
  const sendingRef = useRef(false);

  const handleSend = useCallback(async () => {
    const isCustomWorkspace = !!dir;
    const finalWorkspace = dir || '';

    // AutoWork entry (switch on + tag) creates the session and lets the backend
    // requirement loop drive it — it must NOT also send a first message, which
    // would start a second turn that races the AutoWork turn and loses with
    // "conversation N is already running".
    const entryPlan = planGuidEntry(input, autoWork);

    const agentInfo = selectedAgentInfo;
    const preset_id = presetIdFromSelectionKey(selectedAgentKey);
    const is_preset = preset_id !== undefined;
    if (is_preset && (!agentInfo || agentInfo.preset_id !== preset_id)) {
      throw new TypeError(
        'The selected preset is no longer available. Refresh the preset catalog or choose another preset.',
      );
    }

    const { agent_type: effectiveAgentType } = getEffectiveAgentType(agentInfo);

    // Presets are resolved exclusively by the backend from `preset_id`.
    // Guid-local skill controls remain valid only for bare Agent launches.
    const enabled_skills_to_send = !is_preset && guidEnabledSkills?.length ? guidEnabledSkills : undefined;
    const excludeBuiltinSkills = !is_preset ? guidDisabledBuiltinSkills : undefined;
    const selectedMcpServerIdSet = new Set(selectedMcpServerIds ?? []);
    const selectedUserMcpServerIds = availableMcpServers
      .filter((server) => selectedMcpServerIdSet.has(server.mcp_server_id) && server.builtin !== true)
      .map((server) => server.mcp_server_id);
    const selectedAllSessionMcpServers = availableMcpServers
      .filter((server) => selectedMcpServerIdSet.has(server.mcp_server_id))
      .map((server) => toSessionMcpServer(server));
    const selectedSessionMcpServers = availableMcpServers
      .filter((server) => selectedMcpServerIdSet.has(server.mcp_server_id) && server.builtin === true)
      .map((server) => toSessionMcpServer(server));

    const finalEffectiveAgentType = effectiveAgentType;

    // OpenClaw Gateway path
    if (selectedAgent === 'openclaw-gateway') {
      const openclawAgentInfo = agentInfo || findAgentByKey(selectedAgentKey);
      const openclawConversationParams = buildAgentConversationParams({
        backend: openclawAgentInfo?.backend || 'openclaw-gateway',
        name: entryPlan.conversationName,
        agent_name: openclawAgentInfo?.name,
        preset_id,
        workspace: finalWorkspace,
        model: current_model!,
        cli_path: openclawAgentInfo?.cli_path,
        custom_workspace: isCustomWorkspace,
        is_preset,
        extra: {
          default_files: files,
          runtime_validation: {
            expected_workspace: finalWorkspace,
            expected_backend: openclawAgentInfo?.backend,
            expected_agent_name: openclawAgentInfo?.name,
            expected_cli_path: openclawAgentInfo?.cli_path,
            expected_model: current_model?.use_model,
            switched_at: Date.now(),
          },
          preset_enabled_skills: enabled_skills_to_send,
          exclude_auto_inject_skills: excludeBuiltinSkills,
        },
      });

      try {
        const conversation = await ipcBridge.conversation.create.invoke(openclawConversationParams);

        if (!conversation || !conversation.id) {
          Message.error(t('conversation.createFailed'));
          return;
        }
        assertCreatedConversationPreset(conversation, preset_id);

        // Push the Guid page's advanced drafts (knowledge/AutoWork/IDMM) onto
        // the new conversation before navigating, so they are live when the
        // conversation page consumes the initial message.
        await applyAdvancedConfig?.(conversation.id);

        emitter.emit('chat.history.refresh');

        const initialMessage = {
          conversation_id: conversation.id,
          initial_admission_epoch: 0,
          input,
          files: files.length > 0 ? files : undefined,
          idempotency_key: uuidv7(),
        };
        if (entryPlan.sendInitialMessage) {
          sessionStorage.setItem(
            sessionStorageKey('initial-message-openclaw', conversationTarget(conversation.id)),
            JSON.stringify(initialMessage)
          );
        }

        seedConversationCache(conversation);
        await navigate(`/conversation/${conversation.id}`);
      } catch (error: unknown) {
        console.error('Failed to create OpenClaw conversation:', error);
        throw error;
      }
      return;
    }

    // Nanobot path
    if (selectedAgent === 'nanobot') {
      const nanobotAgentInfo = agentInfo || findAgentByKey(selectedAgentKey);
      const nanobotConversationParams = buildAgentConversationParams({
        backend: nanobotAgentInfo?.backend || 'nanobot',
        name: entryPlan.conversationName,
        agent_name: nanobotAgentInfo?.name,
        preset_id,
        workspace: finalWorkspace,
        model: current_model!,
        custom_workspace: isCustomWorkspace,
        is_preset,
        extra: {
          default_files: files,
          preset_enabled_skills: enabled_skills_to_send,
          exclude_auto_inject_skills: excludeBuiltinSkills,
        },
      });

      try {
        const conversation = await ipcBridge.conversation.create.invoke(nanobotConversationParams);

        if (!conversation || !conversation.id) {
          Message.error(t('conversation.createFailed'));
          return;
        }
        assertCreatedConversationPreset(conversation, preset_id);

        // Push the Guid page's advanced drafts (knowledge/AutoWork/IDMM) onto
        // the new conversation before navigating, so they are live when the
        // conversation page consumes the initial message.
        await applyAdvancedConfig?.(conversation.id);

        emitter.emit('chat.history.refresh');

        const initialMessage = {
          conversation_id: conversation.id,
          initial_admission_epoch: 0,
          input,
          files: files.length > 0 ? files : undefined,
          idempotency_key: uuidv7(),
        };
        if (entryPlan.sendInitialMessage) {
          sessionStorage.setItem(
            sessionStorageKey('initial-message-nanobot', conversationTarget(conversation.id)),
            JSON.stringify(initialMessage)
          );
        }

        seedConversationCache(conversation);
        await navigate(`/conversation/${conversation.id}`);
      } catch (error: unknown) {
        console.error('Failed to create Nanobot conversation:', error);
        throw error;
      }
      return;
    }

    // Nomi path (direct selection or preset preset with nomi as main agent)
    if (selectedAgent === 'nomi' || (is_preset && finalEffectiveAgentType === 'nomi')) {
      if (!current_model) {
        Message.warning(t('conversation.noModelConfigured'));
        return;
      }

      try {
        const conversation = await ipcBridge.conversation.create.invoke({
          type: 'nomi',
          name: entryPlan.conversationName,
          model: current_model,
          preset_id,
          delegation_policy: delegationPolicy,
          execution_model_pool: executionModelPool,
          decision_policy: decisionPolicy,
          execution_template_id: executionTemplateId,
          extra: {
            default_files: files,
            workspace: finalWorkspace,
            custom_workspace: isCustomWorkspace,
            preset_enabled_skills: enabled_skills_to_send,
            exclude_auto_inject_skills: excludeBuiltinSkills,
            selected_mcp_server_ids: selectedUserMcpServerIds,
            // Nomi consumes the authoritative session snapshot instead of
            // reloading only user servers from the global MCP repository.
            selected_session_mcp_servers: selectedAllSessionMcpServers,
            session_mode: selectedMode,
          },
        });

        if (!conversation || !conversation.id) {
          Message.error(t('conversation.createFailed'));
          return;
        }
        assertCreatedConversationPreset(conversation, preset_id);

        // Push the Guid page's advanced drafts (knowledge/AutoWork/IDMM) onto
        // the new conversation before navigating, so they are live when the
        // conversation page consumes the initial message.
        await applyAdvancedConfig?.(conversation.id);

        emitter.emit('chat.history.refresh');

        const initialMessage = {
          conversation_id: conversation.id,
          initial_admission_epoch: 0,
          input,
          files: files.length > 0 ? files : undefined,
          idempotency_key: uuidv7(),
        };
        if (entryPlan.sendInitialMessage) {
          sessionStorage.setItem(
            sessionStorageKey('initial-message-nomi', conversationTarget(conversation.id)),
            JSON.stringify(initialMessage)
          );
        }

        seedConversationCache(conversation);
        await navigate(`/conversation/${conversation.id}`);
      } catch (error: unknown) {
        console.error('Failed to create Nomi conversation:', error);
        throw error;
      }
      return;
    }

    // Remaining agent path (ACP/remote/custom, including preset fallbacks)
    {
      // Agent-type fallback only applies to preset presets whose primary agent
      // was unavailable and got switched. For non-preset
      // agents (including extension-contributed ACP adapters with backend='custom'),
      // we must keep the original selectedAgent so the correct backend/cli_path is used.
      const agent_typeChanged = is_preset && selectedAgent !== finalEffectiveAgentType;
      const acpBackend: string | undefined = agent_typeChanged
        ? finalEffectiveAgentType
        : is_preset
          ? finalEffectiveAgentType
          : selectedAgent;

      const acpAgentInfo = agent_typeChanged
        ? findAgentByKey(acpBackend as string)
        : agentInfo || findAgentByKey(selectedAgentKey);

      if (!acpAgentInfo && !is_preset) {
        console.warn(`${acpBackend} CLI not found, but proceeding to let conversation panel handle it.`);
      }
      const agentBackend = acpBackend || selectedAgent;
      const agentConversationParams = buildAgentConversationParams({
        backend: agentBackend,
        name: entryPlan.conversationName,
        // For row-scoped rows (custom ACP / remote) the backend factory
        // needs the actual catalog id — `backend` collapses to the `custom`
        // slot so it cannot discriminate between rows on its own.
        agent_id: acpAgentInfo?.id,
        agent_name: acpAgentInfo?.name,
        preset_id,
        workspace: finalWorkspace,
        model: current_model!,
        cli_path: acpAgentInfo?.cli_path,
        remote_agent_id: acpAgentInfo?.remote_agent_id,
        custom_workspace: isCustomWorkspace,
        is_preset,
        session_mode: selectedMode,
        current_model_id: selectedAcpModel || currentAcpCachedModelInfo?.current_model_id || undefined,
        extra: {
          default_files: files,
          exclude_auto_inject_skills: excludeBuiltinSkills,
          selected_mcp_server_ids: selectedUserMcpServerIds,
          selected_session_mcp_servers: selectedSessionMcpServers,
          // Bare Agents may still carry a one-off skill selection.
          ...(is_preset ? {} : guidEnabledSkills?.length ? { preset_enabled_skills: guidEnabledSkills } : {}),
        },
      });

      try {
        const conversation = await ipcBridge.conversation.create.invoke(agentConversationParams);
        if (!conversation || !conversation.id) {
          console.error('Failed to create ACP conversation - conversation object is null or missing id');
          return;
        }
        assertCreatedConversationPreset(conversation, preset_id);

        await applyAdvancedConfig?.(conversation.id);

        emitter.emit('chat.history.refresh');

        const initialMessage = {
          conversation_id: conversation.id,
          initial_admission_epoch: 0,
          input,
          files: files.length > 0 ? files : undefined,
          idempotency_key: uuidv7(),
        };
        if (entryPlan.sendInitialMessage) {
          const target = conversationTarget(conversation.id);
          const initialMessageKey = sessionStorageKey(
            agentConversationParams.type === 'remote' ? 'initial-message-remote' : 'initial-message-acp',
            target
          );
          sessionStorage.setItem(initialMessageKey, JSON.stringify(initialMessage));
        }

        seedConversationCache(conversation);
        await navigate(`/conversation/${conversation.id}`);
      } catch (error: unknown) {
        console.error('Failed to create ACP conversation:', error);
        throw error;
      }
    }
  }, [
    input,
    files,
    dir,
    selectedAgent,
    selectedAgentKey,
    selectedAgentInfo,
    selectedMode,
    selectedAcpModel,
    currentAcpCachedModelInfo,
    current_model,
    findAgentByKey,
    getEffectiveAgentType,
    guidDisabledBuiltinSkills,
    guidEnabledSkills,
    availableMcpServers,
    selectedMcpServerIds,
    applyAdvancedConfig,
    autoWork,
    delegationPolicy,
    executionModelPool,
    decisionPolicy,
    executionTemplateId,
    navigate,
    t,
  ]);

  const sendMessageHandler = useCallback(() => {
    if (loading || sendingRef.current) return;
    sendingRef.current = true;
    setLoading(true);
    // Instant feedback: switch the content region to a conversation-shaped
    // loading overlay (echoed message + "creating…") the moment the user sends,
    // BEFORE the create round-trip resolves. Captured here because `.then` below
    // clears `input`. AutoWork entries send no first message → different caption.
    beginPending?.({
      input,
      files: files.length > 0 ? files : undefined,
      sendsInitialMessage: !isAutoWorkEntry(autoWork),
    });
    handleSend()
      .then(() => {
        setInput('');
        setMentionOpen(false);
        setMentionQuery(null);
        setMentionSelectorOpen(false);
        setMentionActiveIndex(0);
        setFiles([]);
        setDir('');
      })
      .catch((error) => {
        console.error('Failed to send message:', error);
        Message.error(getConversationCreateErrorMessage(error, t));
      })
      .finally(() => {
        sendingRef.current = false;
        setLoading(false);
        // Tear down the overlay: on success the real conversation page has
        // already been navigated to (deferred one frame inside `end`); on
        // failure we uncover the composer with the input preserved.
        endPending?.();
      });
  }, [
    loading,
    handleSend,
    setLoading,
    setInput,
    setMentionOpen,
    setMentionQuery,
    setMentionSelectorOpen,
    setMentionActiveIndex,
    setFiles,
    setDir,
    t,
    input,
    files,
    autoWork,
    beginPending,
    endPending,
  ]);

  // Calculate button disabled state
  const isButtonDisabled = loading || !input.trim();

  return {
    handleSend,
    sendMessageHandler,
    isButtonDisabled,
  };
};
