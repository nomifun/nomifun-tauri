/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { AcpModelInfo } from '@/common/types/platform/acpTypes';
import type { AgentSource } from '@/renderer/utils/model/agentTypes';
import type { PresetReference } from '@/common/types/agent/presetTypes';
import type { RemoteAgentId } from '@/common/types/ids';

/**
 * Available agent entry returned by the backend.
 * `agent_type` is the top-level discriminant (acp, nomi, nanobot, etc.).
 * `backend` is only present when `agent_type === 'acp'` (claude, qwen, codex, …).
 */
export type AvailableAgent = {
  /**
   * Opaque AgentRegistry identity. Custom/extension identifiers are external
   * catalog keys, not UUID entities, and are never passed through UUID parsers.
   */
  id?: string;
  agent_type: string;
  agent_source?: AgentSource;
  backend?: string;
  icon?: string;
  name: string;
  cli_path?: string;
  /** Canonical remote-agent entity identity; never routed through the custom-agent catalog key. */
  remote_agent_id?: RemoteAgentId;
  is_preset?: boolean;
  preset_id?: PresetReference;
  context?: string;
  avatar?: string;
  isExtension?: boolean;
  extensionName?: string;
};

/**
 * Computed mention option for the @ mention dropdown.
 */
export type MentionOption = {
  key: string;
  label: string;
  tokens: Set<string>;
  avatar: string | undefined;
  avatarImage: string | undefined;
  logo: string | undefined;
  isExtension?: boolean;
};

/**
 * Effective agent type info used for UI display and send logic.
 */
export type EffectiveAgentInfo = {
  agent_type: string;
  isFallback: boolean;
  originalType: string;
  isAvailable: boolean;
};

export type { AcpModelInfo };
