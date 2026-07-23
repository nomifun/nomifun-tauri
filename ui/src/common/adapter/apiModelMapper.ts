/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { TChatConversation, TProviderWithModel } from '../config/storage';
import {
  parseConversationId,
  parseCronJobId,
  parseExecutionAttemptId,
  parseExecutionId,
  parseExecutionStepId,
  parseExecutionTemplateId,
  parseKnowledgeBaseId,
  parseMcpServerId,
  parseCompanionId,
  parseProviderId,
  parseRemoteAgentId,
} from '../types/ids';

export type ApiProviderWithModel = {
  provider_id: string;
  model: string;
  use_model?: string;
};

function hasCompleteModelIdentity(
  model?: TProviderWithModel
): model is TProviderWithModel & { id: string; use_model: string } {
  return Boolean(
    model &&
    typeof model.id === 'string' &&
    model.id.trim().length > 0 &&
    typeof model.use_model === 'string' &&
    model.use_model.trim().length > 0
  );
}

// ── Frontend → Backend ──────────────────────────────────────────────────

export function toApiModel(m: TProviderWithModel): ApiProviderWithModel {
  return {
    provider_id: m.id,
    model: m.use_model,
  };
}

export function toApiModelOptional(m?: TProviderWithModel): ApiProviderWithModel | undefined {
  return hasCompleteModelIdentity(m) ? toApiModel(m) : undefined;
}

// ── Backend → Frontend ──────────────────────────────────────────────────

export function fromApiModel(raw: ApiProviderWithModel): TProviderWithModel {
  return {
    id: parseProviderId(raw.provider_id),
    platform: '',
    name: '',
    base_url: '',
    api_key: '',
    use_model: raw.use_model ?? raw.model,
  };
}

function fromApiModelOptional(raw?: ApiProviderWithModel | null): TProviderWithModel | undefined {
  return raw ? fromApiModel(raw) : undefined;
}

/** ConversationResponse 顶层置顶字段（conversations 表真列，服务端维护 pinned_at）。 */
export type ApiConversationPinnedFields = {
  pinned?: boolean | null;
  /** 毫秒时间戳；未置顶时服务端省略该 key */
  pinned_at?: number | null;
};

/** First-class Conversation collaboration authoring reference. It is never
 * read from or mirrored into `extra`. */
export type ApiConversationExecutionTemplateFields = {
  execution_template_id?: string | null;
};

type ApiConversationResponse = Record<string, unknown> &
  ApiConversationPinnedFields &
  ApiConversationExecutionTemplateFields & {
    conversation_id: unknown;
    model?: ApiProviderWithModel | null;
    extra?: Record<string, unknown> | null;
    cron_job_id?: string | null;
    linked_execution_id?: string | null;
    execution_step_id?: string | null;
    execution_attempt_id?: string | null;
    preset_snapshot?: Record<string, unknown> | null;
  };

export function fromApiConversation(raw: unknown): TChatConversation {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) {
    throw new TypeError('conversation response must be an object');
  }

  const r = raw as ApiConversationResponse;
  const next = { ...r } as unknown as Record<string, unknown> & {
    id: ReturnType<typeof parseConversationId>;
    model?: TProviderWithModel;
    extra?: Record<string, unknown> | null;
    cron_job_id?: ReturnType<typeof parseCronJobId>;
    execution_template_id?: ReturnType<typeof parseExecutionTemplateId> | null;
    linked_execution_id?: ReturnType<typeof parseExecutionId>;
    execution_step_id?: ReturnType<typeof parseExecutionStepId>;
    execution_attempt_id?: ReturnType<typeof parseExecutionAttemptId>;
  };

  next.id = parseConversationId(r.conversation_id);
  delete next.conversation_id;

  if ('model' in r) {
    next.model = fromApiModelOptional(r.model);
  }

  if (r.cron_job_id != null) next.cron_job_id = parseCronJobId(r.cron_job_id);
  if (r.execution_template_id != null) {
    next.execution_template_id = parseExecutionTemplateId(r.execution_template_id);
  } else if ('execution_template_id' in r) {
    next.execution_template_id = null;
  }
  if (r.linked_execution_id != null) next.linked_execution_id = parseExecutionId(r.linked_execution_id);
  if (r.execution_step_id != null) next.execution_step_id = parseExecutionStepId(r.execution_step_id);
  if (r.execution_attempt_id != null) next.execution_attempt_id = parseExecutionAttemptId(r.execution_attempt_id);

  let extra = r.extra && typeof r.extra === 'object' ? r.extra : null;

  if (extra && !('custom_workspace' in extra)) {
    const workspace = typeof extra.workspace === 'string' ? extra.workspace : '';
    const isTemporary = extra.is_temporary_workspace === true;
    extra = {
      ...extra,
      custom_workspace: workspace.length > 0 && !isTemporary,
    };
  }

  // Remote-agent conversations use one canonical logical-reference field.
  if (extra && 'remote_agent_id' in extra) {
    extra = {
      ...extra,
      remote_agent_id: parseRemoteAgentId(extra.remote_agent_id),
    };
  }

  if (extra && 'mcp_server_ids' in extra) {
    if (!Array.isArray(extra.mcp_server_ids)) {
      throw new TypeError('conversation extra.mcp_server_ids must be an array');
    }
    extra = {
      ...extra,
      mcp_server_ids: extra.mcp_server_ids.map(parseMcpServerId),
    };
  }

  if (extra && 'mcp_statuses' in extra) {
    if (!Array.isArray(extra.mcp_statuses)) {
      throw new TypeError('conversation extra.mcp_statuses must be an array');
    }
    extra = {
      ...extra,
      mcp_statuses: extra.mcp_statuses.map((status) => {
        if (!status || typeof status !== 'object' || Array.isArray(status)) {
          throw new TypeError('conversation extra.mcp_statuses[] must be an object');
        }
        return {
          ...status,
          mcp_server_id: parseMcpServerId(
            (status as Record<string, unknown>).mcp_server_id,
          ),
        };
      }),
    };
  }

  if (extra && 'session_mcp_servers' in extra) {
    if (!Array.isArray(extra.session_mcp_servers)) {
      throw new TypeError('conversation extra.session_mcp_servers must be an array');
    }
    extra = {
      ...extra,
      session_mcp_servers: extra.session_mcp_servers.map((server) => {
        if (!server || typeof server !== 'object' || Array.isArray(server)) {
          throw new TypeError('conversation extra.session_mcp_servers[] must be an object');
        }
        return {
          ...server,
          mcp_server_id: parseMcpServerId(
            (server as Record<string, unknown>).mcp_server_id,
          ),
        };
      }),
    };
  }

  if (extra && extra.acp_session_conversation_id != null) {
    extra = {
      ...extra,
      acp_session_conversation_id: parseConversationId(extra.acp_session_conversation_id),
    };
  }

  if (extra && extra.companion_id != null) {
    extra = {
      ...extra,
      companion_id: parseCompanionId(extra.companion_id),
    };
  }

  if (extra && extra !== r.extra) {
    next.extra = extra;
  }

  if (r.preset_snapshot && typeof r.preset_snapshot === 'object') {
    const snapshot = r.preset_snapshot;
    let mappedSnapshot = { ...snapshot };

    if (snapshot.resolved_model && typeof snapshot.resolved_model === 'object') {
      const resolvedModel = snapshot.resolved_model as Record<string, unknown>;
      mappedSnapshot = {
        ...mappedSnapshot,
        resolved_model:
          resolvedModel.provider_id == null
            ? resolvedModel
            : {
                ...resolvedModel,
                provider_id: parseProviderId(resolvedModel.provider_id),
              },
      };
    }

    if ('knowledge_base_ids' in snapshot) {
      if (!Array.isArray(snapshot.knowledge_base_ids)) {
        throw new TypeError('conversation preset_snapshot.knowledge_base_ids must be an array');
      }
      mappedSnapshot = {
        ...mappedSnapshot,
        knowledge_base_ids: snapshot.knowledge_base_ids.map(parseKnowledgeBaseId),
      };
    }

    next.preset_snapshot = mappedSnapshot;
  }

  return next as unknown as TChatConversation;
}

export function fromApiPaginatedConversations(result: {
  items: unknown[];
  total: number;
  has_more: boolean;
}): {
  items: TChatConversation[];
  total: number;
  has_more: boolean;
} {
  return {
    ...result,
    items: result.items.map(fromApiConversation),
  };
}
