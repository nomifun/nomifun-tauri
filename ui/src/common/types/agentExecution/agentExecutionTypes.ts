import type { PresetReference, ResolvedPresetSnapshot } from '@/common/types/agent/presetTypes';
import type { AgentExecutionEventKind } from '@/common/protocolBindings/AgentExecutionEventKind';
import type {
  AgentId,
  ConversationId,
  ExecutionAttemptId,
  ExecutionId,
  ExecutionParticipantId,
  ExecutionStepId,
  ProviderId,
  UserId,
} from '@/common/types/ids';

/** Keep the UI request boundary aligned with the shared Rust domain ceiling. */
export const MAX_AGENT_EXECUTION_MODELS = 16;

export type TDelegationPolicy = 'disabled' | 'automatic' | 'prefer_parallel';
export type TPlanGate = 'automatic' | 'require_approval';
export type TAdaptationPolicy = 'fixed' | 'adaptive';
export type TDecisionPolicy = 'automatic' | 'ask_user';

export type TAgentExecutionStatus =
  | 'planning'
  | 'awaiting_approval'
  | 'running'
  | 'paused'
  | 'waiting_input'
  | 'completed'
  | 'completed_with_failures'
  | 'failed'
  | 'cancelled';

export type TExecutionStepKind = 'agent' | 'verify' | 'judge' | 'loop';
export type TAgentToolPolicy = 'full' | 'read_only' | 'read_shell';
export type TAgentStepMode = 'normal' | 'synthesis';
export type TExecutionStepStatus = 'pending' | 'running' | 'waiting_input' | 'completed' | 'failed' | 'skipped' | 'cancelled';
export type TExecutionAttemptStatus = 'queued' | 'running' | 'waiting_input' | 'completed' | 'failed' | 'cancelled' | 'interrupted';
export type TStepFailurePolicy = 'fail_execution' | 'skip_dependents';
export type TParticipantAssignmentSource = 'planner' | 'automatic' | 'manual';

export type TExecutionModelRef = {
  provider_id: ProviderId;
  model: string;
};

export type TExecutionModelPool =
  { mode: 'single'; model: TExecutionModelRef } | { mode: 'automatic' } | { mode: 'range'; models: TExecutionModelRef[] };

export type TParticipantCapability = {
  strengths: string[];
  modalities: string[];
  tools: boolean;
  reasoning: string;
  cost_tier: string;
  speed_tier: string;
};

export type TParticipantConstraints = {
  max_concurrency: number | null;
  allowed_profile_kinds: string[] | null;
};

export type TExecutionParticipant = {
  participant_id: ExecutionParticipantId;
  execution_id: ExecutionId;
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
  introduced_in_revision: number;
  retired_in_revision: number | null;
  created_at: number;
};

export type TAgentExecution = {
  execution_id: ExecutionId;
  goal: string;
  lead_conversation_id: ConversationId | null;
  work_dir: string | null;
  delegation_policy: TDelegationPolicy;
  plan_gate: TPlanGate;
  adaptation_policy: TAdaptationPolicy;
  decision_policy: TDecisionPolicy;
  max_parallel: number;
  status: TAgentExecutionStatus;
  summary: string | null;
  total_tokens: number | null;
  version: number;
  plan_revision: number;
  event_sequence: number;
  created_at: number;
  updated_at: number;
};

export type TVerificationPolicy = { mode: 'majority' } | { mode: 'unanimous' } | { mode: 'at_least'; count: number };
export type TJudgeAggregation = 'mean' | 'borda';
export type TLoopStopPolicy =
  { kind: 'max_iterations' } | { kind: 'predicate'; done_marker: string } | { kind: 'stable'; quiet_rounds: number } | { kind: 'approved' };
export type TStepControlPolicy =
  | { kind: 'verify'; vote: TVerificationPolicy }
  | { kind: 'judge'; aggregation: TJudgeAggregation; candidate_count: number | null }
  | { kind: 'loop'; max_iterations: number; stop: TLoopStopPolicy };

export type TExecutionStepProfile = {
  kind: string;
  needs_vision: boolean;
  needs_long_context: boolean;
  needs_high_reasoning: boolean;
  bulk: boolean;
};

export type TExecutionStep = {
  step_id: ExecutionStepId;
  execution_id: ExecutionId;
  title: string;
  spec: string;
  profile: TExecutionStepProfile | null;
  kind: TExecutionStepKind;
  agent_mode: TAgentStepMode | null;
  status: TExecutionStepStatus;
  /** Explicit runtime tool narrowing; role is display/routing metadata only. */
  tool_policy: TAgentToolPolicy;
  role: string | null;
  fanout_group: string | null;
  control_policy: TStepControlPolicy | null;
  /** Engine-derived recursion guard; never accepted from create/add requests. */
  failure_policy: TStepFailurePolicy;
  assigned_participant_id: ExecutionParticipantId | null;
  assignment_source: TParticipantAssignmentSource | null;
  assignment_score: number | null;
  assignment_rationale: string | null;
  assignment_locked: boolean;
  preset_prompt: string | null;
  graph_x: number | null;
  graph_y: number | null;
  dispatch_after: number | null;
  introduced_in_revision: number;
  superseded_in_revision: number | null;
  version: number;
  created_at: number;
  updated_at: number;
};

export type TExecutionStepDependency = {
  execution_id: ExecutionId;
  blocker_step_id: ExecutionStepId;
  blocked_step_id: ExecutionStepId;
  introduced_in_revision: number;
  superseded_in_revision: number | null;
};

export type TExecutionAttempt = {
  attempt_id: ExecutionAttemptId;
  execution_id: ExecutionId;
  step_id: ExecutionStepId;
  attempt_no: number;
  participant_id: ExecutionParticipantId | null;
  conversation_id: ConversationId | null;
  status: TExecutionAttemptStatus;
  trigger_reason: string;
  effective_config: unknown;
  question: string | null;
  error: string | null;
  output_summary: string | null;
  output_files: string[];
  tokens: number | null;
  retry_after: number | null;
  runtime_state: unknown | null;
  started_at: number | null;
  finished_at: number | null;
  version: number;
  created_at: number;
  updated_at: number;
};

export type TAgentExecutionDetail = {
  execution: TAgentExecution;
  participants: TExecutionParticipant[];
  steps: TExecutionStep[];
  dependencies: TExecutionStepDependency[];
  attempts: TExecutionAttempt[];
};

export type TAgentExecutionEvent = {
  execution_id: ExecutionId;
  sequence: number;
  event_type: AgentExecutionEventKind;
  step_id: ExecutionStepId | null;
  attempt_id: ExecutionAttemptId | null;
  actor_type: 'system' | 'user' | 'agent';
  actor_id: string | null;
  actor_conversation_id: ConversationId | null;
  actor_attempt_id: ExecutionAttemptId | null;
  on_behalf_of_user_id: UserId;
  payload: unknown;
  created_at: number;
};

export type TPlannedExecutionStep = {
  title: string;
  spec: string;
  profile?: TExecutionStepProfile;
  kind?: TExecutionStepKind;
  agent_mode?: TAgentStepMode;
  depends_on?: number[];
  participant_index?: number;
  assignment_rationale?: string;
  role?: string;
  tool_policy?: TAgentToolPolicy;
  fanout_group?: string;
  control_policy?: TStepControlPolicy;
  failure_policy?: TStepFailurePolicy;
};

export type TCreateAgentExecution = {
  goal: string;
  work_dir?: string;
  model_pool: TExecutionModelPool;
  delegation_policy?: TDelegationPolicy;
  plan_gate?: TPlanGate;
  adaptation_policy?: TAdaptationPolicy;
  decision_policy?: TDecisionPolicy;
  max_parallel?: number;
  lead_conversation_id?: ConversationId;
  lead_model?: TExecutionModelRef;
  steps?: TPlannedExecutionStep[];
};

export type TReplanAgentExecution = Partial<
  Pick<TCreateAgentExecution, 'goal' | 'model_pool' | 'delegation_policy' | 'plan_gate' | 'adaptation_policy' | 'decision_policy'>
> & { expected_version: number };
export type TAdjustAgentExecution = {
  intent: string;
  expected_version: number;
};
export type TRenameAgentExecution = { goal: string; expected_version: number };
export type TVersionedAgentExecutionCommand = { expected_version: number };
export type TVersionedExecutionStepCommand = {
  expected_execution_version: number;
  expected_step_version: number;
};
export type TRetryExecutionStep = TVersionedExecutionStepCommand;
export type TAdoptExecutionStepOutput = TVersionedExecutionStepCommand;
export type TReassignExecutionStep = TVersionedExecutionStepCommand & {
  participant_id: ExecutionParticipantId;
  locked?: boolean;
};
export type TSteerExecutionStep = TVersionedExecutionStepCommand & {
  text: string;
};
export type TConfigureExecutionStep = {
  model?: TExecutionModelRef | null;
  preset_prompt?: string | null;
} & TVersionedExecutionStepCommand;
export type TAddExecutionSteps = {
  steps: TPlannedExecutionStep[];
  expected_version: number;
};
export type TUpdateExecutionStep = TVersionedExecutionStepCommand & {
  title?: string;
  spec?: string;
};
export type TAnswerExecutionDecision = TVersionedExecutionStepCommand & {
  answer: string;
  expected_attempt_version: number;
};
export type TAgentExecutionEventsQuery = {
  after_sequence?: number;
  limit?: number;
};

export function latestAttemptForStep(
  attempts: TExecutionAttempt[],
  stepId: ExecutionStepId
): TExecutionAttempt | undefined {
  return attempts
    .filter((attempt) => attempt.step_id === stepId)
    .reduce<TExecutionAttempt | undefined>(
      (latest, attempt) => (!latest || attempt.attempt_no > latest.attempt_no ? attempt : latest),
      undefined,
    );
}
