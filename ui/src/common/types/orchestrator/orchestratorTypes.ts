// src/common/types/orchestrator/orchestratorTypes.ts
// 「智能编排」(orchestration) wire types — hand-written mirrors of the backend
// api-types DTOs (Task 4). Field names are kept snake_case to match the JSON
// wire exactly, consistent with the rest of the codebase's wire types.
//
// IDs are STRINGS (`fleet_…`, `fmem_…`, `ows_…`), NOT i64. Numeric fields
// (max_parallel / sort_order / created_at / updated_at) are i64 on the backend
// but arrive as plain `number` over JSON, so they are typed `number` here.

/** A member's declared capability profile, used by the orchestrator for routing. */
export type TCapabilityProfile = {
  strengths: string[];
  modalities: string[];
  tools: boolean;
  reasoning: string;
  cost_tier: string;
  speed_tier: string;
};

/** Per-member execution constraints. */
export type TMemberConstraints = {
  max_concurrency?: number;
  cost_tier?: string;
  allowed_task_kinds?: string[];
};

/** A single agent slot within a fleet. */
export type TFleetMember = {
  id: string;
  agent_id: string;
  provider_id?: string;
  model?: string;
  role_hint?: string;
  capability_profile?: TCapabilityProfile;
  constraints?: TMemberConstraints;
  sort_order: number;
};

/** A persisted fleet (group of agents) record. */
export type TFleet = {
  id: string;
  name: string;
  description?: string;
  max_parallel?: number;
  members: TFleetMember[];
  created_at: number;
  updated_at: number;
};

/** A persisted orchestration workspace record. */
export type TOrchWorkspace = {
  id: string;
  name: string;
  default_fleet_id?: string;
  workspace_dir?: string;
  created_at: number;
  updated_at: number;
};

// ── Request payloads ────────────────────────────────────────────────────────

/** Input shape for a fleet member when creating/updating a fleet. */
export type TFleetMemberInput = {
  agent_id: string;
  provider_id?: string;
  model?: string;
  role_hint?: string;
  capability_profile?: TCapabilityProfile;
  constraints?: TMemberConstraints;
  sort_order?: number;
};

/** Body for `POST /api/orchestrator/fleets`. */
export type TCreateFleet = {
  name: string;
  description?: string;
  max_parallel?: number;
  members: TFleetMemberInput[];
};

/** Body for `PUT /api/orchestrator/fleets/{id}` (all fields optional / partial). */
export type TUpdateFleet = {
  name?: string;
  description?: string;
  max_parallel?: number;
  members?: TFleetMemberInput[];
};

/** Body for `POST /api/orchestrator/workspaces`. */
export type TCreateWorkspace = {
  name: string;
  default_fleet_id?: string;
  workspace_dir?: string;
};

/** Body for `PUT /api/orchestrator/workspaces/{id}` (partial). */
export type TUpdateWorkspace = {
  name?: string;
  default_fleet_id?: string;
};
