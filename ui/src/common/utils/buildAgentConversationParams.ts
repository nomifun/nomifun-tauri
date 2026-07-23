/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ICreateConversationParams } from '@/common/adapter/ipcBridge';
import type { TProviderWithModel } from '@/common/config/storage';
import type { RemoteAgentId } from '@/common/types/ids';
import type { PresetReference } from '@/common/types/agent/presetTypes';

export type BuildAgentConversationInput = {
  backend: string;
  name: string;
  agent_id?: string;
  agent_name?: string;
  preset_id?: PresetReference;
  workspace: string;
  model: TProviderWithModel;
  cli_path?: string;
  remote_agent_id?: RemoteAgentId;
  custom_workspace?: boolean;
  is_preset?: boolean;
  session_mode?: string;
  current_model_id?: string;
  extra?: Partial<ICreateConversationParams['extra']>;
};

export function getConversationTypeForBackend(backend: string): ICreateConversationParams['type'] {
  switch (backend) {
    case 'nomi':
      return 'nomi';
    case 'openclaw-gateway':
    case 'openclaw':
      return 'openclaw-gateway';
    case 'nanobot':
      return 'nanobot';
    case 'remote':
      return 'remote';
    default:
      return 'acp';
  }
}

export function buildAgentConversationParams(input: BuildAgentConversationInput): ICreateConversationParams {
  const {
    backend,
    name,
    agent_id,
    agent_name,
    preset_id,
    workspace,
    model,
    cli_path,
    remote_agent_id,
    custom_workspace = true,
    is_preset = false,
    session_mode,
    current_model_id,
    extra: extraOverrides,
  } = input;

  const type = getConversationTypeForBackend(backend);
  const extra: ICreateConversationParams['extra'] = {
    workspace,
    custom_workspace,
    ...extraOverrides,
  };

  if (!is_preset) {
    if (type === 'remote') {
      if (!remote_agent_id) {
        throw new Error('A valid remote_agent_id is required for remote conversations');
      }
      extra.remote_agent_id = remote_agent_id;
    } else if (type === 'openclaw-gateway') {
      extra.agent_name = agent_name || name;
      extra.gateway = {
        cli_path,
      };
    } else if (type === 'acp') {
      extra.backend = backend as string;
      extra.agent_name = agent_name || name;
      if (agent_id) extra.agent_id = agent_id;
      if (cli_path) extra.cli_path = cli_path;
    }
  }

  if (session_mode) extra.session_mode = session_mode;
  if (current_model_id) extra.current_model_id = current_model_id;

  return {
    type,
    model,
    name,
    preset_id: is_preset ? preset_id : undefined,
    extra,
  };
}
