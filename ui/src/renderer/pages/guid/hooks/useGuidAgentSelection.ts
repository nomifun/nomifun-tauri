/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import { DEFAULT_CODEX_MODELS } from '@/common/types/codex/codexModels';
import { CODEX_MODE_NATIVE_FULL_ACCESS, normalizeCodexMode } from '@/common/types/codex/codexModes';
import type { IProvider } from '@/common/config/storage';
import { configService } from '@/common/config/configService';
import type { Preset, PresetReference } from '@/common/types/agent/presetTypes';
import type { AcpSessionModes } from '@/common/types/platform/acpTypes';
import type { AcpModelInfo, AvailableAgent, EffectiveAgentInfo } from '../types';
import {
  DETECTED_AGENTS_SWR_KEY,
  fetchDetectedAgents,
  type AgentMetadata,
  type AgentSource,
} from '@/renderer/utils/model/agentTypes';
import { getAgentModes, getFullAutoMode } from '@/renderer/utils/model/agentModes';
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import useSWR from 'swr';
import { savePreferredMode, savePreferredModelId, getAgentKey as getAgentKeyUtil } from './agentSelectionUtils';
import { usePresetResolver } from './usePresetResolver';
import { useAgentAvailability } from './useAgentAvailability';
import { useCustomAgentsLoader } from './useCustomAgentsLoader';

export type GuidAgentSelectionResult = {
  selectedAgentKey: string;
  setSelectedAgentKey: (key: string) => void;
  defaultAgentKey: string;
  selectedAgent: string;
  selectedAgentInfo: AvailableAgent | undefined;
  is_presetAgent: boolean;
  availableAgents: AvailableAgent[] | undefined;
  /** Backend-merged preset catalog: builtin + user + extension. */
  presets: Preset[];
  /** User-defined ACP engine rows (agent_source === 'custom') from the backend. */
  customAgents: AgentMetadata[];
  selectedMode: string;
  setSelectedMode: React.Dispatch<React.SetStateAction<string>>;
  selectedAcpModel: string | null;
  setSelectedAcpModel: React.Dispatch<React.SetStateAction<string | null>>;
  currentAcpCachedModelInfo: AcpModelInfo | null;
  currentEffectiveAgentInfo: EffectiveAgentInfo;
  getAgentKey: (agent: {
    agent_type: string;
    agent_source?: AgentSource;
    backend?: string;
    id?: string;
  }) => string;
  findAgentByKey: (key: string) => AvailableAgent | undefined;
  resolvePresetAgentType: (
    agentInfo: { agent_type: string; backend?: string; preset_id?: PresetReference } | undefined
  ) => string;
  isMainAgentAvailable: (agent_type: string) => boolean;
  getEffectiveAgentType: (
    agentInfo: { agent_type: string; backend?: string } | undefined
  ) => EffectiveAgentInfo;
  refreshCustomAgents: () => Promise<void>;
  customAgentAvatarMap: Map<string, string | undefined>;
};

/**
 * Resolve the default session_mode for a given backend.
 *
 * Priority:
 *   1. Handshake `available_modes.current_mode_id` from `/api/agents`
 *   2. First entry of handshake `available_modes`
 *   3. First entry of the static `AGENT_MODES` table
 *   4. Literal `'default'` (legacy fallback — only correct for claude/qwen/gemini/nomi)
 *
 * This mirrors the runtime fallback inside `AgentModeSelector` so the
 * parent-held `selectedMode` stays in sync with what the UI shows.
 */
function resolveDefaultMode(backend: string | undefined, agents: AgentMetadata[] | undefined): string {
  if (!backend) return 'default';

  const matched = agents?.find((a) => (a.backend ?? a.agent_type) === backend);
  const handshakeModes = matched?.handshake?.available_modes as AcpSessionModes | undefined;
  if (handshakeModes) {
    if (handshakeModes.current_mode_id) return handshakeModes.current_mode_id;
    const first = handshakeModes.available_modes?.[0]?.id;
    if (first) return first;
  }

  const staticModes = getAgentModes(backend);
  if (staticModes.length > 0) return staticModes[0].value;

  return 'default';
}

type UseGuidAgentSelectionOptions = {
  modelList: IProvider[];
  isGoogleAuth: boolean;
  localeKey: string;
  resetPreset?: boolean;
  /** Pre-select a specific agent by key (e.g. from "Go to Chat" deep-links). */
  preselectAgentKey?: string;
  /** React Router location.key — changes on every navigation, used to detect new resets. */
  locationKey?: string;
};

/**
 * Hook that manages agent selection, availability, and preset preset logic.
 */
export const useGuidAgentSelection = ({
  modelList,
  isGoogleAuth,
  localeKey: _localeKey,
  resetPreset,
  preselectAgentKey,
  locationKey,
}: UseGuidAgentSelectionOptions): GuidAgentSelectionResult => {
  const [selectedAgentKey, _setSelectedAgentKey] = useState<string>(() => {
    try {
      return configService.get('guid.lastSelectedAgent') || 'nomi';
    } catch {
      return 'nomi';
    }
  });
  const [availableAgents, setAvailableAgents] = useState<AvailableAgent[]>();
  const [selectedMode, _setSelectedMode] = useState<string>('default');
  // Track whether mode was loaded from preferences to avoid overwriting during initial load
  const selectedAgentRef = useRef<string | null>(null);
  // Guard: only run the initial restore once; user selections are never overwritten
  const initialRestoreDoneRef = useRef(false);
  const [selectedAcpModel, _setSelectedAcpModel] = useState<string | null>(null);

  // Wrap setSelectedAgentKey to also save to storage
  const setSelectedAgentKey = useCallback((key: string) => {
    initialRestoreDoneRef.current = true;
    _setSelectedAgentKey(key);
    configService.set('guid.lastSelectedAgent', key).catch((error) => {
      console.error('Failed to save selected agent:', error);
    });
  }, []);

  // Wrap setSelectedMode to also save preferred mode to the agent's own config
  const setSelectedMode = useCallback((mode: React.SetStateAction<string>) => {
    _setSelectedMode((prev) => {
      const newMode = typeof mode === 'function' ? mode(prev) : mode;
      const agentKey = selectedAgentRef.current;
      if (agentKey) {
        void savePreferredMode(agentKey, newMode);
      }
      return newMode;
    });
  }, []);

  // Wrap setSelectedAcpModel to also save preferred model to the agent's config
  const setSelectedAcpModel = useCallback((model_id: React.SetStateAction<string | null>) => {
    _setSelectedAcpModel((prev) => {
      const newModelId = typeof model_id === 'function' ? model_id(prev) : model_id;
      const agentKey = selectedAgentRef.current;
      if (agentKey && agentKey !== 'gemini' && agentKey !== 'custom' && agentKey !== 'preset' && newModelId) {
        void savePreferredModelId(agentKey, newModelId);
      }
      return newModelId;
    });
  }, []);

  const availableCustomAgentIds = useMemo(() => {
    const ids = new Set<string>();
    (availableAgents || []).forEach((agent) => {
      if (agent.agent_source === 'custom' && agent.id) {
        ids.add(agent.id);
      }
    });
    return ids;
  }, [availableAgents]);

  const getAgentKey = getAgentKeyUtil;

  // --- Sub-hooks ---
  const { presets, presetsLoaded, customAgents, customAgentAvatarMap, refreshCustomAgents } = useCustomAgentsLoader({
    availableCustomAgentIds,
  });

  const { resolvePresetAgentType } = usePresetResolver({ presets });

  const { isMainAgentAvailable, getEffectiveAgentType } = useAgentAvailability({
    modelList,
    isGoogleAuth,
    availableAgents,
    resolvePresetAgentType,
  });

  /**
   * Find agent by key.
   *
   * Key formats:
   *   - Plain id (custom ACP / remote rows) → resolved by `AvailableAgent.id`.
   *   - Plain backend or agent_type (builtin rows) → resolved by `backend` or
   *     `agent_type` fallback.
   *   - `preset:<presetId>` → preset preset from the preset catalog
   *     (kept as the only surviving prefix path; preset presets are a
   *     different selection surface from AgentRegistry rows).
   */
  const findAgentByKey = (key: string): AvailableAgent | undefined => {
    if (key.startsWith('preset:')) {
      const presetId = key.slice(7);
      const preset = presets.find((item) => item.preset_id === presetId);
      if (preset) {
        const preferenceIds = [
          ...(preset.preferred_agent_id ? [preset.preferred_agent_id] : []),
          ...preset.agent_preferences.map((preference) => preference.agent_id),
        ];
        const preferredAgent = preferenceIds
          .map((agentId) => availableAgents?.find((agent) => agent.id === agentId))
          .find(Boolean);
        return {
          agent_type: preferredAgent?.agent_type || 'nomi',
          backend: preferredAgent?.backend,
          name: preset.name,
          id: preset.preset_id,
          preset_id: preset.preset_id,
          is_preset: true,
          avatar: preset.avatar,
        };
      }
      return undefined;
    }
    // Opaque AgentRegistry identity (or a remote-agent business ID) takes
    // precedence, so two entries sharing the same backend do not collide.
    const byId = availableAgents?.find((a) => a.id === key);
    if (byId) return byId;
    return availableAgents?.find((a) => a.backend === key || a.agent_type === key);
  };

  // Derived state: collapse row-scoped rows to a stable slot key so shared
  // config namespaces (acp.config / mode preferences) are not fragmented
  // per row.
  const selectedAgent: string = ((): string => {
    if (selectedAgentKey.startsWith('preset:')) return 'preset';
    const info = availableAgents?.find((a) => a.id === selectedAgentKey);
    if (info?.agent_type === 'remote') return 'remote';
    if (info?.agent_source === 'custom') return 'custom';
    return selectedAgentKey;
  })();
  const selectedAgentInfo = useMemo(() => {
    return findAgentByKey(selectedAgentKey);
  }, [selectedAgentKey, availableAgents, presets]);
  // The key is the durable user intent. Catalog metadata may revalidate, but
  // that must never silently downgrade a selected preset to a bare Agent.
  const is_presetAgent = selectedAgentKey.startsWith('preset:');

  // --- SWR: Fetch detected execution engines (shared cache) ---
  const { data: availableAgentsData } = useSWR<AvailableAgent[]>(DETECTED_AGENTS_SWR_KEY, fetchDetectedAgents);

  // Fetch remote agents from DB and merge into available agents
  const { data: remoteAgentsData } = useSWR('remote-agents.list', () => ipcBridge.remoteAgent.list.invoke());

  useEffect(() => {
    if (!availableAgentsData) return;
    // Map the named AgentMetadata wire identity into the local mixed display
    // aggregate. The aggregate's `id` slot also hosts remote/preset identities.
    const normalisedDetected: AvailableAgent[] = availableAgentsData.map((a) => {
      const asAgent = a as AgentMetadata;
      const { agent_id, ...displayFields } = asAgent;
      const isCustomRow = asAgent.agent_source === 'custom';
      return {
        ...displayFields,
        id: agent_id,
        avatar: isCustomRow ? asAgent.icon : (a as AvailableAgent).avatar,
      };
    });
    const remoteAsAvailable: AvailableAgent[] = (remoteAgentsData || [])
      .filter((ra) => ra.protocol === 'openclaw')
      .map((ra) => ({
        agent_type: 'remote',
        name: ra.name,
        id: ra.remote_agent_id,
        remote_agent_id: ra.remote_agent_id,
        avatar: ra.avatar,
      }));
    setAvailableAgents([...normalisedDetected, ...remoteAsAvailable]);
  }, [availableAgentsData, remoteAgentsData]);

  // Track whether the resetPreset flag has been consumed so it only fires once
  // per navigation. Use locationKey (changes on every navigate()) to reset the guard,
  // because window.history.replaceState does NOT update React Router's location.state.
  const resetHandledRef = useRef(false);
  const prevLocationKeyRef = useRef(locationKey);
  if (locationKey !== prevLocationKeyRef.current) {
    prevLocationKeyRef.current = locationKey;
    resetHandledRef.current = false;
  }

  // Apply sidebar "new chat" resets and explicit "Go to Chat" pre-selections
  // before paint so the previous preset selection does not flash for a
  // frame when navigating to /guid again.
  useLayoutEffect(() => {
    if (!availableAgents || availableAgents.length === 0) return;
    if (resetHandledRef.current) return;

    // Explicit pre-selection (e.g. from Settings → Agent "Go to Chat") wins
    // over reset and saved-selection when the agent is actually present.
    if (preselectAgentKey) {
      const matched = availableAgents.find((a) => getAgentKey(a) === preselectAgentKey);
      if (matched) {
        resetHandledRef.current = true;
        const key = getAgentKey(matched);
        _setSelectedAgentKey(key);
        configService.set('guid.lastSelectedAgent', key).catch((error) => {
          console.error('Failed to save preselected agent key:', error);
        });
        return;
      }
    }

    if (resetPreset) {
      resetHandledRef.current = true;
      // Only reset when the current selection is a preset preset.
      // CLI agent selections (Claude Code, Gemini CLI, etc.) are preserved so
      // New Chat keeps the last-used CLI agent.
      const currentIsPreset = selectedAgentKey.startsWith('preset:');
      if (currentIsPreset) {
        const firstCliAgent = availableAgents.find((a) => !a.is_preset);
        const fallbackKey = firstCliAgent ? getAgentKey(firstCliAgent) : 'nomi';
        _setSelectedAgentKey(fallbackKey);
        configService.set('guid.lastSelectedAgent', fallbackKey).catch((error) => {
          console.error('Failed to save reset agent key:', error);
        });
      }
    }
  }, [availableAgents, resetPreset, preselectAgentKey, locationKey]);

  // Load last selected agent when no explicit reset was requested.
  useEffect(() => {
    if (!availableAgents || availableAgents.length === 0) return;
    if (resetPreset) return;
    // An explicit pre-selection from navigation state wins over the
    // persisted last-selected key — skip the saved-restore path so
    // useLayoutEffect's preselect remains the authoritative pick.
    if (preselectAgentKey && availableAgents.some((a) => getAgentKey(a) === preselectAgentKey)) return;

    let cancelled = false;
    initialRestoreDoneRef.current = true;

    const restoreSavedSelection = async () => {
      try {
        const savedKey = configService.get('guid.lastSelectedAgent');
        if (cancelled) return;

        if (savedKey) {
          if (savedKey.startsWith('preset:')) {
            if (!presetsLoaded) return;
            const presetId = savedKey.slice('preset:'.length);
            if (presets.some((preset) => preset.preset_id === presetId)) {
              _setSelectedAgentKey(savedKey);
              return;
            }
          }
          // Plain row key — verify it still exists in detected engines
          if (availableAgents.some((agent) => getAgentKey(agent) === savedKey)) {
            _setSelectedAgentKey(savedKey);
            return;
          }
        }

        // No saved preference or stale key — default to first detected engine
        const firstAgent = availableAgents[0];
        if (firstAgent) {
          const fallbackKey = getAgentKey(firstAgent);
          _setSelectedAgentKey(fallbackKey);
          if (savedKey && savedKey !== fallbackKey) {
            void configService.set('guid.lastSelectedAgent', fallbackKey);
          }
        }
      } catch (error) {
        console.error('Failed to load last selected agent:', error);
      }
    };

    void restoreSavedSelection();

    return () => {
      cancelled = true;
    };
  }, [availableAgents, presets, presetsLoaded, resetPreset, preselectAgentKey, locationKey]);

  const currentEffectiveAgentInfo = useMemo(() => {
    if (!is_presetAgent) {
      const isAvailable = isMainAgentAvailable(selectedAgent as string);
      return {
        agent_type: selectedAgent as string,
        isFallback: false,
        originalType: selectedAgent as string,
        isAvailable,
      };
    }
    return getEffectiveAgentType(selectedAgentInfo);
  }, [is_presetAgent, selectedAgent, selectedAgentInfo, getEffectiveAgentType, isMainAgentAvailable]);

  // Reset selected ACP model when agent changes: prefer the preset preset's
  // own configured model, then the backend's saved preference, then the
  // handshake default.
  useEffect(() => {
    // For preset agents, resolve to the actual backend type for config lookup
    const backend = is_presetAgent ? currentEffectiveAgentInfo.agent_type : selectedAgent;

    // A preset carries provider-qualified model preferences.
    // It must win over the backend's global `preferredModelId`, otherwise the
    // preset's chosen model is silently discarded in favour of whatever the
    // ACP engine was last configured with globally. Mirrors the preset-record
    // Use the first display preference only; execution resolution remains server-side.
    if (is_presetAgent) {
      const presetModel = presets.find(
        (preset) => preset.preset_id === selectedAgentInfo?.preset_id,
      )?.model_preferences?.[0];
      if (presetModel?.model) {
        _setSelectedAcpModel(presetModel.model);
        return;
      }
    }

    const config = configService.get('acp.config');
    const preferred = (config?.[backend as string] as Record<string, unknown>)?.preferredModelId as string | undefined;
    if (preferred) {
      _setSelectedAcpModel(preferred);
      return;
    }

    const metadataAgents = availableAgentsData as unknown as AgentMetadata[] | undefined;
    const matched = metadataAgents?.find((a) => (a.backend ?? a.agent_type) === backend);
    const handshakeModels = matched?.handshake?.available_models as AcpModelInfo | undefined;
    _setSelectedAcpModel(handshakeModels?.current_model_id ?? null);
  }, [selectedAgentKey, availableAgentsData, is_presetAgent, currentEffectiveAgentInfo.agent_type, presets, selectedAgentInfo?.preset_id]);

  // Read preferred mode or fallback to legacy yoloMode config
  useEffect(() => {
    // For preset agents, use the effective backend type for config lookup and mode saving
    const configKey = is_presetAgent ? currentEffectiveAgentInfo.agent_type : selectedAgent;
    selectedAgentRef.current = configKey;
    // Default authorization mode = full-auto (产品决策:开箱即用全自动,不再反复弹授权).
    // Use the backend's full-auto value (`getFullAutoMode`) when it is a mode the
    // backend actually offers (handshake available_modes for ACP engines, else the
    // static AGENT_MODES table); otherwise fall back to the backend's natural
    // default via `resolveDefaultMode` — safe for engines without a yolo-equivalent.
    // A saved `preferredMode` (explicit user choice, incl. a downgrade) still wins
    // below.
    const fullAutoMode = getFullAutoMode(configKey);
    const handshakeModes = (availableAgentsData as unknown as AgentMetadata[] | undefined)?.find(
      (a) => (a.backend ?? a.agent_type) === configKey
    )?.handshake?.available_modes as AcpSessionModes | undefined;
    const availableModeIds = handshakeModes?.available_modes?.map((m) => m.id) ??
      getAgentModes(configKey).map((m) => m.value);
    const fallbackMode = availableModeIds.includes(fullAutoMode)
      ? fullAutoMode
      : resolveDefaultMode(configKey, availableAgentsData as unknown as AgentMetadata[] | undefined);
    _setSelectedMode(fallbackMode);
    if (!configKey) return;

    let cancelled = false;

    const loadPreferredMode = async () => {
      try {
        // Read preferredMode from the agent's own config, fallback to legacy yoloMode
        let preferred: string | undefined;
        let yoloMode = false;

        if (configKey === 'nomi') {
          const config = configService.get('nomi.config');
          preferred = config?.preferredMode;
        } else {
          const config = configService.get('acp.config');
          const backendConfig = config?.[configKey as string] as Record<string, unknown> | undefined;
          preferred = backendConfig?.preferredMode as string | undefined;
          yoloMode = (backendConfig?.yoloMode as boolean) ?? false;
        }

        if (cancelled) return;

        // 1. Use preferredMode if valid
        const normalizedPreferred = configKey === 'codex' ? normalizeCodexMode(preferred) : preferred;
        if (normalizedPreferred) {
          const modes = getAgentModes(configKey);
          if (modes.some((m) => m.value === normalizedPreferred)) {
            _setSelectedMode(normalizedPreferred);
            return;
          }
        }

        // 2. Fallback: legacy yoloMode
        if (yoloMode) {
          const yoloValues: Record<string, string> = {
            claude: 'bypassPermissions',
            gemini: 'yolo',
            codex: CODEX_MODE_NATIVE_FULL_ACCESS,
            qwen: 'yolo',
          };
          _setSelectedMode(yoloValues[configKey] || 'yolo');
        }
      } catch {
        /* silent */
      }
    };

    void loadPreferredMode();

    return () => {
      cancelled = true;
    };
  }, [selectedAgent, is_presetAgent, currentEffectiveAgentInfo.agent_type, availableAgentsData]);

  const currentAcpCachedModelInfo = useMemo(() => {
    // For preset agents, resolve to the actual backend type for model list lookup
    const backend = is_presetAgent ? currentEffectiveAgentInfo.agent_type : selectedAgent;

    // Source: `handshake.available_models` from `/api/agents`.
    // The backend persists the last-seen `ModelInfoPayload` (snake_case) on
    // the agent_metadata row, so this is populated across restarts without
    // requiring a fresh session.
    const metadataAgents = availableAgentsData as unknown as AgentMetadata[] | undefined;
    const matched = metadataAgents?.find((a) => (a.backend ?? a.agent_type) === backend);
    const handshakeModels = matched?.handshake?.available_models as AcpModelInfo | undefined;
    if (
      handshakeModels &&
      Array.isArray(handshakeModels.available_models) &&
      handshakeModels.available_models.length > 0
    ) {
      return handshakeModels;
    }

    // Fallback: when the backend has not yet observed a session for codex
    // (e.g., first launch before any warmup), use the hardcoded default list
    // so the Guid page shows a model selector immediately.
    if (backend === 'codex' && DEFAULT_CODEX_MODELS.length > 0) {
      return {
        current_model_id: DEFAULT_CODEX_MODELS[0].id,
        current_model_label: DEFAULT_CODEX_MODELS[0].label,
        available_models: DEFAULT_CODEX_MODELS.map((m) => ({ id: m.id, label: m.label })),
      } satisfies AcpModelInfo;
    }

    return null;
  }, [selectedAgentKey, is_presetAgent, currentEffectiveAgentInfo.agent_type, availableAgentsData]);

  // Key of the first non-preset CLI agent (used as fallback when leaving preset mode)
  const defaultAgentKey = useMemo(() => {
    const firstCliAgent = availableAgents?.find((a) => !a.is_preset);
    return firstCliAgent ? getAgentKey(firstCliAgent) : 'nomi';
  }, [availableAgents]);

  return {
    selectedAgentKey,
    setSelectedAgentKey,
    defaultAgentKey,
    selectedAgent,
    selectedAgentInfo,
    is_presetAgent,
    availableAgents,
    presets,
    customAgents,
    selectedMode,
    setSelectedMode,
    selectedAcpModel,
    setSelectedAcpModel,
    currentAcpCachedModelInfo,
    currentEffectiveAgentInfo,
    getAgentKey,
    findAgentByKey,
    resolvePresetAgentType,
    isMainAgentAvailable,
    getEffectiveAgentType,
    refreshCustomAgents,
    customAgentAvatarMap,
  };
};
