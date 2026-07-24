import type { PresetOverrides, PresetReference, ResolvedPresetSnapshot } from '@/common/types/agent/presetTypes';
import type {
  AgentId,
  ExecutionTemplateId,
  ExecutionTemplateParticipantId,
  ConversationId,
  ProviderId,
} from '@/common/types/ids';
import type {
  TAdaptationPolicy,
  TAgentExecution,
  TDecisionPolicy,
  TDelegationPolicy,
  TExecutionModelRef,
  TParticipantCapability,
  TParticipantConstraints,
  TPlanGate,
  TPlannedExecutionStep,
} from './agentExecutionTypes';

export type TAgentExecutionTemplate = {
  execution_template_id: ExecutionTemplateId;
  name: string;
  description: string | null;
  max_parallel: number | null;
  work_dir: string | null;
  context: unknown | null;
  version: number;
  created_at: number;
  updated_at: number;
};

export type TAgentExecutionTemplateParticipant = {
  template_participant_id: ExecutionTemplateParticipantId;
  source_agent_id: AgentId;
  preset_id: PresetReference | null;
  preset_revision: number | null;
  preset_snapshot: ResolvedPresetSnapshot | null;
  provider_id: ProviderId | null;
  model: string | null;
  role: string | null;
  capability: TParticipantCapability | null;
  constraints: TParticipantConstraints | null;
  description: string | null;
  system_prompt: string | null;
  enabled_skills: string[];
  disabled_builtin_skills: string[];
  sort_order: number;
  created_at: number;
  updated_at: number;
};

export type TAgentExecutionTemplateDetail = TAgentExecutionTemplate & {
  participants: TAgentExecutionTemplateParticipant[];
};

export type TAgentExecutionTemplateParticipantInput = {
  source_agent_id?: AgentId;
  preset_id?: PresetReference;
  preset_snapshot?: ResolvedPresetSnapshot;
  preset_overrides?: PresetOverrides;
  provider_id?: ProviderId;
  model?: string;
  role?: string;
  capability?: TParticipantCapability;
  constraints?: TParticipantConstraints;
  description?: string;
  system_prompt?: string;
  enabled_skills?: string[];
  disabled_builtin_skills?: string[];
  sort_order?: number;
};

export type TCreateAgentExecutionTemplate = {
  name: string;
  description?: string;
  max_parallel?: number;
  work_dir?: string;
  context?: unknown;
  participants?: TAgentExecutionTemplateParticipantInput[];
};

export type TUpdateAgentExecutionTemplate = {
  expected_version: number;
  name?: string;
  description?: string | null;
  max_parallel?: number | null;
  work_dir?: string | null;
  context?: unknown | null;
  participants?: TAgentExecutionTemplateParticipantInput[];
};

export type TCreateExecutionFromTemplate = {
  goal: string;
  work_dir?: string;
  max_parallel?: number;
  delegation_policy?: TDelegationPolicy;
  plan_gate?: TPlanGate;
  adaptation_policy?: TAdaptationPolicy;
  decision_policy?: TDecisionPolicy;
  lead_conversation_id?: ConversationId;
  lead_model?: TExecutionModelRef;
  steps?: TPlannedExecutionStep[];
};

export type TCreatedExecutionFromTemplate = TAgentExecution;
