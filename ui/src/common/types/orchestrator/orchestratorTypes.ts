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
  /** Role/model description fed to the description-driven planner (P3/P4). */
  description?: string;
  /** Assistant persona (rule text); the worker uses it as `preset_rules` (P4). */
  system_prompt?: string;
  /** Assistant skills the worker enables (P4). Empty for bare-model members. */
  enabled_skills?: string[];
  /** Assistant's disabled built-in skills (P4). Empty for bare-model members. */
  disabled_builtin_skills?: string[];
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

// ── Run engine ───────────────────────────────────────────────────────────────

/** Inferred task profile used by the orchestrator for member routing. */
export type TTaskProfile = {
  kind: string;
  needs_vision: boolean;
  needs_long_context: boolean;
  needs_high_reasoning: boolean;
  bulk: boolean;
};

/** A persisted orchestration run record. */
export type TRun = {
  id: string;
  /** Owning workspace, or absent for an ad-hoc run created straight from a
   * conversation (which carries its own work_dir instead — backend serializes
   * `workspace_id: null`). */
  workspace_id?: string;
  goal: string;
  autonomy: string;
  max_parallel?: number;
  status: string;
  summary?: string;
  lead_conv_id?: number;
  total_tokens?: number;
  /** Ad-hoc run's own working directory (absent for workspace-backed runs, which
   * resolve their dir from the bound workspace). The run-workspace right rail
   * binds its file tree to this path. Backend `Run` DTO always serializes it. */
  work_dir?: string;
  created_at: number;
  updated_at: number;
};

/** A single task within a run's plan (DAG node). */
export type TRunTask = {
  id: string;
  run_id: string;
  title: string;
  spec: string;
  task_profile?: TTaskProfile;
  status: string;
  conversation_id?: number;
  output_summary?: string;
  output_files: string[];
  attempt: number;
  tokens?: number;
  graph_x?: number;
  graph_y?: number;
  /** Short role the planner named for this task (P5 沉淀捕获, migration 022).
   * Nullable: tasks planned before this column existed read back as absent. */
  role?: string;
  /** Task mode (ultracode 模式增强, migration 023):
   * - `'agent'` (default) — a normal single-agent task (current behavior);
   * - `'synthesis'` — merges its dependency tasks' outputs into a final result.
   * Backend serde-defaults missing/legacy values to `'agent'`. */
  kind: string;
  /** Optional per-kind config as a raw JSON string (migration 023). Today only
   * carries the fan-out group tag (`{"group":"<label>"}`) on sibling agent tasks;
   * absent for ordinary tasks. */
  pattern_config?: string;
  /** Per-task model override (启动前配置台, migration 025): when BOTH set, this node
   * dispatches with this provider×model (any available model, not just the run's
   * frozen fleet). Absent = follow auto-routing. */
  override_provider_id?: string;
  override_model?: string;
  /** User 预置要求 (migration 025) appended to the node's worker brief, separate
   * from the planner-written `spec`. Absent/blank = none. */
  preset_prompt?: string;
  /** Creation / last-update timestamps (epoch ms). Drive per-task pacing in the
   * roster + inspector (用时 = updated_at − created_at, 相对时间). */
  created_at: number;
  updated_at: number;
};

/** A dependency edge between two run tasks (blocker → blocked). */
export type TRunTaskDep = {
  blocker_task_id: string;
  blocked_task_id: string;
};

/** An assignment of a task to a fleet member (worker). */
export type TAssignment = {
  id: string;
  task_id: string;
  member_id: string;
  score?: number;
  rationale?: string;
  source: string;
  locked: boolean;
};

/** Full run detail: the run plus its plan (tasks/deps), assignments, and the
 * fleet snapshot the run was launched against (so the UI can resolve an
 * assignment's `member_id` → a friendly agent/model label and offer reassign). */
export type TRunDetail = {
  run: TRun;
  tasks: TRunTask[];
  deps: TRunTaskDep[];
  assignments: TAssignment[];
  fleet_members: TFleetMember[];
};

// ── Request payloads ─────────────────────────────────────────────────────────

/** Body for `POST /api/orchestrator/runs`. */
export type TCreateRun = {
  workspace_id: string;
  goal: string;
  fleet_id: string;
  autonomy?: string;
  max_parallel?: number;
};

/** A single provider+model pair. Mirrors the backend `ModelRef`. */
export type TModelRef = {
  provider_id: string;
  model: string;
};

/** The model range an ad-hoc run executes over. Tagged by `mode`, mirroring the
 * backend `ModelRange` serde EXACTLY (`#[serde(tag = "mode", rename_all =
 * "snake_case")]`):
 * - `single` — one fixed model (every synthetic member uses it);
 * - `auto`   — "pick from all enabled models" (carries no inline list; the
 *   caps_orchestrator layer expands it — the REST/Tab path sends `single`/`range`);
 * - `range`  — an explicit allow-list of models (one synthetic member each). */
export type TModelRange =
  | { mode: 'single'; model: TModelRef }
  | { mode: 'auto' }
  | { mode: 'range'; models: TModelRef[] };

/** Body for `POST /api/orchestrator/runs/adhoc`. Creates an ad-hoc run straight
 * from the「智能编排」Tab's structured form — no workspace, no pre-built fleet
 * (the fleet is synthesized from `model_range`). Defaults: `autonomy` =
 * `interactive` on the backend (the Tab approval门), so the run parks at
 * `awaiting_plan_approval` until approved. `lead_conv_id` is left unset by the
 * Tab. Field names are snake_case to match the JSON wire exactly. */
export type TCreateAdhocRun = {
  goal: string;
  work_dir?: string;
  model_range: TModelRange;
  pinned_roles?: string[];
  autonomy?: string;
  max_parallel?: number;
  /** Originating conversation id to link this run to (会话原生编排 v2, Path B).
   * Lets the right-rail empty-state (Task F5) launch a run straight from the
   * current conversation so the backend writes `extra.orchestrator_run_id` back
   * onto it. Left unset by the「智能编排」Tab. The backend `lead_conv_id` is i64,
   * arriving as a plain `number` over JSON. */
  lead_conv_id?: number;
};

/** Body for `POST /api/orchestrator/runs/{id}/replan`. Re-plans a run IN PLACE:
 * the backend clears the run's old task graph and re-decomposes against the
 * (optionally) edited inputs. Every field is optional — an omitted field keeps
 * the run's current value. `model_range` here must be `single`/`range` (an
 * unexpanded `auto` is rejected, same as create — the caller expands it).
 * Mirrors the backend `ReplanRequest`. */
export type TReplanRequest = {
  goal?: string;
  model_range?: TModelRange;
  autonomy?: string;
  pinned_roles?: string[];
};

/** Body for `POST /api/orchestrator/runs/{id}/adjust`. Conversation-driven
 * intelligent re-adjust (UC-3a): the lead model judges, per task, whether to KEEP
 * the completed work or re-decompose, and the backend reconciles the run to the
 * result — preserving the work it chose to keep (unlike replan, which wipes the
 * whole plan). Rejected (400) for a blank `intent` or a run with any running task
 * (pause first). Mirrors the backend `AdjustRunRequest`. */
export type TAdjustRunRequest = {
  intent: string;
};

/** Body for `PUT /api/orchestrator/runs/{run_id}/tasks/{task_id}/assignment`.
 * Reassign a task to a different fleet member and/or lock the assignment so the
 * orchestrator's auto-router won't override it on the next plan update. */
export type TReassign = {
  member_id: string;
  locked?: boolean;
};

/** Body for `POST /api/orchestrator/runs/{run_id}/tasks/{task_id}/steer`.
 * Mid-turn inject a steering message into a running task's worker conversation. */
export type TSteer = {
  text: string;
};

/** Body for `PATCH /api/orchestrator/runs/{run_id}/tasks/{task_id}/spec`.
 * Fine-tune a node's intent/prompt (UC-2a "意图/prompt 微调"): replace the task's
 * spec (the prompt the worker brief is built from). Rejected (400) for a blank
 * spec or a task that is currently running; a subsequent rerun re-executes the
 * node with the amended spec. */
export type TTaskSpecUpdate = {
  spec: string;
};

/**
 * 启动前配置台 (migration 025): a FULL replace of a node's per-task model override
 * + 预置要求. The panel always sends the desired state; an omitted/empty field
 * clears it. The model override needs BOTH `override_provider_id` +
 * `override_model` (a half-set is normalized to cleared server-side). Rejected
 * (400) for a running task; a pending node picks it up at dispatch, a settled one
 * on the next rerun. */
export type TTaskConfigUpdate = {
  override_provider_id?: string;
  override_model?: string;
  preset_prompt?: string;
};
