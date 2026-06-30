/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

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
import { planGuidEntry } from './autoWorkEntry';
import type { AutoWorkDraftValue } from '@/renderer/pages/conversation/components/AutoWorkControl';
import type { AcpModelInfo, AvailableAgent, EffectiveAgentInfo } from '../types';

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
  is_presetAgent: boolean;
  selectedMode: string;
  selectedAcpModel: string | null;
  currentAcpCachedModelInfo: AcpModelInfo | null;
  current_model: TProviderWithModel | undefined;

  // Agent helpers
  findAgentByKey: (key: string) => AvailableAgent | undefined;
  getEffectiveAgentType: (
    agentInfo: { agent_type: string; backend?: string; custom_agent_id?: string } | undefined
  ) => EffectiveAgentInfo;
  resolvePresetRulesAndSkills: (
    agentInfo: { agent_type: string; backend?: string; custom_agent_id?: string; context?: string } | undefined
  ) => Promise<{ rules?: string; skills?: string }>;
  resolveEnabledSkills: (
    agentInfo: { agent_type: string; backend?: string; custom_agent_id?: string } | undefined
  ) => string[] | undefined;
  resolveDisabledBuiltinSkills: (
    agentInfo: { agent_type: string; backend?: string; custom_agent_id?: string } | undefined
  ) => string[] | undefined;
  guidDisabledBuiltinSkills: string[] | undefined;
  guidEnabledSkills: string[] | undefined;
  availableMcpServers: IMcpServer[];
  selectedMcpServerIds: number[] | undefined;
  currentEffectiveAgentInfo: EffectiveAgentInfo;
  isGoogleAuth: boolean;

  /** Applies the Guid page's advanced drafts (knowledge/AutoWork/IDMM) onto the
   * freshly created conversation, before navigation. Never throws. */
  applyAdvancedConfig?: (conversationId: number) => Promise<void>;

  /** Current AutoWork draft. When enabled with a tag, the entry starts an
   * AutoWork session (no initial message) instead of a normal chat send —
   * sending a first message would race the AutoWork turn and surface
   * "conversation N is already running". */
  autoWork: AutoWorkDraftValue;

  /** When true the homepage 智能编排 strip creates an orchestration LEAD nomi
   * conversation (`extra.orchestrator_role: 'lead'` → backend
   * LEAD_ORCHESTRATOR_PROMPT), stashes the goal as the first message, and
   * navigates immediately. On landing the lead agent natively thinks and calls
   * `nomi_run_create`, which links a multi-agent run back to this conversation
   * (writing `extra.orchestrator_run_id` + broadcasting) so `useConversationRun`
   * lights up the top-panel DAG. No FE adhoc-run creation, no blank wait.
   * Mutually exclusive with AutoWork / preset-agent flows (the homepage strip
   * enforces this). */
  orchestrationMode: boolean;

  // Mention state reset
  setMentionOpen: React.Dispatch<React.SetStateAction<boolean>>;
  setMentionQuery: React.Dispatch<React.SetStateAction<string | null>>;
  setMentionSelectorOpen: React.Dispatch<React.SetStateAction<boolean>>;
  setMentionActiveIndex: React.Dispatch<React.SetStateAction<number>>;

  // Navigation
  navigate: NavigateFunction;
  t: TFunction;
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
    is_presetAgent,
    selectedMode,
    selectedAcpModel,
    currentAcpCachedModelInfo,
    current_model,
    findAgentByKey,
    getEffectiveAgentType,
    resolvePresetRulesAndSkills,
    resolveEnabledSkills,
    resolveDisabledBuiltinSkills,
    guidDisabledBuiltinSkills,
    guidEnabledSkills,
    availableMcpServers,
    selectedMcpServerIds,
    currentEffectiveAgentInfo,
    isGoogleAuth,
    applyAdvancedConfig,
    autoWork,
    orchestrationMode,
    setMentionOpen,
    setMentionQuery,
    setMentionSelectorOpen,
    setMentionActiveIndex,
    navigate,
    t,
  } = deps;
  const sendingRef = useRef(false);

  const handleSend = useCallback(async () => {
    const isCustomWorkspace = !!dir;
    const finalWorkspace = dir || '';

    // 智能编排(首页模式条)Direction B — create a nomi conversation set up as an
    // ORCHESTRATION LEAD (`extra.orchestrator_role: 'lead'` → server-authored
    // LEAD_ORCHESTRATOR_PROMPT), stash the user's goal as the first message, and
    // navigate IMMEDIATELY. On landing the lead agent thinks natively (thinking
    // bubbles), calls `nomi_run_create` (visible「查看步骤」tool card) to decompose
    // the goal into a multi-agent run, and narrates — the whole startup/
    // orchestration process is live in the native chat (no silent await, no blank
    // wait). That tool call links the run to THIS conversation (lead_conv_id +
    // extra.orchestrator_run_id + broadcast), so `useConversationRun` lights up the
    // top-panel DAG on its own. No FE adhoc-run creation here.
    if (orchestrationMode) {
      const goal = input.trim();
      if (!goal) return;
      if (!current_model) {
        Message.error(t('guid.modelRequired', { defaultValue: '请先选择模型' }));
        return;
      }
      const conversation = await ipcBridge.conversation.create.invoke({
        type: 'nomi',
        name: goal.slice(0, 60),
        model: current_model,
        extra: {
          workspace: finalWorkspace,
          custom_workspace: isCustomWorkspace,
          orchestrator_role: 'lead',
          session_mode: selectedMode,
        },
      });
      if (!conversation?.id) {
        Message.error(t('conversation.createFailed', { defaultValue: '创建会话失败' }));
        return;
      }
      await applyAdvancedConfig?.(conversation.id);
      // Send the goal as the first turn — NomiSendBox consumes this on mount and
      // starts a native turn. The lead agent then thinks + orchestrates natively;
      // the live turn IS the visible startup process.
      sessionStorage.setItem(
        `nomi_initial_message_${conversation.id}`,
        JSON.stringify({ input: goal, files: files.length > 0 ? files : undefined }),
      );
      emitter.emit('chat.history.refresh');
      await navigate(`/conversation/${conversation.id}`);
      return;
    }

    // AutoWork entry (switch on + tag) creates the session and lets the backend
    // requirement loop drive it — it must NOT also send a first message, which
    // would start a second turn that races the AutoWork turn and loses with
    // "conversation N is already running".
    const entryPlan = planGuidEntry(input, autoWork);

    const agentInfo = selectedAgentInfo;
    const is_preset = is_presetAgent;
    const preset_assistant_id = is_preset ? agentInfo?.custom_agent_id : undefined;

    const { agent_type: effectiveAgentType } = getEffectiveAgentType(agentInfo);

    const { rules: preset_rules } = await resolvePresetRulesAndSkills(agentInfo);
    // Guid page's per-conversation skill overrides take precedence over the
    // assistant's saved defaults. The combined skills menu lets the user pick
    // any custom skill — not just preset-declared ones — so for non-preset
    // agents we still forward the user's selection (the backend accepts
    // `preset_enabled_skills` regardless of `is_preset`).
    const presetEnabledSkillsDefault = resolveEnabledSkills(agentInfo);
    const enabled_skills = guidEnabledSkills ?? presetEnabledSkillsDefault;
    const enabled_skills_to_send = is_presetAgent
      ? enabled_skills
      : guidEnabledSkills?.length
        ? guidEnabledSkills
        : undefined;
    const excludeBuiltinSkills = guidDisabledBuiltinSkills ?? resolveDisabledBuiltinSkills(agentInfo);
    const selectedMcpServerIdSet = new Set(selectedMcpServerIds ?? []);
    const selectedUserMcpServerIds = availableMcpServers
      .filter((server) => selectedMcpServerIdSet.has(server.id) && server.builtin !== true)
      .map((server) => server.id);
    const selectedAllSessionMcpServers = availableMcpServers
      .filter((server) => selectedMcpServerIdSet.has(server.id))
      .map((server) => toSessionMcpServer(server));
    const selectedSessionMcpServers = availableMcpServers
      .filter((server) => selectedMcpServerIdSet.has(server.id) && server.builtin === true)
      .map((server) => toSessionMcpServer(server));

    const finalEffectiveAgentType = effectiveAgentType;

    // OpenClaw Gateway path
    if (selectedAgent === 'openclaw-gateway') {
      const openclawAgentInfo = agentInfo || findAgentByKey(selectedAgentKey);
      const openclawConversationParams = buildAgentConversationParams({
        backend: openclawAgentInfo?.backend || 'openclaw-gateway',
        name: entryPlan.conversationName,
        agent_name: openclawAgentInfo?.name,
        preset_assistant_id,
        workspace: finalWorkspace,
        model: current_model!,
        cli_path: openclawAgentInfo?.cli_path,
        custom_agent_id: openclawAgentInfo?.custom_agent_id,
        custom_workspace: isCustomWorkspace,
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

        // Push the Guid page's advanced drafts (knowledge/AutoWork/IDMM) onto
        // the new conversation before navigating, so they are live when the
        // conversation page consumes the initial message.
        await applyAdvancedConfig?.(conversation.id);

        emitter.emit('chat.history.refresh');

        const initialMessage = {
          input,
          files: files.length > 0 ? files : undefined,
        };
        if (entryPlan.sendInitialMessage) {
          sessionStorage.setItem(`openclaw_initial_message_${conversation.id}`, JSON.stringify(initialMessage));
        }

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
        preset_assistant_id,
        workspace: finalWorkspace,
        model: current_model!,
        custom_agent_id: nanobotAgentInfo?.custom_agent_id,
        custom_workspace: isCustomWorkspace,
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

        // Push the Guid page's advanced drafts (knowledge/AutoWork/IDMM) onto
        // the new conversation before navigating, so they are live when the
        // conversation page consumes the initial message.
        await applyAdvancedConfig?.(conversation.id);

        emitter.emit('chat.history.refresh');

        const initialMessage = {
          input,
          files: files.length > 0 ? files : undefined,
        };
        if (entryPlan.sendInitialMessage) {
          sessionStorage.setItem(`nanobot_initial_message_${conversation.id}`, JSON.stringify(initialMessage));
        }

        await navigate(`/conversation/${conversation.id}`);
      } catch (error: unknown) {
        console.error('Failed to create Nanobot conversation:', error);
        throw error;
      }
      return;
    }

    // Nomi path (direct selection or preset assistant with nomi as main agent)
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
          extra: {
            default_files: files,
            workspace: finalWorkspace,
            custom_workspace: isCustomWorkspace,
            preset_rules: is_preset ? preset_rules : undefined,
            preset_enabled_skills: enabled_skills_to_send,
            exclude_auto_inject_skills: excludeBuiltinSkills,
            selected_mcp_server_ids: selectedUserMcpServerIds,
            // nomi should consume the authoritative session snapshot, just
            // like team MCP does, instead of reloading only user servers from
            // the global MCP repository at runtime.
            selected_session_mcp_servers: selectedAllSessionMcpServers,
            preset_assistant_id,
            session_mode: selectedMode,
          },
        });

        if (!conversation || !conversation.id) {
          Message.error(t('conversation.createFailed'));
          return;
        }

        // Push the Guid page's advanced drafts (knowledge/AutoWork/IDMM) onto
        // the new conversation before navigating, so they are live when the
        // conversation page consumes the initial message.
        await applyAdvancedConfig?.(conversation.id);

        emitter.emit('chat.history.refresh');

        const initialMessage = {
          input,
          files: files.length > 0 ? files : undefined,
        };
        if (entryPlan.sendInitialMessage) {
          sessionStorage.setItem(`nomi_initial_message_${conversation.id}`, JSON.stringify(initialMessage));
        }

        await navigate(`/conversation/${conversation.id}`);
      } catch (error: unknown) {
        console.error('Failed to create Nomi conversation:', error);
        throw error;
      }
      return;
    }

    // Remaining agent path (ACP/remote/custom, including preset fallbacks)
    {
      // Agent-type fallback only applies to preset assistants whose primary agent
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
        preset_assistant_id,
        workspace: finalWorkspace,
        model: current_model!,
        cli_path: acpAgentInfo?.cli_path,
        custom_agent_id: acpAgentInfo?.custom_agent_id,
        custom_workspace: isCustomWorkspace,
        is_preset,
        preset_agent_type: finalEffectiveAgentType,
        preset_resources: is_preset
          ? {
              rules: preset_rules,
              enabled_skills,
              exclude_auto_inject_skills: excludeBuiltinSkills,
            }
          : undefined,
        session_mode: selectedMode,
        current_model_id: selectedAcpModel || currentAcpCachedModelInfo?.current_model_id || undefined,
        extra: {
          default_files: files,
          exclude_auto_inject_skills: excludeBuiltinSkills,
          selected_mcp_server_ids: selectedUserMcpServerIds,
          selected_session_mcp_servers: selectedSessionMcpServers,
          // Non-preset agents still forward user-selected custom skills via the
          // shared backend slot. For preset assistants this is already wired
          // through `preset_resources.enabled_skills` above.
          ...(is_preset ? {} : guidEnabledSkills?.length ? { preset_enabled_skills: guidEnabledSkills } : {}),
        },
      });

      try {
        const conversation = await ipcBridge.conversation.create.invoke(agentConversationParams);
        if (!conversation || !conversation.id) {
          console.error('Failed to create ACP conversation - conversation object is null or missing id');
          return;
        }

        await applyAdvancedConfig?.(conversation.id);

        emitter.emit('chat.history.refresh');

        const initialMessage = {
          input,
          files: files.length > 0 ? files : undefined,
        };
        if (entryPlan.sendInitialMessage) {
          sessionStorage.setItem(`acp_initial_message_${conversation.id}`, JSON.stringify(initialMessage));
        }

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
    is_presetAgent,
    selectedMode,
    selectedAcpModel,
    currentAcpCachedModelInfo,
    current_model,
    findAgentByKey,
    getEffectiveAgentType,
    resolvePresetRulesAndSkills,
    resolveEnabledSkills,
    resolveDisabledBuiltinSkills,
    guidDisabledBuiltinSkills,
    guidEnabledSkills,
    availableMcpServers,
    selectedMcpServerIds,
    applyAdvancedConfig,
    autoWork,
    orchestrationMode,
    navigate,
    t,
  ]);

  const sendMessageHandler = useCallback(() => {
    if (loading || sendingRef.current) return;
    sendingRef.current = true;
    setLoading(true);
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
  ]);

  // Calculate button disabled state
  const isButtonDisabled = loading || !input.trim();

  return {
    handleSend,
    sendMessageHandler,
    isButtonDisabled,
  };
};
