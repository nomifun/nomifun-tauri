/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import { ipcBridge } from '@/common';
import type { IMcpServer } from '@/common/config/storage';
import { resolveLocaleKey } from '@/common/utils';

import { useInputFocusRing } from '@/renderer/hooks/chat/useInputFocusRing';
import { isSubmitGesture } from '@/renderer/hooks/chat/useCompositionInput';
import { useConfig } from '@/renderer/hooks/config/useConfig';
import { resolveExtensionAssetUrl } from '@/renderer/utils/platform';
import { CUSTOM_AVATAR_IMAGE_MAP } from './constants';
import AgentPillBar from './components/AgentPillBar';
import ComposerEntryStrip, { type GuidActiveSkill } from './components/ComposerEntryStrip';
import GuidAssistantEditorHost from './components/GuidAssistantEditorHost';
import { AgentPillBarSkeleton } from './components/GuidSkeleton';
import GuidActionRow from './components/GuidActionRow';
import GuidCompanionPosterPreview from './components/GuidCompanionPosterPreview';
import GuidInputCard from './components/GuidInputCard';
import GuidModelSelector from './components/GuidModelSelector';
import GuidResourceCards from './components/GuidResourceCards';
import MentionDropdown, { MentionSelectorBadge } from './components/MentionDropdown';
import QuickActionButtons from './components/QuickActionButtons';
import SummonDrawer from './components/SummonDrawer';
import FeedbackReportModal from '@/renderer/components/settings/SettingsModal/contents/FeedbackReportModal';
import AutoWorkControl from '@/renderer/pages/conversation/components/AutoWorkControl';
import IdmmControl from '@/renderer/pages/conversation/components/IdmmControl';
import KnowledgeControl from '@/renderer/pages/conversation/components/KnowledgeControl';
import { useGuidAgentSelection } from './hooks/useGuidAgentSelection';
import { useGuidAdvancedConfig } from './hooks/useGuidAdvancedConfig';
import { autoWorkStartDisabled, isAutoWorkEntry } from './hooks/autoWorkEntry';
import { useGuidInput } from './hooks/useGuidInput';
import { useGuidMention } from './hooks/useGuidMention';
import { useGuidModelSelection } from './hooks/useGuidModelSelection';
import { useGuidSend } from './hooks/useGuidSend';
import { useTypewriterPlaceholder } from './hooks/useTypewriterPlaceholder';
import { ensureBackendMcpCatalog } from '@/renderer/hooks/mcp/catalog';
import { resolveAgentLogo } from '@/renderer/utils/model/agentLogo';
import { ConfigProvider, Message } from '@arco-design/web-react';
import React, { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useLocation, useNavigate } from 'react-router-dom';
import { mutate as swrMutate } from 'swr';
import type { Assistant } from '@/common/types/agent/assistantTypes';
import styles from './index.module.css';

const GuidPage: React.FC = () => {
  const { t, i18n } = useTranslation();
  const navigate = useNavigate();
  const location = useLocation();
  const guidContainerRef = useRef<HTMLDivElement>(null);
  const openAssistantDetailsRef = useRef<(() => void) | null>(null);
  const { activeBorderColor, inactiveBorderColor, activeShadow } = useInputFocusRing();

  const localeKey = resolveLocaleKey(i18n.language);
  const [showFeedbackModal, setShowFeedbackModal] = useState(false);

  // --- Drawer state ---
  const [drawerOpen, setDrawerOpen] = useState(false);
  const [drawerMode, setDrawerMode] = useState<'assistant' | 'skills'>('assistant');

  // --- Orchestration mode (homepage 智能编排 on-ramp) ---
  // When on, the composer's send button starts a conversation-hosted
  // orchestration run instead of a normal chat send. Mutually exclusive with
  // the「召唤助手」preset-agent flow (toggling one resets the other).
  const [orchestrationMode, setOrchestrationMode] = useState(false);

  // --- Skills state ---
  // All available skills (builtin auto-injected + user-imported custom) merged
  // into one catalog for the action-row menu. Auto-injected skills default to
  // checked; the rest are opt-in per conversation (or pre-checked when the
  // active assistant declares them in `enabled_skills`).
  const [allSkills, setAllSkills] = useState<Array<{ name: string; description: string; isAuto: boolean }>>([]);
  const [guidDisabledBuiltinSkills, setGuidDisabledBuiltinSkills] = useState<string[] | undefined>(undefined);
  const [guidEnabledSkills, setGuidEnabledSkills] = useState<string[] | undefined>(undefined);
  const [availableMcpServers, setAvailableMcpServers] = useState<IMcpServer[]>([]);
  const [guidSelectedMcpServerIds, setGuidSelectedMcpServerIds] = useState<number[] | undefined>(undefined);

  useEffect(() => {
    Promise.all([ipcBridge.fs.listBuiltinAutoSkills.invoke(), ipcBridge.fs.listAvailableSkills.invoke()])
      .then(([autoSkills, availableSkills]) => {
        const autoNames = new Set(autoSkills.map((s) => s.name));
        const merged: Array<{ name: string; description: string; isAuto: boolean }> = [
          ...autoSkills.map((s) => ({ name: s.name, description: s.description, isAuto: true })),
          ...availableSkills
            .filter((s) => !autoNames.has(s.name))
            .map((s) => ({ name: s.name, description: s.description, isAuto: false })),
        ];
        setAllSkills(merged);
      })
      .catch(() => setAllSkills([]));
  }, []);

  useEffect(() => {
    void ensureBackendMcpCatalog()
      .then(({ allServers }) => {
        setAvailableMcpServers(allServers);
        setGuidSelectedMcpServerIds((prev) => prev ?? []);
      })
      .catch((error) => {
        console.error('[GuidPage] Failed to load MCP catalog:', error);
        setAvailableMcpServers([]);
        setGuidSelectedMcpServerIds((prev) => prev ?? []);
      });
  }, []);

  const handleToggleSkill = useCallback((skillName: string, isAuto: boolean) => {
    if (isAuto) {
      setGuidDisabledBuiltinSkills((prev) => {
        const list = prev ?? [];
        return list.includes(skillName) ? list.filter((s) => s !== skillName) : [...list, skillName];
      });
    } else {
      setGuidEnabledSkills((prev) => {
        const list = prev ?? [];
        return list.includes(skillName) ? list.filter((s) => s !== skillName) : [...list, skillName];
      });
    }
  }, []);

  const handleToggleMcpServer = useCallback((serverId: number) => {
    setGuidSelectedMcpServerIds((prev) => {
      const current = prev ?? [];
      return current.includes(serverId) ? current.filter((id) => id !== serverId) : [...current, serverId];
    });
  }, []);

  // --- Hooks ---
  // Only nomi uses this provider-based model picker now (Gemini runs as a
  // regular ACP backend with its own model selector).
  const modelSelection = useGuidModelSelection('nomi');

  const navState = location.state as { resetAssistant?: boolean; selectedAgentKey?: string } | null;
  const resetAssistantRequested = navState?.resetAssistant === true;
  const preselectAgentKey = navState?.selectedAgentKey;
  const agentSelection = useGuidAgentSelection({
    modelList: modelSelection.modelList,
    isGoogleAuth: modelSelection.isGoogleAuth,
    localeKey,
    resetAssistant: resetAssistantRequested,
    preselectAgentKey,
    locationKey: location.key,
  });

  const guidInput = useGuidInput({
    locationState: location.state as { workspace?: string } | null,
  });

  // Advanced per-conversation drafts (knowledge mounts / AutoWork / IDMM) —
  // collected up front and applied right after the conversation is created.
  const advancedConfig = useGuidAdvancedConfig();

  const mention = useGuidMention({
    availableAgents: agentSelection.availableAgents,
    customAgentAvatarMap: agentSelection.customAgentAvatarMap,
    selectedAgentKey: agentSelection.selectedAgentKey,
    setSelectedAgentKey: agentSelection.setSelectedAgentKey,
    setInput: guidInput.setInput,
    selectedAgentInfo: agentSelection.selectedAgentInfo,
  });

  const send = useGuidSend({
    // Input state
    input: guidInput.input,
    setInput: guidInput.setInput,
    files: guidInput.files,
    setFiles: guidInput.setFiles,
    dir: guidInput.dir,
    setDir: guidInput.setDir,
    setLoading: guidInput.setLoading,
    loading: guidInput.loading,

    // Agent state
    selectedAgent: agentSelection.selectedAgent,
    selectedAgentKey: agentSelection.selectedAgentKey,
    selectedAgentInfo: agentSelection.selectedAgentInfo,
    is_presetAgent: agentSelection.is_presetAgent,
    selectedMode: agentSelection.selectedMode,
    selectedAcpModel: agentSelection.selectedAcpModel,
    currentAcpCachedModelInfo: agentSelection.currentAcpCachedModelInfo,
    current_model: modelSelection.current_model,

    // Agent helpers
    findAgentByKey: agentSelection.findAgentByKey,
    getEffectiveAgentType: agentSelection.getEffectiveAgentType,
    resolvePresetRulesAndSkills: agentSelection.resolvePresetRulesAndSkills,
    resolveEnabledSkills: agentSelection.resolveEnabledSkills,
    resolveDisabledBuiltinSkills: agentSelection.resolveDisabledBuiltinSkills,
    guidDisabledBuiltinSkills,
    guidEnabledSkills,
    availableMcpServers,
    selectedMcpServerIds: guidSelectedMcpServerIds,
    currentEffectiveAgentInfo: agentSelection.currentEffectiveAgentInfo,
    isGoogleAuth: modelSelection.isGoogleAuth,
    applyAdvancedConfig: advancedConfig.applyToConversation,
    autoWork: advancedConfig.autoWork,

    // Orchestration entry
    orchestrationMode,

    // Mention state reset
    setMentionOpen: mention.setMentionOpen,
    setMentionQuery: mention.setMentionQuery,
    setMentionSelectorOpen: mention.setMentionSelectorOpen,
    setMentionActiveIndex: mention.setMentionActiveIndex,

    // Navigation
    navigate,
    t,
  });

  // --- Coordinated handlers (depend on multiple hooks) ---
  const handleInputChange = useCallback(
    (value: string) => {
      guidInput.setInput(value);
      const match = value.match(mention.mentionMatchRegex);
      // 首页不根据输入 @ 呼起 mention 列表，占位符里的 @agent 仅为提示，选 agent 用顶部栏或下拉手动选
      if (match) {
        mention.setMentionQuery(match[1]);
        mention.setMentionOpen(false);
      } else {
        mention.setMentionQuery(null);
        mention.setMentionOpen(false);
      }
    },
    [mention.mentionMatchRegex, guidInput.setInput, mention.setMentionQuery, mention.setMentionOpen]
  );

  const [sendKeyPref] = useConfig('chat.sendKey');
  const sendKey = sendKeyPref ?? 'enter';

  const handleInputKeyDown = useCallback(
    (event: React.KeyboardEvent) => {
      if (
        (mention.mentionOpen || mention.mentionSelectorOpen) &&
        (event.key === 'ArrowDown' || event.key === 'ArrowUp')
      ) {
        event.preventDefault();
        if (mention.filteredMentionOptions.length === 0) return;
        mention.setMentionActiveIndex((prev) => {
          if (event.key === 'ArrowDown') {
            return (prev + 1) % mention.filteredMentionOptions.length;
          }
          return (prev - 1 + mention.filteredMentionOptions.length) % mention.filteredMentionOptions.length;
        });
        return;
      }
      if ((mention.mentionOpen || mention.mentionSelectorOpen) && event.key === 'Enter' && !event.shiftKey) {
        event.preventDefault();
        if (mention.filteredMentionOptions.length > 0) {
          const query = mention.mentionQuery?.toLowerCase();
          const exactMatch = query
            ? mention.filteredMentionOptions.find(
                (option) => option.label.toLowerCase() === query || option.tokens.has(query)
              )
            : undefined;
          const selected =
            exactMatch ||
            mention.filteredMentionOptions[mention.mentionActiveIndex] ||
            mention.filteredMentionOptions[0];
          if (selected) {
            mention.selectMentionAgent(selected.key);
            return;
          }
        }
        mention.setMentionOpen(false);
        mention.setMentionQuery(null);
        mention.setMentionSelectorOpen(false);
        mention.setMentionActiveIndex(0);
        return;
      }
      if (mention.mentionOpen && (event.key === 'Backspace' || event.key === 'Delete') && !mention.mentionQuery) {
        mention.setMentionOpen(false);
        mention.setMentionQuery(null);
        mention.setMentionActiveIndex(0);
        return;
      }
      if (
        !mention.mentionOpen &&
        mention.mentionSelectorVisible &&
        !guidInput.input.trim() &&
        (event.key === 'Backspace' || event.key === 'Delete')
      ) {
        event.preventDefault();
        mention.setMentionSelectorVisible(false);
        mention.setMentionSelectorOpen(false);
        mention.setMentionActiveIndex(0);
        return;
      }
      if ((mention.mentionOpen || mention.mentionSelectorOpen) && event.key === 'Escape') {
        event.preventDefault();
        mention.setMentionOpen(false);
        mention.setMentionQuery(null);
        mention.setMentionSelectorOpen(false);
        mention.setMentionActiveIndex(0);
        return;
      }
      if (isSubmitGesture(event, sendKey)) {
        event.preventDefault();
        if (!guidInput.input.trim()) return;
        send.sendMessageHandler();
      }
    },
    [mention, guidInput.input, send.sendMessageHandler, sendKey]
  );

  const handleSelectAgentFromPillBar = useCallback(
    (key: string) => {
      setOrchestrationMode(false);
      agentSelection.setSelectedAgentKey(key);
      mention.setMentionOpen(false);
      mention.setMentionQuery(null);
      mention.setMentionSelectorOpen(false);
      mention.setMentionActiveIndex(0);
    },
    [
      agentSelection.setSelectedAgentKey,
      mention.setMentionOpen,
      mention.setMentionQuery,
      mention.setMentionSelectorOpen,
      mention.setMentionActiveIndex,
    ]
  );

  const handleSelectAssistant = useCallback(
    (assistantId: string) => {
      setOrchestrationMode(false);
      agentSelection.setSelectedAgentKey(assistantId);
      mention.setMentionOpen(false);
      mention.setMentionQuery(null);
      mention.setMentionSelectorOpen(false);
      mention.setMentionActiveIndex(0);
    },
    [
      agentSelection.setSelectedAgentKey,
      mention.setMentionOpen,
      mention.setMentionQuery,
      mention.setMentionSelectorOpen,
      mention.setMentionActiveIndex,
    ]
  );

  // Typewriter placeholder
  const typewriterPlaceholder = useTypewriterPlaceholder(t('conversation.welcome.placeholder'));
  const selectedAssistantRecord = useMemo(() => {
    if (!agentSelection.is_presetAgent || !agentSelection.selectedAgentInfo?.custom_agent_id) return undefined;
    const selectedId = agentSelection.selectedAgentInfo.custom_agent_id;
    const strippedId = selectedId.replace(/^builtin-/, '');
    const candidates = new Set([selectedId, `builtin-${strippedId}`, strippedId]);
    return agentSelection.assistants.find((item) => candidates.has(item.id));
  }, [agentSelection.assistants, agentSelection.is_presetAgent, agentSelection.selectedAgentInfo?.custom_agent_id]);

  // Sync disabledBuiltinSkills + enabledSkills from preset assistant config
  useEffect(() => {
    if (agentSelection.is_presetAgent && selectedAssistantRecord) {
      setGuidDisabledBuiltinSkills(selectedAssistantRecord.disabled_builtin_skills ?? []);
      setGuidEnabledSkills(selectedAssistantRecord.enabled_skills ?? []);
    } else {
      setGuidDisabledBuiltinSkills(undefined);
      setGuidEnabledSkills(undefined);
    }
  }, [agentSelection.is_presetAgent, selectedAssistantRecord]);

  const heroTitle = useMemo(() => {
    if (!agentSelection.is_presetAgent) return t('conversation.welcome.title');
    const i18nName = selectedAssistantRecord?.name_i18n?.[localeKey];
    if (i18nName) return i18nName;
    return mention.selectedAgentLabel || t('conversation.welcome.title');
  }, [agentSelection.is_presetAgent, selectedAssistantRecord, localeKey, mention.selectedAgentLabel, t]);
  const selectedAssistantAvatar = useMemo(() => {
    if (!agentSelection.is_presetAgent) return null;
    const selectedId = agentSelection.selectedAgentInfo?.custom_agent_id;
    const strippedId = selectedId?.replace(/^builtin-/, '');
    const candidates = new Set(selectedId && strippedId ? [selectedId, `builtin-${strippedId}`, strippedId] : []);
    const selectedAssistant = agentSelection.assistants.find((item) => candidates.has(item.id));
    const avatarValue = selectedAssistant?.avatar?.trim() || agentSelection.selectedAgentInfo?.avatar?.trim();
    if (!avatarValue) return { kind: 'icon' as const };
    const mappedAvatar = CUSTOM_AVATAR_IMAGE_MAP[avatarValue];
    const resolvedAvatar = resolveExtensionAssetUrl(avatarValue);
    const avatarImage = mappedAvatar || resolvedAvatar;
    const isImageAvatar = Boolean(
      avatarImage &&
      (/\.(svg|png|jpe?g|webp|gif)$/i.test(avatarImage) || /^(https?:|file:\/\/|data:|\/)/i.test(avatarImage))
    );
    if (isImageAvatar && avatarImage) {
      return { kind: 'image' as const, value: avatarImage };
    }
    return { kind: 'emoji' as const, value: avatarValue };
  }, [
    agentSelection.assistants,
    agentSelection.is_presetAgent,
    agentSelection.selectedAgentInfo?.avatar,
    agentSelection.selectedAgentInfo?.custom_agent_id,
  ]);
  // Reset guid-local UI state before paint so same-route navigations do not
  // briefly show the previous draft or preset assistant layout.
  useLayoutEffect(() => {
    guidInput.setInput('');
    guidInput.setFiles([]);
    guidInput.setLoading(false);
    setOrchestrationMode(false);
    if (!(location.state as { workspace?: string } | null)?.workspace) {
      guidInput.setDir('');
    }
    advancedConfig.reset();
  }, [
    guidInput.setDir,
    guidInput.setFiles,
    guidInput.setInput,
    guidInput.setLoading,
    advancedConfig.reset,
    location.key,
    location.state,
  ]);

  // Clear resetAssistant from location.state after the hook has consumed it,
  // so that re-renders don't re-trigger the reset logic.
  //
  // Must go through React Router's navigate — raw window.history.replaceState
  // with `location.pathname` would write the HashRouter virtual path (e.g.
  // '/guid') into the browser's real URL and strip the leading '#'. On the
  // next hard reload, the browser would then request '/guid' directly from
  // the dev server (which has no SPA fallback) and 404.
  useEffect(() => {
    if (!resetAssistantRequested && !preselectAgentKey) return;
    navigate(`${location.pathname}${location.search}${location.hash}`, { replace: true, state: null });
  }, [resetAssistantRequested, preselectAgentKey, location.pathname, location.search, location.hash, navigate]);

  const currentPresetAgentType = selectedAssistantRecord?.preset_agent_type || 'gemini';
  // Mirrors AssistantEditDrawer's Main Agent options — detected execution
  // engines from AgentPillBar's data source, so avatars resolve the same way.
  const agentSwitcherItems = useMemo(() => {
    if (!agentSelection.availableAgents) return [];
    return agentSelection.availableAgents
      .filter((a) => !a.is_preset && a.agent_type !== 'remote')
      .map((a) => {
        const key = a.backend || a.agent_type;
        const extensionAvatar = a.isExtension ? resolveExtensionAssetUrl(a.avatar) : undefined;
        const logo =
          extensionAvatar ||
          resolveAgentLogo({
            icon: a.icon,
            backend: a.backend || a.agent_type,
            custom_agent_id: a.custom_agent_id,
            isExtension: a.isExtension,
          });
        return {
          key,
          label: a.name,
          logo,
          isCurrent: key === currentPresetAgentType,
          isExtension: a.isExtension,
        };
      });
  }, [agentSelection.availableAgents, currentPresetAgentType]);

  const effectiveAgentRecord = useMemo(() => {
    return agentSelection.availableAgents?.find(
      (agent) =>
        !agent.is_preset && (agent.backend || agent.agent_type) === agentSelection.currentEffectiveAgentInfo.agent_type
    );
  }, [agentSelection.availableAgents, agentSelection.currentEffectiveAgentInfo.agent_type]);

  const effectiveAgentLogo = useMemo(
    () =>
      resolveAgentLogo({
        icon: effectiveAgentRecord?.icon,
        backend: effectiveAgentRecord?.backend || agentSelection.currentEffectiveAgentInfo.agent_type,
        custom_agent_id: effectiveAgentRecord?.custom_agent_id,
        isExtension: effectiveAgentRecord?.isExtension,
      }),
    [effectiveAgentRecord, agentSelection.currentEffectiveAgentInfo.agent_type]
  );
  const handlePresetAgentTypeSwitch = useCallback(
    async (nextType: string) => {
      // Only preset assistants (is_preset=true) expose `custom_agent_id` here, so this id is
      // always backed by the `/api/assistants` store. ACP custom agents are a separate store
      // (`ipcBridge.acpConversation.updateCustomAgent`) and do not carry `preset_agent_type`.
      // See commit 13858579d on main for the legacy single-store fix that this split already covers.
      const assistantId = agentSelection.selectedAgentInfo?.custom_agent_id;
      if (!assistantId || nextType === currentPresetAgentType) return;
      try {
        // Optimistically patch the shared `assistants.list` SWR cache so the hero
        // avatar/logo reflect the new preset_agent_type on the same frame as the
        // click. Without this, downstream memos (selectedAssistantRecord →
        // currentEffectiveAgentInfo → effectiveAgentLogo) lag a network roundtrip
        // behind the user action.
        await swrMutate(
          'assistants.list',
          (prev: Assistant[] | undefined) =>
            prev?.map((a) => (a.id === assistantId ? { ...a, preset_agent_type: nextType } : a)),
          { revalidate: false }
        );
        await ipcBridge.assistants.update.invoke({ id: assistantId, preset_agent_type: nextType });
        await Promise.all([swrMutate('assistants.list'), agentSelection.refreshCustomAgents()]);
        const agent_name =
          agentSelection.availableAgents?.find((a) => (a.backend || a.agent_type) === nextType)?.name || nextType;
        Message.success(t('guid.switchedToAgent', { agent: agent_name }));
      } catch (error) {
        console.error('[GuidPage] Failed to switch preset agent type:', error);
        Message.error(t('common.failed', { defaultValue: 'Failed' }));
      }
    },
    [agentSelection, currentPresetAgentType, t]
  );

  // Resolve the effective agent type once — covers both direct selection and preset assistants
  const effectiveAgentType = agentSelection.is_presetAgent
    ? agentSelection.currentEffectiveAgentInfo.agent_type
    : agentSelection.selectedAgent;

  // Agents that use configured model providers instead of ACP probe-based models.
  // Only nomi now — Gemini runs as a regular ACP backend with ACP-cached models.
  const PROVIDER_BASED_AGENTS = new Set(['nomi']);
  const isGeminiMode =
    PROVIDER_BASED_AGENTS.has(effectiveAgentType) &&
    (!agentSelection.is_presetAgent || agentSelection.currentEffectiveAgentInfo.isAvailable);

  // Build the mention dropdown node
  const mentionDropdownNode = (
    <MentionDropdown
      menuRef={mention.mentionMenuRef}
      options={mention.filteredMentionOptions}
      selectedKey={mention.mentionMenuSelectedKey}
      onSelect={mention.selectMentionAgent}
    />
  );

  // Build the model selector node — a plain single-select model picker.
  const modelSelectorNode = (
    <GuidModelSelector
      isGeminiMode={isGeminiMode}
      modelList={modelSelection.modelList}
      current_model={modelSelection.current_model}
      setCurrentModel={modelSelection.setCurrentModel}
      currentAcpCachedModelInfo={agentSelection.currentAcpCachedModelInfo}
      selectedAcpModel={agentSelection.selectedAcpModel}
      setSelectedAcpModel={agentSelection.setSelectedAcpModel}
    />
  );

  // Advanced drafts — the same controls as the conversation header, in draft
  // mode (collected locally, applied right after the conversation is created).
  // Keyed by location.key so same-route navigations (which reset the drafts in
  // the layout effect above) also remount the controls and re-run their
  // mount-time seeding (e.g. IDMM's global default steering prompt).
  const advancedControlsNode = (
    <>
      <AutoWorkControl
        key={`autowork-${location.key}`}
        draft={{ value: advancedConfig.autoWork, onChange: advancedConfig.setAutoWork }}
        applyNote={t('guid.advanced.applyNote')}
      />
      <IdmmControl
        key={`idmm-${location.key}`}
        draft={{ value: advancedConfig.idmm, onChange: advancedConfig.setIdmm }}
        applyNote={t('guid.advanced.applyNote')}
      />
      <KnowledgeControl
        key={`knowledge-${location.key}`}
        draft={{ value: advancedConfig.knowledge, onChange: advancedConfig.setKnowledge }}
        applyNote={t('guid.advanced.applyNote')}
      />
    </>
  );

  // Build the action row
  // When AutoWork is enabled (with a tag) the primary button becomes a
  // "Start AutoWork" action: clickable without typed input, and it creates the
  // session + starts AutoWork without sending a first message (see planGuidEntry).
  const isAutoWorkMode = isAutoWorkEntry(advancedConfig.autoWork);
  const actionRowNode = (
    <GuidActionRow
      files={guidInput.files}
      onFilesUploaded={guidInput.handleFilesUploaded}
      modelSelectorNode={modelSelectorNode}
      selectedAgent={agentSelection.selectedAgent}
      effectiveModeAgent={agentSelection.currentEffectiveAgentInfo.agent_type}
      selectedMode={agentSelection.selectedMode}
      onModeSelect={agentSelection.setSelectedMode}
      is_presetAgent={agentSelection.is_presetAgent}
      selectedAgentInfo={agentSelection.selectedAgentInfo}
      assistants={agentSelection.assistants}
      localeKey={localeKey}
      onClosePresetTag={() => agentSelection.setSelectedAgentKey(agentSelection.defaultAgentKey)}
      agentLogo={effectiveAgentLogo}
      agentSwitcherItems={agentSwitcherItems}
      onAgentSwitch={(key) => {
        handlePresetAgentTypeSwitch(key).catch((err) => console.error('Failed to switch agent type:', err));
      }}
      mcpServers={availableMcpServers}
      selectedMcpServerIds={guidSelectedMcpServerIds ?? []}
      onToggleMcpServer={handleToggleMcpServer}
      hidePresetTag
      loading={guidInput.loading}
      autoWorkMode={isAutoWorkMode && !orchestrationMode}
      orchestrationMode={orchestrationMode}
      isButtonDisabled={
        // 智能编排 takes precedence over AutoWork: orchestration needs a typed goal,
        // so it always uses the normal send-disabled (typed-input) predicate.
        isAutoWorkMode && !orchestrationMode
          ? autoWorkStartDisabled(guidInput.loading, advancedConfig.autoWork)
          : send.isButtonDisabled
      }
      onSend={send.sendMessageHandler}
    />
  );

  // --- Active skills (for ComposerEntryStrip badge + summary popover) ---
  const activeSkills = useMemo<GuidActiveSkill[]>(() => {
    const disabled = guidDisabledBuiltinSkills ?? [];
    const enabled = guidEnabledSkills ?? [];
    return allSkills.filter((s) => (s.isAuto ? !disabled.includes(s.name) : enabled.includes(s.name)));
  }, [allSkills, guidDisabledBuiltinSkills, guidEnabledSkills]);
  const activeSkillCount = activeSkills.length;

  const handleOpenSkillsDrawer = useCallback(() => {
    setDrawerMode('skills');
    setDrawerOpen(true);
  }, []);

  const handleRegisterOpenDetails = useCallback((openDetails: (() => void) | null) => {
    openAssistantDetailsRef.current = openDetails;
  }, []);

  return (
    <ConfigProvider getPopupContainer={() => guidContainerRef.current || document.body}>
      <div ref={guidContainerRef} className={styles.guidContainer}>
        {/* Advanced controls (AutoWork / IDMM / Knowledge / MultiAgent) hang in
            the content area's top-right corner — mirroring the active-session
            ChatLayout header placement, and freeing the input box's bottom row.
            Desktop only (hidden on mobile via CSS), matching the session header. */}
        <div className={styles.guidAdvancedControls}>{advancedControlsNode}</div>
        <div className={styles.guidPrimaryStage}>
          <div className={styles.guidLayout}>
            <div className={styles.heroHeader}>
              <p className='text-2xl font-semibold mb-0 text-0 text-center'>
                {t('conversation.welcome.title')}
              </p>
            </div>

            {agentSelection.availableAgents === undefined ? (
              <AgentPillBarSkeleton />
            ) : agentSelection.availableAgents.length > 0 ? (
              <AgentPillBar
                availableAgents={agentSelection.availableAgents}
                selectedAgentKey={agentSelection.selectedAgentKey}
                getAgentKey={agentSelection.getAgentKey}
                onSelectAgent={handleSelectAgentFromPillBar}
                suppressSelectionAnimation={resetAssistantRequested}
              />
            ) : null}

            <GuidInputCard
              input={guidInput.input}
              onInputChange={handleInputChange}
              onKeyDown={handleInputKeyDown}
              onPaste={guidInput.onPaste}
              onFocus={guidInput.handleTextareaFocus}
              onBlur={guidInput.handleTextareaBlur}
              placeholder={`${mention.selectedAgentLabel}, ${typewriterPlaceholder || t('conversation.welcome.placeholder')}`}
              isInputActive={guidInput.isInputFocused}
              isFileDragging={guidInput.isFileDragging}
              activeBorderColor={activeBorderColor}
              inactiveBorderColor={inactiveBorderColor}
              activeShadow={activeShadow}
              dragHandlers={guidInput.dragHandlers}
              mentionOpen={mention.mentionOpen}
              mentionSelectorBadge={
                <MentionSelectorBadge
                  visible={mention.mentionSelectorVisible}
                  open={mention.mentionSelectorOpen}
                  onOpenChange={mention.setMentionSelectorOpen}
                  agentLabel={mention.selectedAgentLabel}
                  mentionMenu={mentionDropdownNode}
                  onResetQuery={() => mention.setMentionQuery(null)}
                />
              }
              mentionDropdown={mentionDropdownNode}
              files={guidInput.files}
              onRemoveFile={guidInput.handleRemoveFile}
              actionRow={actionRowNode}
              workspaceDir={guidInput.dir}
              onSelectWorkspace={(dir) => guidInput.setDir(dir)}
              onClearWorkspace={() => guidInput.setDir('')}
              entryStrip={
                <ComposerEntryStrip
                  isPresetAgent={agentSelection.is_presetAgent}
                  assistantLabel={heroTitle !== t('conversation.welcome.title') ? heroTitle : undefined}
                  assistantAvatar={selectedAssistantAvatar ?? undefined}
                  onSummon={() => { setOrchestrationMode(false); setDrawerMode('assistant'); setDrawerOpen(true); }}
                  onAdjustSkills={handleOpenSkillsDrawer}
                  onFree={() => { setOrchestrationMode(false); agentSelection.setSelectedAgentKey(agentSelection.defaultAgentKey); }}
                  onOrchestrate={() =>
                    setOrchestrationMode((prev) => {
                      const next = !prev;
                      // Mutually exclusive with the summon flow — drop back to the
                      // default (free) agent when entering orchestration mode.
                      if (next) agentSelection.setSelectedAgentKey(agentSelection.defaultAgentKey);
                      return next;
                    })
                  }
                  isOrchestrationMode={orchestrationMode}
                  activeSkillCount={activeSkillCount}
                  activeSkills={activeSkills}
                />
              }
            />

            <GuidResourceCards />

            {/* Editor host (modals + example prompts + fallback notice) */}
            <GuidAssistantEditorHost
              assistants={agentSelection.assistants}
              localeKey={localeKey}
              selectedAgentKey={agentSelection.selectedAgentKey}
              selectedAgentInfo={agentSelection.selectedAgentInfo}
              currentEffectiveAgentInfo={agentSelection.currentEffectiveAgentInfo}
              onSetInput={guidInput.setInput}
              onFocusInput={guidInput.handleTextareaFocus}
              onRegisterOpenDetails={handleRegisterOpenDetails}
            />
          </div>
        </div>

        <div className={styles.guidDiscoveryArea}>
          <GuidCompanionPosterPreview />
        </div>

        {/* SummonDrawer (right-side) */}
        <SummonDrawer
          visible={drawerOpen}
          mode={drawerMode}
          onModeChange={setDrawerMode}
          onClose={() => setDrawerOpen(false)}
          assistants={agentSelection.assistants}
          localeKey={localeKey}
          onSelectAssistant={(id) => { handleSelectAssistant(`custom:${id}`); setDrawerOpen(false); }}
          onFree={() => { setOrchestrationMode(false); agentSelection.setSelectedAgentKey(agentSelection.defaultAgentKey); setDrawerOpen(false); }}
          allSkills={allSkills}
          enabledSkills={guidEnabledSkills ?? []}
          disabledBuiltinSkills={guidDisabledBuiltinSkills ?? []}
          onToggleSkill={handleToggleSkill}
        />

        <QuickActionButtons
          onOpenBugReport={() => setShowFeedbackModal(true)}
          inactiveBorderColor={inactiveBorderColor}
          activeShadow={activeShadow}
        />
        <FeedbackReportModal visible={showFeedbackModal} onCancel={() => setShowFeedbackModal(false)} />
      </div>
    </ConfigProvider>
  );
};

export default GuidPage;
