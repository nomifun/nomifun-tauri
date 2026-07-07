/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';

/** SWR key for agent metadata rows (from `/api/agents`). */
export const DETECTED_AGENTS_SWR_KEY = 'agents.detected';

/** Type of an agent. */
export type AgentType = 'acp' | 'remote' | 'nomi' | 'openclaw-gateway' | 'nanobot';

/** Source tier of an agent row, mirroring backend `agent_source` enum. */
export type AgentSource = 'internal' | 'builtin' | 'extension' | 'custom';

/** Source-specific bookkeeping (how to probe, how to upgrade). */
export type AgentSourceInfo = {
  binary_name?: string;
  bridge_binary?: string;
  hub_package_id?: string;
  version?: string;
};

/** Environment variable entry passed to a spawned agent process. */
export type AgentEnvEntry = {
  name: string;
  value: string;
  description?: string;
};

/**
 * Adapter-side behaviour switches. New flags are added here by extending
 * the struct on the backend — the frontend should read them defensively
 * because older rows may not have every field populated.
 *
 * Whether the agent supports session/load is NOT in this bag — read
 * `handshake.agent_capabilities.load_session` instead, since the CLI
 * advertises that during init.
 */
export type BehaviorPolicy = {
  supports_side_question?: boolean;
};

/**
 * Handshake-derived fields captured from the ACP init/session-response.
 * Each field is opaque JSON the backend passes through verbatim; typing
 * happens in whatever call site actually consumes it.
 */
export type AgentHandshake = {
  agent_capabilities?: unknown;
  auth_methods?: unknown;
  config_options?: unknown;
  available_modes?: unknown;
  available_models?: unknown;
  available_commands?: unknown;
};

/**
 * Unified agent metadata returned by `/api/agents`.
 *
 * Replaces the old split of `DetectedAgent` / `AvailableAgent` — the
 * backend now stores the same shape in the `agent_metadata` table,
 * caches it in-process, and serves it directly over HTTP.
 */
export type AgentMetadata = {
  id: string;
  icon?: string;
  name: string;
  name_i18n?: Record<string, string>;
  description?: string;
  description_i18n?: Record<string, string>;

  /** Vendor label (e.g. "claude"). Absent for agents without vendor grouping. */
  backend?: string;
  /** Top-level runtime discriminant: "acp" | "remote" | "nanobot" | "nomi" | … */
  agent_type: AgentType;
  agent_source: AgentSource;
  agent_source_info?: AgentSourceInfo;

  enabled: boolean;
  /** True iff the backend resolved the spawn command on `$PATH` at hydrate time. */
  available: boolean;

  /** Pre-resolution spawn command as stored in the catalog (e.g. "bun"). */
  command?: string;
  args?: string[];
  env?: AgentEnvEntry[];
  native_skills_dirs?: string[];

  behavior_policy?: BehaviorPolicy;

  /** Native mode id that Nomi's legacy `yolo` / `yoloNoSandbox`
   *  aliases resolve to before calling `session/set_mode`. Absent
   *  when the backend has no yolo equivalent. */
  yolo_id?: string;

  handshake?: AgentHandshake;
};

/** Shared fetcher for DETECTED_AGENTS_SWR_KEY — single source of truth. */
export async function fetchDetectedAgents(): Promise<AgentMetadata[]> {
  try {
    const agents = await ipcBridge.acpConversation.getAvailableAgents.invoke();
    if (Array.isArray(agents)) {
      return agents as AgentMetadata[];
    }
  } catch {
    // fallback to empty
  }
  return [];
}
