//! Orchestration ("智能编排") request/response DTOs: fleets, fleet members,
//! capability profiles, and orchestration workspaces. Plain serde only (no
//! ts-rs in P0).

use serde::{Deserialize, Serialize};

use crate::webhook::double_option;

/// A fleet (编队) as returned to clients: a named group of agent members with an
/// optional parallelism cap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fleet {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub max_parallel: Option<i64>,
    pub members: Vec<FleetMember>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One member of a fleet: an agent reference plus its routing hints, capability
/// profile, and constraints.
///
/// **Enrichment (P4 Task 2).** The trailing four fields carry an assistant's
/// resolved persona into the run's self-contained fleet snapshot, so the
/// orchestrator engine/worker never need an assistant-crate dependency: they
/// read everything from the snapshot. All four are `#[serde(default)]` so old
/// snapshots (and bare model-range members) deserialize unchanged:
/// - `description` — the role/model description fed to the description-driven
///   planner (P3). Set for both assistant-backed AND bare model members.
/// - `system_prompt` — the assistant's persona/rule text; the worker uses it as
///   `preset_rules` (consumed in Task 3). `None` for bare model members.
/// - `enabled_skills` / `disabled_builtin_skills` — the assistant's skill set;
///   the worker applies them (Task 3). Empty for bare model members.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetMember {
    pub id: String,
    pub agent_id: String,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub role_hint: Option<String>,
    pub capability_profile: Option<CapabilityProfile>,
    pub constraints: Option<MemberConstraints>,
    pub sort_order: i64,
    /// Role/model description → fed to the planner (P3). Additive: P3 still
    /// builds its own description map from the provider rows, so this never
    /// breaks `produce`.
    #[serde(default)]
    pub description: Option<String>,
    /// Assistant persona (rule text); the worker uses it as `preset_rules`.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Assistant skills the worker enables.
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    /// Assistant's disabled built-in skills.
    #[serde(default)]
    pub disabled_builtin_skills: Vec<String>,
}

/// Declarative capability profile used to route tasks to a member.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityProfile {
    pub strengths: Vec<String>,
    pub modalities: Vec<String>,
    pub tools: bool,
    pub reasoning: String,
    pub cost_tier: String,
    pub speed_tier: String,
}

/// Per-member runtime constraints applied by the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberConstraints {
    pub max_concurrency: Option<i64>,
    pub cost_tier: Option<String>,
    pub allowed_task_kinds: Option<Vec<String>>,
}

/// Derive a conservative [`CapabilityProfile`] for an assistant-backed member
/// from its tags + description (P4 Task 2).
///
/// This is intentionally light: the description-driven LLM planner reading
/// `FleetMember::description` is the PRIMARY routing signal — the profile only
/// supplies the Router's hard-filter / tie-break baseline. So:
/// - `strengths` is seeded from keyword hits across the tags + description
///   (deduped, lowercased), giving the Router a small discriminating signal
///   when one exists; an empty `strengths` is fine (the planner still routes by
///   description).
/// - everything else is the neutral baseline: `reasoning = "medium"`,
///   `cost_tier = speed_tier = "standard"`, `tools = true` when the assistant
///   has skills (it can call them) else `false`, `modalities = []` (no
///   over-claiming of vision — the hard filter must not falsely admit a member
///   for a vision task it cannot do).
pub fn derive_capability(
    audience_tags: &[String],
    scenario_tags: &[String],
    description: Option<&str>,
    has_skills: bool,
) -> CapabilityProfile {
    // Keyword → canonical strength. Matched case-insensitively as substrings
    // against each tag and the description. Kept small + conservative.
    const KEYWORDS: &[(&str, &str)] = &[
        ("cod", "coding"),
        ("program", "coding"),
        ("develop", "coding"),
        ("write", "writing"),
        ("writ", "writing"),
        ("文案", "writing"),
        ("research", "research"),
        ("调研", "research"),
        ("search", "research"),
        ("analy", "analysis"),
        ("分析", "analysis"),
        ("data", "analysis"),
        ("design", "design"),
        ("设计", "design"),
        ("translat", "translation"),
        ("翻译", "translation"),
        ("plan", "planning"),
        ("规划", "planning"),
        ("vision", "vision"),
        ("image", "vision"),
        ("视觉", "vision"),
    ];

    let mut haystacks: Vec<String> = Vec::new();
    for t in audience_tags.iter().chain(scenario_tags.iter()) {
        haystacks.push(t.to_lowercase());
    }
    if let Some(d) = description {
        haystacks.push(d.to_lowercase());
    }

    let mut strengths: Vec<String> = Vec::new();
    for (needle, strength) in KEYWORDS {
        if haystacks.iter().any(|h| h.contains(needle)) && !strengths.iter().any(|s| s == strength) {
            strengths.push((*strength).to_string());
        }
    }

    CapabilityProfile {
        strengths,
        modalities: Vec::new(),
        tools: has_skills,
        reasoning: "medium".to_string(),
        cost_tier: "standard".to_string(),
        speed_tier: "standard".to_string(),
    }
}

/// An orchestration workspace: a named scope with an optional default fleet and
/// working directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchWorkspace {
    pub id: String,
    pub name: String,
    pub default_fleet_id: Option<String>,
    pub workspace_dir: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Create a fleet with an initial set of members.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFleetRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub max_parallel: Option<i64>,
    #[serde(default)]
    pub members: Vec<FleetMemberInput>,
}

/// Partial update of a fleet. The `Option<Option<T>>` patch fields distinguish
/// "absent" (keep current) from explicit `null` (clear) via [`double_option`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateFleetRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_parallel: Option<Option<i64>>,
    #[serde(default)]
    pub members: Option<Vec<FleetMemberInput>>,
}

/// Member payload used when creating or replacing fleet members.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetMemberInput {
    pub agent_id: String,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub role_hint: Option<String>,
    #[serde(default)]
    pub capability_profile: Option<CapabilityProfile>,
    #[serde(default)]
    pub constraints: Option<MemberConstraints>,
    #[serde(default)]
    pub sort_order: Option<i64>,
}

/// Create an orchestration workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    #[serde(default)]
    pub default_fleet_id: Option<String>,
    #[serde(default)]
    pub workspace_dir: Option<String>,
}

/// Partial update of an orchestration workspace. `default_fleet_id` uses
/// [`double_option`]: absent keeps the current binding, explicit `null` clears it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateWorkspaceRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub default_fleet_id: Option<Option<String>>,
}

// ---------------------------------------------------------------------------
// Run / Task / Plan DTOs
//
// `status` (run + task) and `source` stay as plain `String` on the wire to match
// the TEXT columns in the Row structs (`OrchRunRow`, `OrchRunTaskRow`,
// `OrchAssignmentRow`) and the project-wide convention (acp/cron/team/... all use
// `status: String`). The service layer (Task 6) maps Row↔DTO 1:1, decoding the
// JSON-as-TEXT columns (`task_profile`, `output_files`) into the structured
// shapes below.
// ---------------------------------------------------------------------------

/// An orchestration run as returned to clients: a goal plus its decomposed task
/// DAG. Mirrors [`OrchRunRow`](../../nomifun_db) minus `user_id`/`fleet_snapshot`/
/// `forked_from` (internal columns not surfaced on the wire).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    /// Owning workspace, or `None` for an ad-hoc run created straight from a
    /// conversation (such a run carries its own [`work_dir`](Self::work_dir)
    /// instead). See [`CreateAdhocRunRequest`].
    pub workspace_id: Option<String>,
    pub goal: String,
    pub autonomy: String,
    pub max_parallel: Option<i64>,
    pub status: String,
    pub summary: Option<String>,
    /// Lead/coordinator worker conversation — local `conversations.id` INTEGER.
    pub lead_conv_id: Option<i64>,
    pub total_tokens: Option<i64>,
    /// Working directory for an ad-hoc (workspace-less) run; the engine prefers
    /// this over the workspace's dir when resolving the run's cwd.
    pub work_dir: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One decomposed task within a run. Mirrors `OrchRunTaskRow` with the
/// JSON-as-TEXT columns decoded: `task_profile` (JSON → [`TaskProfile`]) and
/// `output_files` (JSON → `Vec<String>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunTask {
    pub id: String,
    pub run_id: String,
    pub title: String,
    pub spec: String,
    pub task_profile: Option<TaskProfile>,
    pub status: String,
    /// Worker conversation — local `conversations.id` INTEGER.
    pub conversation_id: Option<i64>,
    pub output_summary: Option<String>,
    pub output_files: Vec<String>,
    pub attempt: i64,
    pub tokens: Option<i64>,
    pub graph_x: Option<f64>,
    pub graph_y: Option<f64>,
    /// Short Chinese role the planner named for this task (P5 沉淀捕获). `None`
    /// for tasks planned before this column existed.
    #[serde(default)]
    pub role: Option<String>,
    /// Task mode (ultracode 模式增强, 迁移 023). `"agent"`(默认)= 现状单 agent;
    /// `"synthesis"` = 综合上游产出为最终结果。`#[serde(default = "default_task_kind")]`
    /// so legacy rows (and plans without `kind`) deserialize as `"agent"` —
    /// zero-regression.
    #[serde(default = "default_task_kind")]
    pub kind: String,
    /// Optional per-kind config as a raw JSON string (迁移 023), e.g. fan-out
    /// 兄弟任务共享的分组标签 `{"group":"<label>"}`。`None` when unused (like
    /// `task_profile`, kept as opaque JSON text so the wire/DB stay stable).
    #[serde(default)]
    pub pattern_config: Option<String>,
    /// Per-task model override (迁移 025): when BOTH set, the node dispatches with
    /// this provider×model (any available model, not just the run's fleet). `None`
    /// = follow auto-routing. Set via the 「启动前配置台」.
    #[serde(default)]
    pub override_provider_id: Option<String>,
    #[serde(default)]
    pub override_model: Option<String>,
    /// User 预置要求 (迁移 025) appended to the node's worker brief, separate from
    /// the planner-written `spec`. `None`/blank = none.
    #[serde(default)]
    pub preset_prompt: Option<String>,
    /// Creation / last-update timestamps (epoch ms). Surfaced so the UI can show
    /// per-task pacing (用时 / 相对时间) in the roster and inspector.
    pub created_at: i64,
    pub updated_at: i64,
}

/// The serde default for a task's `kind`: `"agent"` (现状单 agent，零回归).
/// Used by both [`RunTask`] and [`PlannedTask`] so legacy rows/plans without a
/// `kind` deserialize as a normal agent task.
fn default_task_kind() -> String {
    "agent".to_string()
}

/// A blocker→blocked edge in the task DAG. Mirrors `OrchRunTaskDepRow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunTaskDep {
    pub blocker_task_id: String,
    pub blocked_task_id: String,
}

/// A member assigned to a task (auto-scored or locked). Mirrors
/// `OrchAssignmentRow` with `locked` decoded from the INTEGER column to `bool`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub id: String,
    pub task_id: String,
    pub member_id: String,
    pub score: Option<f64>,
    pub rationale: Option<String>,
    pub source: String,
    pub locked: bool,
}

/// Structured capability requirements of a task, used to route it to a member.
/// Stored as JSON in the `orch_run_tasks.task_profile` TEXT column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskProfile {
    pub kind: String,
    pub needs_vision: bool,
    pub needs_long_context: bool,
    pub needs_high_reasoning: bool,
    pub bulk: bool,
}

/// A run plus its full task DAG: tasks, dependency edges, and assignments.
/// `fleet_members` is the run's frozen fleet snapshot (decoded from the run row's
/// `fleet_snapshot` JSON) so the UI can render assignment/reassign choices against
/// the exact members the run was created with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDetail {
    pub run: Run,
    pub tasks: Vec<RunTask>,
    pub deps: Vec<RunTaskDep>,
    pub assignments: Vec<Assignment>,
    pub fleet_members: Vec<FleetMember>,
}

/// Override (or lock) the member assigned to a task. `member_id` references a
/// member in the run's `fleet_snapshot`. `locked` defaults to `true` (an explicit
/// human override should survive re-planning); pass `false` to override without
/// locking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReassignRequest {
    pub member_id: String,
    #[serde(default)]
    pub locked: Option<bool>,
}

/// Steer (mid-turn inject) a message into a running task's worker conversation.
/// `text` is sent into the live turn via `ConversationService::steer_message`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteerRequest {
    pub text: String,
}

/// Fine-tune a node's intent/prompt (UC-2a "意图/prompt 微调"). The body of
/// `PATCH /api/orchestrator/runs/{run_id}/tasks/{task_id}/spec`. `spec` is the new
/// task spec (the prompt the worker brief is built from); the service rejects a
/// blank value and a task that is currently `running`. A subsequent rerun
/// re-executes the node with the amended spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpecUpdateRequest {
    pub spec: String,
}

/// 启动前配置台 (迁移 025): the body of `PATCH .../tasks/{task_id}/config`. A FULL
/// replace of the node's per-task model override + 预置要求 — the config panel
/// always sends the desired state; a `null`/blank field clears it. The model
/// override needs BOTH `override_provider_id` + `override_model` (a half-set is
/// normalized to cleared server-side). The service rejects a `running` task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConfigUpdateRequest {
    #[serde(default)]
    pub override_provider_id: Option<String>,
    #[serde(default)]
    pub override_model: Option<String>,
    #[serde(default)]
    pub preset_prompt: Option<String>,
}

/// Rename a run = change its goal. The body of `PATCH /api/orchestrator/runs/{id}`.
/// `goal` is the new goal text; the service rejects a blank value (a run goal is
/// `NOT NULL` and must not be empty). Named `RunRenameRequest` to avoid clashing
/// with the file crate's `RenameRequest` (`/api/fs/rename`) at the crate root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRenameRequest {
    pub goal: String,
}

/// Re-design a run in place. The body of `POST /api/orchestrator/runs/{id}/replan`.
/// clears the run's old plan (tasks/deps/assignments) and re-decomposes against
/// the (optionally) edited goal / model range / autonomy. `model_range` here must
/// already be `single`/`range` (an unexpanded `auto` is rejected, like create).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplanRequest {
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub model_range: Option<ModelRange>,
    #[serde(default)]
    pub autonomy: Option<String>,
    #[serde(default)]
    pub pinned_roles: Vec<String>,
}

/// Conversation-driven intelligent re-adjust (UC-3a). The body of
/// `POST /api/orchestrator/runs/{id}/adjust`. `intent` is the user's free-form
/// instruction; the lead model judges, per task, whether to KEEP the completed
/// work or re-decompose, and the service reconciles the run to the result. The
/// service rejects a blank intent (400) and a run with any `running` task (400 —
/// pause first). Unlike [`ReplanRequest`] this does NOT wipe the whole plan: it
/// preserves the work the lead chose to keep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdjustRunRequest {
    pub intent: String,
}

/// Create (and kick off) an orchestration run within a workspace against a fleet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRunRequest {
    pub workspace_id: String,
    pub goal: String,
    pub fleet_id: String,
    #[serde(default)]
    pub autonomy: Option<String>,
    #[serde(default)]
    pub max_parallel: Option<i64>,
}

/// A single provider+model pair. Used to synthesize ad-hoc fleet members
/// straight from a conversation's chosen model range (no pre-built fleet).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider_id: String,
    pub model: String,
}

/// The model range a conversation-native run may execute over. Tagged by `mode`:
/// - `single` — one fixed model (every synthetic member uses it);
/// - `range` — an explicit allow-list of models (one synthetic member each);
/// - `auto` — "pick from all enabled models". `auto` carries no inline list: the
///   expansion to a concrete `range` is done by the caps_orchestrator layer
///   (Task 3), which has provider access. `RunService::create_adhoc` therefore
///   only accepts `single` / `range` — an unexpanded `auto` is a `BadRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ModelRange {
    Single { model: ModelRef },
    Auto,
    Range { models: Vec<ModelRef> },
}

/// Create an ad-hoc orchestration run straight from a conversation: no workspace,
/// no pre-built fleet. The fleet is synthesized on the fly from `model_range`
/// (see [`ModelRange`]), and the run carries its own [`work_dir`](Self::work_dir).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAdhocRunRequest {
    pub goal: String,
    #[serde(default)]
    pub work_dir: Option<String>,
    pub model_range: ModelRange,
    /// Reserved for P4 (role pinning). Parsed but ignored in P1.
    #[serde(default)]
    pub pinned_roles: Vec<String>,
    /// Pre-constructed role members (P4 Task 2): the caps_orchestrator layer
    /// resolves each ENABLED assistant into an enriched [`FleetMember`]
    /// (persona/skills/model folded in) and passes them here. `RunService`
    /// merges these with the bare model-range members (dedup by
    /// `(provider_id, model, agent_id)`) into the run's fleet snapshot.
    /// Defaults empty so existing callers (and the workspace path) are
    /// unaffected.
    #[serde(default)]
    pub role_members: Vec<FleetMember>,
    #[serde(default)]
    pub autonomy: Option<String>,
    #[serde(default)]
    pub max_parallel: Option<i64>,
    /// Originating conversation — local `conversations.id` INTEGER.
    #[serde(default)]
    pub lead_conv_id: Option<i64>,
    /// The user-designated 主模型 (homepage「主模型+协作模型」picker): the model that
    /// should be the run's lead/planner. When set, `create_adhoc` floats the
    /// matching fleet member to the front of the snapshot so `pick_lead` selects
    /// it (the prefer-the-bare-model rule keeps it a pure model, not an assistant
    /// that happens to share the model). `None` ⇒ the engine keeps its positional
    /// default (first member) — zero behavior change for callers that omit it.
    #[serde(default)]
    pub lead_model: Option<ModelRef>,
}

/// One planned task in a [`PlannedDag`]. Produced by the PlanProducer (主管规划)
/// and accepted as the `nomi_run_plan` gateway input. Dependencies and member
/// assignment reference other tasks/members by 0-based index because no ids exist
/// yet at planning time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedTask {
    pub title: String,
    pub spec: String,
    #[serde(default)]
    pub task_profile: Option<TaskProfile>,
    /// 0-based indices into [`PlannedDag::tasks`] this task depends on.
    #[serde(default)]
    pub depends_on: Vec<usize>,
    /// 0-based index into the fleet's members, if pre-assigned.
    #[serde(default)]
    pub member_index: Option<usize>,
    #[serde(default)]
    pub rationale: Option<String>,
    /// Short Chinese role the planner named for this task (e.g. 规划/前端/后端/
    /// 测试/设计/文档). Persisted onto the task (P5 沉淀捕获) so a later UI can
    /// distill the roles a run used into reusable assistants. `#[serde(default)]`
    /// so old plans without it deserialize to `None`.
    #[serde(default)]
    pub role: Option<String>,
    /// Task mode the planner chose (ultracode 模式增强, 迁移 023):
    /// - `"agent"`(默认)— 一个 agent 执行一个任务(现状)。
    /// - `"synthesis"` — 把依赖任务的输出综合/合并为最终结果。
    ///
    /// `#[serde(default = "default_task_kind")]` so a plan without `kind`
    /// (legacy / fallback) deserializes as `"agent"` — zero-regression. An
    /// unknown value is also accepted as-is here; the engine treats anything
    /// other than the known kinds as `"agent"` (fail-soft).
    #[serde(default = "default_task_kind")]
    pub kind: String,
    /// Optional per-kind config as raw JSON text. Fan-out is expressed via
    /// PLANNING (not a new kind): sibling `agent` tasks share a group tag
    /// `{"group":"<label>"}` here, and a downstream task depends on all of them.
    /// `None` when the task needs no extra config.
    #[serde(default)]
    pub pattern_config: Option<String>,
}

/// The planned task DAG: the PlanProducer output / `nomi_run_plan` input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedDag {
    pub tasks: Vec<PlannedTask>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_fleet_request_round_trips() {
        let req = CreateFleetRequest {
            name: "research-fleet".to_string(),
            description: Some("multi-agent research".to_string()),
            max_parallel: Some(3),
            members: vec![FleetMemberInput {
                agent_id: "agent_abc".to_string(),
                provider_id: Some("provider_xyz".to_string()),
                model: Some("claude-opus".to_string()),
                role_hint: Some("lead".to_string()),
                capability_profile: Some(CapabilityProfile {
                    strengths: vec!["analysis".to_string(), "writing".to_string()],
                    modalities: vec!["text".to_string()],
                    tools: true,
                    reasoning: "high".to_string(),
                    cost_tier: "premium".to_string(),
                    speed_tier: "medium".to_string(),
                }),
                constraints: Some(MemberConstraints {
                    max_concurrency: Some(2),
                    cost_tier: Some("premium".to_string()),
                    allowed_task_kinds: Some(vec!["research".to_string()]),
                }),
                sort_order: Some(0),
            }],
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let back: CreateFleetRequest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.name, "research-fleet");
        assert_eq!(back.description.as_deref(), Some("multi-agent research"));
        assert_eq!(back.max_parallel, Some(3));
        assert_eq!(back.members.len(), 1);

        let member = &back.members[0];
        assert_eq!(member.agent_id, "agent_abc");
        assert_eq!(member.provider_id.as_deref(), Some("provider_xyz"));
        assert_eq!(member.model.as_deref(), Some("claude-opus"));
        assert_eq!(member.role_hint.as_deref(), Some("lead"));
        assert_eq!(member.sort_order, Some(0));

        let profile = member.capability_profile.as_ref().expect("profile");
        assert_eq!(profile.strengths, vec!["analysis", "writing"]);
        assert_eq!(profile.modalities, vec!["text"]);
        assert!(profile.tools);
        assert_eq!(profile.reasoning, "high");
        assert_eq!(profile.cost_tier, "premium");
        assert_eq!(profile.speed_tier, "medium");

        let constraints = member.constraints.as_ref().expect("constraints");
        assert_eq!(constraints.max_concurrency, Some(2));
        assert_eq!(constraints.cost_tier.as_deref(), Some("premium"));
        assert_eq!(
            constraints.allowed_task_kinds.as_ref().map(|v| v.as_slice()),
            Some(["research".to_string()].as_slice())
        );
    }

    #[test]
    fn update_fleet_request_distinguishes_clear_from_absent() {
        // Explicit null => clear (present-as-null): Some(None).
        let clear: UpdateFleetRequest =
            serde_json::from_str(r#"{"description": null}"#).expect("deserialize clear");
        assert_eq!(clear.description, Some(None), "explicit null must be Some(None)");
        // Other fields absent => None (keep).
        assert_eq!(clear.name, None);
        assert_eq!(clear.max_parallel, None);
        assert!(clear.members.is_none());

        // Key absent => keep current: None.
        let keep: UpdateFleetRequest = serde_json::from_str(r#"{}"#).expect("deserialize keep");
        assert_eq!(keep.description, None, "absent key must be None");

        // Explicit value => set: Some(Some(v)).
        let set: UpdateFleetRequest =
            serde_json::from_str(r#"{"description": "new"}"#).expect("deserialize set");
        assert_eq!(set.description, Some(Some("new".to_string())));

        // max_parallel patch semantics.
        let clear_mp: UpdateFleetRequest =
            serde_json::from_str(r#"{"max_parallel": null}"#).expect("deserialize clear mp");
        assert_eq!(clear_mp.max_parallel, Some(None));
        let set_mp: UpdateFleetRequest =
            serde_json::from_str(r#"{"max_parallel": 5}"#).expect("deserialize set mp");
        assert_eq!(set_mp.max_parallel, Some(Some(5)));
    }

    #[test]
    fn update_workspace_request_clears_default_fleet() {
        let clear: UpdateWorkspaceRequest =
            serde_json::from_str(r#"{"default_fleet_id": null}"#).expect("deserialize clear");
        assert_eq!(clear.default_fleet_id, Some(None));

        let keep: UpdateWorkspaceRequest =
            serde_json::from_str(r#"{}"#).expect("deserialize keep");
        assert_eq!(keep.default_fleet_id, None);
    }

    #[test]
    fn planned_dag_round_trips() {
        let dag = PlannedDag {
            tasks: vec![
                PlannedTask {
                    title: "Gather sources".to_string(),
                    spec: "Search the web for primary sources.".to_string(),
                    task_profile: Some(TaskProfile {
                        kind: "research".to_string(),
                        needs_vision: false,
                        needs_long_context: true,
                        needs_high_reasoning: false,
                        bulk: true,
                    }),
                    depends_on: vec![],
                    member_index: None,
                    rationale: Some("breadth-first collection".to_string()),
                    role: Some("调研".to_string()),
                    kind: "agent".to_string(),
                    pattern_config: None,
                },
                PlannedTask {
                    title: "Synthesize report".to_string(),
                    spec: "Write a cited synthesis from the gathered sources.".to_string(),
                    task_profile: None,
                    depends_on: vec![0],
                    member_index: Some(1),
                    rationale: None,
                    role: None,
                    kind: "synthesis".to_string(),
                    pattern_config: None,
                },
            ],
        };

        let json = serde_json::to_string(&dag).expect("serialize");
        let back: PlannedDag = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.tasks.len(), 2);
        assert_eq!(back.tasks[0].title, "Gather sources");
        assert!(back.tasks[0].depends_on.is_empty());
        assert_eq!(back.tasks[0].member_index, None);
        assert_eq!(back.tasks[0].role.as_deref(), Some("调研"));
        assert_eq!(back.tasks[0].kind, "agent", "default agent kind round-trips");
        assert!(back.tasks[0].pattern_config.is_none());
        let profile = back.tasks[0].task_profile.as_ref().expect("profile");
        assert_eq!(profile.kind, "research");
        assert!(profile.needs_long_context);
        assert!(profile.bulk);
        assert!(!profile.needs_vision);

        assert_eq!(back.tasks[1].title, "Synthesize report");
        assert_eq!(back.tasks[1].depends_on, vec![0]);
        assert_eq!(back.tasks[1].member_index, Some(1));
        assert!(back.tasks[1].task_profile.is_none());
        assert_eq!(back.tasks[1].rationale, None);
        assert_eq!(back.tasks[1].role, None);
        assert_eq!(back.tasks[1].kind, "synthesis", "synthesis kind round-trips");
    }

    #[test]
    fn create_run_request_round_trips() {
        let req = CreateRunRequest {
            workspace_id: "ws_abc".to_string(),
            goal: "Research the orchestration market.".to_string(),
            fleet_id: "fleet_xyz".to_string(),
            autonomy: Some("supervised".to_string()),
            max_parallel: Some(4),
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let back: CreateRunRequest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.workspace_id, "ws_abc");
        assert_eq!(back.goal, "Research the orchestration market.");
        assert_eq!(back.fleet_id, "fleet_xyz");
        assert_eq!(back.autonomy.as_deref(), Some("supervised"));
        assert_eq!(back.max_parallel, Some(4));

        // Optional fields default when absent.
        let minimal: CreateRunRequest = serde_json::from_str(
            r#"{"workspace_id":"ws_1","goal":"g","fleet_id":"f_1"}"#,
        )
        .expect("deserialize minimal");
        assert_eq!(minimal.autonomy, None);
        assert_eq!(minimal.max_parallel, None);
    }

    #[test]
    fn model_range_tagged_round_trips() {
        // `single` carries one model.
        let single: ModelRange = serde_json::from_str(
            r#"{"mode":"single","model":{"provider_id":"p1","model":"m1"}}"#,
        )
        .expect("single");
        match single {
            ModelRange::Single { model } => {
                assert_eq!(model.provider_id, "p1");
                assert_eq!(model.model, "m1");
            }
            other => panic!("expected single, got {other:?}"),
        }

        // `auto` is a bare tag (no payload).
        let auto: ModelRange = serde_json::from_str(r#"{"mode":"auto"}"#).expect("auto");
        assert!(matches!(auto, ModelRange::Auto));

        // `range` carries an explicit allow-list.
        let range: ModelRange = serde_json::from_str(
            r#"{"mode":"range","models":[{"provider_id":"p1","model":"m1"},{"provider_id":"p2","model":"m2"}]}"#,
        )
        .expect("range");
        match range {
            ModelRange::Range { models } => assert_eq!(models.len(), 2),
            other => panic!("expected range, got {other:?}"),
        }
    }

    #[test]
    fn create_adhoc_run_request_round_trips() {
        // Full payload.
        let req = CreateAdhocRunRequest {
            goal: "Build the thing.".to_string(),
            work_dir: Some("/tmp/wd".to_string()),
            model_range: ModelRange::Range {
                models: vec![
                    ModelRef { provider_id: "p1".to_string(), model: "m1".to_string() },
                    ModelRef { provider_id: "p2".to_string(), model: "m2".to_string() },
                ],
            },
            pinned_roles: vec!["lead".to_string()],
            role_members: vec![],
            autonomy: Some("supervised".to_string()),
            max_parallel: Some(3),
            lead_conv_id: Some(77),
            lead_model: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: CreateAdhocRunRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.goal, "Build the thing.");
        assert_eq!(back.work_dir.as_deref(), Some("/tmp/wd"));
        assert_eq!(back.lead_conv_id, Some(77));
        assert_eq!(back.max_parallel, Some(3));
        assert_eq!(back.pinned_roles, vec!["lead"]);
        assert!(matches!(back.model_range, ModelRange::Range { .. }));

        // Minimal payload: only goal + model_range; the rest default.
        let minimal: CreateAdhocRunRequest = serde_json::from_str(
            r#"{"goal":"g","model_range":{"mode":"single","model":{"provider_id":"p","model":"m"}}}"#,
        )
        .expect("deserialize minimal");
        assert_eq!(minimal.goal, "g");
        assert!(minimal.work_dir.is_none());
        assert!(minimal.pinned_roles.is_empty());
        assert!(minimal.autonomy.is_none());
        assert!(minimal.max_parallel.is_none());
        assert!(minimal.lead_conv_id.is_none());
        assert!(matches!(minimal.model_range, ModelRange::Single { .. }));
    }

    #[test]
    fn run_detail_round_trips() {
        let detail = RunDetail {
            run: Run {
                id: "run_1".to_string(),
                workspace_id: Some("ws_abc".to_string()),
                goal: "Research and synthesize.".to_string(),
                autonomy: "supervised".to_string(),
                max_parallel: Some(2),
                status: "running".to_string(),
                summary: Some("in progress".to_string()),
                lead_conv_id: Some(101),
                total_tokens: Some(4242),
                work_dir: None,
                created_at: 1_700_000_000_000,
                updated_at: 1_700_000_500_000,
            },
            tasks: vec![
                RunTask {
                    id: "task_1".to_string(),
                    run_id: "run_1".to_string(),
                    title: "Gather".to_string(),
                    spec: "collect".to_string(),
                    task_profile: Some(TaskProfile {
                        kind: "research".to_string(),
                        needs_vision: false,
                        needs_long_context: true,
                        needs_high_reasoning: true,
                        bulk: false,
                    }),
                    status: "done".to_string(),
                    conversation_id: Some(201),
                    output_summary: Some("found 12 sources".to_string()),
                    output_files: vec!["sources.md".to_string(), "notes.txt".to_string()],
                    attempt: 1,
                    tokens: Some(1234),
                    graph_x: Some(10.5),
                    graph_y: Some(20.0),
                    role: Some("研究".to_string()),
                    kind: "synthesis".to_string(),
                    pattern_config: Some("{\"group\":\"variants\"}".to_string()),
                    override_provider_id: None,
                    override_model: None,
                    preset_prompt: None,
                    created_at: 1_700_000_000_000,
                    updated_at: 1_700_000_400_000,
                },
                RunTask {
                    id: "task_2".to_string(),
                    run_id: "run_1".to_string(),
                    title: "Synthesize".to_string(),
                    spec: "write".to_string(),
                    task_profile: None,
                    status: "pending".to_string(),
                    conversation_id: None,
                    output_summary: None,
                    output_files: vec![],
                    attempt: 0,
                    tokens: None,
                    graph_x: None,
                    graph_y: None,
                    role: None,
                    kind: "agent".to_string(),
                    pattern_config: None,
                    override_provider_id: None,
                    override_model: None,
                    preset_prompt: None,
                    created_at: 1_700_000_000_000,
                    updated_at: 1_700_000_000_000,
                },
            ],
            deps: vec![RunTaskDep {
                blocker_task_id: "task_1".to_string(),
                blocked_task_id: "task_2".to_string(),
            }],
            assignments: vec![Assignment {
                id: "asg_1".to_string(),
                task_id: "task_1".to_string(),
                member_id: "fm_1".to_string(),
                score: Some(0.87),
                rationale: Some("best at research".to_string()),
                source: "auto".to_string(),
                locked: false,
            }],
            fleet_members: vec![FleetMember {
                id: "fm_1".to_string(),
                agent_id: "agent_a".to_string(),
                provider_id: None,
                model: None,
                role_hint: None,
                capability_profile: None,
                constraints: None,
                sort_order: 0,
                description: None,
                system_prompt: None,
                enabled_skills: vec![],
                disabled_builtin_skills: vec![],
            }],
        };

        let json = serde_json::to_string(&detail).expect("serialize");
        let back: RunDetail = serde_json::from_str(&json).expect("deserialize");

        // Run.
        assert_eq!(back.run.id, "run_1");
        assert_eq!(back.run.workspace_id.as_deref(), Some("ws_abc"));
        assert_eq!(back.run.autonomy, "supervised");
        assert_eq!(back.run.max_parallel, Some(2));
        assert_eq!(back.run.status, "running");
        assert_eq!(back.run.summary.as_deref(), Some("in progress"));
        assert_eq!(back.run.lead_conv_id, Some(101));
        assert_eq!(back.run.total_tokens, Some(4242));
        assert_eq!(back.run.created_at, 1_700_000_000_000);
        assert_eq!(back.run.updated_at, 1_700_000_500_000);

        // Tasks.
        assert_eq!(back.tasks.len(), 2);
        let t1 = &back.tasks[0];
        assert_eq!(t1.id, "task_1");
        assert_eq!(t1.run_id, "run_1");
        assert_eq!(t1.status, "done");
        assert_eq!(t1.conversation_id, Some(201));
        assert_eq!(t1.output_files, vec!["sources.md", "notes.txt"]);
        assert_eq!(t1.attempt, 1);
        assert_eq!(t1.tokens, Some(1234));
        assert_eq!(t1.graph_x, Some(10.5));
        assert_eq!(t1.graph_y, Some(20.0));
        let t1_profile = t1.task_profile.as_ref().expect("t1 profile");
        assert!(t1_profile.needs_high_reasoning);
        assert!(!t1_profile.bulk);
        // kind + pattern_config round-trip (synthesis task carrying a group tag).
        assert_eq!(t1.kind, "synthesis");
        assert_eq!(t1.pattern_config.as_deref(), Some("{\"group\":\"variants\"}"));

        let t2 = &back.tasks[1];
        assert_eq!(t2.id, "task_2");
        assert_eq!(t2.status, "pending");
        assert_eq!(t2.conversation_id, None);
        assert!(t2.output_files.is_empty());
        assert!(t2.task_profile.is_none());
        assert_eq!(t2.attempt, 0);
        // Default agent kind round-trips; no pattern_config.
        assert_eq!(t2.kind, "agent");
        assert!(t2.pattern_config.is_none());

        // Deps.
        assert_eq!(back.deps.len(), 1);
        assert_eq!(back.deps[0].blocker_task_id, "task_1");
        assert_eq!(back.deps[0].blocked_task_id, "task_2");

        // Assignments.
        assert_eq!(back.assignments.len(), 1);
        let asg = &back.assignments[0];
        assert_eq!(asg.id, "asg_1");
        assert_eq!(asg.task_id, "task_1");
        assert_eq!(asg.member_id, "fm_1");
        assert_eq!(asg.score, Some(0.87));
        assert_eq!(asg.rationale.as_deref(), Some("best at research"));
        assert_eq!(asg.source, "auto");
        assert!(!asg.locked);

        // Fleet members snapshot.
        assert_eq!(back.fleet_members.len(), 1);
        assert_eq!(back.fleet_members[0].id, "fm_1");
        assert_eq!(back.fleet_members[0].agent_id, "agent_a");
    }

    #[test]
    fn reassign_request_locked_defaults_absent() {
        // Explicit value round-trips.
        let req = ReassignRequest {
            member_id: "fm_2".to_string(),
            locked: Some(false),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: ReassignRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.member_id, "fm_2");
        assert_eq!(back.locked, Some(false));

        // `locked` absent => None (the service decides the default, i.e. true).
        let minimal: ReassignRequest =
            serde_json::from_str(r#"{"member_id":"fm_9"}"#).expect("deserialize minimal");
        assert_eq!(minimal.member_id, "fm_9");
        assert_eq!(minimal.locked, None);
    }

    // ── P4 Task 2: FleetMember enrichment + role_members + derive_capability ──

    /// An OLD fleet snapshot JSON (no `description` / `system_prompt` /
    /// `enabled_skills` / `disabled_builtin_skills`) must still deserialize, with
    /// the four enrichment fields taking their serde defaults. This is the
    /// back-compat invariant: existing persisted runs must keep loading.
    #[test]
    fn fleet_member_old_snapshot_deserializes_with_defaults() {
        let old = r#"{
            "id": "fm_1",
            "agent_id": "",
            "provider_id": "p1",
            "model": "m1",
            "role_hint": null,
            "capability_profile": null,
            "constraints": null,
            "sort_order": 0
        }"#;
        let m: FleetMember = serde_json::from_str(old).expect("old snapshot must deserialize");
        assert_eq!(m.id, "fm_1");
        assert_eq!(m.provider_id.as_deref(), Some("p1"));
        // The four enrichment fields default.
        assert_eq!(m.description, None);
        assert_eq!(m.system_prompt, None);
        assert!(m.enabled_skills.is_empty());
        assert!(m.disabled_builtin_skills.is_empty());
    }

    /// A fully-enriched member round-trips through JSON unchanged.
    #[test]
    fn fleet_member_enriched_round_trips() {
        let m = FleetMember {
            id: "rmbr_1".to_string(),
            agent_id: "asst_research".to_string(),
            provider_id: Some("p1".to_string()),
            model: Some("m1".to_string()),
            role_hint: Some("研究员".to_string()),
            capability_profile: None,
            constraints: None,
            sort_order: 0,
            description: Some("善于多源调研".to_string()),
            system_prompt: Some("你是一名严谨的研究员".to_string()),
            enabled_skills: vec!["web_search".to_string()],
            disabled_builtin_skills: vec!["browser".to_string()],
        };
        let json = serde_json::to_string(&m).expect("serialize");
        let back: FleetMember = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.agent_id, "asst_research");
        assert_eq!(back.role_hint.as_deref(), Some("研究员"));
        assert_eq!(back.description.as_deref(), Some("善于多源调研"));
        assert_eq!(back.system_prompt.as_deref(), Some("你是一名严谨的研究员"));
        assert_eq!(back.enabled_skills, vec!["web_search"]);
        assert_eq!(back.disabled_builtin_skills, vec!["browser"]);
    }

    /// `CreateAdhocRunRequest::role_members` defaults to empty when absent (the
    /// existing minimal payload must keep parsing) and carries enriched members
    /// when present.
    #[test]
    fn create_adhoc_role_members_default_and_round_trip() {
        // Absent → empty (back-compat with the P1 minimal payload).
        let minimal: CreateAdhocRunRequest = serde_json::from_str(
            r#"{"goal":"g","model_range":{"mode":"single","model":{"provider_id":"p","model":"m"}}}"#,
        )
        .expect("minimal still parses");
        assert!(minimal.role_members.is_empty(), "role_members defaults empty");

        // Present → carried through.
        let req = CreateAdhocRunRequest {
            goal: "g".to_string(),
            work_dir: None,
            model_range: ModelRange::Single { model: ModelRef { provider_id: "p".to_string(), model: "m".to_string() } },
            pinned_roles: vec![],
            role_members: vec![FleetMember {
                id: "rmbr_x".to_string(),
                agent_id: "asst_x".to_string(),
                provider_id: Some("p".to_string()),
                model: Some("m".to_string()),
                role_hint: Some("X".to_string()),
                capability_profile: None,
                constraints: None,
                sort_order: 0,
                description: Some("d".to_string()),
                system_prompt: Some("persona".to_string()),
                enabled_skills: vec!["s".to_string()],
                disabled_builtin_skills: vec![],
            }],
            autonomy: None,
            max_parallel: None,
            lead_conv_id: None,
            lead_model: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: CreateAdhocRunRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.role_members.len(), 1);
        assert_eq!(back.role_members[0].agent_id, "asst_x");
        assert_eq!(back.role_members[0].system_prompt.as_deref(), Some("persona"));
    }

    /// `derive_capability` maps tag/description keywords to strengths and applies
    /// the conservative baseline for everything else.
    #[test]
    fn derive_capability_maps_keywords_and_baseline() {        // Scenario tag "coding-help" + description mentioning research → both
        // strengths picked up; tools=true because the assistant has skills.
        let prof = derive_capability(
            &["developer".to_string()],
            &["coding-help".to_string()],
            Some("Great at research and 分析 tasks"),
            true,
        );
        assert!(prof.strengths.contains(&"coding".to_string()), "coding from tags: {:?}", prof.strengths);
        assert!(prof.strengths.contains(&"research".to_string()), "research from desc: {:?}", prof.strengths);
        assert!(prof.strengths.contains(&"analysis".to_string()), "analysis (分析) from desc: {:?}", prof.strengths);
        assert!(prof.tools, "has skills → tools true");
        assert_eq!(prof.reasoning, "medium");
        assert_eq!(prof.cost_tier, "standard");
        assert_eq!(prof.speed_tier, "standard");
        assert!(prof.modalities.is_empty(), "no over-claimed modalities");

        // No keywords + no skills → empty strengths, tools false, still baseline.
        let bare = derive_capability(&[], &[], None, false);
        assert!(bare.strengths.is_empty());
        assert!(!bare.tools, "no skills → tools false");
        assert_eq!(bare.reasoning, "medium");
    }

    // ── P5 Task 1: PlannedTask.role (沉淀捕获) ──

    /// A `PlannedTask` carrying a `role` deserializes it; an old plan JSON without
    /// `role` defaults it to `None` (back-compat: existing plans must keep parsing).
    #[test]
    fn planned_task_role_deserializes_and_defaults() {
        // Present → Some.
        let with_role: PlannedTask = serde_json::from_str(
            r#"{"title":"做前端","spec":"实现页面","role":"前端"}"#,
        )
        .expect("planned task with role parses");
        assert_eq!(with_role.role.as_deref(), Some("前端"));

        // Absent → None (serde default).
        let no_role: PlannedTask = serde_json::from_str(
            r#"{"title":"做前端","spec":"实现页面"}"#,
        )
        .expect("planned task without role parses");
        assert_eq!(no_role.role, None, "absent role defaults to None");

        // Round-trips through JSON unchanged.
        let json = serde_json::to_string(&with_role).expect("serialize");
        let back: PlannedTask = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.role.as_deref(), Some("前端"));
    }

    // ── 迁移 023: PlannedTask/RunTask.kind + pattern_config (ultracode 模式增强) ──

    /// A `PlannedTask` carrying `kind:"synthesis"` + a fan-out group `pattern_config`
    /// deserializes them; an old plan JSON WITHOUT `kind` defaults to `"agent"`
    /// (zero-regression: existing plans must keep parsing as a normal agent task).
    #[test]
    fn planned_task_kind_deserializes_and_defaults_to_agent() {
        // Explicit synthesis kind + a pattern_config blob → carried through.
        let synth: PlannedTask = serde_json::from_str(
            r#"{"title":"汇总","spec":"合并以上产出","kind":"synthesis","pattern_config":"{\"group\":\"shards\"}"}"#,
        )
        .expect("synthesis plan parses");
        assert_eq!(synth.kind, "synthesis");
        assert_eq!(synth.pattern_config.as_deref(), Some("{\"group\":\"shards\"}"));

        // A legacy plan WITHOUT `kind`/`pattern_config` → kind defaults to "agent",
        // pattern_config to None. This is the zero-regression invariant.
        let legacy: PlannedTask = serde_json::from_str(
            r#"{"title":"做事","spec":"执行","depends_on":[],"member_index":0}"#,
        )
        .expect("legacy plan without kind parses");
        assert_eq!(legacy.kind, "agent", "missing kind must default to agent");
        assert!(legacy.pattern_config.is_none());

        // Round-trips unchanged.
        let json = serde_json::to_string(&synth).expect("serialize");
        let back: PlannedTask = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.kind, "synthesis");
        assert_eq!(back.pattern_config.as_deref(), Some("{\"group\":\"shards\"}"));
    }

    /// A whole legacy `PlannedDag` JSON (no `kind` on any task) deserializes with
    /// every task defaulting to `"agent"` — the engine's agent path stays the
    /// default for any pre-023 plan.
    #[test]
    fn legacy_plan_dag_without_kind_all_tasks_default_agent() {
        let raw = r#"{"tasks":[
            {"title":"A","spec":"do A","depends_on":[]},
            {"title":"B","spec":"do B","depends_on":[0]}
        ]}"#;
        let dag: PlannedDag = serde_json::from_str(raw).expect("legacy dag parses");
        assert_eq!(dag.tasks.len(), 2);
        for t in &dag.tasks {
            assert_eq!(t.kind, "agent", "every legacy task defaults to agent kind");
            assert!(t.pattern_config.is_none());
        }
    }
}
