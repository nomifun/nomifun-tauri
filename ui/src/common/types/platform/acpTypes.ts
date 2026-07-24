/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { PersistedArtifactId } from '@/common/types/ids';

/**
 * Advanced overrides exposed through the JSON panel of the custom agent
 * editor. These map directly onto backend `AgentMetadata` columns that
 * are not covered by the 5 form fields (name / avatar / command / args
 * / env). Snake_case keys match the backend wire format.
 */
export interface CustomAgentAdvancedOverrides {
  yolo_id?: string;
  native_skills_dirs?: string[];
  behavior_policy?: { supports_side_question?: boolean };
  description?: string;
}

// ── Initialize response types (from ACP spec) ──────────────────────────

/**
 * Prompt content types the agent can accept.
 * Per ACP spec, omitted fields default to false.
 */
export type AcpPromptCapabilities = {
  image: boolean;
  audio: boolean;
  embeddedContext: boolean;
};

/**
 * MCP transport types the agent supports.
 * stdio is mandatory per ACP spec IF the agent declares mcpCapabilities at all.
 * If mcpCapabilities is absent from the initialize response, all transports are false.
 */
export type AcpMcpCapabilities = {
  stdio: boolean;
  http: boolean;
  sse: boolean;
};

/**
 * Session operations the agent supports.
 * Per ACP spec, key presence (e.g. `{ fork: {} }`) indicates support;
 * values are `{}` reserved for future extension.
 * null = unsupported (key was omitted in the response).
 */
export type AcpSessionCapabilities = {
  fork: Record<string, unknown> | null;
  resume: Record<string, unknown> | null;
  list: Record<string, unknown> | null;
  close: Record<string, unknown> | null;
};

/**
 * Parsed agent capabilities from the initialize response.
 * Field names match the ACP protocol wire format to avoid confusion.
 * All fields have safe defaults — no undefined checks needed by callers.
 */
export type AcpAgentCapabilities = {
  loadSession: boolean;
  promptCapabilities: AcpPromptCapabilities;
  mcpCapabilities: AcpMcpCapabilities;
  sessionCapabilities: AcpSessionCapabilities;
  /** Backend-specific metadata (_meta from agentCapabilities) */
  _meta: Record<string, unknown>;
};

/** Agent identity info from initialize response. */
export type AcpAgentInfo = {
  name: string;
  version: string;
  title?: string;
};

/**
 * Authentication method descriptor from initialize response.
 * Backends may extend this with extra fields (e.g. `type`, `vars`).
 */
export type AcpAuthMethod = {
  id: string;
  name: string;
  description?: string;
  /** Extended fields — e.g. Codex uses `type: "env_var"` and `vars` */
  [key: string]: unknown;
};

/**
 * Fully parsed initialize response (the `result` from JSON-RPC).
 * Consolidates all top-level fields per ACP initialization spec.
 */
export type AcpInitializeResult = {
  protocolVersion: number;
  capabilities: AcpAgentCapabilities;
  agentInfo: AcpAgentInfo | null;
  auth_methods: AcpAuthMethod[];
};

// ── Session update payloads retained for chatLib message shapes ────────

/** Shared base — every session update notification carries a session id. */
export interface BaseSessionUpdate {
  session_id: string;
}

export interface PersistedToolArtifact {
  id: PersistedArtifactId;
  kind: 'image' | 'audio' | 'video' | 'text' | 'file';
  mime_type: string;
  /** Canonical native path on the current host. */
  path: string;
  /** Portable path relative to the conversation workspace. */
  relative_path: string;
  size_bytes: number;
  sha256: string;
}

/** Tool call 内容项类型 / Tool call content item type */
export type ToolCallContentItem =
  | {
      type: 'content';
      content: {
        type: 'text';
        text: string;
      };
    }
  | {
      type: 'diff';
      path: string;
      old_text?: string | null;
      new_text: string;
    }
  | {
      type: 'artifact';
      artifact: PersistedToolArtifact;
      source_uri?: string;
    }
  | {
      type: 'resource_link';
      name: string;
      uri: string;
      title?: string;
      description?: string;
      mime_type?: string;
      size_bytes?: number;
    }
  | {
      type: 'terminal';
      terminal_id: string;
    }
  | {
      type: 'artifact_error';
      message: string;
    };

/** Tool call 位置项类型 / Tool call location item type */
export interface ToolCallLocationItem {
  path: string;
}

/** Tool call session update */
export interface ToolCallUpdate extends BaseSessionUpdate {
  /** Persistence-only two-phase delivery marker; absent on live ACP frames. */
  artifact_delivery_committed?: boolean;
  update: {
    sessionUpdate: 'tool_call' | 'tool_call_update';
    tool_call_id: string;
    /** ACP tool_call_update fields are partial and must not synthesize defaults. */
    status?: 'pending' | 'in_progress' | 'completed' | 'failed';
    title?: string;
    kind?: 'read' | 'edit' | 'execute';
    rawInput?: Record<string, unknown>;
    rawOutput?: unknown;
    content?: ToolCallContentItem[];
    locations?: ToolCallLocationItem[];
  };
}

/** Plan session update */
export interface PlanUpdate extends BaseSessionUpdate {
  update: {
    sessionUpdate: 'plan';
    entries: Array<{
      content: string;
      status: 'pending' | 'in_progress' | 'completed';
      priority?: 'low' | 'medium' | 'high';
    }>;
  };
}

// ===== ACP ConfigOption types (stable API) =====

/** A single select option within a config option */
export interface AcpConfigSelectOption {
  value: string;
  name?: string;
  label?: string; // Some agents may use label instead of name
}

/** A configuration option returned by session/new */
export interface AcpSessionConfigOption {
  id: string;
  name?: string;
  label?: string; // Some agents may use label instead of name
  description?: string;
  category?: string;
  type: 'select' | 'boolean' | 'string';
  current_value?: string;
  selected_value?: string; // Some agents may use selected_value instead of current_value
  options?: AcpConfigSelectOption[];
}

// ===== ACP Mode / Model types (unstable API) =====

/** Mode entry in the top-level `modes` object of session/new response */
export interface AcpAvailableMode {
  id: string;
  name?: string;
  description?: string;
}

/** Modes info returned by session/new (used by qoder, opencode, etc.) */
export interface AcpSessionModes {
  current_mode_id?: string;
  available_modes?: AcpAvailableMode[];
}

// ===== Unified model info for UI =====

export interface AcpModelInfo {
  /** Currently active model ID */
  current_model_id: string | null;
  /** Display label for the current model */
  current_model_label: string | null;
  /** Available models for switching */
  available_models: Array<{ id: string; label: string }>;
}

// ===== Permission request (session/request_permission) =====

export interface AcpPermissionOption {
  option_id: string;
  name: string;
  kind: 'allow_once' | 'allow_always' | 'reject_once' | 'reject_always';
}

export interface AcpPermissionRequest {
  session_id: string;
  options: Array<AcpPermissionOption>;
  tool_call: {
    tool_call_id: string;
    raw_input?: {
      command?: string;
      description?: string;
      [key: string]: unknown;
    };
    status?: string;
    title?: string;
    kind?: string;
    content?: ToolCallContentItem[];
    locations?: ToolCallLocationItem[];
  };
}
